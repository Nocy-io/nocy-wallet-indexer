use serde_json::Value as JsonValue;
use sqlx::{PgPool, Postgres, Transaction};
use std::collections::HashMap;
use std::time::Instant;

use crate::error::AppError;
use crate::feed::{BlockMeta, ContractActionSummary};
use crate::metrics::record_db_query;

/// Wallet information relevant for feed readiness calculations
#[derive(Debug)]
pub struct WalletInfo {
    /// The last transaction ID indexed for this wallet
    pub last_indexed_transaction_id: Option<i64>,
}

/// Source of transaction identifiers in the upstream schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentifiersSource {
    Inline,
    Table,
    None,
}

/// Begin a read-only, repeatable-read transaction for consistent feed snapshots.
pub async fn begin_readonly_tx(pool: &PgPool) -> Result<Transaction<'_, Postgres>, AppError> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| AppError::InternalError(format!("Failed to begin snapshot transaction: {}", e)))?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(tx.as_mut())
        .await
        .map_err(|e| AppError::InternalError(format!("Failed to set snapshot isolation: {}", e)))?;
    Ok(tx)
}

/// Detect where transaction identifiers are stored (inline column vs table).
pub async fn detect_identifiers_source(pool: &PgPool) -> Result<IdentifiersSource, AppError> {
    let inline_exists: bool = sqlx::query_scalar(
        r#"
        SELECT EXISTS(
            SELECT 1
            FROM information_schema.columns
            WHERE table_schema = 'public'
              AND table_name = 'regular_transactions'
              AND column_name = 'identifiers'
        )
        "#,
    )
    .fetch_one(pool)
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to check identifiers column: {}", e)))?;

    if inline_exists {
        return Ok(IdentifiersSource::Inline);
    }

    let table_exists: bool = sqlx::query_scalar(
        r#"
        SELECT EXISTS(
            SELECT 1
            FROM information_schema.tables
            WHERE table_schema = 'public'
              AND table_name = 'transaction_identifiers'
        )
        "#,
    )
    .fetch_one(pool)
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to check identifiers table: {}", e)))?;

    if table_exists {
        return Ok(IdentifiersSource::Table);
    }

    Ok(IdentifiersSource::None)
}

/// Fetch wallet info for a session, returning None if not found
/// Note: wallets.session_id is stored as BYTEA (32-byte SHA256 hash)
pub async fn get_wallet_info(
    pool: &PgPool,
    session_id_bytes: &[u8],
) -> Result<Option<WalletInfo>, AppError> {
    let row: Option<(Option<i64>,)> = sqlx::query_as(
        r#"
        SELECT last_indexed_transaction_id
        FROM wallets
        WHERE session_id = $1
        "#,
    )
    .bind(&session_id_bytes)
    .fetch_optional(pool)
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to fetch wallet info: {}", e)))?;

    Ok(row.map(|(last_indexed_transaction_id,)| WalletInfo {
        last_indexed_transaction_id,
    }))
}

pub async fn get_wallet_info_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id_bytes: &[u8],
) -> Result<Option<WalletInfo>, AppError> {
    let row: Option<(Option<i64>,)> = sqlx::query_as(
        r#"
        SELECT last_indexed_transaction_id
        FROM wallets
        WHERE session_id = $1
        "#,
    )
    .bind(&session_id_bytes)
    .fetch_optional(tx.as_mut())
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to fetch wallet info: {}", e)))?;

    Ok(row.map(|(last_indexed_transaction_id,)| WalletInfo {
        last_indexed_transaction_id,
    }))
}

/// Fetch the upstream wallet-indexer session ID (wallets.session_id) for a local sidecar session.
///
/// sidecar session_id (input) is the local, server-derived session identifier used by clients.
/// upstream_session_id is the actual wallet-indexer session identifier used in the upstream DB
/// tables (wallets / relevant_transactions).
pub async fn get_upstream_session_id(
    pool: &PgPool,
    local_session_id_bytes: &[u8],
) -> Result<Vec<u8>, AppError> {
    let row: Option<(Option<Vec<u8>>,)> = sqlx::query_as(
        r#"
        SELECT upstream_session_id
        FROM nocy_sidecar.session_profile
        WHERE session_id = $1
        "#,
    )
    .bind(local_session_id_bytes)
    .fetch_optional(pool)
    .await
    .map_err(|e| {
        AppError::InternalError(format!("Failed to fetch upstream session id: {}", e))
    })?;

    match row {
        None => Err(AppError::NotFound(
            "Session not found. Use /v1/session/bootstrap to create a session first.".into(),
        )),
        Some((None,)) => Err(AppError::NotFound(
            "Session missing upstream mapping. Re-run /v1/session/bootstrap.".into(),
        )),
        Some((Some(upstream_session_id),)) => {
            if upstream_session_id.len() != 32 {
                return Err(AppError::InternalError(format!(
                    "Invalid upstream_session_id length: expected 32 bytes, got {}",
                    upstream_session_id.len()
                )));
            }
            Ok(upstream_session_id)
        }
    }
}

/// Transactional version of get_upstream_session_id() (uses the caller's snapshot transaction).
pub async fn get_upstream_session_id_tx(
    tx: &mut Transaction<'_, Postgres>,
    local_session_id_bytes: &[u8],
) -> Result<Vec<u8>, AppError> {
    let row: Option<(Option<Vec<u8>>,)> = sqlx::query_as(
        r#"
        SELECT upstream_session_id
        FROM nocy_sidecar.session_profile
        WHERE session_id = $1
        "#,
    )
    .bind(local_session_id_bytes)
    .fetch_optional(tx.as_mut())
    .await
    .map_err(|e| {
        AppError::InternalError(format!("Failed to fetch upstream session id: {}", e))
    })?;

    match row {
        None => Err(AppError::NotFound(
            "Session not found. Use /v1/session/bootstrap to create a session first.".into(),
        )),
        Some((None,)) => Err(AppError::NotFound(
            "Session missing upstream mapping. Re-run /v1/session/bootstrap.".into(),
        )),
        Some((Some(upstream_session_id),)) => {
            if upstream_session_id.len() != 32 {
                return Err(AppError::InternalError(format!(
                    "Invalid upstream_session_id length: expected 32 bytes, got {}",
                    upstream_session_id.len()
                )));
            }
            Ok(upstream_session_id)
        }
    }
}

/// Fetch the maximum regular transaction ID for a given block height.
/// Returns None if the block has no regular transactions.
pub async fn get_block_max_regular_tx_id(
    pool: &PgPool,
    block_height: i64,
) -> Result<Option<i64>, AppError> {
    // First check our cache table
    let cached: Option<(Option<i64>,)> = sqlx::query_as(
        r#"
        SELECT max_regular_tx_id
        FROM nocy_sidecar.block_max_regular_tx
        WHERE block_height = $1
        "#,
    )
    .bind(block_height)
    .fetch_optional(pool)
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to fetch cached max_regular_tx: {}", e)))?;

    if let Some((max_tx_id,)) = cached {
        return Ok(max_tx_id);
    }

    // Cache miss: compute from regular_transactions table
    // Note: regular_transactions.id is PK that references transactions.id
    let computed: Option<(Option<i64>,)> = sqlx::query_as(
        r#"
        SELECT MAX(rt.id) as max_regular_tx_id
        FROM regular_transactions rt
        JOIN transactions t ON rt.id = t.id
        JOIN blocks b ON t.block_id = b.id
        WHERE b.height = $1
        "#,
    )
    .bind(block_height)
    .fetch_optional(pool)
    .await
    .map_err(|e| {
        AppError::InternalError(format!("Failed to compute max_regular_tx_id: {}", e))
    })?;

    let max_tx_id = computed.and_then(|(id,)| id);

    // Cache the result for future lookups
    sqlx::query(
        r#"
        INSERT INTO nocy_sidecar.block_max_regular_tx (block_height, max_regular_tx_id)
        VALUES ($1, $2)
        ON CONFLICT (block_height) DO NOTHING
        "#,
    )
    .bind(block_height)
    .bind(max_tx_id)
    .execute(pool)
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to cache max_regular_tx_id: {}", e)))?;

    Ok(max_tx_id)
}

