-- Create nocy_sidecar schema for sidecar-owned tables
CREATE SCHEMA IF NOT EXISTS nocy_sidecar;

-- Session profile table stores dust public key per session
-- session_id is a 32-byte SHA256 hash (derived from viewing key by upstream)
CREATE TABLE nocy_sidecar.session_profile (
    session_id BYTEA PRIMARY KEY CHECK (octet_length(session_id) = 32),
    dust_public_key BYTEA,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Index for cleanup queries
CREATE INDEX idx_session_profile_updated_at ON nocy_sidecar.session_profile(updated_at);

-- Comment for documentation
COMMENT ON TABLE nocy_sidecar.session_profile IS 'Stores session-specific profile data including dust public key. session_id is 32-byte SHA256 hash.';
