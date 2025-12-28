//! NATS ledger_state snapshot client
//!
//! Provides cached access to the latest ledger state snapshot from NATS.
//! Used for collapsed updates and first-free index queries.

use async_nats::Client as NatsClient;
use futures::StreamExt;
use serde::Deserialize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

/// Default snapshot cache TTL (freshness window)
const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(30);

/// NATS subject for ledger state snapshots
const LEDGER_STATE_SUBJECT: &str = "ledger_state";

/// Ledger state snapshot from NATS
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LedgerStateSnapshot {
    /// Block height this snapshot is valid for
    pub block_height: i64,
    /// Protocol version at this height
    pub protocol_version: u32,
    /// Merkle tree root hash (hex encoded)
    pub merkle_root: String,
    /// First free index in the commitment tree
    pub first_free_index: u64,
    /// Total number of commitments
    pub commitment_count: u64,
    /// Timestamp when snapshot was created (Unix millis)
    pub timestamp_ms: i64,
}

/// Cached snapshot with metadata
struct CachedSnapshot {
    snapshot: LedgerStateSnapshot,
    fetched_at: Instant,
}

/// Client for accessing ledger state snapshots via NATS
#[derive(Clone)]
pub struct SnapshotClient {
    nats_client: NatsClient,
    cache: Arc<RwLock<Option<CachedSnapshot>>>,
    cache_ttl: Duration,
}

impl SnapshotClient {
    /// Create a new snapshot client
    pub fn new(nats_client: NatsClient) -> Self {
        Self {
            nats_client,
            cache: Arc::new(RwLock::new(None)),
            cache_ttl: DEFAULT_CACHE_TTL,
        }
    }

    /// Create a new snapshot client with custom cache TTL
    pub fn with_cache_ttl(nats_client: NatsClient, cache_ttl: Duration) -> Self {
        Self {
            nats_client,
            cache: Arc::new(RwLock::new(None)),
            cache_ttl,
        }
    }

    /// Get the latest cached snapshot, only if it's still within the freshness TTL.
    ///
    /// This method never blocks on a NATS request; it only returns what the background
    /// subscriber has already populated.
    pub async fn get_cached_snapshot(&self) -> Option<LedgerStateSnapshot> {
        let cache = self.cache.read().await;
        if let Some(cached) = cache.as_ref() {
            if cached.fetched_at.elapsed() < self.cache_ttl {
                debug!(
                    block_height = cached.snapshot.block_height,
                    age_ms = cached.fetched_at.elapsed().as_millis(),
                    "Returning cached snapshot"
                );
                return Some(cached.snapshot.clone());
            }
        }
        None
    }

    /// Get the latest snapshot, using cache if fresh, otherwise fetching via NATS request-reply.
    pub async fn get_snapshot(&self) -> Option<LedgerStateSnapshot> {
        if let Some(snapshot) = self.get_cached_snapshot().await {
            return Some(snapshot);
        }

        // Cache miss or stale, fetch fresh snapshot
        self.fetch_and_cache_snapshot().await
    }

    /// Force refresh the snapshot cache
    pub async fn refresh(&self) -> Option<LedgerStateSnapshot> {
        self.fetch_and_cache_snapshot().await
    }

    /// Get the first free index from the latest snapshot
    pub async fn get_first_free_index(&self) -> Option<u64> {
        self.get_snapshot().await.map(|s| s.first_free_index)
    }

    /// Check if a snapshot is fresh enough for the given block height
    pub async fn is_fresh_for_height(&self, min_height: i64) -> bool {
        match self.get_snapshot().await {
            Some(snapshot) => snapshot.block_height >= min_height,
            None => false,
        }
    }

    /// Fetch a fresh snapshot from NATS and update cache
    async fn fetch_and_cache_snapshot(&self) -> Option<LedgerStateSnapshot> {
        debug!("Fetching fresh ledger state snapshot from NATS");

        // Request latest snapshot from NATS (request-reply pattern)
        let response = match tokio::time::timeout(
            Duration::from_secs(5),
            self.nats_client.request(LEDGER_STATE_SUBJECT, "".into()),
        )
        .await
        {
            Ok(Ok(msg)) => msg,
            Ok(Err(e)) => {
                warn!(error = %e, "Failed to request ledger state snapshot from NATS");
                return None;
            }
            Err(_) => {
                warn!("Timeout requesting ledger state snapshot from NATS");
                return None;
            }
        };

        // Parse the snapshot
        let snapshot: LedgerStateSnapshot = match serde_json::from_slice(&response.payload) {
            Ok(s) => s,
            Err(e) => {
                error!(error = %e, "Failed to parse ledger state snapshot");
                return None;
            }
        };

        info!(
            block_height = snapshot.block_height,
            first_free_index = snapshot.first_free_index,
            "Fetched fresh ledger state snapshot"
        );

        // Update cache
        {
            let mut cache = self.cache.write().await;
            *cache = Some(CachedSnapshot {
                snapshot: snapshot.clone(),
                fetched_at: Instant::now(),
            });
        }

        Some(snapshot)
    }

    /// Start background subscription to snapshot updates
    /// This keeps the cache warm by processing incoming snapshots
    pub async fn start_background_subscriber(self: Arc<Self>) {
        let client = self.clone();
        tokio::spawn(async move {
            client.background_subscriber_loop().await;
        });
    }

    /// Background loop that subscribes to snapshot updates
    async fn background_subscriber_loop(&self) {
        loop {
            match self.nats_client.subscribe(LEDGER_STATE_SUBJECT.to_string()).await {
                Ok(mut subscriber) => {
                    info!("Subscribed to ledger state snapshots");

                    while let Some(msg) = subscriber.next().await {
                        // Ignore request messages (they have a reply subject) and empty payloads.
                        // Without this, request-reply probes to `ledger_state` can be seen by the
                        // subscriber and incorrectly treated as snapshot updates.
                        if msg.reply.is_some() || msg.payload.is_empty() {
                            continue;
                        }

                        match serde_json::from_slice::<LedgerStateSnapshot>(&msg.payload) {
                            Ok(snapshot) => {
                                debug!(
                                    block_height = snapshot.block_height,
                                    "Received ledger state snapshot update"
                                );

                                // Update cache
                                let mut cache = self.cache.write().await;
                                *cache = Some(CachedSnapshot {
                                    snapshot,
                                    fetched_at: Instant::now(),
                                });
                            }
                            Err(e) => {
                                warn!(error = %e, "Failed to parse snapshot update");
                            }
                        }
                    }

                    warn!("Ledger state subscription ended, reconnecting...");
                }
                Err(e) => {
                    error!(error = %e, "Failed to subscribe to ledger state snapshots");
                }
            }

            // Wait before reconnecting
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_snapshot_deserialization() {
        let json = r#"{
            "blockHeight": 12345,
            "protocolVersion": 2,
            "merkleRoot": "abc123",
            "firstFreeIndex": 1000,
            "commitmentCount": 999,
            "timestampMs": 1703123456789
        }"#;

        let snapshot: LedgerStateSnapshot = serde_json::from_str(json).unwrap();
        assert_eq!(snapshot.block_height, 12345);
        assert_eq!(snapshot.protocol_version, 2);
        assert_eq!(snapshot.first_free_index, 1000);
    }
}
