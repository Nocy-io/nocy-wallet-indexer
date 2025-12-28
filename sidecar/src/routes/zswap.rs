//! Zswap endpoints for collapsed updates and first-free index
//!
//! Implements:
//! - GET /v1/zswap/first-free - Get the first free commitment tree index
//! - GET /v1/zswap/collapsed-update - Get collapsed zswap state update (metadata)
//! - GET /v1/zswap/merkle-update - Get collapsed merkle tree update bytes for SDK

use axum::{
    extract::{Query, State},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize, Serializer};
use sqlx::PgPool;
use std::sync::Arc;
use tracing::{debug, info, instrument, warn};

use crate::db::get_upstream_session_id;
use crate::error::AppError;
use crate::graphql_ws::SubscriptionManager;
use crate::merkle_cache::MerkleCacheManager;
use crate::snapshot::SnapshotClient;

/// Serialize i64 as a decimal string for JavaScript compatibility
fn serialize_i64_as_string<S>(value: &i64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&value.to_string())
}

/// Session ID is a 32-byte value, hex-encoded (64 chars)
const SESSION_ID_BYTES: usize = 32;

/// Parse and validate a session ID (hex-encoded 32 bytes)
fn parse_session_id(session_id: &str) -> Result<Vec<u8>, AppError> {
    let bytes = hex::decode(session_id)
        .map_err(|_| AppError::BadRequest("session_id must be valid hex".into()))?;

    if bytes.len() != SESSION_ID_BYTES {
        return Err(AppError::BadRequest(format!(
            "session_id must be {} bytes ({} hex chars), got {} bytes",
            SESSION_ID_BYTES,
            SESSION_ID_BYTES * 2,
            bytes.len()
        )));
    }

    Ok(bytes)
}

/// State for zswap routes
#[derive(Clone)]
pub struct ZswapState {
    pub db_pool: PgPool,
    pub snapshot_client: Arc<SnapshotClient>,
    pub subscription_manager: Option<Arc<SubscriptionManager>>,
    pub merkle_cache_manager: Option<Arc<MerkleCacheManager>>,
}

/// Response for GET /v1/zswap/first-free
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FirstFreeResponse {
    /// The first free index in the commitment tree
    pub first_free_index: String,
    /// Block height this value is valid for (decimal string for JS compatibility)
    #[serde(serialize_with = "serialize_i64_as_string")]
    pub block_height: i64,
    /// Source of the data ("snapshot" or "db")
    pub source: &'static str,
}

/// GET /v1/zswap/first-free - Get the first free commitment tree index
#[instrument(skip(state))]
pub async fn get_first_free(
    State(state): State<ZswapState>,
) -> Result<Json<FirstFreeResponse>, AppError> {
    // Try to get from NATS snapshot first (preferred, more up-to-date)
    if let Some(snapshot) = state.snapshot_client.get_cached_snapshot().await {
        info!(
            first_free_index = snapshot.first_free_index,
            block_height = snapshot.block_height,
            "Returning first-free from snapshot"
        );
        return Ok(Json(FirstFreeResponse {
            first_free_index: snapshot.first_free_index.to_string(),
            block_height: snapshot.block_height,
            source: "snapshot",
        }));
    }

    // Fall back to database query
    debug!("Snapshot not available, falling back to DB query for first-free");
    let (first_free_index, block_height) = get_first_free_from_db(&state.db_pool).await?;

    info!(
        first_free_index,
        block_height,
        "Returning first-free from DB"
    );

    Ok(Json(FirstFreeResponse {
        first_free_index: first_free_index.to_string(),
        block_height,
        source: "db",
    }))
}

