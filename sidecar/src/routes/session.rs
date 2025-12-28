use axum::{
    extract::{Path, State},
    routing::{post, put},
    Json, Router,
};
use bech32::{FromBase32, ToBase32, Variant};
use blake3::Hasher;
use serde::{Deserialize, Serialize, Serializer};
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, instrument, warn};

use crate::config::SERVER_SECRET_BYTES;
use crate::error::AppError;
use crate::graphql_ws::SubscriptionManager;
use crate::keepalive::KeepaliveManager;
use crate::merkle_cache::MerkleCacheManager;
use crate::upstream::UpstreamClient;

/// Serialize Option<i64> as a decimal string for JavaScript compatibility
fn serialize_option_i64_as_string<S>(value: &Option<i64>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match value {
        Some(v) => serializer.serialize_some(&v.to_string()),
        None => serializer.serialize_none(),
    }
}

/// Session ID is a 32-byte SHA256 hash, hex-encoded (64 chars)
const SESSION_ID_BYTES: usize = 32;

/// Parse and validate a session ID (hex-encoded 32 bytes)
fn parse_session_id(session_id: &str) -> Result<Vec<u8>, AppError> {
    let bytes = hex::decode(session_id)
        .map_err(|_| AppError::BadRequest("session_id must be valid hex".into()))?;

    if bytes.len() != SESSION_ID_BYTES {
        return Err(AppError::BadRequest(format!(
            "session_id must be {} bytes ({} hex chars), got {} bytes",
            SESSION_ID_BYTES,
            SESSION_ID_BYTES * 2,
            bytes.len()
        )));
    }

    Ok(bytes)
}

/// Expected dust public key length in bytes (32 bytes = 64 hex chars)
const DUST_PUBLIC_KEY_BYTES: usize = 32;

const VIEWING_KEY_HRP_UNDEPLOYED: &str = "mn_shield-esk_undeployed";

fn is_hex_32_bytes(value: &str) -> bool {
    let value = value.strip_prefix("0x").unwrap_or(value);
    value.len() == 64 && value.chars().all(|c| c.is_ascii_hexdigit())
}

/// Normalize a viewing key for the undeployed network.
///
/// Supported formats:
/// - Bech32m with HRP `mn_shield-esk_undeployed`
/// - 32-byte hex (64 hex chars, optional `0x` prefix)
///
/// Returns a canonical bech32m-encoded viewing key for upstream.
fn normalize_viewing_key(viewing_key: &str) -> Result<String, AppError> {
    let viewing_key = viewing_key.trim();
    if viewing_key.is_empty() {
        return Err(AppError::BadRequest("viewing_key is required".into()));
    }

    if is_hex_32_bytes(viewing_key) {
        let value = viewing_key.strip_prefix("0x").unwrap_or(viewing_key);
        let bytes = hex::decode(value)
            .map_err(|_| AppError::BadRequest("viewing_key must be valid hex".into()))?;
        if bytes.len() != 32 {
            return Err(AppError::BadRequest(
                "viewing_key must be 32 bytes (64 hex chars)".into(),
            ));
        }
        let encoded = bech32::encode(
            VIEWING_KEY_HRP_UNDEPLOYED,
            bytes.to_base32(),
            Variant::Bech32m,
        )
        .map_err(|_| AppError::BadRequest("viewing_key must be bech32m or 32-byte hex".into()))?;
        return Ok(encoded);
    }

    let (hrp, data, variant) = bech32::decode(viewing_key)
        .map_err(|_| AppError::BadRequest("viewing_key must be bech32m or 32-byte hex".into()))?;

    if !hrp.eq_ignore_ascii_case(VIEWING_KEY_HRP_UNDEPLOYED) {
        return Err(AppError::BadRequest(format!(
            "viewing_key HRP must be '{}'",
            VIEWING_KEY_HRP_UNDEPLOYED
        )));
    }

    if variant != Variant::Bech32m {
        return Err(AppError::BadRequest("viewing_key must be bech32m".into()));
    }

    let bytes = Vec::<u8>::from_base32(&data)
        .map_err(|_| AppError::BadRequest("viewing_key must be bech32m".into()))?;

    bech32::encode(
        VIEWING_KEY_HRP_UNDEPLOYED,
        bytes.to_base32(),
        Variant::Bech32m,
    )
    .map_err(|_| AppError::BadRequest("viewing_key must be bech32m".into()))
}