/// Compute the wallet-ready height for a session.
/// This is the highest block height where the wallet-indexer has processed ALL transactions.
///
/// The wallet-indexer tracks `last_indexed_transaction_id` which represents the last
/// transaction ID that has been checked for relevance. We find the first block that
/// contains a transaction with ID > last_indexed - that block (and all after it) are
/// not yet ready.
///
/// Returns Err(NotFound) if the wallet doesn't exist.
/// Returns None only if there's a blocking transaction at height 0.
pub async fn compute_wallet_ready_height(
    pool: &PgPool,
    session_id_bytes: &[u8],
    chain_head: i64,
) -> Result<Option<i64>, AppError> {
    let start = Instant::now();

    // Get wallet's last indexed transaction ID
    // Missing wallet = NotFound error (gated access)
    // NULL last_indexed = treat as 0 (new wallet, no transactions indexed yet)
    let wallet_info = match get_wallet_info(pool, session_id_bytes).await? {
        Some(info) => info,
        None => return Err(AppError::NotFound("Wallet not found for session".into())),
    };
    let last_indexed = wallet_info.last_indexed_transaction_id.unwrap_or(0);

    // Find the first block that has ANY transaction with ID > last_indexed.
    // This is the first block where the wallet-indexer hasn't finished processing.
    // The wallet-ready height is one less than this blocking block.
    //
    // IMPORTANT: We check ALL transactions, not just regular_transactions.
    // The wallet-indexer processes ALL transactions to check for relevance,
    // and updates last_indexed_transaction_id accordingly.
    let first_blocking: Option<(i64,)> = sqlx::query_as(
        r#"
        SELECT b.height
        FROM blocks b
        JOIN transactions t ON t.block_id = b.id
        WHERE t.id > $1
        ORDER BY b.height ASC
        LIMIT 1
        "#,
    )
    .bind(last_indexed)
    .fetch_optional(pool)
    .await
    .map_err(|e| {
        AppError::InternalError(format!("Failed to find first blocking block: {}", e))
    })?;

    let wallet_ready_height = match first_blocking {
        Some((blocking_height,)) => {
            if blocking_height == 0 {
                return Ok(None);
            } else {
                blocking_height - 1
            }
        }
        None => chain_head,
    };

    record_db_query("compute_wallet_ready_height", start.elapsed());

    Ok(Some(wallet_ready_height))
}

/// Compute the wallet-ready height for a session (transactional version).
/// This is the highest block height where the wallet-indexer has processed ALL transactions.
///
/// The wallet-indexer tracks `last_indexed_transaction_id` which represents the last
/// transaction ID that has been checked for relevance. We find the first block that
/// contains a transaction with ID > last_indexed - that block (and all after it) are
/// not yet ready.
///
/// Returns Err(NotFound) if the wallet doesn't exist.
/// Returns None only if there's a blocking transaction at height 0.
pub async fn compute_wallet_ready_height_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id_bytes: &[u8],
    chain_head: i64,
) -> Result<Option<i64>, AppError> {
    let start = Instant::now();

    // Get wallet's last indexed transaction ID
    // Missing wallet = NotFound error (gated access)
    // NULL last_indexed = treat as 0 (new wallet, no transactions indexed yet)
    let wallet_info = match get_wallet_info_tx(tx, session_id_bytes).await? {
        Some(info) => info,
        None => return Err(AppError::NotFound("Wallet not found for session".into())),
    };
    let last_indexed = wallet_info.last_indexed_transaction_id.unwrap_or(0);

    // Find the first block that has ANY transaction with ID > last_indexed.
    // This is the first block where the wallet-indexer hasn't finished processing.
    // The wallet-ready height is one less than this blocking block.
    //
    // IMPORTANT: We check ALL transactions, not just regular_transactions.
    // The wallet-indexer processes ALL transactions to check for relevance,
    // and updates last_indexed_transaction_id accordingly.
    let first_blocking: Option<(i64,)> = sqlx::query_as(
        r#"
        SELECT b.height
        FROM blocks b
        JOIN transactions t ON t.block_id = b.id
        WHERE t.id > $1
        ORDER BY b.height ASC
        LIMIT 1
        "#,
    )
    .bind(last_indexed)
    .fetch_optional(tx.as_mut())
    .await
    .map_err(|e| {
        AppError::InternalError(format!("Failed to find first blocking block: {}", e))
    })?;

    let wallet_ready_height = match first_blocking {
        Some((blocking_height,)) => {
            if blocking_height == 0 {
                return Ok(None);
            } else {
                blocking_height - 1
            }
        }
        None => chain_head,
    };

    record_db_query("compute_wallet_ready_height", start.elapsed());

    Ok(Some(wallet_ready_height))
}

/// Get the current chain head height
pub async fn get_chain_head(pool: &PgPool) -> Result<i64, AppError> {
    let start = Instant::now();
    let row: (i64,) = sqlx::query_as(
        r#"
        SELECT COALESCE(MAX(height), 0) as height
        FROM blocks
        "#,
    )
    .fetch_one(pool)
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to fetch chain head: {}", e)))?;

    record_db_query("get_chain_head", start.elapsed());
    Ok(row.0)
}

pub async fn get_chain_head_tx(
    tx: &mut Transaction<'_, Postgres>,
) -> Result<i64, AppError> {
    let start = Instant::now();
    let row: (i64,) = sqlx::query_as(
        r#"
        SELECT COALESCE(MAX(height), 0) as height
        FROM blocks
        "#,
    )
    .fetch_one(tx.as_mut())
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to fetch chain head: {}", e)))?;

    record_db_query("get_chain_head", start.elapsed());
    Ok(row.0)
}

/// Get the hash of a block at a specific height (for boundary reorg detection)
/// Returns None if no block exists at that height
pub async fn get_block_hash_at_height(
    pool: &PgPool,
    block_height: i64,
) -> Result<Option<String>, AppError> {
    let row: Option<(Vec<u8>,)> = sqlx::query_as(
        r#"
        SELECT hash
        FROM blocks
        WHERE height = $1
        "#,
    )
    .bind(block_height)
    .fetch_optional(pool)
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to fetch block hash: {}", e)))?;

    Ok(row.map(|(hash,)| hex::encode(hash)))
}

/// Get the hash of a block at a specific height (for boundary reorg detection)
/// Returns None if no block exists at that height
pub async fn get_block_hash_at_height_tx(
    tx: &mut Transaction<'_, Postgres>,
    block_height: i64,
) -> Result<Option<String>, AppError> {
    let row: Option<(Vec<u8>,)> = sqlx::query_as(
        r#"
        SELECT hash
        FROM blocks
        WHERE height = $1
        "#,
    )
    .bind(block_height)
    .fetch_optional(tx.as_mut())
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to fetch block hash: {}", e)))?;

    Ok(row.map(|(hash,)| hex::encode(hash)))
}

