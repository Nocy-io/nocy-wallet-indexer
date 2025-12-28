//! SSE live stream endpoint for real-time feed updates
//!
//! Implements GET /v1/feed/subscribe with:
//! - Backlog mode: fetch existing blocks from fromHeight
//! - Live mode: stream new blocks as they arrive
//! - Heartbeats while waiting for walletReadyHeight to advance
//! - Backpressure handling for slow clients

use axum::{
    extract::{Query, State},
    response::sse::{Event, KeepAlive, Sse},
};
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use std::{convert::Infallible, time::Duration};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{info, instrument, warn};

use crate::db::{
    begin_readonly_tx, compute_wallet_ready_height_tx, detect_identifiers_source,
    get_all_dust_events_tx, get_all_zswap_input_events_in_range_tx, get_block_hash_at_height_tx,
    get_blocks_in_range_tx, get_chain_head_tx, get_contract_actions_for_txs_tx,
    get_dust_events_tx, get_relevant_transactions_tx, get_transaction_metadata_tx,
    get_unshielded_utxo_events_tx, get_upstream_session_id, get_zswap_events_for_txs_tx,
    IdentifiersSource,
};
use crate::error::AppError;
use crate::feed::{
    BlockBundle, ControlEvent, DustLedgerEventRaw, FeedItem, MerkleCollapsedUpdate,
    OrderedFeedItem, ShieldedRelevantTx, TransactionFees, TransactionMerkle, TransactionMeta,
    UnshieldedUtxoEvent, Watermarks, ZswapInputRaw,
};
use crate::graphql_ws::SubscriptionManager;
use crate::merkle_cache::MerkleCacheManager;
use crate::routes::session::{get_session_nullifier_streaming, NullifierStreaming, SessionState};
use crate::sequence::{parse_cursor, SequenceKey};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Session ID is a 32-byte SHA256 hash, hex-encoded (64 chars)
const SESSION_ID_BYTES: usize = 32;

/// Parse and validate a session ID (hex-encoded 32 bytes)
fn parse_session_id(session_id: &str) -> Result<Vec<u8>, crate::error::AppError> {
    let bytes = hex::decode(session_id)
        .map_err(|_| crate::error::AppError::BadRequest("session_id must be valid hex".into()))?;

    if bytes.len() != SESSION_ID_BYTES {
        return Err(crate::error::AppError::BadRequest(format!(
            "session_id must be {} bytes ({} hex chars), got {} bytes",
            SESSION_ID_BYTES,
            SESSION_ID_BYTES * 2,
            bytes.len()
        )));
    }

    Ok(bytes)
}

/// Query parameters for GET /v1/feed/subscribe
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscribeQuery {
    /// Session ID (hex-encoded 32 bytes)
    pub session_id: String,
    /// Starting block height (defaults to 0)
    #[serde(default)]
    pub from_height: Option<i64>,
    /// Starting sequence cursor (overrides from_height if provided)
    #[serde(default)]
    pub from_cursor: Option<String>,
    /// Whether to include global ZswapInput events for spend detection (defaults to true)
    /// Set to false if using 0-value change outputs for spend detection instead
    #[serde(default = "default_include_nullifiers")]
    pub include_nullifiers: bool,
    /// Whether to include transaction metadata items (defaults to true)
    #[serde(default = "default_include_tx_meta")]
    pub include_tx_meta: bool,
    /// Whether to include raw transaction bytes in transactionMeta (defaults to false)
    #[serde(default = "default_include_raw_tx")]
    pub include_raw_tx: bool,
    /// Whether to include transaction identifiers in transactionMeta (defaults to true)
    #[serde(default = "default_include_identifiers")]
    pub include_identifiers: bool,
    /// Whether to include ALL dust events (not just wallet-relevant) (defaults to false)
    /// Required for dust merkle tree sync because the SDK needs all events in sequence.
    /// When false, only wallet-relevant dust events are returned.
    #[serde(default = "default_include_all_dust")]
    pub include_all_dust: bool,
}

fn default_include_nullifiers() -> bool {
    true
}

fn default_include_all_dust() -> bool {
    false
}

fn default_include_tx_meta() -> bool {
    true
}

fn default_include_raw_tx() -> bool {
    false
}

fn default_include_identifiers() -> bool {
    true
}

/// SSE event types
/// Uses adjacently tagged serde format: {"type": "...", "data": {...}}
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(tag = "type", content = "data")]
pub enum SseEvent {
    /// A block bundle with feed items
    Block(BlockBundle),
    /// Progress heartbeat while waiting for new data
    Heartbeat {
        watermarks: Watermarks,
        /// Current position being processed
        current_height: String,
    },
    /// Control event (reorg, reset, etc.)
    Control(ControlEvent),
}