/// Validate and decode a hex-encoded dust public key
fn validate_dust_public_key(dust_key: &str) -> Result<Vec<u8>, AppError> {
    let bytes = hex::decode(dust_key)
        .map_err(|_| AppError::BadRequest("dust_public_key must be valid hex".into()))?;

    if bytes.len() != DUST_PUBLIC_KEY_BYTES {
        return Err(AppError::BadRequest(format!(
            "dust_public_key must be {} bytes ({} hex chars), got {} bytes",
            DUST_PUBLIC_KEY_BYTES,
            DUST_PUBLIC_KEY_BYTES * 2,
            bytes.len()
        )));
    }

    Ok(bytes)
}

/// Shared state for session and feed routes
#[derive(Clone)]
pub struct SessionState {
    pub db_pool: PgPool,
    pub upstream_client: UpstreamClient,
    pub keepalive_manager: KeepaliveManager,
    pub subscription_manager: Arc<SubscriptionManager>,
    /// Server secret for deterministic session ID generation (32 bytes).
    /// Session IDs are derived as: BLAKE3(server_secret || viewingKey || dustPublicKey || unshieldedAddress)
    /// This ensures session IDs are:
    /// - Deterministic per-server (client reconnection works)
    /// - Not correlatable across servers (attacker can't reverse lookup)
    pub server_secret: [u8; SERVER_SECRET_BYTES],
    /// Maximum number of blocks per feed request (from config)
    pub limit_blocks_max: u32,
    /// SSE heartbeat interval
    pub heartbeat_interval: Duration,
    /// SSE poll interval
    pub poll_interval: Duration,
    /// SSE send timeout
    pub sse_send_timeout: Duration,
    /// Merkle cache manager for serving collapsed merkle updates inline
    pub merkle_cache_manager: Option<Arc<MerkleCacheManager>>,
}

/// Generate a deterministic session ID from wallet credentials and server secret.
///
/// Formula: sessionId = BLAKE3(server_secret || viewingKey || dustPublicKey || unshieldedAddress)
///
/// Security properties:
/// - Deterministic: Same inputs always produce same output (client reconnection works)
/// - Server-specific: Different servers produce different session IDs (no cross-server correlation)
/// - One-way: Cannot recover credentials from session ID
/// - Domain-separated: Uses BLAKE3's keyed mode for additional security
pub fn generate_session_id(
    server_secret: &[u8; SERVER_SECRET_BYTES],
    viewing_key: &str,
    dust_public_key: Option<&[u8]>,
    unshielded_address: Option<&[u8]>,
) -> [u8; SESSION_ID_BYTES] {
    // Use BLAKE3 keyed mode with server_secret as the key
    // This provides domain separation and ensures the secret is used cryptographically correctly
    let mut hasher = Hasher::new_keyed(server_secret);

    // Hash the viewing key (bech32m string)
    hasher.update(viewing_key.as_bytes());

    // Hash dust public key if provided, with length prefix for domain separation
    if let Some(dust_key) = dust_public_key {
        hasher.update(&[0x01]); // Present marker
        hasher.update(dust_key);
    } else {
        hasher.update(&[0x00]); // Absent marker
    }

    // Hash unshielded address if provided, with length prefix for domain separation
    if let Some(addr) = unshielded_address {
        hasher.update(&[0x01]); // Present marker
        hasher.update(addr);
    } else {
        hasher.update(&[0x00]); // Absent marker
    }

    // Finalize and return 32-byte session ID
    let hash = hasher.finalize();
    let mut session_id = [0u8; SESSION_ID_BYTES];
    session_id.copy_from_slice(hash.as_bytes());
    session_id
}