/// Query block metadata for a range [from_height, to_height]
/// Note: blocks.timestamp is BIGINT (Unix seconds), not TIMESTAMPTZ
pub async fn get_blocks_in_range(
    pool: &PgPool,
    from_height: i64,
    to_height: i64,
) -> Result<Vec<BlockMeta>, AppError> {
    let start = Instant::now();
    let rows: Vec<(i64, Vec<u8>, Vec<u8>, i64)> = sqlx::query_as(
        r#"
        SELECT height, hash, parent_hash, timestamp
        FROM blocks
        WHERE height >= $1 AND height <= $2
        ORDER BY height ASC
        "#,
    )
    .bind(from_height)
    .bind(to_height)
    .fetch_all(pool)
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to fetch blocks: {}", e)))?;

    record_db_query("get_blocks_in_range", start.elapsed());
    Ok(rows
        .into_iter()
        .map(|(height, hash, parent_hash, timestamp)| BlockMeta {
            height,
            hash: hex::encode(hash),
            parent_hash: hex::encode(parent_hash),
            timestamp,
        })
        .collect())
}

/// Query block metadata for a range [from_height, to_height] within a transaction
/// Note: blocks.timestamp is BIGINT (Unix seconds), not TIMESTAMPTZ
pub async fn get_blocks_in_range_tx(
    tx: &mut Transaction<'_, Postgres>,
    from_height: i64,
    to_height: i64,
) -> Result<Vec<BlockMeta>, AppError> {
    let start = Instant::now();
    let rows: Vec<(i64, Vec<u8>, Vec<u8>, i64)> = sqlx::query_as(
        r#"
        SELECT height, hash, parent_hash, timestamp
        FROM blocks
        WHERE height >= $1 AND height <= $2
        ORDER BY height ASC
        "#,
    )
    .bind(from_height)
    .bind(to_height)
    .fetch_all(tx.as_mut())
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to fetch blocks: {}", e)))?;

    record_db_query("get_blocks_in_range", start.elapsed());
    Ok(rows
        .into_iter()
        .map(|(height, hash, parent_hash, timestamp)| BlockMeta {
            height,
            hash: hex::encode(hash),
            parent_hash: hex::encode(parent_hash),
            timestamp,
        })
        .collect())
}

/// Relevant transaction data for feed assembly
#[derive(Debug)]
pub struct RelevantTxRow {
    pub tx_id: i64,
    pub tx_hash: Vec<u8>,
    pub block_height: i64,
    pub paid_fees: Option<Vec<u8>>,
    /// Merkle tree start index (for inline merkle update injection)
    pub start_index: Option<i64>,
    /// Merkle tree end index (for inline merkle update injection)
    pub end_index: Option<i64>,
}

/// Query relevant transactions for a session in a block range
/// Note: relevant_transactions.transaction_id references transactions.id
/// Note: wallets.session_id is BYTEA (32-byte SHA256 hash)
/// Note: Joins regular_transactions to ensure we only return regular txs
pub async fn get_relevant_transactions(
    pool: &PgPool,
    session_id_bytes: &[u8],
    from_height: i64,
    to_height: i64,
) -> Result<Vec<RelevantTxRow>, AppError> {
    let start = Instant::now();
    let rows: Vec<(i64, Vec<u8>, i64, Option<Vec<u8>>, Option<i64>, Option<i64>)> = sqlx::query_as(
        r#"
        SELECT t.id as tx_id, t.hash as tx_hash, b.height as block_height,
               reg.paid_fees, reg.start_index, reg.end_index
        FROM relevant_transactions rt
        JOIN wallets w ON rt.wallet_id = w.id
        JOIN transactions t ON rt.transaction_id = t.id
        LEFT JOIN regular_transactions reg ON reg.id = t.id
        JOIN blocks b ON t.block_id = b.id
        WHERE w.session_id = $1
          AND b.height >= $2
          AND b.height <= $3
        ORDER BY b.height ASC, t.id ASC
        "#,
    )
    .bind(&session_id_bytes)
    .bind(from_height)
    .bind(to_height)
    .fetch_all(pool)
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to fetch relevant transactions: {}", e)))?;

    record_db_query("get_relevant_transactions", start.elapsed());
    Ok(rows
        .into_iter()
        .map(|(tx_id, tx_hash, block_height, paid_fees, start_index, end_index)| RelevantTxRow {
            tx_id,
            tx_hash,
            block_height,
            paid_fees,
            start_index,
            end_index,
        })
        .collect())
}

/// Query relevant transactions for a session in a block range (transactional snapshot)
/// Note: relevant_transactions.transaction_id references transactions.id
/// Note: wallets.session_id is BYTEA (32-byte SHA256 hash)
/// Note: Joins regular_transactions to ensure we only return regular txs
pub async fn get_relevant_transactions_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id_bytes: &[u8],
    from_height: i64,
    to_height: i64,
) -> Result<Vec<RelevantTxRow>, AppError> {
    let start = Instant::now();
    let rows: Vec<(i64, Vec<u8>, i64, Option<Vec<u8>>, Option<i64>, Option<i64>)> = sqlx::query_as(
        r#"
        SELECT t.id as tx_id, t.hash as tx_hash, b.height as block_height,
               reg.paid_fees, reg.start_index, reg.end_index
        FROM relevant_transactions rt
        JOIN wallets w ON rt.wallet_id = w.id
        JOIN transactions t ON rt.transaction_id = t.id
        LEFT JOIN regular_transactions reg ON reg.id = t.id
        JOIN blocks b ON t.block_id = b.id
        WHERE w.session_id = $1
          AND b.height >= $2
          AND b.height <= $3
        ORDER BY b.height ASC, t.id ASC
        "#,
    )
    .bind(&session_id_bytes)
    .bind(from_height)
    .bind(to_height)
    .fetch_all(tx.as_mut())
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to fetch relevant txs: {}", e)))?;

    record_db_query("get_relevant_transactions", start.elapsed());
    Ok(rows
        .into_iter()
        .map(|(tx_id, tx_hash, block_height, paid_fees, start_index, end_index)| RelevantTxRow {
            tx_id,
            tx_hash,
            block_height,
            paid_fees,
            start_index,
            end_index,
        })
        .collect())
}

/// Transaction metadata row for feed assembly
#[derive(Debug)]
pub struct TransactionMetaRow {
    pub tx_id: i64,
    pub tx_hash: Vec<u8>,
    pub block_height: i64,
    pub protocol_version: i64,
    pub transaction_result: Option<JsonValue>,
    pub merkle_tree_root: Option<Vec<u8>>,
    pub start_index: Option<i64>,
    pub end_index: Option<i64>,
    pub paid_fees: Option<Vec<u8>>,
    pub estimated_fees: Option<Vec<u8>>,
    pub raw: Option<Vec<u8>>,
    pub identifiers: Option<Vec<Vec<u8>>>,
}