/// Constants for SSE stream behavior
const BACKLOG_BATCH_SIZE: i64 = 50;
const CHANNEL_BUFFER_SIZE: usize = 32;
const PHASE_TX: i64 = 0;
const PHASE_LEDGER_EVENT: i64 = 1;
const PHASE_UNSHIELDED: i64 = 2;

fn decode_u128_value(bytes: &[u8]) -> u128 {
    if bytes.len() == 16 {
        u128::from_be_bytes(bytes.try_into().unwrap_or([0u8; 16]))
    } else {
        let mut padded = [0u8; 16];
        let len = bytes.len().min(16);
        padded[16 - len..].copy_from_slice(&bytes[..len]);
        u128::from_be_bytes(padded)
    }
}

fn decode_fee_string(bytes: &Option<Vec<u8>>) -> String {
    match bytes {
        Some(raw) if !raw.is_empty() => decode_u128_value(raw).to_string(),
        _ => "0".to_string(),
    }
}

fn decode_fee_option(bytes: &Option<Vec<u8>>) -> Option<String> {
    match bytes {
        Some(raw) if !raw.is_empty() => Some(decode_u128_value(raw).to_string()),
        Some(_) => Some("0".to_string()),
        None => None,
    }
}

/// GET /v1/feed/subscribe - SSE stream of feed updates
#[instrument(skip(state), fields(session_id = %query.session_id))]
pub async fn subscribe_feed(
    State(state): State<SessionState>,
    Query(query): Query<SubscribeQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // Capture config values upfront for use in error path and main path
    let heartbeat_interval = state.heartbeat_interval;

    // Parse and validate session ID upfront
    let local_session_id_bytes = match parse_session_id(&query.session_id) {
        Ok(bytes) => bytes,
        Err(e) => {
            // For SSE, we can't return an error directly, so send an error event
            warn!(error = %e, "Invalid session_id");
            let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(1);
            let _ = tx.send(Ok(Event::default().event("error").data(format!("{}", e)))).await;
            drop(tx);
            return Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::new().interval(heartbeat_interval));
        }
    };

    // Resolve the upstream wallet-indexer session ID for this local session.
    // The SSE stream needs it for walletReadyHeight and inline merkle update fallback.
    let upstream_session_id_bytes = match get_upstream_session_id(&state.db_pool, &local_session_id_bytes).await {
        Ok(bytes) => bytes,
        Err(e) => {
            warn!(error = %e, "Invalid or unknown session_id");
            let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(1);
            let _ = tx.send(Ok(Event::default().event("error").data(format!("{}", e)))).await;
            drop(tx);
            return Sse::new(ReceiverStream::new(rx))
                .keep_alive(KeepAlive::new().interval(heartbeat_interval));
        }
    };
    let upstream_session_id_str = hex::encode(&upstream_session_id_bytes);

    let from_cursor = match query.from_cursor.as_deref() {
        Some(cursor) => match parse_cursor(cursor) {
            Ok(parsed) => Some(parsed),
            Err(message) => {
                warn!(error = %message, "Invalid from_cursor");
                let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(1);
                let _ = tx
                    .send(Ok(Event::default().event("error").data(message)))
                    .await;
                drop(tx);
                return Sse::new(ReceiverStream::new(rx))
                    .keep_alive(KeepAlive::new().interval(heartbeat_interval));
            }
        },
        None => None,
    };
    let from_height = from_cursor
        .map(|cursor| cursor.block_height)
        .unwrap_or_else(|| query.from_height.unwrap_or(0))
        .max(0);

    info!(
        from_height,
        include_all_dust = query.include_all_dust,
        include_nullifiers = query.include_nullifiers,
        include_tx_meta = query.include_tx_meta,
        "Starting SSE subscription"
    );

    // Create a channel for sending events
    let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(CHANNEL_BUFFER_SIZE);

    // Track SSE client connection
    crate::metrics::sse_client_connected();

    // Capture values for the spawned task
    let include_nullifiers = query.include_nullifiers;
    let include_tx_meta = query.include_tx_meta;
    let include_raw_tx = query.include_raw_tx;
    let include_identifiers = query.include_identifiers;
    let include_all_dust = query.include_all_dust;
    let local_session_id_str = query.session_id.clone();

    // Spawn the stream producer task
    tokio::spawn(async move {
        if let Err(e) = run_stream(
            state,
            local_session_id_bytes,
            local_session_id_str,
            upstream_session_id_bytes,
            upstream_session_id_str,
            from_height,
            from_cursor,
            include_nullifiers,
            include_tx_meta,
            include_raw_tx,
            include_identifiers,
            include_all_dust,
            tx,
        )
        .await
        {
            warn!(error = %e, "SSE stream error");
        }
        // Track SSE client disconnection
        crate::metrics::sse_client_disconnected();
    });

    // Convert the receiver to a stream
    let stream = ReceiverStream::new(rx);

    Sse::new(stream).keep_alive(KeepAlive::new().interval(heartbeat_interval))
}