/// Request body for session bootstrap.
/// All credentials are required for deterministic session ID generation.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapRequest {
    /// The viewing key (bech32m HRP `mn_shield-esk_undeployed`, or 32-byte hex).
    /// Used for shielded transaction indexing via upstream indexer.
    pub viewing_key: String,
    /// The dust public key (hex-encoded 32 bytes).
    /// Used for dust token indexing.
    pub dust_public_key: String,
    /// The unshielded address (hex-encoded 32 bytes).
    /// Used for unshielded transaction indexing.
    pub unshielded_address: String,
    /// Optional nullifier streaming preference (default: enabled).
    pub nullifier_streaming: Option<NullifierStreaming>,
}

/// Response for session bootstrap
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapResponse {
    /// The session ID (hex-encoded 32 bytes).
    /// This is a deterministic hash of (server_secret || credentials).
    /// Same credentials on same server always produce same session ID.
    pub session_id: String,
}

/// Request body for session connect (proxy only)
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectRequest {
    /// The viewing key to connect with (bech32m HRP `mn_shield-esk_undeployed`, or 32-byte hex)
    pub viewing_key: String,
}

/// Response for session connect
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectResponse {
    /// The session ID assigned by the upstream indexer (hex-encoded 32 bytes)
    pub session_id: String,
}

/// Request body for session disconnect
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DisconnectRequest {
    /// The session ID to disconnect (hex-encoded 32 bytes)
    pub session_id: String,
}

/// Response for session disconnect
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DisconnectResponse {
    /// Whether the disconnect was successful
    pub success: bool,
}

/// Expected unshielded address length in bytes (32 bytes = 64 hex chars)
const UNSHIELDED_ADDRESS_BYTES: usize = 32;

/// Nullifier streaming preference for a session
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NullifierStreaming {
    /// Stream all ZswapInputRaw events (default)
    #[default]
    Enabled,
    /// Skip nullifier streaming (for wallets using 0-value change outputs)
    Disabled,
}

impl NullifierStreaming {
    /// Convert to database string value
    pub fn as_db_str(&self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
        }
    }

    /// Parse from database string value
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "enabled" => Some(Self::Enabled),
            "disabled" => Some(Self::Disabled),
            _ => None,
        }
    }
}

/// Check if a session exists in session_profile, returning 404 if not found
async fn require_session_exists(pool: &PgPool, session_id_bytes: &[u8]) -> Result<(), AppError> {
    let exists: bool = sqlx::query_scalar(
        r#"
        SELECT EXISTS(
            SELECT 1 FROM nocy_sidecar.session_profile WHERE session_id = $1
        )
        "#,
    )
    .bind(session_id_bytes)
    .fetch_one(pool)
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to check session existence: {}", e)))?;

    if !exists {
        return Err(AppError::NotFound(format!(
            "Session {} not found. Use /bootstrap to create a session first.",
            hex::encode(session_id_bytes)
        )));
    }

    Ok(())
}

/// Validate and decode a hex-encoded unshielded address
fn validate_unshielded_address(address: &str) -> Result<Vec<u8>, AppError> {
    let bytes = hex::decode(address)
        .map_err(|_| AppError::BadRequest("unshielded address must be valid hex".into()))?;

    if bytes.len() != UNSHIELDED_ADDRESS_BYTES {
        return Err(AppError::BadRequest(format!(
            "unshielded address must be {} bytes ({} hex chars), got {} bytes",
            UNSHIELDED_ADDRESS_BYTES,
            UNSHIELDED_ADDRESS_BYTES * 2,
            bytes.len()
        )));
    }

    Ok(bytes)
}

