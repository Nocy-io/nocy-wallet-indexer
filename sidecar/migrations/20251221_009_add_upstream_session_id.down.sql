-- Revert: Remove upstream_session_id column from session_profile

DROP INDEX IF EXISTS nocy_sidecar.idx_session_profile_upstream_session_id;
ALTER TABLE nocy_sidecar.session_profile DROP COLUMN IF EXISTS upstream_session_id;
