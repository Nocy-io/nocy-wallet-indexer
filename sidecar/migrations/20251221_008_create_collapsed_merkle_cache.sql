-- Cache table for collapsed merkle tree updates from shieldedTransactions subscription
-- Used to serve historical merkle updates without requiring live subscription
CREATE TABLE nocy_sidecar.collapsed_merkle_cache (
    start_index BIGINT NOT NULL,
    end_index BIGINT NOT NULL,
    update_bytes BYTEA NOT NULL,
    protocol_version INT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (start_index, end_index)
);

-- Index for efficient range queries when fetching updates for a given range
CREATE INDEX idx_collapsed_merkle_cache_end_index
    ON nocy_sidecar.collapsed_merkle_cache(end_index);

-- Singleton table to track background subscription state
-- Used to resume subscription from last cached index after restart
CREATE TABLE nocy_sidecar.merkle_cache_state (
    id INT PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    highest_end_index BIGINT NOT NULL DEFAULT 0,
    session_id TEXT,
    viewing_key TEXT,
    last_updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Initialize with zero state (singleton row)
INSERT INTO nocy_sidecar.merkle_cache_state (id, highest_end_index)
VALUES (1, 0);

COMMENT ON TABLE nocy_sidecar.collapsed_merkle_cache IS
    'Stores collapsed merkle tree updates for historical queries. Each row represents one transaction''s merkle update.';

COMMENT ON TABLE nocy_sidecar.merkle_cache_state IS
    'Singleton table tracking the background merkle cache subscription state for resumption.';