/// Request body for profile update
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateProfileRequest {
    /// Optional dust public key (hex encoded, 32 bytes)
    pub dust_public_key: Option<String>,
    /// Optional list of unshielded addresses (hex encoded, 32 bytes each)
    pub unshielded_addresses: Option<Vec<String>>,
    /// Optional nullifier streaming preference
    pub nullifier_streaming: Option<NullifierStreaming>,
}

/// Response for profile update
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateProfileResponse {
    /// Whether the update was successful
    pub success: bool,
    /// Whether unshielded addresses changed - if true, client MUST restart sync
    pub addresses_changed: bool,
    /// If addresses changed, client MUST restart feed subscription from this height
    /// to receive historical unshielded deltas for the new addresses.
    /// Null if addresses did not change.
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_option_i64_as_string")]
    pub restart_from_height: Option<i64>,
    /// Current nullifier streaming preference after update
    pub nullifier_streaming: NullifierStreaming,
}

/// Bootstrap a new session with all wallet credentials.
///
/// This endpoint:
/// 1. Validates all credentials
/// 2. Generates a deterministic session ID using BLAKE3(server_secret || credentials)
/// 3. Connects to upstream indexer for shielded transaction indexing
/// 4. Persists session profile with all credentials
/// 5. Returns the local session ID
///
/// The session ID is deterministic per-server: same credentials always produce
/// the same session ID on the same server, enabling client reconnection without
/// storing session IDs. However, different servers produce different session IDs,
/// preventing cross-server correlation attacks.
#[instrument(skip(state, req))]
async fn bootstrap(
    State(state): State<SessionState>,
    Json(req): Json<BootstrapRequest>,
) -> Result<Json<BootstrapResponse>, AppError> {
    // === Step 1: Validate all credentials ===

    // Normalize and validate viewing key
    let viewing_key = normalize_viewing_key(&req.viewing_key)?;

    // Validate and decode dust public key (required)
    let dust_public_key_bytes = validate_dust_public_key(&req.dust_public_key)?;

    // Validate and decode unshielded address (required)
    let unshielded_address_bytes = validate_unshielded_address(&req.unshielded_address)?;

    // === Step 2: Generate deterministic session ID ===

    let session_id_bytes = generate_session_id(
        &state.server_secret,
        &viewing_key,
        Some(&dust_public_key_bytes),
        Some(&unshielded_address_bytes),
    );
    let session_id = hex::encode(&session_id_bytes);

    info!(%session_id, "Generated session ID from credentials");

    // === Step 3: Connect to upstream indexer ===
    // We need upstream connection for shielded transaction indexing and keepalive

    info!("Connecting to upstream indexer");
    let upstream_response = state.upstream_client.connect(&viewing_key).await?;
    let upstream_session_id = upstream_response.session_id.clone();
    let upstream_session_id_bytes = hex::decode(&upstream_session_id).map_err(|e| {
        AppError::UpstreamError(format!(
            "Upstream returned invalid session_id hex: {}",
            e
        ))
    })?;
    if upstream_session_id_bytes.len() != SESSION_ID_BYTES {
        return Err(AppError::UpstreamError(format!(
            "Upstream returned invalid session_id length: expected {} bytes, got {}",
            SESSION_ID_BYTES,
            upstream_session_id_bytes.len()
        )));
    }

    info!(
        local_session_id = %session_id,
        upstream_session_id = %upstream_session_id,
        "Upstream connection established"
    );

    // === Step 4: Persist session profile with all credentials ===

    let nullifier_streaming = req.nullifier_streaming.unwrap_or_default();

    // Use a transaction to ensure atomicity
    let mut tx = state
        .db_pool
        .begin()
        .await
        .map_err(|e| AppError::InternalError(format!("Failed to start transaction: {}", e)))?;

    // Upsert session profile with upstream session ID mapping
    let persist_result = sqlx::query(
        r#"
        INSERT INTO nocy_sidecar.session_profile (
            session_id,
            upstream_session_id,
            dust_public_key,
            nullifier_streaming,
            updated_at
        )
        VALUES ($1, $2, $3, $4, NOW())
        ON CONFLICT (session_id) DO UPDATE SET
            upstream_session_id = EXCLUDED.upstream_session_id,
            dust_public_key = EXCLUDED.dust_public_key,
            nullifier_streaming = EXCLUDED.nullifier_streaming,
            updated_at = NOW()
        "#,
    )
    .bind(&session_id_bytes.as_slice())
    .bind(&upstream_session_id_bytes)
    .bind(&dust_public_key_bytes)
    .bind(nullifier_streaming.as_db_str())
    .execute(&mut *tx)
    .await;

    if let Err(e) = persist_result {
        // Rollback: disconnect from upstream on profile persistence failure
        warn!(%session_id, error = %e, "Profile persistence failed, rolling back upstream connection");
        let _ = state.upstream_client.disconnect(&upstream_session_id).await;
        return Err(AppError::InternalError(format!(
            "Failed to persist session profile: {}",
            e
        )));
    }

    // Insert unshielded address
    sqlx::query(
        r#"
        INSERT INTO nocy_sidecar.session_unshielded_address (session_id, address)
        VALUES ($1, $2)
        ON CONFLICT (session_id, address) DO NOTHING
        "#,
    )
    .bind(&session_id_bytes.as_slice())
    .bind(&unshielded_address_bytes)
    .execute(&mut *tx)
    .await
    .map_err(|e| {
        AppError::InternalError(format!("Failed to persist unshielded address: {}", e))
    })?;

    tx.commit()
        .await
        .map_err(|e| AppError::InternalError(format!("Failed to commit transaction: {}", e)))?;

    info!(%session_id, "Session profile persisted");

    // === Step 5: Start keepalive using upstream session ID ===
    // The keepalive manager uses the upstream session ID to keep the upstream connection alive
    state.keepalive_manager.start(&upstream_session_id).await;

    Ok(Json(BootstrapResponse { session_id }))
}