/// Query transaction metadata for a list of tx_ids.
pub async fn get_transaction_metadata(
    pool: &PgPool,
    tx_ids: &[i64],
    include_raw: bool,
    include_identifiers: bool,
    identifiers_source: IdentifiersSource,
) -> Result<Vec<TransactionMetaRow>, AppError> {
    if tx_ids.is_empty() {
        return Ok(Vec::new());
    }

    let start = Instant::now();
    let raw_select = if include_raw {
        "t.raw"
    } else {
        "NULL::bytea"
    };
    let identifiers_select = if include_identifiers && identifiers_source == IdentifiersSource::Inline {
        "rt.identifiers"
    } else {
        "NULL::bytea[]"
    };

    let query = format!(
        r#"
        SELECT
            t.id as tx_id,
            t.hash as tx_hash,
            b.height as block_height,
            t.protocol_version,
            rt.transaction_result,
            rt.merkle_tree_root,
            rt.start_index,
            rt.end_index,
            rt.paid_fees,
            rt.estimated_fees,
            {raw_select} as raw,
            {identifiers_select} as identifiers
        FROM transactions t
        JOIN blocks b ON t.block_id = b.id
        LEFT JOIN regular_transactions rt ON rt.id = t.id
        WHERE t.id = ANY($1)
        "#
    );

    let rows: Vec<(
        i64,
        Vec<u8>,
        i64,
        i64,
        Option<JsonValue>,
        Option<Vec<u8>>,
        Option<i64>,
        Option<i64>,
        Option<Vec<u8>>,
        Option<Vec<u8>>,
        Option<Vec<u8>>,
        Option<Vec<Vec<u8>>>,
    )> = sqlx::query_as(&query)
        .bind(tx_ids)
        .fetch_all(pool)
        .await
        .map_err(|e| AppError::InternalError(format!("Failed to fetch transaction metadata: {}", e)))?;

    record_db_query("get_transaction_metadata", start.elapsed());

    let mut results: Vec<TransactionMetaRow> = rows
        .into_iter()
        .map(
            |(
                tx_id,
                tx_hash,
                block_height,
                protocol_version,
                transaction_result,
                merkle_tree_root,
                start_index,
                end_index,
                paid_fees,
                estimated_fees,
                raw,
                identifiers,
            )| TransactionMetaRow {
                tx_id,
                tx_hash,
                block_height,
                protocol_version,
                transaction_result,
                merkle_tree_root,
                start_index,
                end_index,
                paid_fees,
                estimated_fees,
                raw,
                identifiers,
            },
        )
        .collect();

    if include_identifiers && identifiers_source == IdentifiersSource::Table {
        let id_rows: Vec<(i64, Vec<u8>)> = sqlx::query_as(
            r#"
            SELECT transaction_id, identifier
            FROM transaction_identifiers
            WHERE transaction_id = ANY($1)
            "#,
        )
        .bind(tx_ids)
        .fetch_all(pool)
        .await
        .map_err(|e| AppError::InternalError(format!("Failed to fetch transaction identifiers: {}", e)))?;

        let mut identifiers_by_tx: HashMap<i64, Vec<Vec<u8>>> = HashMap::new();
        for (tx_id, identifier) in id_rows {
            identifiers_by_tx.entry(tx_id).or_default().push(identifier);
        }

        for row in &mut results {
            if row.identifiers.is_none() {
                row.identifiers = identifiers_by_tx.remove(&row.tx_id);
            }
        }
    }

    Ok(results)
}

/// Query transaction metadata for a list of tx_ids using a transaction snapshot.
pub async fn get_transaction_metadata_tx(
    tx: &mut Transaction<'_, Postgres>,
    tx_ids: &[i64],
    include_raw: bool,
    include_identifiers: bool,
    identifiers_source: IdentifiersSource,
) -> Result<Vec<TransactionMetaRow>, AppError> {
    if tx_ids.is_empty() {
        return Ok(Vec::new());
    }

    let start = Instant::now();
    let raw_select = if include_raw {
        "t.raw"
    } else {
        "NULL::bytea"
    };
    let identifiers_select = if include_identifiers && identifiers_source == IdentifiersSource::Inline {
        "rt.identifiers"
    } else {
        "NULL::bytea[]"
    };

    let query = format!(
        r#"
        SELECT
            t.id as tx_id,
            t.hash as tx_hash,
            b.height as block_height,
            t.protocol_version,
            rt.transaction_result,
            rt.merkle_tree_root,
            rt.start_index,
            rt.end_index,
            rt.paid_fees,
            rt.estimated_fees,
            {raw_select} as raw,
            {identifiers_select} as identifiers
        FROM transactions t
        JOIN blocks b ON t.block_id = b.id
        LEFT JOIN regular_transactions rt ON rt.id = t.id
        WHERE t.id = ANY($1)
        "#
    );

    let rows: Vec<(
        i64,
        Vec<u8>,
        i64,
        i64,
        Option<JsonValue>,
        Option<Vec<u8>>,
        Option<i64>,
        Option<i64>,
        Option<Vec<u8>>,
        Option<Vec<u8>>,
        Option<Vec<u8>>,
        Option<Vec<Vec<u8>>>,
    )> = sqlx::query_as(&query)
        .bind(tx_ids)
        .fetch_all(tx.as_mut())
        .await
        .map_err(|e| AppError::InternalError(format!("Failed to fetch transaction metadata: {}", e)))?;

    record_db_query("get_transaction_metadata", start.elapsed());

    let mut results: Vec<TransactionMetaRow> = rows
        .into_iter()
        .map(
            |(
                tx_id,
                tx_hash,
                block_height,
                protocol_version,
                transaction_result,
                merkle_tree_root,
                start_index,
                end_index,
                paid_fees,
                estimated_fees,
                raw,
                identifiers,
            )| TransactionMetaRow {
                tx_id,
                tx_hash,
                block_height,
                protocol_version,
                transaction_result,
                merkle_tree_root,
                start_index,
                end_index,
                paid_fees,
                estimated_fees,
                raw,
                identifiers,
            },
        )
        .collect();

    if include_identifiers && identifiers_source == IdentifiersSource::Table {
        let id_rows: Vec<(i64, Vec<u8>)> = sqlx::query_as(
            r#"
            SELECT transaction_id, identifier
            FROM transaction_identifiers
            WHERE transaction_id = ANY($1)
            "#,
        )
        .bind(tx_ids)
        .fetch_all(tx.as_mut())
        .await
        .map_err(|e| AppError::InternalError(format!("Failed to fetch transaction identifiers: {}", e)))?;

        let mut identifiers_by_tx: HashMap<i64, Vec<Vec<u8>>> = HashMap::new();
        for (tx_id, identifier) in id_rows {
            identifiers_by_tx.entry(tx_id).or_default().push(identifier);
        }

        for row in &mut results {
            if row.identifiers.is_none() {
                row.identifiers = identifiers_by_tx.remove(&row.tx_id);
            }
        }
    }

    Ok(results)
}

/// Zswap ledger event data
#[derive(Debug)]
pub struct ZswapEventRow {
    pub tx_id: i64,
    pub ledger_event_id: i64,
    pub raw: Vec<u8>,
}

/// Zswap input event row with transaction hash
#[derive(Debug)]
pub struct ZswapInputEventRow {
    pub tx_id: i64,
    pub tx_hash: Vec<u8>,
    pub ledger_event_id: i64,
    pub block_height: i64,
    pub raw: Vec<u8>,
}

