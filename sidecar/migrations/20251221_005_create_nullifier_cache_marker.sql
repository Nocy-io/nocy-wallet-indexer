-- Marker table to track fully cached nullifier events
CREATE TABLE IF NOT EXISTS nocy_sidecar.zswap_nullifier_cache_marker (
    ledger_event_id BIGINT PRIMARY KEY,
    nullifier_count INTEGER NOT NULL,
    cached_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Index for quick lookups
CREATE INDEX IF NOT EXISTS idx_nullifier_cache_marker_event
    ON nocy_sidecar.zswap_nullifier_cache_marker (ledger_event_id);
