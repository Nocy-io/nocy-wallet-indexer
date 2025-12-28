//! Background Merkle Cache Manager
//!
//! Maintains a persistent subscription to the upstream indexer's shieldedTransactions
//! and caches all MerkleTreeCollapsedUpdate events in the database for serving
//! historical merkle update requests.
//!
//! ## Strategy
//!
//! - Uses a random viewing key that matches NO transactions
//! - This causes all updates to be emitted as MerkleTreeCollapsedUpdate (not RelevantTransaction)
//! - Caches updates in nocy_sidecar.collapsed_merkle_cache table
//! - Tracks highest cached index for subscription resumption after restart

use bech32::{ToBase32, Variant};
use futures::{SinkExt, StreamExt};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        client::IntoClientRequest,
        http::{header::SEC_WEBSOCKET_PROTOCOL, HeaderValue},
        protocol::Message,
    },
};
use tracing::{debug, error, info, warn};

use crate::error::AppError;
use crate::graphql_ws::CollapsedMerkleUpdate;
use crate::upstream::UpstreamClient;

const VIEWING_KEY_HRP_UNDEPLOYED: &str = "mn_shield-esk_undeployed";

// === GraphQL WebSocket Protocol Types (shared with graphql_ws.rs) ===

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum ClientMessage {
    #[serde(rename = "connection_init")]
    ConnectionInit { payload: serde_json::Value },
    #[serde(rename = "subscribe")]
    Subscribe { id: String, payload: SubscribePayload },
    #[serde(rename = "complete")]
    Complete { id: String },
}