/// Query zswap ledger events for a list of transaction IDs (for relevant tx zswap_events field)
/// Note: ledger_events uses grouping enum ('Zswap', 'Dust')
pub async fn get_zswap_events_for_txs(
    pool: &PgPool,
    tx_ids: &[i64],
) -> Result<Vec<ZswapEventRow>, AppError> {
    if tx_ids.is_empty() {
        return Ok(Vec::new());
    }

    let rows: Vec<(i64, i64, Vec<u8>)> = sqlx::query_as(
        r#"
        SELECT t.id as tx_id, le.id as ledger_event_id, le.raw
        FROM ledger_events le
        JOIN transactions t ON le.transaction_id = t.id
        WHERE t.id = ANY($1)
          AND le.grouping = 'Zswap'
        ORDER BY t.id ASC, le.id ASC
        "#,
    )
    .bind(tx_ids)
    .fetch_all(pool)
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to fetch zswap events: {}", e)))?;

    Ok(rows
        .into_iter()
        .map(|(tx_id, ledger_event_id, raw)| ZswapEventRow {
            tx_id,
            ledger_event_id,
            raw,
        })
        .collect())
}

/// Query zswap ledger events for a list of transaction IDs (transactional snapshot)
pub async fn get_zswap_events_for_txs_tx(
    tx: &mut Transaction<'_, Postgres>,
    tx_ids: &[i64],
) -> Result<Vec<ZswapEventRow>, AppError> {
    if tx_ids.is_empty() {
        return Ok(Vec::new());
    }

    let rows: Vec<(i64, i64, Vec<u8>)> = sqlx::query_as(
        r#"
        SELECT t.id as tx_id, le.id as ledger_event_id, le.raw
        FROM ledger_events le
        JOIN transactions t ON le.transaction_id = t.id
        WHERE t.id = ANY($1)
          AND le.grouping = 'Zswap'
        ORDER BY t.id ASC, le.id ASC
        "#,
    )
    .bind(tx_ids)
    .fetch_all(tx.as_mut())
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to fetch zswap events: {}", e)))?;

    Ok(rows
        .into_iter()
        .map(|(tx_id, ledger_event_id, raw)| ZswapEventRow {
            tx_id,
            ledger_event_id,
            raw,
        })
        .collect())
}

/// Query ALL zswap input events in a block range (global nullifiers)
/// Filters for variant = 'ZswapInput' to only get ZswapInput events
/// Note: ledger_events uses grouping enum ('Zswap', 'Dust') and variant enum ('ZswapInput', etc.)
pub async fn get_all_zswap_input_events_in_range(
    pool: &PgPool,
    from_height: i64,
    to_height: i64,
) -> Result<Vec<ZswapInputEventRow>, AppError> {
    let start = Instant::now();
    let rows: Vec<(i64, Vec<u8>, i64, i64, Vec<u8>)> = sqlx::query_as(
        r#"
        SELECT t.id as tx_id, t.hash as tx_hash, le.id as ledger_event_id,
               b.height as block_height, le.raw
        FROM ledger_events le
        JOIN transactions t ON le.transaction_id = t.id
        JOIN blocks b ON t.block_id = b.id
        WHERE b.height >= $1
          AND b.height <= $2
          AND le.grouping = 'Zswap'
          AND le.variant = 'ZswapInput'
        ORDER BY b.height ASC, t.id ASC, le.id ASC
        "#,
    )
    .bind(from_height)
    .bind(to_height)
    .fetch_all(pool)
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to fetch zswap input events: {}", e)))?;

    record_db_query("get_all_zswap_input_events_in_range", start.elapsed());
    Ok(rows
        .into_iter()
        .map(|(tx_id, tx_hash, ledger_event_id, block_height, raw)| ZswapInputEventRow {
            tx_id,
            tx_hash,
            ledger_event_id,
            block_height,
            raw,
        })
        .collect())
}

/// Query ALL zswap input events in a block range (global nullifiers) in a transaction snapshot
pub async fn get_all_zswap_input_events_in_range_tx(
    tx: &mut Transaction<'_, Postgres>,
    from_height: i64,
    to_height: i64,
) -> Result<Vec<ZswapInputEventRow>, AppError> {
    let start = Instant::now();
    let rows: Vec<(i64, Vec<u8>, i64, i64, Vec<u8>)> = sqlx::query_as(
        r#"
        SELECT t.id as tx_id, t.hash as tx_hash, le.id as ledger_event_id,
               b.height as block_height, le.raw
        FROM ledger_events le
        JOIN transactions t ON le.transaction_id = t.id
        JOIN blocks b ON t.block_id = b.id
        WHERE le.grouping = 'Zswap'
          AND le.variant = 'ZswapInput'
          AND b.height >= $1
          AND b.height <= $2
        ORDER BY b.height ASC, t.id ASC, le.id ASC
        "#,
    )
    .bind(from_height)
    .bind(to_height)
    .fetch_all(tx.as_mut())
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to fetch zswap input events: {}", e)))?;

    record_db_query("get_all_zswap_input_events_in_range", start.elapsed());
    Ok(rows
        .into_iter()
        .map(|(tx_id, tx_hash, ledger_event_id, block_height, raw)| ZswapInputEventRow {
            tx_id,
            tx_hash,
            ledger_event_id,
            block_height,
            raw,
        })
        .collect())
}

/// Query contract actions for a list of transaction IDs
/// Note: contract_actions uses address (BYTEA), variant enum ('Deploy', 'Call', 'Update'),
/// and attributes JSONB which contains entry_point for 'Call' actions
pub async fn get_contract_actions_for_txs(
    pool: &PgPool,
    tx_ids: &[i64],
) -> Result<Vec<(i64, ContractActionSummary)>, AppError> {
    if tx_ids.is_empty() {
        return Ok(Vec::new());
    }

    let rows: Vec<(i64, Vec<u8>, String, Option<String>)> = sqlx::query_as(
        r#"
        SELECT t.id as tx_id, ca.address, ca.variant::text as action_type,
               ca.attributes->'Call'->>'entry_point' as entry_point
        FROM contract_actions ca
        JOIN transactions t ON ca.transaction_id = t.id
        WHERE t.id = ANY($1)
        ORDER BY t.id ASC, ca.id ASC
        "#,
    )
    .bind(tx_ids)
    .fetch_all(pool)
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to fetch contract actions: {}", e)))?;

    Ok(rows
        .into_iter()
        .map(|(tx_id, contract_address, action_type, entry_point)| {
            (
                tx_id,
                ContractActionSummary {
                    contract_address: hex::encode(contract_address),
                    action_type,
                    entry_point,
                },
            )
        })
        .collect())
}

/// Query contract actions for a list of transaction IDs (transaction snapshot)
/// Note: contract_actions uses address (BYTEA), variant enum ('Deploy', 'Call', 'Update'),
/// and attributes JSONB which contains entry_point for 'Call' actions
pub async fn get_contract_actions_for_txs_tx(
    tx: &mut Transaction<'_, Postgres>,
    tx_ids: &[i64],
) -> Result<Vec<(i64, ContractActionSummary)>, AppError> {
    if tx_ids.is_empty() {
        return Ok(Vec::new());
    }

    let rows: Vec<(i64, Vec<u8>, String, Option<String>)> = sqlx::query_as(
        r#"
        SELECT t.id as tx_id, ca.address, ca.variant::text as action_type,
               ca.attributes->'Call'->>'entry_point' as entry_point
        FROM contract_actions ca
        JOIN transactions t ON ca.transaction_id = t.id
        WHERE t.id = ANY($1)
        ORDER BY t.id ASC, ca.id ASC
        "#,
    )
    .bind(tx_ids)
    .fetch_all(tx.as_mut())
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to fetch contract actions: {}", e)))?;

    Ok(rows
        .into_iter()
        .map(|(tx_id, contract_address, action_type, entry_point)| {
            (
                tx_id,
                ContractActionSummary {
                    contract_address: hex::encode(contract_address),
                    action_type,
                    entry_point,
                },
            )
        })
        .collect())
}

