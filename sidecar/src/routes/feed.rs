use axum::{
    extract::{Query, State},
    Json,
};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use tracing::{info, instrument};

use crate::db::{
    begin_readonly_tx, compute_wallet_ready_height_tx, detect_identifiers_source,
    get_all_dust_events_tx, get_all_zswap_input_events_in_range_tx, get_block_hash_at_height_tx,
    get_blocks_in_range_tx, get_chain_head_tx, get_contract_actions_for_txs_tx,
    get_dust_events_tx, get_relevant_transactions_tx, get_transaction_metadata_tx,
    get_unshielded_utxo_events_tx, get_upstream_session_id_tx, get_zswap_events_for_txs_tx,
    IdentifiersSource,
};
use crate::error::AppError;
use crate::feed::{
    BlockBundle, ControlEvent, DustLedgerEventRaw, FeedItem, FeedResponse, OrderedFeedItem,
    ShieldedRelevantTx, TransactionFees, TransactionMerkle, TransactionMeta, UnshieldedUtxoEvent,
    Watermarks, ZswapInputRaw,
};
use crate::routes::session::{get_session_nullifier_streaming, NullifierStreaming, SessionState};
use crate::sequence::{parse_cursor, SequenceKey};

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

/// Session ID is a 32-byte SHA256 hash, hex-encoded (64 chars)
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

