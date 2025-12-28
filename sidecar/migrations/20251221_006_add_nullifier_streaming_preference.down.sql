-- Revert nullifier_streaming preference
ALTER TABLE nocy_sidecar.session_profile DROP COLUMN IF EXISTS nullifier_streaming;