/// Dust ledger event data
#[derive(Debug)]
pub struct DustEventRow {
    pub tx_id: i64,
    pub ledger_event_id: i64,
    pub block_height: i64,
    pub raw: Vec<u8>,
}

/// Query dust ledger events for a session in a block range
/// Note: ledger_events uses grouping enum ('Zswap', 'Dust')
pub async fn get_dust_events(
    pool: &PgPool,
    session_id_bytes: &[u8],
    from_height: i64,
    to_height: i64,
) -> Result<Vec<DustEventRow>, AppError> {
    // Get the dust public key for this session
    let dust_key: Option<(Vec<u8>,)> = sqlx::query_as(
        r#"
        SELECT dust_public_key
        FROM nocy_sidecar.session_profile
        WHERE session_id = $1 AND dust_public_key IS NOT NULL
        "#,
    )
    .bind(session_id_bytes)
    .fetch_optional(pool)
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to fetch dust public key: {}", e)))?;

    let dust_public_key = match dust_key {
        Some((key,)) => key,
        None => return Ok(Vec::new()), // No dust key registered
    };

    // Query dust ledger events that contain this public key in the raw payload
    // Uses bytea position() for binary substring matching
    let rows: Vec<(i64, i64, i64, Vec<u8>)> = sqlx::query_as(
        r#"
        SELECT t.id as tx_id, le.id as ledger_event_id, b.height as block_height, le.raw
        FROM ledger_events le
        JOIN transactions t ON le.transaction_id = t.id
        JOIN blocks b ON t.block_id = b.id
        WHERE le.grouping = 'Dust'
          AND b.height >= $1
          AND b.height <= $2
          AND (
            position($3::bytea in le.raw) > 0
            OR le.variant = 'DustSpendProcessed'
          )
        ORDER BY b.height ASC, t.id ASC, le.id ASC
        "#,
    )
    .bind(from_height)
    .bind(to_height)
    .bind(&dust_public_key)
    .fetch_all(pool)
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to fetch dust events: {}", e)))?;

    Ok(rows
        .into_iter()
        .map(|(tx_id, ledger_event_id, block_height, raw)| DustEventRow {
            tx_id,
            ledger_event_id,
            block_height,
            raw,
        })
        .collect())
}

/// Query dust ledger events for a session in a block range (transaction snapshot)
/// Note: ledger_events uses grouping enum ('Zswap', 'Dust')
pub async fn get_dust_events_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id_bytes: &[u8],
    from_height: i64,
    to_height: i64,
) -> Result<Vec<DustEventRow>, AppError> {
    // Get the dust public key for this session
    let dust_key: Option<(Vec<u8>,)> = sqlx::query_as(
        r#"
        SELECT dust_public_key
        FROM nocy_sidecar.session_profile
        WHERE session_id = $1 AND dust_public_key IS NOT NULL
        "#,
    )
    .bind(session_id_bytes)
    .fetch_optional(tx.as_mut())
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to fetch dust public key: {}", e)))?;

    let dust_public_key = match dust_key {
        Some((key,)) => key,
        None => return Ok(Vec::new()), // No dust key registered
    };

    // Query dust ledger events that contain this public key in the raw payload
    // Uses bytea position() for binary substring matching
    let rows: Vec<(i64, i64, i64, Vec<u8>)> = sqlx::query_as(
        r#"
        SELECT t.id as tx_id, le.id as ledger_event_id, b.height as block_height, le.raw
        FROM ledger_events le
        JOIN transactions t ON le.transaction_id = t.id
        JOIN blocks b ON t.block_id = b.id
        WHERE le.grouping = 'Dust'
          AND b.height >= $1
          AND b.height <= $2
          AND (
            position($3::bytea in le.raw) > 0
            OR le.variant = 'DustSpendProcessed'
          )
        ORDER BY b.height ASC, t.id ASC, le.id ASC
        "#,
    )
    .bind(from_height)
    .bind(to_height)
    .bind(&dust_public_key)
    .fetch_all(tx.as_mut())
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to fetch dust events: {}", e)))?;

    Ok(rows
        .into_iter()
        .map(|(tx_id, ledger_event_id, block_height, raw)| DustEventRow {
            tx_id,
            ledger_event_id,
            block_height,
            raw,
        })
        .collect())
}

/// Get ALL dust ledger events for a block range (not filtered by wallet).
/// This is needed because the dust SDK's merkle tree requires ALL events in sequence
/// to maintain correct indices - there's no collapsed merkle update mechanism for dust.
pub async fn get_all_dust_events_tx(
    tx: &mut Transaction<'_, Postgres>,
    from_height: i64,
    to_height: i64,
) -> Result<Vec<DustEventRow>, AppError> {
    let start = Instant::now();
    let rows: Vec<(i64, i64, i64, Vec<u8>)> = sqlx::query_as(
        r#"
        SELECT t.id as tx_id, le.id as ledger_event_id, b.height as block_height, le.raw
        FROM ledger_events le
        JOIN transactions t ON le.transaction_id = t.id
        JOIN blocks b ON t.block_id = b.id
        WHERE le.grouping = 'Dust'
          AND b.height >= $1
          AND b.height <= $2
        ORDER BY b.height ASC, t.id ASC, le.id ASC
        "#,
    )
    .bind(from_height)
    .bind(to_height)
    .fetch_all(tx.as_mut())
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to fetch all dust events: {}", e)))?;

    record_db_query("get_all_dust_events", start.elapsed());

    Ok(rows
        .into_iter()
        .map(|(tx_id, ledger_event_id, block_height, raw)| DustEventRow {
            tx_id,
            ledger_event_id,
            block_height,
            raw,
        })
        .collect())
}

/// Unshielded delta data
#[derive(Debug)]
pub struct UnshieldedDeltaRow {
    pub utxo_id: i64,
    pub tx_id: i64,
    pub block_height: i64,
    pub address: Vec<u8>,
    /// Delta value as bytes (BYTEA from unshielded_utxos.value)
    /// Positive for creates, would be negative for spends
    pub value_bytes: Vec<u8>,
    /// Whether this is a create (+) or spend (-) delta
    pub is_create: bool,
}

/// Unshielded UTXO event data
/// Note: Contract actions are NOT included here. They are fetched separately
/// via get_contract_actions_for_txs() and attached by tx_id, ensuring ALL
/// contract actions are captured (not just the first one).
#[derive(Debug)]
pub struct UnshieldedUtxoEventRow {
    pub utxo_id: i64,
    pub tx_id: i64,
    pub tx_hash: Vec<u8>,
    pub block_height: i64,
    pub is_create: bool,
    pub address: Vec<u8>,
    pub intent_hash: Vec<u8>,
    pub output_index: i64,
    pub token_type: Vec<u8>,
    pub value_bytes: Vec<u8>,
    pub initial_nonce: Vec<u8>,
    pub registered_for_dust_generation: bool,
    pub ctime: Option<i64>,
    pub paid_fees: Option<Vec<u8>>,
    // contract_address and entry_point REMOVED
    // Contract actions are now fetched separately via get_contract_actions_for_txs()
    // to ensure ALL actions are captured (fixes LIMIT 1 bug)
}