/// Internal stream producer that sends events through the channel
async fn run_stream(
    state: SessionState,
    local_session_id_bytes: Vec<u8>,
    local_session_id_str: String,
    upstream_session_id_bytes: Vec<u8>,
    upstream_session_id_str: String,
    mut current_height: i64,
    mut from_cursor: Option<SequenceKey>,
    include_nullifiers: bool,
    include_tx_meta: bool,
    include_raw_tx: bool,
    include_identifiers: bool,
    include_all_dust: bool,
    tx: mpsc::Sender<Result<Event, Infallible>>,
) -> Result<(), AppError> {
    let mut last_heartbeat = std::time::Instant::now();

    // Track merkle tree position for inline collapsed update injection
    // -1 means no transactions processed yet (fresh sync)
    let mut last_merkle_end_index: i64 = -1;

    // Determine nullifier streaming mode
    // Query param include_nullifiers=false takes precedence, else use session preference
    let nullifier_mode = if !include_nullifiers {
        NullifierStreaming::Disabled
    } else {
        get_session_nullifier_streaming(&state.db_pool, &local_session_id_str).await?
    };

    let identifiers_source = if include_tx_meta && include_identifiers {
        detect_identifiers_source(&state.db_pool).await?
    } else {
        IdentifiersSource::None
    };

    // Track the last block hash across batches for boundary reorg detection
    // On first batch, we'll fetch the previous block's hash if current_height > 0
    let mut last_block_hash: Option<String> = None;

    loop {
        // Check if the channel is closed (client disconnected)
        if tx.is_closed() {
            info!("Client disconnected, stopping stream");
            break;
        }

        let mut snapshot = begin_readonly_tx(&state.db_pool).await?;

        // Get current chain state
        let chain_head = get_chain_head_tx(&mut snapshot).await?;
        let wallet_ready_height =
            compute_wallet_ready_height_tx(&mut snapshot, &upstream_session_id_bytes, chain_head)
                .await?;

        // Note: finalized_height equals chain_head because the upstream chain-indexer
        // only processes finalized blocks (via subscribe_finalized() in subxt_node.rs).
        let watermarks = Watermarks {
            chain_head,
            wallet_ready_height,
            finalized_height: Some(chain_head),
        };

        // Determine the ready height we can stream up to
        let ready_height = match wallet_ready_height {
            Some(h) => h,
            None => {
                snapshot.commit().await.map_err(|e| {
                    AppError::InternalError(format!(
                        "Failed to commit subscribe snapshot (not ready): {}",
                        e
                    ))
                })?;
                // Wallet not ready, send heartbeat and wait
                if last_heartbeat.elapsed() >= state.heartbeat_interval {
                    let event = SseEvent::Heartbeat {
                        watermarks: watermarks.clone(),
                        current_height: current_height.to_string(),
                    };
                    if send_event(&tx, "heartbeat", &event, state.sse_send_timeout).await.is_err() {
                        break;
                    }
                    last_heartbeat = std::time::Instant::now();
                }
                tokio::time::sleep(state.poll_interval).await;
                continue;
            }
        };

        // If we're caught up, wait for new blocks
        if current_height > ready_height {
            snapshot.commit().await.map_err(|e| {
                AppError::InternalError(format!(
                    "Failed to commit subscribe snapshot (caught up): {}",
                    e
                ))
            })?;
            if last_heartbeat.elapsed() >= state.heartbeat_interval {
                let event = SseEvent::Heartbeat {
                    watermarks: watermarks.clone(),
                    current_height: current_height.to_string(),
                };
                if send_event(&tx, "heartbeat", &event, state.sse_send_timeout).await.is_err() {
                    break;
                }
                last_heartbeat = std::time::Instant::now();
            }
            tokio::time::sleep(state.poll_interval).await;
            continue;
        }

        // Fetch a batch of blocks
        let to_height = (current_height + BACKLOG_BATCH_SIZE - 1).min(ready_height);
        let blocks_meta = get_blocks_in_range_tx(&mut snapshot, current_height, to_height).await?;

        if blocks_meta.is_empty() {
            snapshot.commit().await.map_err(|e| {
                AppError::InternalError(format!(
                    "Failed to commit subscribe snapshot (empty batch): {}",
                    e
                ))
            })?;
            tokio::time::sleep(state.poll_interval).await;
            continue;
        }

        // Validate contiguous blocks and check for reorgs (parent hash mismatch)
        let mut expected_height = current_height;

        // For boundary reorg detection: use tracked last_block_hash or fetch previous block
        // On first batch, fetch the previous block's hash if current_height > 0
        if last_block_hash.is_none() && current_height > 0 {
            last_block_hash =
                get_block_hash_at_height_tx(&mut snapshot, current_height - 1).await?;
        }
        let mut prev_hash: Option<&str> = last_block_hash.as_deref();

        for block in &blocks_meta {
            // Check for height gaps
            if block.height != expected_height {
                let safe_restart_height = (expected_height - 1).max(0);

                snapshot.commit().await.map_err(|e| {
                    AppError::InternalError(format!(
                        "Failed to commit subscribe snapshot (gap): {}",
                        e
                    ))
                })?;

                let control = ControlEvent::reset_required(
                    safe_restart_height,
                    format!(
                        "Block gap detected: expected height {}, got {}",
                        expected_height, block.height
                    ),
                );
                let event = SseEvent::Control(control);
                if send_event(&tx, "control", &event, state.sse_send_timeout).await.is_err() {
                    return Ok(());
                }
                return Ok(());
            }

            // Check for parent hash mismatch (reorg detection)
            if let Some(expected_parent) = prev_hash {
                if block.parent_hash != expected_parent {
                    let safe_restart_height = (block.height - 1).max(0);

                    snapshot.commit().await.map_err(|e| {
                        AppError::InternalError(format!(
                            "Failed to commit subscribe snapshot (reorg): {}",
                            e
                        ))
                    })?;

                    let control = ControlEvent::reset_required(
                        safe_restart_height,
                        format!(
                            "Reorg detected at height {}: parent hash mismatch",
                            block.height
                        ),
                    );
                    let event = SseEvent::Control(control);
                    if send_event(&tx, "control", &event, state.sse_send_timeout).await.is_err() {
                        return Ok(());
                    }
                    return Ok(());
                }
            }

            prev_hash = Some(&block.hash);
            expected_height += 1;
        }

        // Build and send block bundles (with inline merkle updates)
        let (bundles, new_merkle_end_index) = build_block_bundles_tx(
            &mut snapshot,
            &local_session_id_bytes,
            &upstream_session_id_bytes,
            &upstream_session_id_str,
            &blocks_meta,
            &state.subscription_manager,
            nullifier_mode,
            include_tx_meta,
            include_raw_tx,
            include_identifiers,
            include_all_dust,
            identifiers_source,
            last_merkle_end_index,
            state.merkle_cache_manager.as_ref(),
        )
        .await?;
        last_merkle_end_index = new_merkle_end_index;

        snapshot.commit().await.map_err(|e| {
            AppError::InternalError(format!(
                "Failed to commit subscribe snapshot (bundle build): {}",
                e
            ))
        })?;

        for mut bundle in bundles {
            let height = bundle.meta.height;
            if let Some(cursor) = from_cursor {
                if height == cursor.block_height {
                    bundle.items.retain(|item| {
                        parse_cursor(&item.sequence)
                            .map(|sequence| sequence > cursor)
                            .unwrap_or(true)
                    });
                    from_cursor = None;
                }
            }
            if bundle.items.is_empty() {
                current_height = height + 1;
                continue;
            }
            let event = SseEvent::Block(bundle);
            if send_event(&tx, "block", &event, state.sse_send_timeout).await.is_err() {
                return Ok(());
            }
            current_height = height + 1;
        }

        // Update last_block_hash for next batch boundary check
        if let Some(last_block) = blocks_meta.last() {
            last_block_hash = Some(last_block.hash.clone());
        }

        // Reset heartbeat timer after sending blocks
        last_heartbeat = std::time::Instant::now();
    }

    Ok(())
}