/// Query first-free index from database
/// This computes end_index from the latest regular transaction's merkle tree
async fn get_first_free_from_db(pool: &PgPool) -> Result<(u64, i64), AppError> {
    // Get the latest regular transaction's end_index and block height
    // end_index is the next free index after the last commitment in that tx
    let row: Option<(i64, i64)> = sqlx::query_as(
        r#"
        SELECT rt.end_index, b.height
        FROM regular_transactions rt
        JOIN transactions t ON rt.id = t.id
        JOIN blocks b ON t.block_id = b.id
        ORDER BY t.id DESC
        LIMIT 1
        "#,
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to query first-free from DB: {}", e)))?;

    match row {
        Some((end_index, height)) => {
            // end_index is the first free index (exclusive end of the range)
            Ok((end_index as u64, height))
        }
        None => {
            // No transactions yet, first free index is 0
            Ok((0, 0))
        }
    }
}

/// Query parameters for GET /v1/zswap/collapsed-update
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CollapsedUpdateQuery {
    /// Minimum block height to include (inclusive)
    pub from_height: i64,
    /// Maximum block height to include (inclusive)
    pub to_height: i64,
    /// Expected protocol version (for validation)
    pub protocol_version: u32,
}

/// Response for GET /v1/zswap/collapsed-update
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CollapsedUpdateResponse {
    /// Block height this update is valid for (decimal string for JS compatibility)
    #[serde(serialize_with = "serialize_i64_as_string")]
    pub block_height: i64,
    /// Protocol version at this height
    pub protocol_version: u32,
    /// Merkle tree root hash (hex encoded)
    pub merkle_root: String,
    /// First free index in the commitment tree (decimal string for JS compatibility)
    pub first_free_index: String,
    /// Total number of commitments (decimal string for JS compatibility)
    pub commitment_count: String,
    /// Timestamp when snapshot was created (Unix millis, decimal string for JS compatibility)
    #[serde(serialize_with = "serialize_i64_as_string")]
    pub timestamp_ms: i64,
}

/// GET /v1/zswap/collapsed-update - Get collapsed zswap state update
///
/// This endpoint returns the current zswap state snapshot.
/// It validates:
/// - Bounds: fromHeight < toHeight, fromHeight >= 0
/// - toHeight must equal current chain head (collapsed updates are latest-only)
/// - Protocol version: must match the snapshot's protocol version
#[instrument(skip(state))]
pub async fn get_collapsed_update(
    State(state): State<ZswapState>,
    Query(query): Query<CollapsedUpdateQuery>,
) -> Result<Json<CollapsedUpdateResponse>, AppError> {
    // Validate bounds
    if query.from_height >= query.to_height {
        return Err(AppError::BadRequest(
            "fromHeight must be less than toHeight".into(),
        ));
    }

    if query.from_height < 0 {
        return Err(AppError::BadRequest(
            "fromHeight must be non-negative".into(),
        ));
    }

    // Get snapshot (required for collapsed updates)
    let snapshot = state
        .snapshot_client
        .get_cached_snapshot()
        .await
        .ok_or_else(|| {
        AppError::ServiceUnavailable("Ledger state snapshot not available".into())
    })?;

    // Validate toHeight == snapshot height (we can only provide current snapshot state)
    if query.to_height != snapshot.block_height {
        return Err(AppError::BadRequest(format!(
            "toHeight ({}) must equal current chain head ({}); collapsed updates only available for latest state",
            query.to_height, snapshot.block_height
        )));
    }

    // Validate protocol version
    if query.protocol_version != snapshot.protocol_version {
        return Err(AppError::BadRequest(format!(
            "Protocol version mismatch: requested {} but chain is at {}",
            query.protocol_version, snapshot.protocol_version
        )));
    }

    info!(
        from_height = query.from_height,
        to_height = query.to_height,
        protocol_version = query.protocol_version,
        snapshot_height = snapshot.block_height,
        "Returning collapsed update from snapshot"
    );

    Ok(Json(CollapsedUpdateResponse {
        block_height: snapshot.block_height,
        protocol_version: snapshot.protocol_version,
        merkle_root: snapshot.merkle_root,
        first_free_index: snapshot.first_free_index.to_string(),
        commitment_count: snapshot.commitment_count.to_string(),
        timestamp_ms: snapshot.timestamp_ms,
    }))
}

/// Query parameters for GET /v1/zswap/merkle-update
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MerkleUpdateQuery {
    /// Session ID (hex-encoded 32 bytes)
    pub session_id: String,
    /// Start index in the commitment tree
    pub from_index: i64,
    /// End index in the commitment tree
    pub to_index: i64,
}

/// A single collapsed merkle tree update
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MerkleUpdateItem {
    /// Start index
    #[serde(serialize_with = "serialize_i64_as_string")]
    pub start_index: i64,
    /// End index
    #[serde(serialize_with = "serialize_i64_as_string")]
    pub end_index: i64,
    /// Hex-encoded collapsed merkle tree update bytes (for SDK)
    pub update: String,
    /// Protocol version
    pub protocol_version: u32,
}

/// Response for GET /v1/zswap/merkle-update
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MerkleUpdateResponse {
    /// List of collapsed merkle updates covering the requested range
    pub updates: Vec<MerkleUpdateItem>,
}

/// GET /v1/zswap/merkle-update - Get collapsed merkle tree update bytes
///
/// This endpoint fetches the actual collapsed merkle tree update bytes.
/// It first tries to serve from the local cache (populated by background subscription),
/// and falls back to the upstream indexer via GraphQL WebSocket subscription if needed.
/// These bytes can be used by the SDK to fast-forward merkle tree state.
#[instrument(skip(state))]
pub async fn get_merkle_update(
    State(state): State<ZswapState>,
    Query(query): Query<MerkleUpdateQuery>,
) -> Result<Json<MerkleUpdateResponse>, AppError> {
    use crate::merkle_cache::MerkleCacheManager;

    // Validate local session ID and resolve upstream wallet-indexer session ID.
    let local_session_id_bytes = parse_session_id(&query.session_id)?;
    let upstream_session_id_bytes =
        get_upstream_session_id(&state.db_pool, &local_session_id_bytes).await?;
    let upstream_session_id_str = hex::encode(&upstream_session_id_bytes);

    // Validate bounds
    if query.from_index >= query.to_index {
        return Err(AppError::BadRequest(
            "fromIndex must be less than toIndex".into(),
        ));
    }

    if query.from_index < 0 {
        return Err(AppError::BadRequest(
            "fromIndex must be non-negative".into(),
        ));
    }

    // Try to serve from cache first
    if let Some(ref cache) = state.merkle_cache_manager {
        match cache.get_cached_updates(query.from_index, query.to_index).await {
            Ok(cached_updates) => {
                // Check if cache covers the complete range
                if MerkleCacheManager::covers_range(&cached_updates, query.from_index, query.to_index) {
                    let response_updates: Vec<MerkleUpdateItem> = cached_updates
                        .into_iter()
                        .map(|u| MerkleUpdateItem {
                            start_index: u.start_index,
                            end_index: u.end_index,
                            update: u.update_hex(),
                            protocol_version: u.protocol_version as u32,
                        })
                        .collect();

                    info!(
                        session_id = %query.session_id,
                        from_index = query.from_index,
                        to_index = query.to_index,
                        update_count = response_updates.len(),
                        source = "cache",
                        "Returning merkle updates from cache"
                    );

                    metrics::counter!("merkle_update_cache_hits_total").increment(1);

                    return Ok(Json(MerkleUpdateResponse {
                        updates: response_updates,
                    }));
                } else {
                    info!(
                        from_index = query.from_index,
                        to_index = query.to_index,
                        cached_count = cached_updates.len(),
                        "Cache does not cover complete range, falling back to upstream"
                    );
                    metrics::counter!("merkle_update_cache_misses_total").increment(1);
                }
            }
            Err(e) => {
                warn!(
                    error = %e,
                    "Failed to query merkle cache, falling back to upstream"
                );
            }
        }
    }

    // Fall back to upstream subscription manager
    let manager = state.subscription_manager.as_ref().ok_or_else(|| {
        AppError::ServiceUnavailable(
            "Merkle update service not configured (no cache or upstream WebSocket available)".into(),
        )
    })?;

    info!(
        session_id = %query.session_id,
        from_index = query.from_index,
        to_index = query.to_index,
        source = "upstream",
        "Fetching merkle updates from upstream"
    );

    // Fetch collapsed updates from upstream indexer
    let updates = manager
        .fetch_merkle_updates(&upstream_session_id_str, query.from_index, query.to_index)
        .await?;

    // Opportunistically store upstream updates in the local cache for future SSE injection and
    // /v1/zswap/merkle-update requests.
    if let Some(ref cache) = state.merkle_cache_manager {
        if let Err(e) = cache.store_updates(&updates).await {
            warn!(error = %e, "Failed to store upstream merkle updates in cache");
        }
    }

    let response_updates: Vec<MerkleUpdateItem> = updates
        .into_iter()
        .map(|u| MerkleUpdateItem {
            start_index: u.start_index,
            end_index: u.end_index,
            update: u.update,
            protocol_version: u.protocol_version,
        })
        .collect();

    info!(
        session_id = %query.session_id,
        update_count = response_updates.len(),
        source = "upstream",
        "Returning merkle updates from upstream"
    );

    Ok(Json(MerkleUpdateResponse {
        updates: response_updates,
    }))
}

/// Create the zswap router
pub fn zswap_routes<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    ZswapState: axum::extract::FromRef<S>,
{
    Router::new()
        .route("/first-free", get(get_first_free))
        .route("/collapsed-update", get(get_collapsed_update))
        .route("/merkle-update", get(get_merkle_update))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::LedgerStateSnapshot;

    #[test]
    fn test_collapsed_update_query_deserialization() {
        let json = r#"{"fromHeight": 0, "toHeight": 100, "protocolVersion": 2}"#;
        let query: CollapsedUpdateQuery = serde_json::from_str(json).unwrap();
        assert_eq!(query.from_height, 0);
        assert_eq!(query.to_height, 100);
        assert_eq!(query.protocol_version, 2);
    }

    #[test]
    fn test_collapsed_update_response_serialization() {
        let response = CollapsedUpdateResponse {
            block_height: 100,
            protocol_version: 2,
            merkle_root: "abc123".to_string(),
            first_free_index: "1000".to_string(),
            commitment_count: "999".to_string(),
            timestamp_ms: 1703123456789,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"blockHeight\":\"100\""));
        assert!(json.contains("\"protocolVersion\":2"));
        assert!(json.contains("\"firstFreeIndex\":\"1000\""));
        assert!(json.contains("\"commitmentCount\":\"999\""));
        assert!(json.contains("\"timestampMs\":\"1703123456789\""));
    }

    #[test]
    fn test_first_free_response_serialization() {
        let response = FirstFreeResponse {
            first_free_index: "500".to_string(),
            block_height: 50,
            source: "snapshot",
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"firstFreeIndex\":\"500\""));
        assert!(json.contains("\"blockHeight\":\"50\""));
        assert!(json.contains("\"source\":\"snapshot\""));
    }

    /// Helper to validate collapsed-update bounds without needing full handler
    fn validate_collapsed_update_bounds(
        query: &CollapsedUpdateQuery,
        snapshot: &LedgerStateSnapshot,
    ) -> Result<(), &'static str> {
        if query.from_height >= query.to_height {
            return Err("fromHeight must be less than toHeight");
        }
        if query.from_height < 0 {
            return Err("fromHeight must be non-negative");
        }
        if query.to_height != snapshot.block_height {
            return Err("toHeight must equal current chain head");
        }
        if query.protocol_version != snapshot.protocol_version {
            return Err("Protocol version mismatch");
        }
        Ok(())
    }

    fn test_snapshot(height: i64, protocol_version: u32) -> LedgerStateSnapshot {
        LedgerStateSnapshot {
            block_height: height,
            protocol_version,
            merkle_root: "abc123".to_string(),
            first_free_index: 1000,
            commitment_count: 999,
            timestamp_ms: 1703123456789,
        }
    }

    #[test]
    fn test_collapsed_update_rejects_from_height_ge_to_height() {
        let snapshot = test_snapshot(100, 2);

        // fromHeight == toHeight
        let query = CollapsedUpdateQuery {
            from_height: 100,
            to_height: 100,
            protocol_version: 2,
        };
        assert_eq!(
            validate_collapsed_update_bounds(&query, &snapshot),
            Err("fromHeight must be less than toHeight")
        );

        // fromHeight > toHeight
        let query = CollapsedUpdateQuery {
            from_height: 101,
            to_height: 100,
            protocol_version: 2,
        };
        assert_eq!(
            validate_collapsed_update_bounds(&query, &snapshot),
            Err("fromHeight must be less than toHeight")
        );
    }

    #[test]
    fn test_collapsed_update_rejects_negative_from_height() {
        let snapshot = test_snapshot(100, 2);
        let query = CollapsedUpdateQuery {
            from_height: -1,
            to_height: 100,
            protocol_version: 2,
        };
        assert_eq!(
            validate_collapsed_update_bounds(&query, &snapshot),
            Err("fromHeight must be non-negative")
        );
    }

    #[test]
    fn test_collapsed_update_rejects_to_height_not_equal_chain_head() {
        let snapshot = test_snapshot(100, 2);

        // toHeight < chain head
        let query = CollapsedUpdateQuery {
            from_height: 0,
            to_height: 50,
            protocol_version: 2,
        };
        assert_eq!(
            validate_collapsed_update_bounds(&query, &snapshot),
            Err("toHeight must equal current chain head")
        );

        // toHeight > chain head
        let query = CollapsedUpdateQuery {
            from_height: 0,
            to_height: 150,
            protocol_version: 2,
        };
        assert_eq!(
            validate_collapsed_update_bounds(&query, &snapshot),
            Err("toHeight must equal current chain head")
        );
    }

    #[test]
    fn test_collapsed_update_rejects_protocol_version_mismatch() {
        let snapshot = test_snapshot(100, 2);
        let query = CollapsedUpdateQuery {
            from_height: 0,
            to_height: 100,
            protocol_version: 1, // snapshot has version 2
        };
        assert_eq!(
            validate_collapsed_update_bounds(&query, &snapshot),
            Err("Protocol version mismatch")
        );
    }

    #[test]
    fn test_collapsed_update_accepts_valid_request() {
        let snapshot = test_snapshot(100, 2);
        let query = CollapsedUpdateQuery {
            from_height: 0,
            to_height: 100,
            protocol_version: 2,
        };
        assert!(validate_collapsed_update_bounds(&query, &snapshot).is_ok());

        // Also valid with non-zero fromHeight
        let query = CollapsedUpdateQuery {
            from_height: 50,
            to_height: 100,
            protocol_version: 2,
        };
        assert!(validate_collapsed_update_bounds(&query, &snapshot).is_ok());
    }
}