/// Get protocol version for a specific block height
pub async fn get_block_protocol_version(
    pool: &PgPool,
    block_height: i64,
) -> Result<u32, AppError> {
    let row: (i32,) = sqlx::query_as(
        r#"
        SELECT protocol_version
        FROM blocks
        WHERE height = $1
        "#,
    )
    .bind(block_height)
    .fetch_one(pool)
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to fetch protocol version: {}", e)))?;

    Ok(row.0 as u32)
}

/// Query unshielded deltas for a session in a block range
/// Note: unshielded_utxos uses owner (BYTEA), creating_transaction_id/spending_transaction_id, value (BYTEA)
pub async fn get_unshielded_deltas(
    pool: &PgPool,
    session_id_bytes: &[u8],
    from_height: i64,
    to_height: i64,
) -> Result<Vec<UnshieldedDeltaRow>, AppError> {
    // Get the registered unshielded addresses for this session
    let addresses: Vec<(Vec<u8>,)> = sqlx::query_as(
        r#"
        SELECT address
        FROM nocy_sidecar.session_unshielded_address
        WHERE session_id = $1
        "#,
    )
    .bind(session_id_bytes)
    .fetch_all(pool)
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to fetch unshielded addresses: {}", e)))?;

    if addresses.is_empty() {
        return Ok(Vec::new());
    }

    let address_list: Vec<Vec<u8>> = addresses.into_iter().map(|(a,)| a).collect();

    // Query unshielded UTXOs created for these addresses (positive deltas)
    let create_rows: Vec<(i64, i64, i64, Vec<u8>, Vec<u8>)> = sqlx::query_as(
        r#"
        SELECT uu.id as utxo_id, t.id as tx_id, b.height as block_height, uu.owner, uu.value
        FROM unshielded_utxos uu
        JOIN transactions t ON uu.creating_transaction_id = t.id
        JOIN blocks b ON t.block_id = b.id
        WHERE uu.owner = ANY($1)
          AND b.height >= $2
          AND b.height <= $3
        ORDER BY b.height ASC, t.id ASC, uu.id ASC
        "#,
    )
    .bind(&address_list)
    .bind(from_height)
    .bind(to_height)
    .fetch_all(pool)
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to fetch unshielded creates: {}", e)))?;

    // Query unshielded UTXOs spent by these addresses (negative deltas)
    let spend_rows: Vec<(i64, i64, i64, Vec<u8>, Vec<u8>)> = sqlx::query_as(
        r#"
        SELECT uu.id as utxo_id, t.id as tx_id, b.height as block_height, uu.owner, uu.value
        FROM unshielded_utxos uu
        JOIN transactions t ON uu.spending_transaction_id = t.id
        JOIN blocks b ON t.block_id = b.id
        WHERE uu.owner = ANY($1)
          AND uu.spending_transaction_id IS NOT NULL
          AND b.height >= $2
          AND b.height <= $3
        ORDER BY b.height ASC, t.id ASC, uu.id ASC
        "#,
    )
    .bind(&address_list)
    .bind(from_height)
    .bind(to_height)
    .fetch_all(pool)
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to fetch unshielded spends: {}", e)))?;

    let mut results: Vec<UnshieldedDeltaRow> = Vec::new();

    // Add creates (positive deltas)
    for (utxo_id, tx_id, block_height, address, value_bytes) in create_rows {
        results.push(UnshieldedDeltaRow {
            utxo_id,
            tx_id,
            block_height,
            address,
            value_bytes,
            is_create: true,
        });
    }

    // Add spends (negative deltas)
    for (utxo_id, tx_id, block_height, address, value_bytes) in spend_rows {
        results.push(UnshieldedDeltaRow {
            utxo_id,
            tx_id,
            block_height,
            address,
            value_bytes,
            is_create: false,
        });
    }

    // Sort by block_height, tx_id, utxo_id
    results.sort_by(|a, b| {
        a.block_height
            .cmp(&b.block_height)
            .then(a.tx_id.cmp(&b.tx_id))
            .then(a.utxo_id.cmp(&b.utxo_id))
    });

    Ok(results)
}

/// Query unshielded UTXO create/spend events for a session in a block range
pub async fn get_unshielded_utxo_events(
    pool: &PgPool,
    session_id_bytes: &[u8],
    from_height: i64,
    to_height: i64,
) -> Result<Vec<UnshieldedUtxoEventRow>, AppError> {
    // Get the registered unshielded addresses for this session
    let addresses: Vec<(Vec<u8>,)> = sqlx::query_as(
        r#"
        SELECT address
        FROM nocy_sidecar.session_unshielded_address
        WHERE session_id = $1
        "#,
    )
    .bind(session_id_bytes)
    .fetch_all(pool)
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to fetch unshielded addresses: {}", e)))?;

    if addresses.is_empty() {
        return Ok(Vec::new());
    }

    let address_list: Vec<Vec<u8>> = addresses.into_iter().map(|(a,)| a).collect();

    // Query UTXO creates - contract actions fetched separately via get_contract_actions_for_txs()
    let create_rows: Vec<(
        i64,
        i64,
        Vec<u8>,
        i64,
        Vec<u8>,
        Vec<u8>,
        i64,
        Vec<u8>,
        Vec<u8>,
        Vec<u8>,
        bool,
        Option<i64>,
        Option<Vec<u8>>,
    )> = sqlx::query_as(
        r#"
        SELECT
            uu.id as utxo_id,
            t.id as tx_id,
            t.hash as tx_hash,
            b.height as block_height,
            uu.owner,
            uu.intent_hash,
            uu.output_index,
            uu.token_type,
            uu.value,
            uu.initial_nonce,
            uu.registered_for_dust_generation,
            uu.ctime,
            reg.paid_fees
        FROM unshielded_utxos uu
        JOIN transactions t ON uu.creating_transaction_id = t.id
        JOIN blocks b ON t.block_id = b.id
        LEFT JOIN regular_transactions reg ON reg.id = t.id
        WHERE uu.owner = ANY($1)
          AND b.height >= $2
          AND b.height <= $3
        ORDER BY b.height ASC, t.id ASC, uu.id ASC
        "#,
    )
    .bind(&address_list)
    .bind(from_height)
    .bind(to_height)
    .fetch_all(pool)
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to fetch unshielded creates: {}", e)))?;

    // Query UTXO spends - contract actions fetched separately via get_contract_actions_for_txs()
    let spend_rows: Vec<(
        i64,
        i64,
        Vec<u8>,
        i64,
        Vec<u8>,
        Vec<u8>,
        i64,
        Vec<u8>,
        Vec<u8>,
        Vec<u8>,
        bool,
        Option<i64>,
        Option<Vec<u8>>,
    )> = sqlx::query_as(
        r#"
        SELECT
            uu.id as utxo_id,
            t.id as tx_id,
            t.hash as tx_hash,
            b.height as block_height,
            uu.owner,
            uu.intent_hash,
            uu.output_index,
            uu.token_type,
            uu.value,
            uu.initial_nonce,
            uu.registered_for_dust_generation,
            uu.ctime,
            reg.paid_fees
        FROM unshielded_utxos uu
        JOIN transactions t ON uu.spending_transaction_id = t.id
        JOIN blocks b ON t.block_id = b.id
        LEFT JOIN regular_transactions reg ON reg.id = t.id
        WHERE uu.owner = ANY($1)
          AND uu.spending_transaction_id IS NOT NULL
          AND b.height >= $2
          AND b.height <= $3
        ORDER BY b.height ASC, t.id ASC, uu.id ASC
        "#,
    )
    .bind(&address_list)
    .bind(from_height)
    .bind(to_height)
    .fetch_all(pool)
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to fetch unshielded spends: {}", e)))?;

    let mut results: Vec<UnshieldedUtxoEventRow> = Vec::new();

    for (
        utxo_id,
        tx_id,
        tx_hash,
        block_height,
        address,
        intent_hash,
        output_index,
        token_type,
        value_bytes,
        initial_nonce,
        registered_for_dust_generation,
        ctime,
        paid_fees,
    ) in create_rows
    {
        results.push(UnshieldedUtxoEventRow {
            utxo_id,
            tx_id,
            tx_hash,
            block_height,
            is_create: true,
            address,
            intent_hash,
            output_index,
            token_type,
            value_bytes,
            initial_nonce,
            registered_for_dust_generation,
            ctime,
            paid_fees,
        });
    }

    for (
        utxo_id,
        tx_id,
        tx_hash,
        block_height,
        address,
        intent_hash,
        output_index,
        token_type,
        value_bytes,
        initial_nonce,
        registered_for_dust_generation,
        ctime,
        paid_fees,
    ) in spend_rows
    {
        results.push(UnshieldedUtxoEventRow {
            utxo_id,
            tx_id,
            tx_hash,
            block_height,
            is_create: false,
            address,
            intent_hash,
            output_index,
            token_type,
            value_bytes,
            initial_nonce,
            registered_for_dust_generation,
            ctime,
            paid_fees,
        });
    }

    results.sort_by(|a, b| {
        a.block_height
            .cmp(&b.block_height)
            .then(a.tx_id.cmp(&b.tx_id))
            .then(a.utxo_id.cmp(&b.utxo_id))
            .then(a.is_create.cmp(&b.is_create))
    });

    Ok(results)
}