#[derive(Debug, Serialize)]
struct SubscribePayload {
    query: String,
    variables: serde_json::Value,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ServerMessage {
    #[serde(rename = "connection_ack")]
    ConnectionAck,
    #[serde(rename = "next")]
    Next { id: String, payload: NextPayload },
    #[serde(rename = "error")]
    Error { id: String, payload: Vec<GraphQLError> },
    #[serde(rename = "complete")]
    Complete { id: String },
    #[serde(rename = "ping")]
    Ping,
    #[serde(rename = "pong")]
    Pong,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct NextPayload {
    data: Option<ShieldedTransactionsData>,
    errors: Option<Vec<GraphQLError>>,
}

#[derive(Debug, Deserialize)]
struct GraphQLError {
    message: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShieldedTransactionsData {
    shielded_transactions: ShieldedTransactionsEvent,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "__typename")]
enum ShieldedTransactionsEvent {
    RelevantTransaction(RelevantTransaction),
    ShieldedTransactionsProgress(ShieldedTransactionsProgress),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RelevantTransaction {
    collapsed_merkle_tree: Option<CollapsedMerkleTree>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CollapsedMerkleTree {
    start_index: i64,
    end_index: i64,
    update: String,
    protocol_version: i32,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShieldedTransactionsProgress {
    highest_end_index: i64,
    highest_checked_end_index: i64,
    highest_relevant_end_index: i64,
}

// === Cached Update Structure ===

/// A collapsed merkle update retrieved from cache
#[derive(Debug, Clone)]
pub struct CachedMerkleUpdate {
    pub start_index: i64,
    pub end_index: i64,
    pub update_bytes: Vec<u8>,
    pub protocol_version: i32,
}

impl CachedMerkleUpdate {
    /// Convert update bytes to hex string for API response
    pub fn update_hex(&self) -> String {
        hex::encode(&self.update_bytes)
    }
}

// === Merkle Cache Manager ===

/// Background manager for merkle update caching
pub struct MerkleCacheManager {
    db_pool: PgPool,
    upstream_client: UpstreamClient,
    ws_url: String,
    shutdown: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Copy)]
struct SubscriptionOutcome {
    updates_cached: u64,
    end_reason: SubscriptionEndReason,
}

#[derive(Debug, Clone, Copy)]
enum SubscriptionEndReason {
    Completed,
    WebSocketClosed,
    StreamEnded,
    Shutdown,
}

impl MerkleCacheManager {
    /// Create a new merkle cache manager
    pub fn new(
        db_pool: PgPool,
        upstream_client: UpstreamClient,
        upstream_indexer_url: &str,
    ) -> Self {
        // Convert HTTP URL to WebSocket URL
        let ws_url = upstream_indexer_url
            .replace("https://", "wss://")
            .replace("http://", "ws://")
            .trim_end_matches('/')
            .to_string()
            + "/api/v3/graphql/ws";

        Self {
            db_pool,
            upstream_client,
            ws_url,
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Start the background subscription task
    pub fn start(self: Arc<Self>) {
        let manager = self.clone();
        tokio::spawn(async move {
            manager.subscription_loop().await;
        });
        info!("Merkle cache background subscription started");
    }

    /// Signal shutdown
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    /// Check if shutdown was requested
    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Relaxed)
    }

    /// Main subscription loop with reconnection
    async fn subscription_loop(&self) {
        // Backoff for actual failures.
        let base_error_delay = Duration::from_secs(1);
        let max_error_delay = Duration::from_secs(60);
        let mut error_delay = base_error_delay;

        // When the upstream completes the subscription immediately (we're caught up),
        // avoid a tight reconnect loop that spams logs and hammers the upstream.
        let base_idle_delay = Duration::from_secs(5);
        let max_idle_delay = Duration::from_secs(60);
        let mut idle_delay = base_idle_delay;

        loop {
            if self.is_shutdown() {
                info!("Merkle cache subscription loop shutting down");
                break;
            }

            match self.run_subscription().await {
                Ok(outcome) => {
                    // Any successful run resets error backoff.
                    error_delay = base_error_delay;

                    if self.is_shutdown() || matches!(outcome.end_reason, SubscriptionEndReason::Shutdown) {
                        info!("Merkle cache subscription loop shutting down");
                        break;
                    }

                    if outcome.updates_cached == 0 {
                        debug!(
                            idle_delay_secs = idle_delay.as_secs(),
                            end_reason = ?outcome.end_reason,
                            "Merkle cache caught up; waiting before resubscribing"
                        );
                        tokio::time::sleep(idle_delay).await;
                        idle_delay = std::cmp::min(idle_delay * 2, max_idle_delay);
                    } else {
                        // We made progress; keep the cache fresh by reconnecting quickly.
                        idle_delay = base_idle_delay;
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        reconnect_delay_secs = error_delay.as_secs(),
                        "Merkle cache subscription error, reconnecting"
                    );
                    metrics::counter!("merkle_cache_subscription_reconnects_total").increment(1);

                    if self.is_shutdown() {
                        break;
                    }

                    // Exponential backoff on errors
                    tokio::time::sleep(error_delay).await;
                    error_delay = std::cmp::min(error_delay * 2, max_error_delay);
                    idle_delay = base_idle_delay;
                }
            }
        }
    }

    /// Run a single subscription session
    async fn run_subscription(&self) -> Result<SubscriptionOutcome, AppError> {
        // 1. Get or create session with random viewing key
        let (session_id, _viewing_key) = self.ensure_session().await?;
        info!(
            session_id = %session_id,
            "Using session for merkle cache subscription"
        );

        // 2. Get last cached index from DB
        let start_index = self.get_highest_cached_index().await?;
        info!(
            start_index = start_index,
            "Resuming merkle cache subscription from index"
        );

        // 3. Connect WebSocket.
        //
        // The upstream indexer uses the graphql-transport-ws subprotocol; without it
        // the handshake is rejected (HTTP 400).
        let mut request = self.ws_url.clone().into_client_request().map_err(|e| {
            AppError::UpstreamError(format!("Invalid WebSocket URL '{}': {}", self.ws_url, e))
        })?;
        request.headers_mut().insert(
            SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("graphql-transport-ws"),
        );

        let (ws_stream, _) = connect_async(request)
            .await
            .map_err(|e| AppError::UpstreamError(format!("WebSocket connection failed: {}", e)))?;

        let (mut write, mut read) = ws_stream.split();

        // 4. Send connection_init
        let init_msg = ClientMessage::ConnectionInit {
            payload: serde_json::json!({}),
        };
        write
            .send(Message::Text(serde_json::to_string(&init_msg).unwrap()))
            .await
            .map_err(|e| AppError::UpstreamError(format!("Failed to send init: {}", e)))?;

        // 5. Wait for connection_ack
        let ack_timeout = Duration::from_secs(10);
        let ack_msg = timeout(ack_timeout, read.next())
            .await
            .map_err(|_| AppError::UpstreamError("Connection ack timeout".into()))?
            .ok_or_else(|| AppError::UpstreamError("Connection closed before ack".into()))?
            .map_err(|e| AppError::UpstreamError(format!("WebSocket read error: {}", e)))?;

        if let Message::Text(text) = ack_msg {
            let msg: ServerMessage = serde_json::from_str(&text)
                .map_err(|e| AppError::UpstreamError(format!("Failed to parse ack: {}", e)))?;
            match msg {
                ServerMessage::ConnectionAck => {
                    debug!("Merkle cache WebSocket connection acknowledged");
                }
                _ => {
                    return Err(AppError::UpstreamError("Expected connection_ack".into()));
                }
            }
        }

        // 6. Subscribe to shieldedTransactions
        let subscription_id = "merkle-cache-bg";
        let subscribe_msg = ClientMessage::Subscribe {
            id: subscription_id.to_string(),
            payload: SubscribePayload {
                query: SHIELDED_TRANSACTIONS_SUBSCRIPTION.to_string(),
                variables: serde_json::json!({
                    "sessionId": session_id,
                    "index": start_index
                }),
            },
        };
        write
            .send(Message::Text(serde_json::to_string(&subscribe_msg).unwrap()))
            .await
            .map_err(|e| AppError::UpstreamError(format!("Failed to send subscribe: {}", e)))?;

        info!(
            session_id = %session_id,
            start_index = start_index,
            "Merkle cache subscription active"
        );

        // 7. Process incoming updates (this runs until connection drops or shutdown)
        let mut updates_cached = 0u64;
        let mut end_reason = SubscriptionEndReason::StreamEnded;
        while let Some(msg_result) = read.next().await {
            if self.is_shutdown() {
                info!("Shutdown requested, closing merkle cache subscription");
                end_reason = SubscriptionEndReason::Shutdown;
                break;
            }

            match msg_result {
                Ok(Message::Text(text)) => {
                    if let Ok(server_msg) = serde_json::from_str::<ServerMessage>(&text) {
                        match server_msg {
                            ServerMessage::Next { id: _, payload } => {
                                if let Some(errors) = payload.errors {
                                    let msg = errors
                                        .into_iter()
                                        .map(|e| e.message)
                                        .collect::<Vec<_>>()
                                        .join("; ");
                                    return Err(AppError::UpstreamError(format!(
                                        "GraphQL subscription error: {}",
                                        msg
                                    )));
                                }

                                if let Some(data) = payload.data {
                                    match data.shielded_transactions {
                                        ShieldedTransactionsEvent::RelevantTransaction(rt) => {
                                            if let Some(tree) = rt.collapsed_merkle_tree {
                                                if let Err(e) = self.cache_update(&tree).await {
                                                    error!(error = %e, "Failed to cache merkle update");
                                                } else {
                                                    updates_cached += 1;
                                                    if updates_cached % 100 == 0 {
                                                        info!(
                                                            updates_cached = updates_cached,
                                                            latest_end_index = tree.end_index,
                                                            "Merkle cache progress"
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                        ShieldedTransactionsEvent::ShieldedTransactionsProgress(progress) => {
                                            debug!(
                                                highest_end_index = progress.highest_end_index,
                                                highest_checked_end_index = progress.highest_checked_end_index,
                                                "Shielded transactions progress"
                                            );
                                            // Update metrics
                                            metrics::gauge!("merkle_cache_chain_highest_index")
                                                .set(progress.highest_end_index as f64);
                                        }
                                    }
                                }
                            }
                            ServerMessage::Complete { .. } => {
                                end_reason = SubscriptionEndReason::Completed;
                                break;
                            }
                            ServerMessage::Error { payload, .. } => {
                                let msg = payload
                                    .into_iter()
                                    .map(|e| e.message)
                                    .collect::<Vec<_>>()
                                    .join("; ");
                                return Err(AppError::UpstreamError(format!(
                                    "GraphQL subscription error: {}",
                                    msg
                                )));
                            }
                            ServerMessage::Ping => {
                                // Respond with pong
                                let _ = write
                                    .send(Message::Text(
                                        serde_json::to_string(&serde_json::json!({"type": "pong"}))
                                            .unwrap(),
                                    ))
                                    .await;
                            }
                            _ => {}
                        }
                    }
                }
                Ok(Message::Close(_)) => {
                    info!("WebSocket closed by server");
                    end_reason = SubscriptionEndReason::WebSocketClosed;
                    break;
                }
                Err(e) => {
                    return Err(AppError::UpstreamError(format!("WebSocket error: {}", e)));
                }
                _ => {}
            }
        }

        // Send complete message
        let complete_msg = ClientMessage::Complete {
            id: subscription_id.to_string(),
        };
        let _ = write
            .send(Message::Text(serde_json::to_string(&complete_msg).unwrap()))
            .await;

        Ok(SubscriptionOutcome {
            updates_cached,
            end_reason,
        })
    }

    /// Get or create a session with a random viewing key
    async fn ensure_session(&self) -> Result<(String, String), AppError> {
        // Check if we have a stored session
        let stored: Option<(Option<String>, Option<String>)> = sqlx::query_as(
            r#"
            SELECT session_id, viewing_key
            FROM nocy_sidecar.merkle_cache_state
            WHERE id = 1
            "#,
        )
        .fetch_optional(&self.db_pool)
        .await
        .map_err(|e| AppError::InternalError(format!("Failed to query merkle cache state: {}", e)))?;

        if let Some((Some(session_id), Some(viewing_key))) = stored {
            if !session_id.is_empty() && !viewing_key.is_empty() {
                // Verify the session is still valid by trying to use it
                // For now, just return it - if it's invalid, the subscription will fail
                // and we'll create a new one on reconnect
                return Ok((session_id, viewing_key));
            }
        }

        // Generate a new random viewing key.
        //
        // Note: the upstream expects a bech32m-encoded viewing key for the current network.
        // Random bytes are fine as long as they decode to a valid secret key.
        let mut session_id = String::new();
        let mut viewing_key = String::new();

        // Retry a few times in case random bytes happen to produce an invalid key.
        for attempt in 1..=5 {
            let candidate = self.generate_random_viewing_key();
            match self.upstream_client.connect(&candidate).await {
                Ok(response) => {
                    session_id = response.session_id;
                    viewing_key = candidate;
                    break;
                }
                Err(e) => {
                    warn!(attempt, error = %e, "Failed to create upstream session for merkle cache, retrying");
                }
            }
        }

        if session_id.is_empty() || viewing_key.is_empty() {
            return Err(AppError::UpstreamError(
                "Failed to create upstream session for merkle cache".into(),
            ));
        }

        // Store session in database
        sqlx::query(
            r#"
            UPDATE nocy_sidecar.merkle_cache_state
            SET session_id = $1, viewing_key = $2, last_updated_at = NOW()
            WHERE id = 1
            "#,
        )
        .bind(&session_id)
        .bind(&viewing_key)
        .execute(&self.db_pool)
        .await
        .map_err(|e| AppError::InternalError(format!("Failed to store session: {}", e)))?;

        info!(session_id = %session_id, "Created new upstream session for merkle cache");
        Ok((session_id, viewing_key))
    }

    /// Generate a random 32-byte viewing key (bech32m-encoded for undeployed network).
    fn generate_random_viewing_key(&self) -> String {
        let mut rng = rand::thread_rng();
        let bytes: [u8; 32] = rng.gen();
        bech32::encode(VIEWING_KEY_HRP_UNDEPLOYED, bytes.to_base32(), Variant::Bech32m)
            .expect("bech32m encoding cannot fail for valid HRP and data")
    }

    /// Get the highest cached end_index from database
    async fn get_highest_cached_index(&self) -> Result<i64, AppError> {
        let result: (i64,) = sqlx::query_as(
            r#"
            SELECT highest_end_index
            FROM nocy_sidecar.merkle_cache_state
            WHERE id = 1
            "#,
        )
        .fetch_one(&self.db_pool)
        .await
        .map_err(|e| AppError::InternalError(format!("Failed to query highest index: {}", e)))?;

        Ok(result.0)
    }

    /// Cache a merkle update to the database
    async fn cache_update(&self, update: &CollapsedMerkleTree) -> Result<(), AppError> {
        // Decode hex string to bytes
        let update_bytes = hex::decode(&update.update)
            .map_err(|e| AppError::InternalError(format!("Invalid hex in update: {}", e)))?;

        // Insert into cache table
        sqlx::query(
            r#"
            INSERT INTO nocy_sidecar.collapsed_merkle_cache
                (start_index, end_index, update_bytes, protocol_version)
            VALUES ($1, $2, $3, $4)
            ON CONFLICT (start_index, end_index) DO NOTHING
            "#,
        )
        .bind(update.start_index)
        .bind(update.end_index)
        .bind(&update_bytes)
        .bind(update.protocol_version)
        .execute(&self.db_pool)
        .await
        .map_err(|e| AppError::InternalError(format!("Failed to cache update: {}", e)))?;

        // Update highest index
        sqlx::query(
            r#"
            UPDATE nocy_sidecar.merkle_cache_state
            SET highest_end_index = GREATEST(highest_end_index, $1),
                last_updated_at = NOW()
            WHERE id = 1
            "#,
        )
        .bind(update.end_index)
        .execute(&self.db_pool)
        .await
        .map_err(|e| AppError::InternalError(format!("Failed to update highest index: {}", e)))?;

        // Update metrics
        metrics::counter!("merkle_cache_updates_total").increment(1);
        metrics::gauge!("merkle_cache_highest_index").set(update.end_index as f64);

        Ok(())
    }

    /// Query cached updates for a given range
    ///
    /// Returns the best-effort chain of cached updates that covers the index range
    /// `[from_index, to_index)`, where `to_index` is exclusive (next free index).
    pub async fn get_cached_updates(
        &self,
        from_index: i64,
        to_index: i64,
    ) -> Result<Vec<CachedMerkleUpdate>, AppError> {
        if from_index >= to_index {
            return Ok(Vec::new());
        }

        // Updates use inclusive indices (start/end are commitment indices),
        // but callers use exclusive `to_index` (next free index).
        let required_end = to_index - 1;

        let mut chain = Vec::new();
        let mut current_start = from_index;

        while current_start <= required_end {
            let row: Option<(i64, i64, Vec<u8>, i32)> = sqlx::query_as(
                r#"
                SELECT start_index, end_index, update_bytes, protocol_version
                FROM nocy_sidecar.collapsed_merkle_cache
                WHERE start_index = $1 AND end_index <= $2
                ORDER BY end_index DESC
                LIMIT 1
                "#,
            )
            .bind(current_start)
            .bind(required_end)
            .fetch_optional(&self.db_pool)
            .await
            .map_err(|e| AppError::InternalError(format!("Failed to query cache: {}", e)))?;

            let Some((start_index, end_index, update_bytes, protocol_version)) = row else {
                break;
            };

            // Defensive: avoid infinite loops if the cache contains invalid data.
            if end_index < start_index {
                break;
            }

            chain.push(CachedMerkleUpdate {
                start_index,
                end_index,
                update_bytes,
                protocol_version,
            });

            current_start = end_index + 1;
        }

        Ok(chain)
    }

    /// Check if the cache covers a complete range
    ///
    /// Returns true if cached updates cover `[from_index, to_index)` without gaps.
    /// `to_index` is exclusive (next free index).
    pub fn covers_range(updates: &[CachedMerkleUpdate], from_index: i64, to_index: i64) -> bool {
        if from_index >= to_index {
            return true;
        }

        if updates.is_empty() {
            return false;
        }

        // Updates use inclusive indices; to cover `[from_index, to_index)` we need to end at
        // `to_index - 1`.
        let required_end = to_index - 1;

        // We must start exactly at from_index to be applicable.
        if updates[0].start_index != from_index {
            return false;
        }

        let mut expected_start = from_index;
        for update in updates {
            if update.start_index != expected_start {
                return false;
            }
            if update.end_index < update.start_index {
                return false;
            }

            expected_start = update.end_index + 1;
        }

        // Exact coverage (no overshoot): last end index must be `to_index - 1`.
        expected_start == required_end + 1
    }

    /// Get the current highest cached index
    pub async fn get_current_highest_index(&self) -> Result<i64, AppError> {
        self.get_highest_cached_index().await
    }

    /// Store upstream-provided collapsed merkle updates in the local cache.
    ///
    /// These updates are global (indexed by start/end) and can be reused across sessions.
    pub async fn store_updates(&self, updates: &[CollapsedMerkleUpdate]) -> Result<(), AppError> {
        for update in updates {
            let update_bytes = hex::decode(&update.update).map_err(|e| {
                AppError::InternalError(format!("Invalid hex in upstream merkle update: {}", e))
            })?;

            sqlx::query(
                r#"
                INSERT INTO nocy_sidecar.collapsed_merkle_cache
                    (start_index, end_index, update_bytes, protocol_version)
                VALUES ($1, $2, $3, $4)
                ON CONFLICT (start_index, end_index) DO NOTHING
                "#,
            )
            .bind(update.start_index)
            .bind(update.end_index)
            .bind(&update_bytes)
            .bind(update.protocol_version as i32)
            .execute(&self.db_pool)
            .await
            .map_err(|e| AppError::InternalError(format!("Failed to cache update: {}", e)))?;

            // Update highest index
            sqlx::query(
                r#"
                UPDATE nocy_sidecar.merkle_cache_state
                SET highest_end_index = GREATEST(highest_end_index, $1),
                    last_updated_at = NOW()
                WHERE id = 1
                "#,
            )
            .bind(update.end_index)
            .execute(&self.db_pool)
            .await
            .map_err(|e| {
                AppError::InternalError(format!("Failed to update highest index: {}", e))
            })?;
        }

        Ok(())
    }
}

const SHIELDED_TRANSACTIONS_SUBSCRIPTION: &str = r#"
	subscription ShieldedTransactions($sessionId: HexEncoded!, $index: Int) {
	  shieldedTransactions(sessionId: $sessionId, index: $index) {
	    __typename
	    ... on RelevantTransaction {
	      collapsedMerkleTree {
	        startIndex
	        endIndex
	        update
	        protocolVersion
	      }
	    }
	    ... on ShieldedTransactionsProgress {
	      highestEndIndex
	      highestCheckedEndIndex
	      highestRelevantEndIndex
    }
  }
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_covers_range_empty() {
        assert!(MerkleCacheManager::covers_range(&[], 0, 0));
        assert!(!MerkleCacheManager::covers_range(&[], 0, 10));
    }

    #[test]
    fn test_covers_range_single_update() {
        let updates = vec![CachedMerkleUpdate {
            start_index: 0,
            end_index: 9,
            update_bytes: vec![],
            protocol_version: 2,
        }];

        assert!(MerkleCacheManager::covers_range(&updates, 0, 10));
        assert!(!MerkleCacheManager::covers_range(&updates, 0, 5));
        assert!(!MerkleCacheManager::covers_range(&updates, 5, 10));
    }

    #[test]
    fn test_covers_range_multiple_updates() {
        let updates = vec![
            CachedMerkleUpdate {
                start_index: 0,
                end_index: 9,
                update_bytes: vec![],
                protocol_version: 2,
            },
            CachedMerkleUpdate {
                start_index: 10,
                end_index: 19,
                update_bytes: vec![],
                protocol_version: 2,
            },
            CachedMerkleUpdate {
                start_index: 20,
                end_index: 29,
                update_bytes: vec![],
                protocol_version: 2,
            },
        ];

        assert!(MerkleCacheManager::covers_range(&updates, 0, 30));
        assert!(!MerkleCacheManager::covers_range(&updates, 0, 25));
        assert!(!MerkleCacheManager::covers_range(&updates, 5, 25));

        let partial = updates[..2].to_vec();
        assert!(MerkleCacheManager::covers_range(&partial, 0, 20));
    }

    #[test]
    fn test_covers_range_with_gap() {
        let updates = vec![
            CachedMerkleUpdate {
                start_index: 0,
                end_index: 9,
                update_bytes: vec![],
                protocol_version: 2,
            },
            CachedMerkleUpdate {
                start_index: 15, // Gap! Expected 10.
                end_index: 24,
                update_bytes: vec![],
                protocol_version: 2,
            },
        ];

        assert!(!MerkleCacheManager::covers_range(&updates, 0, 25));
    }

    #[test]
    fn test_update_hex() {
        let update = CachedMerkleUpdate {
            start_index: 0,
            end_index: 10,
            update_bytes: vec![0xde, 0xad, 0xbe, 0xef],
            protocol_version: 2,
        };
        assert_eq!(update.update_hex(), "deadbeef");
    }
}
