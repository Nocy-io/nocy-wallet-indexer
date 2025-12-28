# Sidecar Implementation Audit Report

This document records the audit of the `nocy-wallet-feed` sidecar against the upstream Midnight repositories to verify correctness and alignment.

## Repositories Audited

- `./repositories/midnight-indexer` - Chain/wallet indexer and API
- `./repositories/midnight-ledger` - Ledger event types and SDK
- `./repositories/midnight-node` - Node runtime and block structure

---

## 1. Data Ordering Consistency

### Upstream Behavior
- **Blocks**: Fetched sequentially with parent hash validation; ordered by height
- **Transactions**: Ordered by `transactions.id` within a block
- **Ledger Events**: Ordered by `ledger_events.id` within a transaction

### Sidecar Implementation
**Status: ALIGNED**

- Blocks fetched via `get_blocks_in_range()` with `ORDER BY height ASC`
- Feed items sorted by `(tx_id, ledger_event_id)` before assembly
- `SortableFeedItem` wrapper ensures consistent ordering
- Block bundles are contiguous (validated with gap detection)

**Reference**: [routes/feed.rs:349-352](src/routes/feed.rs#L349-L352)
```rust
sortable_items.sort_by(|a, b| {
    a.tx_id.cmp(&b.tx_id).then(a.ledger_event_id.cmp(&b.ledger_event_id))
});
```

---

## 2. WalletReadyHeight Gating

### Upstream Behavior
- Wallet indexer tracks `wallets.last_indexed_transaction_id`
- Processes transactions sequentially starting from `last_indexed + 1`
- Emits `WalletIndexed` event after saving relevant transactions

### Sidecar Implementation
**Status: ALIGNED**

- `compute_wallet_ready_height()` finds first block with `regular_transactions.id > last_indexed`
- Returns `height - 1` as the ready height (ready up to the block before blocking tx)
- Handles edge cases: NULL last_indexed treated as 0, missing wallet returns NotFound
- Feed returns empty blocks if `from_height > wallet_ready_height`

**Reference**: [db.rs:103-160](src/db.rs#L103-L160)

---

## 3. Full History Completeness

### Upstream Behavior
- Shielded: `relevant_transactions` table links wallets to transactions
- Unshielded: `unshielded_utxos` table tracks creates and spends by owner
- Dust: `ledger_events` with `grouping = 'Dust'`
- Global Nullifiers: All `ZswapInput` events, not just relevant txs

### Sidecar Implementation
**Status: ALIGNED**

Feed includes all required data types:
1. **ShieldedRelevantTx**: Relevant transactions with zswap events and contract actions
2. **ZswapInputRaw**: Raw bytes from ALL ZswapInput events for client-side nullifier extraction
3. **DustLedgerEvent**: Dust events filtered by session's dust public key
4. **UnshieldedDelta**: Creates (+) and spends (-) for registered addresses

**Reference**: [routes/feed.rs:242-244](src/routes/feed.rs#L242-L244)
```rust
// Fetch ALL zswap input events for global nullifiers (not just relevant txs)
let global_zswap_inputs =
    get_all_zswap_input_events_in_range(&state.db_pool, from_height, to_height).await?;
```

---

## 4. Nullifier Streaming (Raw Bytes)

### Upstream Behavior (midnight-ledger + midnight-indexer)
- Events use tagged serialization: `"event[v6]"` with `EventDetailsV6`
- Chain indexer stores raw bytes via `event.tagged_serialize_v6()` (see `indexer-common/src/domain/ledger/ledger_state.rs:378-380`)
- `ZswapInput { nullifier: CoinNullifier, contract: Option<ContractAddress> }`
- Nullifier is 32-byte persistent hash derived from coin info + sender evidence

### Sidecar Implementation
**Status: ALIGNED (Raw Pass-through)**

The sidecar passes through raw ZswapInput event bytes without server-side parsing:
- Queries `ledger_events.raw` for ZswapInput events directly from upstream tables
- Hex-encodes raw bytes and sends as `zswapInputRaw` feed items
- Client parses raw bytes using midnight-ledger SDK to extract nullifiers

**Spend Detection Approaches**:
1. **Global nullifier streaming** (default): Client receives all `zswapInputRaw` events, parses with midnight-ledger SDK
2. **0-value change outputs**: Client uses wallet-side spend detection, disables streaming via `includeNullifiers=false`

**Control Flow**:
- `includeNullifiers` query param (default: `true`) overrides session preference
- Session preference `nullifierStreaming` stored in `nocy_sidecar.session_profile`
- When disabled, DB query is skipped entirely for performance

**Reference**:
- ZswapInputRaw struct: [feed.rs:113-127](src/feed.rs#L113-L127)
- Feed assembly: [routes/feed.rs:264-306](src/routes/feed.rs#L264-L306)
- Upstream serialization: `indexer-common/src/domain/ledger/ledger_state.rs:378-380`

---

## 5. Reorg Handling

### Upstream Behavior (midnight-node)
- Uses Substrate's GRANDPA finality
- Blocks not finalized may be reverted
- Parent hash chain must be continuous

### Sidecar Implementation
**Status: ALIGNED**

Implements comprehensive reorg detection:
1. **Height gap detection**: Checks for missing blocks in sequence
2. **Parent hash mismatch**: Compares block N+1's parent_hash to block N's hash
3. **Boundary detection**: Fetches previous block hash for first block in batch
4. **Reset signaling**: Emits `ControlEvent::ResetRequired` with safe restart height

**Reference**: [routes/feed.rs:134-207](src/routes/feed.rs#L134-L207)
```rust
// For boundary reorg detection: fetch the previous block's hash if from_height > 0
let boundary_prev_hash: Option<String> = if from_height > 0 {
    get_block_hash_at_height(&state.db_pool, from_height - 1).await?
} else {
    None
};
```

---

## 6. Session Management

### Upstream Behavior
- `connect(viewingKey)` derives session_id from viewing key
- Session tracked in `wallets` table with encrypted viewing key
- `last_indexed_transaction_id` tracks indexing progress

### Sidecar Implementation
**Status: ALIGNED**

- Bootstrap proxies to upstream indexer for session creation
- Profile stored in `nocy_sidecar.session_profile` (dust key only, no viewing key)
- Unshielded addresses stored in `nocy_sidecar.session_unshielded_address`
- Keepalive maintains wallet activity in upstream

---

## 7. Event Types for Ledger SDK

### Required by SDK
The ledger SDK needs events to:
1. Update Merkle tree with commitments (ZswapOutput)
2. Remove spent coins via nullifiers (ZswapInput)
3. Track dust generation lifecycle
4. Maintain unshielded balance

### Sidecar Provides
1. **ZswapInputRaw**: Raw event bytes for client-side nullifier extraction and spend detection
2. **ShieldedRelevantTx.zswap_events**: Raw event bytes for full event replay
3. **DustLedgerEvent**: Raw dust events for local processing
4. **UnshieldedDelta**: Balance changes for unshielded addresses

**Note**: The sidecar provides raw event bytes, allowing clients to deserialize and replay events using the midnight-ledger SDK directly. Nullifier extraction is performed client-side.

---

## Summary

| Area | Status | Notes |
|------|--------|-------|
| Data Ordering | ALIGNED | Sorted by (tx_id, ledger_event_id) |
| WalletReadyHeight | ALIGNED | Matches upstream last_indexed semantics |
| History Completeness | ALIGNED | All event types included |
| Nullifier Streaming | ALIGNED | Raw bytes passed through; client-side parsing |
| Reorg Handling | ALIGNED | Comprehensive detection and reset |
| Session Management | ALIGNED | Proxies to upstream, stores profile locally |
| SDK Event Types | ALIGNED | Raw bytes enable client-side SDK replay |

---

## Recommendations

1. **Finalized Height**: ✅ IMPLEMENTED - The upstream chain-indexer only subscribes to finalized blocks via `subscribe_finalized()`, meaning all blocks in the database are finalized. `finalizedHeight` now equals `chainHead` in watermarks.

2. **Event Ordering**: ✅ IMPLEMENTED - Added `validate_event_ordering()` function that detects gaps in `ledger_event_id` sequences within transactions. Gaps are logged with warnings and recorded via the `event_ordering_anomalies_total` Prometheus metric (with `type=ledger_event_gap` label).

3. **Client SDK Version**: ✅ IMPLEMENTED - Added "SDK Compatibility" section to API.md documenting `@midnight/ledger ^6.1.0` (JS/TS) and `midnight-ledger ^6.1.0` (Rust) requirements, including code examples for parsing raw event bytes and a protocol version compatibility matrix.

---

## Files Referenced

- `sidecar/src/feed.rs` - Feed types and serialization
- `sidecar/src/db.rs` - Database queries and wallet readiness
- `sidecar/src/routes/feed.rs` - Feed endpoint and assembly logic
- `sidecar/src/routes/subscribe.rs` - SSE subscription with reorg detection
- `sidecar/src/routes/session.rs` - Session management and nullifier streaming preference