/// Query unshielded UTXO create/spend events for a session in a block range (transaction snapshot)
pub async fn get_unshielded_utxo_events_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id_bytes: &[u8],
    from_height: i64,
    to_height: i64,
) -> Result<Vec<UnshieldedUtxoEventRow>, AppError> {
    // Get the registered unshielded addresses for this session
    let addresses: Vec<(Vec<u8>,)> = sqlx::query_as(
        r#"
        SELECT address
        FROM nocy_sidecar.session_unshielded_address
        WHERE session_id = $1
        "#,
    )
    .bind(session_id_bytes)
    .fetch_all(tx.as_mut())
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to fetch unshielded addresses: {}", e)))?;

    if addresses.is_empty() {
        return Ok(Vec::new());
    }

    let address_list: Vec<Vec<u8>> = addresses.into_iter().map(|(a,)| a).collect();

    // Query UTXO creates - contract actions fetched separately via get_contract_actions_for_txs_tx()
    let create_rows: Vec<(
        i64,
        i64,
        Vec<u8>,
        i64,
        Vec<u8>,
        Vec<u8>,
        i64,
        Vec<u8>,
        Vec<u8>,
        Vec<u8>,
        bool,
        Option<i64>,
        Option<Vec<u8>>,
    )> = sqlx::query_as(
        r#"
        SELECT
            uu.id as utxo_id,
            t.id as tx_id,
            t.hash as tx_hash,
            b.height as block_height,
            uu.owner,
            uu.intent_hash,
            uu.output_index,
            uu.token_type,
            uu.value,
            uu.initial_nonce,
            uu.registered_for_dust_generation,
            uu.ctime,
            reg.paid_fees
        FROM unshielded_utxos uu
        JOIN transactions t ON uu.creating_transaction_id = t.id
        JOIN blocks b ON t.block_id = b.id
        LEFT JOIN regular_transactions reg ON reg.id = t.id
        WHERE uu.owner = ANY($1)
          AND b.height >= $2
          AND b.height <= $3
        ORDER BY b.height ASC, t.id ASC, uu.id ASC
        "#,
    )
    .bind(&address_list)
    .bind(from_height)
    .bind(to_height)
    .fetch_all(tx.as_mut())
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to fetch unshielded creates: {}", e)))?;

    // Query UTXO spends - contract actions fetched separately via get_contract_actions_for_txs_tx()
    let spend_rows: Vec<(
        i64,
        i64,
        Vec<u8>,
        i64,
        Vec<u8>,
        Vec<u8>,
        i64,
        Vec<u8>,
        Vec<u8>,
        Vec<u8>,
        bool,
        Option<i64>,
        Option<Vec<u8>>,
    )> = sqlx::query_as(
        r#"
        SELECT
            uu.id as utxo_id,
            t.id as tx_id,
            t.hash as tx_hash,
            b.height as block_height,
            uu.owner,
            uu.intent_hash,
            uu.output_index,
            uu.token_type,
            uu.value,
            uu.initial_nonce,
            uu.registered_for_dust_generation,
            uu.ctime,
            reg.paid_fees
        FROM unshielded_utxos uu
        JOIN transactions t ON uu.spending_transaction_id = t.id
        JOIN blocks b ON t.block_id = b.id
        LEFT JOIN regular_transactions reg ON reg.id = t.id
        WHERE uu.owner = ANY($1)
          AND uu.spending_transaction_id IS NOT NULL
          AND b.height >= $2
          AND b.height <= $3
        ORDER BY b.height ASC, t.id ASC, uu.id ASC
        "#,
    )
    .bind(&address_list)
    .bind(from_height)
    .bind(to_height)
    .fetch_all(tx.as_mut())
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to fetch unshielded spends: {}", e)))?;

    let mut results: Vec<UnshieldedUtxoEventRow> = Vec::new();

    for (
        utxo_id,
        tx_id,
        tx_hash,
        block_height,
        address,
        intent_hash,
        output_index,
        token_type,
        value_bytes,
        initial_nonce,
        registered_for_dust_generation,
        ctime,
        paid_fees,
    ) in create_rows
    {
        results.push(UnshieldedUtxoEventRow {
            utxo_id,
            tx_id,
            tx_hash,
            block_height,
            is_create: true,
            address,
            intent_hash,
            output_index,
            token_type,
            value_bytes,
            initial_nonce,
            registered_for_dust_generation,
            ctime,
            paid_fees,
        });
    }

    for (
        utxo_id,
        tx_id,
        tx_hash,
        block_height,
        address,
        intent_hash,
        output_index,
        token_type,
        value_bytes,
        initial_nonce,
        registered_for_dust_generation,
        ctime,
        paid_fees,
    ) in spend_rows
    {
        results.push(UnshieldedUtxoEventRow {
            utxo_id,
            tx_id,
            tx_hash,
            block_height,
            is_create: false,
            address,
            intent_hash,
            output_index,
            token_type,
            value_bytes,
            initial_nonce,
            registered_for_dust_generation,
            ctime,
            paid_fees,
        });
    }

    results.sort_by(|a, b| {
        a.block_height
            .cmp(&b.block_height)
            .then(a.tx_id.cmp(&b.tx_id))
            .then(a.utxo_id.cmp(&b.utxo_id))
            .then(a.is_create.cmp(&b.is_create))
    });

    Ok(results)
}
