-- Session unshielded addresses for filtering unshielded deltas
-- session_id is a 32-byte SHA256 hash matching session_profile.session_id
CREATE TABLE nocy_sidecar.session_unshielded_address (
    session_id BYTEA NOT NULL CHECK (octet_length(session_id) = 32),
    address BYTEA NOT NULL,
    PRIMARY KEY (session_id, address),
    FOREIGN KEY (session_id) REFERENCES nocy_sidecar.session_profile(session_id) ON DELETE CASCADE
);

CREATE INDEX idx_session_unshielded_address_session_id ON nocy_sidecar.session_unshielded_address(session_id);
