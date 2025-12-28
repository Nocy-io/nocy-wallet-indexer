-- Rollback collapsed merkle cache tables
DROP TABLE IF EXISTS nocy_sidecar.merkle_cache_state;
DROP TABLE IF EXISTS nocy_sidecar.collapsed_merkle_cache;
