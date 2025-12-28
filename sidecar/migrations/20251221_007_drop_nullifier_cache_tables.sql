-- Drop nullifier cache tables (no longer needed)
-- Raw bytes are now passed through directly from ledger_events.raw
-- Clients parse nullifiers using midnight-ledger SDK

DROP TABLE IF EXISTS nocy_sidecar.zswap_input_nullifiers;
DROP TABLE IF EXISTS nocy_sidecar.zswap_nullifier_cache_marker;