/// Send an SSE event through the channel with timeout for slow clients
async fn send_event<T: Serialize>(
    tx: &mpsc::Sender<Result<Event, Infallible>>,
    event_type: &str,
    data: &T,
    timeout: Duration,
) -> Result<(), ()> {
    let json = serde_json::to_string(data).map_err(|_| ())?;
    let event = Event::default().event(event_type).data(json);

    // Use timeout to handle slow clients - drop them if they can't keep up
    match tokio::time::timeout(timeout, tx.send(Ok(event))).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => {
            warn!("Failed to send SSE event, client disconnected");
            Err(())
        }
        Err(_) => {
            warn!(
                timeout_secs = timeout.as_secs(),
                "SSE send timeout, dropping slow client"
            );
            Err(())
        }
    }
}

/// Build block bundles for a range of blocks (reuses logic from feed.rs)
/// Returns the bundles and the updated last_merkle_end_index for tracking across batches
async fn build_block_bundles_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    local_session_id_bytes: &[u8],
    upstream_session_id_bytes: &[u8],
    upstream_session_id_str: &str,
    blocks_meta: &[crate::feed::BlockMeta],
    subscription_manager: &Arc<SubscriptionManager>,
    nullifier_mode: NullifierStreaming,
    include_tx_meta: bool,
    include_raw_tx: bool,
    include_identifiers: bool,
    include_all_dust: bool,
    identifiers_source: IdentifiersSource,
    mut last_merkle_end_index: i64,
    merkle_cache: Option<&Arc<MerkleCacheManager>>,
) -> Result<(Vec<BlockBundle>, i64), AppError> {
    if blocks_meta.is_empty() {
        return Ok((Vec::new(), last_merkle_end_index));
    }

    let from_height = blocks_meta.first().unwrap().height;
    let to_height = blocks_meta.last().unwrap().height;

    // Fetch relevant transactions for this range
    let relevant_txs =
        get_relevant_transactions_tx(tx, upstream_session_id_bytes, from_height, to_height).await?;

    // Collect tx IDs for batch queries (shielded txs)
    let shielded_tx_ids: Vec<i64> = relevant_txs.iter().map(|tx| tx.tx_id).collect();

    // Fetch zswap events for relevant txs
    let zswap_events = get_zswap_events_for_txs_tx(tx, &shielded_tx_ids).await?;

    // Group zswap events by tx_id
    let mut zswap_by_tx: HashMap<i64, Vec<String>> = HashMap::new();
    for event in zswap_events.iter() {
        zswap_by_tx
            .entry(event.tx_id)
            .or_default()
            .push(hex::encode(&event.raw));
    }

    // Fetch dust events
    // When include_all_dust is true, fetch ALL dust events for proper merkle tree sync
    // (the SDK needs all events in sequence since there's no collapsed update mechanism for dust)
    let dust_events = if include_all_dust {
        let events = get_all_dust_events_tx(tx, from_height, to_height).await?;
        info!(
            from_height,
            to_height,
            dust_event_count = events.len(),
            include_all_dust = true,
            "Fetched ALL dust events for block range"
        );
        events
    } else {
        let events =
            get_dust_events_tx(tx, local_session_id_bytes, from_height, to_height).await?;
        info!(
            from_height,
            to_height,
            dust_event_count = events.len(),
            include_all_dust = false,
            "Fetched wallet-filtered dust events for block range"
        );
        events
    };

    // Fetch unshielded UTXO events
    let unshielded_events =
        get_unshielded_utxo_events_tx(tx, local_session_id_bytes, from_height, to_height).await?;

    // Collect ALL tx_ids that need contract actions (shielded + unshielded + dust)
    // This ensures ALL contract actions are fetched, including dust-only txs (e.g., contract updates)
    let mut all_tx_ids_for_actions: HashSet<i64> = shielded_tx_ids.iter().copied().collect();
    for event in &unshielded_events {
        all_tx_ids_for_actions.insert(event.tx_id);
    }
    for dust in &dust_events {
        all_tx_ids_for_actions.insert(dust.tx_id);
    }
    let all_tx_ids: Vec<i64> = all_tx_ids_for_actions.into_iter().collect();

    // Fetch contract actions for ALL relevant txs (shielded + unshielded + dust)
    let contract_actions = get_contract_actions_for_txs_tx(tx, &all_tx_ids).await?;

    // Group contract actions by tx_id
    let mut actions_by_tx: HashMap<i64, Vec<_>> = HashMap::new();
    for (tx_id, action) in contract_actions {
        actions_by_tx.entry(tx_id).or_default().push(action);
    }

    // Determine wallet-relevant tx_ids for transaction metadata
    let mut wallet_tx_ids: HashSet<i64> = HashSet::new();
    for tx in &relevant_txs {
        wallet_tx_ids.insert(tx.tx_id);
    }
    for dust in &dust_events {
        wallet_tx_ids.insert(dust.tx_id);
    }
    for event in &unshielded_events {
        wallet_tx_ids.insert(event.tx_id);
    }

    let tx_meta_rows = if include_tx_meta {
        let ids: Vec<i64> = wallet_tx_ids.iter().copied().collect();
        get_transaction_metadata_tx(
            tx,
            &ids,
            include_raw_tx,
            include_identifiers,
            identifiers_source,
        )
        .await?
    } else {
        Vec::new()
    };

    // Fetch ALL zswap input events for global nullifiers
    // Skip fetch entirely if disabled to avoid unnecessary DB load
    let global_zswap_inputs = match nullifier_mode {
        NullifierStreaming::Disabled => Vec::new(),
        _ => get_all_zswap_input_events_in_range_tx(tx, from_height, to_height).await?,
    };

    // Build block bundles
    let mut block_bundles: Vec<BlockBundle> = Vec::new();

    for block_meta in blocks_meta {
        let height = block_meta.height;
        let mut sortable_items: Vec<SortableFeedItem> = Vec::new();

        // Add transaction metadata for wallet-relevant txs in this block
        if include_tx_meta {
            for meta in tx_meta_rows.iter().filter(|row| row.block_height == height) {
                let identifiers = meta.identifiers.as_ref().map(|items| {
                    items.iter().map(|item| hex::encode(item)).collect()
                });
                let merkle_root = meta
                    .merkle_tree_root
                    .as_ref()
                    .map(|root| hex::encode(root));
                let raw = meta.raw.as_ref().map(|bytes| hex::encode(bytes));

                let sequence = SequenceKey {
                    block_height: height,
                    tx_id: meta.tx_id,
                    phase: PHASE_TX,
                    ordinal: 0,
                };
                // Get contract actions for this transaction (for dust-only txs like contract updates)
                let tx_contract_actions = actions_by_tx
                    .get(&meta.tx_id)
                    .cloned()
                    .unwrap_or_default();
                sortable_items.push(SortableFeedItem::new(
                    sequence,
                    FeedItem::TransactionMeta(TransactionMeta {
                        tx_id: meta.tx_id,
                        tx_hash: hex::encode(&meta.tx_hash),
                        block_height: height,
                        protocol_version: meta.protocol_version,
                        transaction_result: meta.transaction_result.clone(),
                        identifiers,
                        fees: TransactionFees {
                            paid: decode_fee_option(&meta.paid_fees),
                            estimated: decode_fee_option(&meta.estimated_fees),
                        },
                        merkle: TransactionMerkle {
                            root: merkle_root,
                            start_index: meta.start_index,
                            end_index: meta.end_index,
                        },
                        raw,
                        contract_actions: tx_contract_actions,
                    }),
                ));
            }
        }

        // Add shielded relevant transactions for this block
        // Before each tx, check if we need to inject merkle collapsed updates
        for rtx in relevant_txs.iter().filter(|tx| tx.block_height == height) {
            // Check for merkle tree gap and inject collapsed update if needed
            if let (Some(tx_start_index), Some(tx_end_index)) = (rtx.start_index, rtx.end_index) {
                // Gap exists if tx starts after our current merkle position
                // last_merkle_end_index of -1 means fresh sync (need update from 0)
                let need_update = if last_merkle_end_index < 0 {
                    tx_start_index > 0
                } else {
                    tx_start_index > last_merkle_end_index
                };

                if need_update {
                    let from_index = if last_merkle_end_index < 0 { 0 } else { last_merkle_end_index };
                    let to_index = tx_start_index;

                    let mut injected = false;

                    // 1) Try local cache first (fast path).
                    if let Some(cache) = merkle_cache {
                        match cache.get_cached_updates(from_index, to_index).await {
                            Ok(updates)
                                if MerkleCacheManager::covers_range(&updates, from_index, to_index) =>
                            {
                                for (idx, update) in updates.iter().enumerate() {
                                    let sequence = SequenceKey {
                                        block_height: height,
                                        tx_id: rtx.tx_id,
                                        phase: PHASE_TX,
                                        ordinal: -(updates.len() as i64) + idx as i64,
                                    };
                                    sortable_items.push(SortableFeedItem::new(
                                        sequence,
                                        FeedItem::MerkleCollapsedUpdate(MerkleCollapsedUpdate {
                                            start_index: update.start_index,
                                            end_index: update.end_index,
                                            update: update.update_hex(),
                                            protocol_version: update.protocol_version as u32,
                                        }),
                                    ));
                                }

                                info!(
                                    from_index,
                                    to_index,
                                    update_count = updates.len(),
                                    tx_id = rtx.tx_id,
                                    source = "cache",
                                    "Injected inline merkle collapsed updates"
                                );
                                metrics::counter!("merkle_inline_updates_total")
                                    .increment(updates.len() as u64);
                                injected = true;
                            }
                            Ok(_) => {
                                // Cache doesn't cover range; fall back to upstream below.
                            }
                            Err(e) => {
                                warn!(
                                    error = %e,
                                    from_index,
                                    to_index,
                                    tx_id = rtx.tx_id,
                                    "Failed to query merkle cache for inline update"
                                );
                            }
                        }
                    }

                    // 2) Fall back to upstream on cache miss so the SSE stream stays self-contained.
                    if !injected {
                        match subscription_manager
                            .fetch_merkle_updates(upstream_session_id_str, from_index, to_index)
                            .await
                        {
                            Ok(updates) if !updates.is_empty() => {
                                if let Some(cache) = merkle_cache {
                                    if let Err(e) = cache.store_updates(&updates).await {
                                        warn!(
                                            error = %e,
                                            from_index,
                                            to_index,
                                            tx_id = rtx.tx_id,
                                            "Failed to store upstream merkle updates in cache"
                                        );
                                    }
                                }

                                for (idx, update) in updates.iter().enumerate() {
                                    let sequence = SequenceKey {
                                        block_height: height,
                                        tx_id: rtx.tx_id,
                                        phase: PHASE_TX,
                                        ordinal: -(updates.len() as i64) + idx as i64,
                                    };
                                    sortable_items.push(SortableFeedItem::new(
                                        sequence,
                                        FeedItem::MerkleCollapsedUpdate(MerkleCollapsedUpdate {
                                            start_index: update.start_index,
                                            end_index: update.end_index,
                                            update: update.update.clone(),
                                            protocol_version: update.protocol_version,
                                        }),
                                    ));
                                }

                                info!(
                                    from_index,
                                    to_index,
                                    update_count = updates.len(),
                                    tx_id = rtx.tx_id,
                                    source = "upstream",
                                    "Injected inline merkle collapsed updates"
                                );
                                metrics::counter!("merkle_inline_updates_total")
                                    .increment(updates.len() as u64);
                            }
                            Ok(_) => {
                                // No updates available; the client may need to recover or restart from 0.
                                warn!(
                                    from_index,
                                    to_index,
                                    tx_id = rtx.tx_id,
                                    "Upstream returned no merkle updates for inline injection"
                                );
                            }
                            Err(e) => {
                                warn!(
                                    error = %e,
                                    from_index,
                                    to_index,
                                    tx_id = rtx.tx_id,
                                    "Failed to fetch upstream merkle updates for inline injection"
                                );
                            }
                        }
                    }
                }

                // Update merkle position to this transaction's end index
                last_merkle_end_index = tx_end_index;
            }

            let sequence = SequenceKey {
                block_height: height,
                tx_id: rtx.tx_id,
                phase: PHASE_TX,
                ordinal: 1,
            };
            sortable_items.push(SortableFeedItem::new(
                sequence,
                FeedItem::ShieldedRelevantTx(ShieldedRelevantTx {
                    tx_id: rtx.tx_id,
                    tx_hash: hex::encode(&rtx.tx_hash),
                    block_height: height,
                    zswap_events: zswap_by_tx.get(&rtx.tx_id).cloned().unwrap_or_default(),
                    contract_actions: actions_by_tx.get(&rtx.tx_id).cloned().unwrap_or_default(),
                    fees_paid: decode_fee_string(&rtx.paid_fees),
                }),
            ));
        }

        // Add ZswapInputRaw for ALL zswap input events in this block (global spend detection)
        // Raw bytes are passed through - client parses with midnight-ledger SDK
        for event in global_zswap_inputs.iter().filter(|e| e.block_height == height) {
            let sequence = SequenceKey {
                block_height: height,
                tx_id: event.tx_id,
                phase: PHASE_LEDGER_EVENT,
                ordinal: event.ledger_event_id,
            };
            sortable_items.push(SortableFeedItem::new(
                sequence,
                FeedItem::ZswapInputRaw(ZswapInputRaw {
                    tx_id: event.tx_id,
                    tx_hash: hex::encode(&event.tx_hash),
                    ledger_event_id: event.ledger_event_id,
                    block_height: height,
                    raw: hex::encode(&event.raw),
                }),
            ));
        }

        // Add dust events for this block
        for dust in dust_events.iter().filter(|d| d.block_height == height) {
            let sequence = SequenceKey {
                block_height: height,
                tx_id: dust.tx_id,
                phase: PHASE_LEDGER_EVENT,
                ordinal: dust.ledger_event_id,
            };
            sortable_items.push(SortableFeedItem::new(
                sequence,
                FeedItem::DustLedgerEvent(DustLedgerEventRaw {
                    tx_id: dust.tx_id,
                    ledger_event_id: dust.ledger_event_id,
                    block_height: height,
                    raw: hex::encode(&dust.raw),
                }),
            ));
        }

        // Add unshielded UTXO events for this block
        for event in unshielded_events.iter().filter(|d| d.block_height == height) {
            let value = decode_u128_value(&event.value_bytes).to_string();
            let fees_paid = decode_fee_string(&event.paid_fees);
            let sequence = SequenceKey {
                block_height: height,
                tx_id: event.tx_id,
                phase: PHASE_UNSHIELDED,
                ordinal: event.utxo_id,
            };
            // Get ALL contract actions for this transaction (not just LIMIT 1!)
            let contract_actions = actions_by_tx
                .get(&event.tx_id)
                .cloned()
                .unwrap_or_default();
            sortable_items.push(SortableFeedItem::new(
                sequence,
                FeedItem::UnshieldedUtxo(UnshieldedUtxoEvent {
                    tx_id: event.tx_id,
                    tx_hash: hex::encode(&event.tx_hash),
                    block_height: height,
                    is_create: event.is_create,
                    address: hex::encode(&event.address),
                    intent_hash: hex::encode(&event.intent_hash),
                    output_index: event.output_index,
                    token_type: hex::encode(&event.token_type),
                    value,
                    initial_nonce: hex::encode(&event.initial_nonce),
                    registered_for_dust_generation: event.registered_for_dust_generation,
                    ctime: event.ctime,
                    fees_paid,
                    contract_actions,
                }),
            ));
        }

        // Sort items by global sequence ordering
        sortable_items.sort_by(|a, b| a.sequence.cmp(&b.sequence));

        // Validate event ordering (detect gaps in ledger_event_id sequences)
        validate_event_ordering(&sortable_items, height);

        let items: Vec<OrderedFeedItem> = sortable_items
            .into_iter()
            .map(|s| OrderedFeedItem {
                sequence: s.sequence.to_cursor(),
                item: s.item,
            })
            .collect();

        block_bundles.push(BlockBundle {
            meta: block_meta.clone(),
            items,
        });
    }

    Ok((block_bundles, last_merkle_end_index))
}

