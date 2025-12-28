-- Add upstream_session_id column to session_profile
-- This stores the session ID from the upstream indexer, which is used for keepalive.
-- The local session_id (primary key) is now generated deterministically from credentials,
-- while upstream_session_id is the actual session ID from the upstream indexer.

ALTER TABLE nocy_sidecar.session_profile
ADD COLUMN upstream_session_id BYTEA;

-- Index for looking up by upstream session ID (used in keepalive and disconnect)
CREATE INDEX idx_session_profile_upstream_session_id
ON nocy_sidecar.session_profile(upstream_session_id)
WHERE upstream_session_id IS NOT NULL;

COMMENT ON COLUMN nocy_sidecar.session_profile.upstream_session_id IS
'Session ID from upstream indexer (32 bytes). Used for keepalive and upstream disconnect. May differ from local session_id.';
