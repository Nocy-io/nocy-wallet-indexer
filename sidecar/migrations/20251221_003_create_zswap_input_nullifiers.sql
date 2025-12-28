-- Cache table for parsed zswap input nullifiers from ledger_events.raw
CREATE TABLE nocy_sidecar.zswap_input_nullifiers (
    ledger_event_id BIGINT NOT NULL,
    transaction_id BIGINT NOT NULL,
    block_height BIGINT NOT NULL,
    tx_hash BYTEA NOT NULL,
    nullifier BYTEA NOT NULL,
    contract_address BYTEA,
    PRIMARY KEY (ledger_event_id, nullifier)
);

CREATE INDEX idx_zswap_input_nullifiers_block_height ON nocy_sidecar.zswap_input_nullifiers(block_height);
CREATE INDEX idx_zswap_input_nullifiers_tx_hash ON nocy_sidecar.zswap_input_nullifiers(tx_hash);
CREATE INDEX idx_zswap_input_nullifiers_transaction_id ON nocy_sidecar.zswap_input_nullifiers(transaction_id);