/// A sortable feed item wrapper for ordering by sequence
struct SortableFeedItem {
    sequence: SequenceKey,
    item: FeedItem,
}

impl SortableFeedItem {
    fn new(sequence: SequenceKey, item: FeedItem) -> Self {
        Self { sequence, item }
    }
}

/// Validate event ordering within a block and detect gaps in ledger_event_id sequences.
/// This is an informational check - gaps are logged and recorded as metrics but don't block processing.
fn validate_event_ordering(items: &[SortableFeedItem], block_height: i64) {
    // Group real ledger_event_ids by tx_id (exclude synthetic values 0 and i64::MAX)
    let mut events_by_tx: HashMap<i64, Vec<i64>> = HashMap::new();

    for item in items {
        let (tx_id, ledger_event_id) = match &item.item {
            FeedItem::ZswapInputRaw(event) => (event.tx_id, event.ledger_event_id),
            FeedItem::DustLedgerEvent(event) => (event.tx_id, event.ledger_event_id),
            _ => continue,
        };
        if ledger_event_id > 0 {
            events_by_tx.entry(tx_id).or_default().push(ledger_event_id);
        }
    }

    // Check each transaction for gaps in ledger_event_id sequence
    for (tx_id, mut event_ids) in events_by_tx {
        if event_ids.len() < 2 {
            continue; // Can't have a gap with 0 or 1 events
        }

        event_ids.sort();

        // Check for gaps (each ID should be previous + 1)
        for window in event_ids.windows(2) {
            let prev = window[0];
            let curr = window[1];
            if curr != prev + 1 {
                tracing::warn!(
                    block_height,
                    tx_id,
                    expected_event_id = prev + 1,
                    actual_event_id = curr,
                    gap_size = curr - prev - 1,
                    "Detected gap in ledger_event_id sequence"
                );
                crate::metrics::record_event_ordering_anomaly("ledger_event_gap");
            }
        }
    }
}
