use sqlx::PgPool;

/// Ensure the sidecar-owned schema and tables exist.
///
/// The indexer components already use SQLx migrations (tracked in `_sqlx_migrations`) for the
/// shared Postgres database. To avoid migration-table conflicts, the sidecar bootstraps its own
/// tables with idempotent DDL instead of running SQLx migrations.
pub async fn ensure_schema(pool: &PgPool) -> anyhow::Result<()> {
    // Schema
    sqlx::query("CREATE SCHEMA IF NOT EXISTS nocy_sidecar")
        .execute(pool)
        .await?;

    // Session profile
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS nocy_sidecar.session_profile (
            session_id BYTEA PRIMARY KEY CHECK (octet_length(session_id) = 32),
            upstream_session_id BYTEA,
            dust_public_key BYTEA,
            nullifier_streaming VARCHAR(16) NOT NULL DEFAULT 'enabled',
            updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            CONSTRAINT session_profile_nullifier_streaming_check
                CHECK (nullifier_streaming IN ('enabled', 'disabled'))
        )
        "#,
    )
    .execute(pool)
    .await?;

    // For older deployments: add missing upstream mapping column (no-op if already present).
    sqlx::query(
        r#"
        ALTER TABLE nocy_sidecar.session_profile
        ADD COLUMN IF NOT EXISTS upstream_session_id BYTEA
        "#,
    )
    .execute(pool)
    .await?;

    // For older deployments: add missing column (no-op if already present).
    sqlx::query(
        r#"
        ALTER TABLE nocy_sidecar.session_profile
        ADD COLUMN IF NOT EXISTS nullifier_streaming VARCHAR(16) NOT NULL DEFAULT 'enabled'
        "#,
    )
    .execute(pool)
    .await?;

    // Add the CHECK constraint if missing (Postgres has no `ADD CONSTRAINT IF NOT EXISTS`).
    let constraint_exists: (bool,) = sqlx::query_as(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM pg_constraint
            WHERE conname = 'session_profile_nullifier_streaming_check'
              AND conrelid = 'nocy_sidecar.session_profile'::regclass
        )
        "#,
    )
    .fetch_one(pool)
    .await?;

    if !constraint_exists.0 {
        sqlx::query(
            r#"
            ALTER TABLE nocy_sidecar.session_profile
            ADD CONSTRAINT session_profile_nullifier_streaming_check
                CHECK (nullifier_streaming IN ('enabled', 'disabled'))
            "#,
        )
        .execute(pool)
        .await?;
    }

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_session_profile_updated_at ON nocy_sidecar.session_profile(updated_at)",
    )
    .execute(pool)
    .await?;

    // Index for looking up by upstream session ID (keepalive/disconnect lookups).
    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_session_profile_upstream_session_id
        ON nocy_sidecar.session_profile(upstream_session_id)
        WHERE upstream_session_id IS NOT NULL
        "#,
    )
    .execute(pool)
    .await?;

    // Unshielded addresses
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS nocy_sidecar.session_unshielded_address (
            session_id BYTEA NOT NULL CHECK (octet_length(session_id) = 32),
            address BYTEA NOT NULL,
            PRIMARY KEY (session_id, address),
            FOREIGN KEY (session_id) REFERENCES nocy_sidecar.session_profile(session_id) ON DELETE CASCADE
        )
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_session_unshielded_address_session_id ON nocy_sidecar.session_unshielded_address(session_id)",
    )
    .execute(pool)
    .await?;

    // Optional cache table (not required for correctness).
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS nocy_sidecar.block_max_regular_tx (
            block_height BIGINT PRIMARY KEY,
            max_regular_tx_id BIGINT
        )
        "#,
    )
    .execute(pool)
    .await?;

    // Collapsed merkle cache table - stores merkle updates from background subscription
    // for serving historical merkle update requests without live subscription
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS nocy_sidecar.collapsed_merkle_cache (
            start_index BIGINT NOT NULL,
            end_index BIGINT NOT NULL,
            update_bytes BYTEA NOT NULL,
            protocol_version INT NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            PRIMARY KEY (start_index, end_index)
        )
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_collapsed_merkle_cache_end_index ON nocy_sidecar.collapsed_merkle_cache(end_index)",
    )
    .execute(pool)
    .await?;

    // Merkle cache state table - singleton tracking background subscription state
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS nocy_sidecar.merkle_cache_state (
            id INT PRIMARY KEY DEFAULT 1 CHECK (id = 1),
            highest_end_index BIGINT NOT NULL DEFAULT 0,
            session_id TEXT,
            viewing_key TEXT,
            last_updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )
        "#,
    )
    .execute(pool)
    .await?;

    // Ensure singleton row exists
    sqlx::query(
        r#"
        INSERT INTO nocy_sidecar.merkle_cache_state (id, highest_end_index)
        VALUES (1, 0)
        ON CONFLICT (id) DO NOTHING
        "#,
    )
    .execute(pool)
    .await?;

    Ok(())
}
