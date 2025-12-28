-- Recreate nullifier cache tables (if reverting)

-- Cache table for parsed zswap input nullifiers
CREATE TABLE IF NOT EXISTS nocy_sidecar.zswap_input_nullifiers (
    ledger_event_id BIGINT NOT NULL,
    transaction_id BIGINT NOT NULL,
    block_height BIGINT NOT NULL,
    tx_hash BYTEA NOT NULL,
    nullifier BYTEA NOT NULL,
    contract_address BYTEA,
    PRIMARY KEY (ledger_event_id, nullifier)
);

CREATE INDEX IF NOT EXISTS idx_zswap_input_nullifiers_block_height ON nocy_sidecar.zswap_input_nullifiers(block_height);
CREATE INDEX IF NOT EXISTS idx_zswap_input_nullifiers_tx_hash ON nocy_sidecar.zswap_input_nullifiers(tx_hash);
CREATE INDEX IF NOT EXISTS idx_zswap_input_nullifiers_transaction_id ON nocy_sidecar.zswap_input_nullifiers(transaction_id);

-- Marker table to track fully cached nullifier events
CREATE TABLE IF NOT EXISTS nocy_sidecar.zswap_nullifier_cache_marker (
    ledger_event_id BIGINT PRIMARY KEY,
    nullifier_count INTEGER NOT NULL,
    cached_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_nullifier_cache_marker_event
    ON nocy_sidecar.zswap_nullifier_cache_marker (ledger_event_id);