/// Query parameters for GET /v1/feed
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedQuery {
    /// Session ID (hex-encoded 32 bytes)
    pub session_id: String,
    /// Starting block height (defaults to 0)
    #[serde(default)]
    pub from_height: Option<i64>,
    /// Starting sequence cursor (overrides from_height if provided)
    #[serde(default)]
    pub from_cursor: Option<String>,
    /// Maximum number of blocks to return (defaults to config limit)
    pub limit_blocks: Option<i64>,
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

/// Default blocks per request if not specified
const DEFAULT_LIMIT_BLOCKS: i64 = 50;

const PHASE_TX: i64 = 0;
const PHASE_LEDGER_EVENT: i64 = 1;
const PHASE_UNSHIELDED: i64 = 2;
const PHASE_CONTROL: i64 = -1;

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

/// GET /v1/feed - Fetch feed blocks with watermarks
#[instrument(skip(state), fields(session_id = %query.session_id))]
pub async fn get_feed(
    State(state): State<SessionState>,
    Query(query): Query<FeedQuery>,
) -> Result<Json<FeedResponse>, AppError> {
    let timer = crate::metrics::Timer::start();

    // Parse and validate session ID
    let local_session_id_bytes = parse_session_id(&query.session_id)?;
    let from_cursor = match query.from_cursor.as_deref() {
        Some(cursor) => Some(
            parse_cursor(cursor).map_err(|message| AppError::BadRequest(message))?,
        ),
        None => None,
    };

    // Use limit_blocks_max from config as the ceiling
    let max_limit = state.limit_blocks_max as i64;
    let from_height = from_cursor
        .map(|cursor| cursor.block_height)
        .unwrap_or_else(|| query.from_height.unwrap_or(0))
        .max(0);
    let limit_blocks = query
        .limit_blocks
        .unwrap_or(DEFAULT_LIMIT_BLOCKS)
        .min(max_limit)
        .max(1);

    info!(from_height, limit_blocks, "Fetching feed");

    // Determine nullifier streaming mode
    // Query param include_nullifiers=false takes precedence, else use session preference
    let nullifier_mode = if !query.include_nullifiers {
        NullifierStreaming::Disabled
    } else {
        get_session_nullifier_streaming(&state.db_pool, &query.session_id).await?
    };

    let identifiers_source = if query.include_tx_meta && query.include_identifiers {
        detect_identifiers_source(&state.db_pool).await?
    } else {
        IdentifiersSource::None
    };

    let mut snapshot = begin_readonly_tx(&state.db_pool).await?;

    // Resolve upstream wallet-indexer session ID for this local session.
    // Shielded wallet data is keyed by upstream session_id in the upstream schema.
    let upstream_session_id_bytes =
        get_upstream_session_id_tx(&mut snapshot, &local_session_id_bytes).await?;

    // Get chain head
    let chain_head = get_chain_head_tx(&mut snapshot).await?;

    // Compute wallet ready height
    let wallet_ready_height =
        compute_wallet_ready_height_tx(&mut snapshot, &upstream_session_id_bytes, chain_head)
            .await?;

    // Build watermarks (always returned, even with empty blocks)
    // Note: finalized_height equals chain_head since indexer only processes finalized blocks
    let watermarks = Watermarks {
        chain_head,
        wallet_ready_height,
        finalized_height: Some(chain_head),
    };

    // If wallet isn't ready or from_height is beyond ready height, return empty
    let ready_height = match wallet_ready_height {
        Some(h) => h,
        None => {
            info!("Wallet not ready, returning empty feed");
            return Ok(Json(FeedResponse::empty(watermarks)));
        }
    };

    if from_height > ready_height {
        info!(from_height, ready_height, "from_height beyond ready height");
        return Ok(Json(FeedResponse::empty(watermarks)));
    }

    // Calculate the actual range to fetch (bounded by ready height and limit)
    let to_height = (from_height + limit_blocks - 1).min(ready_height);

    // Fetch block metadata
    let blocks_meta = get_blocks_in_range_tx(&mut snapshot, from_height, to_height).await?;

    if blocks_meta.is_empty() {
        return Ok(Json(FeedResponse::empty(watermarks)));
    }

    // Validate contiguous blocks - check for gaps and parent hash mismatches (reorg detection)
    let mut expected_height = from_height;

    // For boundary reorg detection: fetch the previous block's hash if from_height > 0
    // This ensures we detect reorgs at the batch boundary (first block in range)
    let boundary_prev_hash: Option<String> = if from_height > 0 {
        get_block_hash_at_height_tx(&mut snapshot, from_height - 1).await?
    } else {
        None
    };
    let mut prev_hash: Option<&str> = boundary_prev_hash.as_deref();

    for block in &blocks_meta {
        // Check for height gaps
        if block.height != expected_height {
            let safe_restart_height = (expected_height - 1).max(0);
            info!(
                expected = expected_height,
                actual = block.height,
                safe_restart_height,
                "Block gap detected, emitting reset"
            );

            // Track reorg metric
            crate::metrics::record_reorg_detected("gap");

            let mut response = FeedResponse::new(watermarks);
            let sequence = SequenceKey {
                block_height: blocks_meta[0].height,
                tx_id: -1,
                phase: PHASE_CONTROL,
                ordinal: 0,
            };
            response.blocks.push(BlockBundle {
                meta: blocks_meta[0].clone(),
                items: vec![OrderedFeedItem {
                    sequence: sequence.to_cursor(),
                    item: FeedItem::Control(ControlEvent::reset_required(
                        safe_restart_height,
                        format!(
                            "Block gap detected: expected height {}, got {}",
                            expected_height, block.height
                        ),
                    )),
                }],
            });
            return Ok(Json(response));
        }

        // Check for parent hash mismatch (reorg detection)
        if let Some(expected_parent) = prev_hash {
            if block.parent_hash != expected_parent {
                // Reorg detected - parent hash doesn't match previous block's hash
                let safe_restart_height = (block.height - 1).max(0);
                info!(
                    height = block.height,
                    expected_parent = expected_parent,
                    actual_parent = %block.parent_hash,
                    safe_restart_height,
                    "Reorg detected: parent hash mismatch"
                );

                // Track reorg metric
                crate::metrics::record_reorg_detected("parent_hash_mismatch");

                let mut response = FeedResponse::new(watermarks);
                let sequence = SequenceKey {
                    block_height: blocks_meta[0].height,
                    tx_id: -1,
                    phase: PHASE_CONTROL,
                    ordinal: 0,
                };
                response.blocks.push(BlockBundle {
                    meta: blocks_meta[0].clone(),
                    items: vec![OrderedFeedItem {
                        sequence: sequence.to_cursor(),
                        item: FeedItem::Control(ControlEvent::reset_required(
                            safe_restart_height,
                            format!(
                                "Reorg detected at height {}: parent hash mismatch",
                                block.height
                            ),
                        )),
                    }],
                });
                return Ok(Json(response));
            }
        }

        prev_hash = Some(&block.hash);
        expected_height += 1;
    }

    // Fetch relevant transactions for this range
    let relevant_txs = get_relevant_transactions_tx(
        &mut snapshot,
        &upstream_session_id_bytes,
        from_height,
        to_height,
    )
    .await?;

    // Collect tx IDs for batch queries (shielded relevant txs)
    let shielded_tx_ids: Vec<i64> = relevant_txs.iter().map(|tx| tx.tx_id).collect();

    // Fetch zswap events for relevant txs
    let zswap_events = get_zswap_events_for_txs_tx(&mut snapshot, &shielded_tx_ids).await?;

    // Group zswap events by tx_id (use iter to avoid consuming)
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
    let dust_events = if query.include_all_dust {
        get_all_dust_events_tx(&mut snapshot, from_height, to_height).await?
    } else {
        get_dust_events_tx(&mut snapshot, &local_session_id_bytes, from_height, to_height).await?
    };

    // Fetch unshielded UTXO events
    let unshielded_events =
        get_unshielded_utxo_events_tx(&mut snapshot, &local_session_id_bytes, from_height, to_height)
            .await?;

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
    let contract_actions = get_contract_actions_for_txs_tx(&mut snapshot, &all_tx_ids).await?;

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

    let tx_meta_rows = if query.include_tx_meta {
        let ids: Vec<i64> = wallet_tx_ids.iter().copied().collect();
        get_transaction_metadata_tx(
            &mut snapshot,
            &ids,
            query.include_raw_tx,
            query.include_identifiers,
            identifiers_source,
        )
        .await?
    } else {
        Vec::new()
    };

    // Fetch ALL zswap input events for global nullifiers (not just relevant txs)
    // Skip fetch entirely if disabled to avoid unnecessary DB load
    let global_zswap_inputs = match nullifier_mode {
        NullifierStreaming::Disabled => Vec::new(),
        _ => get_all_zswap_input_events_in_range_tx(&mut snapshot, from_height, to_height).await?,
    };

    snapshot
        .commit()
        .await
        .map_err(|e| AppError::InternalError(format!("Failed to commit feed snapshot: {}", e)))?;

    // Build block bundles with proper ordering
    let mut block_bundles: Vec<BlockBundle> = Vec::new();

    for block_meta in blocks_meta {
        let height = block_meta.height;
        let mut sortable_items: Vec<SortableFeedItem> = Vec::new();

        // Add transaction metadata for wallet-relevant txs in this block
        if query.include_tx_meta {
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
        for tx in relevant_txs.iter().filter(|tx| tx.block_height == height) {
            let sequence = SequenceKey {
                block_height: height,
                tx_id: tx.tx_id,
                phase: PHASE_TX,
                ordinal: 1,
            };
            sortable_items.push(SortableFeedItem::new(
                sequence,
                FeedItem::ShieldedRelevantTx(ShieldedRelevantTx {
                    tx_id: tx.tx_id,
                    tx_hash: hex::encode(&tx.tx_hash),
                    block_height: height,
                    zswap_events: zswap_by_tx.get(&tx.tx_id).cloned().unwrap_or_default(),
                    contract_actions: actions_by_tx.get(&tx.tx_id).cloned().unwrap_or_default(),
                    fees_paid: decode_fee_string(&tx.paid_fees),
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

        // Extract sorted items
        let mut items: Vec<OrderedFeedItem> = sortable_items
            .into_iter()
            .map(|s| OrderedFeedItem {
                sequence: s.sequence.to_cursor(),
                item: s.item,
            })
            .collect();

        if let Some(cursor) = from_cursor {
            if cursor.block_height == height {
                items.retain(|item| {
                    parse_cursor(&item.sequence)
                        .map(|sequence| sequence > cursor)
                        .unwrap_or(true)
                });
            }
        }

        if items.is_empty() {
            continue;
        }

        block_bundles.push(BlockBundle {
            meta: block_meta,
            items,
        });
    }

    // Calculate next height cursor (advance by whole blocks only)
    let next_height = if to_height < ready_height {
        Some(to_height + 1)
    } else {
        None
    };

    let next_cursor = block_bundles
        .last()
        .and_then(|bundle| bundle.items.last())
        .map(|item| item.sequence.clone());

    let response = FeedResponse {
        api_version: crate::feed::API_VERSION,
        watermarks,
        blocks: block_bundles,
        next_height,
        next_cursor,
    };

    // Record metrics
    crate::metrics::record_feed_assembly(timer.elapsed());
    crate::metrics::record_blocks_returned(response.blocks.len() as u64);

    info!(
        blocks_returned = response.blocks.len(),
        next_height = ?response.next_height,
        "Feed response ready"
    );

    Ok(Json(response))
}