/// Connect to upstream indexer (proxy only, no profile persistence)
#[instrument(skip(state, req))]
async fn connect(
    State(state): State<SessionState>,
    Json(req): Json<ConnectRequest>,
) -> Result<Json<ConnectResponse>, AppError> {
    let viewing_key = normalize_viewing_key(&req.viewing_key)?;

    // Proxy connect to upstream indexer
    info!("Connecting to upstream indexer (proxy only)");
    let upstream_response = state.upstream_client.connect(&viewing_key).await?;

    info!(session_id = %upstream_response.session_id, "Upstream connection established");

    Ok(Json(ConnectResponse {
        session_id: upstream_response.session_id,
    }))
}

/// Disconnect a session from the sidecar and upstream indexer.
///
/// This endpoint:
/// 1. Looks up the upstream session ID from the local session ID
/// 2. Stops the keepalive task
/// 3. Disconnects from upstream indexer
#[instrument(skip(state, req), fields(session_id = %req.session_id))]
async fn disconnect(
    State(state): State<SessionState>,
    Json(req): Json<DisconnectRequest>,
) -> Result<Json<DisconnectResponse>, AppError> {
    // Validate session ID format
    let session_id_bytes = parse_session_id(&req.session_id)?;

    info!("Looking up upstream session ID for disconnect");

    // Look up the upstream session ID from the database
    let upstream_result: Option<(Option<Vec<u8>>,)> = sqlx::query_as(
        r#"
        SELECT upstream_session_id FROM nocy_sidecar.session_profile
        WHERE session_id = $1
        "#,
    )
    .bind(&session_id_bytes)
    .fetch_optional(&state.db_pool)
    .await
    .map_err(|e| AppError::InternalError(format!("Failed to look up upstream session: {}", e)))?;

    let upstream_session_id = match upstream_result {
        Some((Some(bytes),)) => {
            if bytes.len() != SESSION_ID_BYTES {
                warn!(
                    session_id = %req.session_id,
                    upstream_len = bytes.len(),
                    "Invalid upstream_session_id length, nothing to disconnect"
                );
                return Ok(Json(DisconnectResponse { success: true }));
            }
            hex::encode(bytes)
        }
        Some((None,)) => {
            warn!(
                session_id = %req.session_id,
                "Session missing upstream mapping, nothing to disconnect"
            );
            return Ok(Json(DisconnectResponse { success: true }));
        }
        None => {
            warn!(session_id = %req.session_id, "Session not found, nothing to disconnect");
            return Ok(Json(DisconnectResponse { success: true }));
        }
    };

    info!(%upstream_session_id, "Found upstream session ID");

    // Stop keepalive using upstream session ID
    state.keepalive_manager.stop(&upstream_session_id).await;
    info!("Keepalive stopped for session");

    // Proxy disconnect to upstream indexer using upstream session ID
    let upstream_response = state.upstream_client.disconnect(&upstream_session_id).await?;

    info!(success = upstream_response.success, "Upstream disconnection complete");

    Ok(Json(DisconnectResponse {
        success: upstream_response.success,
    }))
}

