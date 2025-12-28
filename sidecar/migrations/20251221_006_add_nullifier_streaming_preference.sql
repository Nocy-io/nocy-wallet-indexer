-- Add nullifier_streaming preference to session_profile for existing databases
-- Values: 'enabled' (default), 'disabled'
-- 'enabled': stream all ZswapInputRaw events
-- 'disabled': skip nullifier streaming (for wallets using 0-value change outputs)

-- Add column with default (backfills existing rows automatically)
ALTER TABLE nocy_sidecar.session_profile
ADD COLUMN IF NOT EXISTS nullifier_streaming VARCHAR(16) NOT NULL DEFAULT 'enabled'
CHECK (nullifier_streaming IN ('enabled', 'disabled'));

COMMENT ON COLUMN nocy_sidecar.session_profile.nullifier_streaming IS 'Nullifier streaming preference: enabled (all events), disabled (no events)';