/// Update session profile: upsert dustPublicKey and unshielded addresses
#[instrument(skip(state, req), fields(%session_id))]
async fn update_profile(
    State(state): State<SessionState>,
    Path(session_id): Path<String>,
    Json(req): Json<UpdateProfileRequest>,
) -> Result<Json<UpdateProfileResponse>, AppError> {
    // Parse and validate session ID
    let session_id_bytes = parse_session_id(&session_id)?;

    info!("Updating session profile");

    // Verify session exists before updating
    require_session_exists(&state.db_pool, &session_id_bytes).await?;

    // Validate and decode dust public key if provided
    let dust_public_key_bytes = if let Some(ref dust_key) = req.dust_public_key {
        Some(validate_dust_public_key(dust_key)?)
    } else {
        None
    };

    // Validate and decode all unshielded addresses if provided
    let unshielded_addresses_bytes: Option<Vec<Vec<u8>>> =
        if let Some(ref addresses) = req.unshielded_addresses {
            let mut decoded = Vec::with_capacity(addresses.len());
            for addr in addresses {
                decoded.push(validate_unshielded_address(addr)?);
            }
            Some(decoded)
        } else {
            None
        };

    // Use a transaction to ensure atomicity
    let mut tx = state
        .db_pool
        .begin()
        .await
        .map_err(|e| AppError::InternalError(format!("Failed to start transaction: {}", e)))?;

    // Upsert session_profile if dust_public_key is provided
    if let Some(ref dust_key) = dust_public_key_bytes {
        sqlx::query(
            r#"
            INSERT INTO nocy_sidecar.session_profile (session_id, dust_public_key, updated_at)
            VALUES ($1, $2, NOW())
            ON CONFLICT (session_id) DO UPDATE SET
                dust_public_key = EXCLUDED.dust_public_key,
                updated_at = NOW()
            "#,
        )
        .bind(&session_id_bytes)
        .bind(dust_key)
        .execute(&mut *tx)
        .await
        .map_err(|e| AppError::InternalError(format!("Failed to upsert session profile: {}", e)))?;

        info!("Updated dust_public_key");
    }

    // Track whether addresses changed (for client resubscribe notification)
    let mut addresses_changed = false;

    // Replace unshielded addresses if provided (delete all, then insert new)
    if let Some(ref addresses) = unshielded_addresses_bytes {
        // Fetch existing addresses to detect changes
        let existing: Vec<(Vec<u8>,)> = sqlx::query_as(
            r#"
            SELECT address FROM nocy_sidecar.session_unshielded_address
            WHERE session_id = $1
            ORDER BY address
            "#,
        )
        .bind(&session_id_bytes)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| {
            AppError::InternalError(format!("Failed to fetch existing addresses: {}", e))
        })?;

        let existing_set: std::collections::HashSet<&[u8]> =
            existing.iter().map(|(a,)| a.as_slice()).collect();
        let new_set: std::collections::HashSet<&[u8]> =
            addresses.iter().map(|a| a.as_slice()).collect();

        // Addresses changed if sets differ
        addresses_changed = existing_set != new_set;

        // Delete existing addresses for this session
        sqlx::query(
            r#"
            DELETE FROM nocy_sidecar.session_unshielded_address
            WHERE session_id = $1
            "#,
        )
        .bind(&session_id_bytes)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            AppError::InternalError(format!("Failed to delete existing addresses: {}", e))
        })?;

        // Insert new addresses (ON CONFLICT DO NOTHING handles duplicates in input)
        for addr in addresses {
            sqlx::query(
                r#"
                INSERT INTO nocy_sidecar.session_unshielded_address (session_id, address)
                VALUES ($1, $2)
                ON CONFLICT (session_id, address) DO NOTHING
                "#,
            )
            .bind(&session_id_bytes)
            .bind(addr)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                AppError::InternalError(format!("Failed to insert unshielded address: {}", e))
            })?;
        }

        info!(count = addresses.len(), addresses_changed, "Updated unshielded addresses");
    }

    // Update nullifier_streaming preference if provided
    if let Some(ref preference) = req.nullifier_streaming {
        sqlx::query(
            r#"
            UPDATE nocy_sidecar.session_profile
            SET nullifier_streaming = $2, updated_at = NOW()
            WHERE session_id = $1
            "#,
        )
        .bind(&session_id_bytes)
        .bind(preference.as_db_str())
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            AppError::InternalError(format!("Failed to update nullifier_streaming: {}", e))
        })?;

        info!(preference = %preference.as_db_str(), "Updated nullifier_streaming preference");
    }

    tx.commit()
        .await
        .map_err(|e| AppError::InternalError(format!("Failed to commit transaction: {}", e)))?;

    // Fetch current nullifier_streaming preference to return in response
    let current_preference: (String,) = sqlx::query_as(
        r#"
        SELECT nullifier_streaming FROM nocy_sidecar.session_profile
        WHERE session_id = $1
        "#,
    )
    .bind(&session_id_bytes)
    .fetch_one(&state.db_pool)
    .await
    .map_err(|e| {
        AppError::InternalError(format!("Failed to fetch nullifier_streaming: {}", e))
    })?;

    let nullifier_streaming =
        NullifierStreaming::from_db_str(&current_preference.0).unwrap_or_default();

    info!("Session profile updated successfully");

    // If addresses changed, client MUST restart from height 0 to receive
    // historical unshielded deltas for the new addresses
    let restart_from_height = if addresses_changed { Some(0) } else { None };

    Ok(Json(UpdateProfileResponse {
        success: true,
        addresses_changed,
        restart_from_height,
        nullifier_streaming,
    }))
}

/// Get the nullifier streaming preference for a session
/// Returns the preference, or Enabled as default if session not found
pub async fn get_session_nullifier_streaming(
    pool: &PgPool,
    session_id: &str,
) -> Result<NullifierStreaming, AppError> {
    let session_id_bytes = parse_session_id(session_id)?;

    let result: Option<(String,)> = sqlx::query_as(
        r#"
        SELECT nullifier_streaming FROM nocy_sidecar.session_profile
        WHERE session_id = $1
        "#,
    )
    .bind(&session_id_bytes)
    .fetch_optional(pool)
    .await
    .map_err(|e| {
        AppError::InternalError(format!("Failed to fetch nullifier_streaming: {}", e))
    })?;

    Ok(result
        .and_then(|(s,)| NullifierStreaming::from_db_str(&s))
        .unwrap_or_default())
}

/// Create the session routes router.
/// The router is generic over state S, where SessionState can be extracted from S via FromRef.
pub fn session_routes<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    SessionState: axum::extract::FromRef<S>,
{
    Router::new()
        .route("/bootstrap", post(bootstrap))
        .route("/connect", post(connect))
        .route("/disconnect", post(disconnect))
        .route("/:session_id/profile", put(update_profile))
}
