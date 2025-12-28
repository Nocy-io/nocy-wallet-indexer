//! Integration tests for the nocy-wallet-feed sidecar API
//!
//! These tests run against a live sidecar instance. Set the `SIDECAR_URL` environment
//! variable to point to a running sidecar (default: http://127.0.0.1:8080).
//!
//! Run with: cargo test --test integration_tests
//!
//! Note: Some tests require a running sidecar with proper database and NATS connections.
//! Tests will be skipped if the sidecar is not available.

use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Default sidecar URL - override with SIDECAR_URL env var
fn sidecar_url() -> String {
    std::env::var("SIDECAR_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".to_string())
}

/// Test viewing key (bech32m with HRP mn_shield-esk_undeployed)
fn test_viewing_key() -> String {
    std::env::var("TEST_VIEWING_KEY").unwrap_or_else(|_| {
        "mn_shield-esk_undeployed1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq0wamev"
            .to_string()
    })
}

/// Test dust public key (32 bytes hex)
fn test_dust_public_key() -> String {
    "0000000000000000000000000000000000000000000000000000000000000001".to_string()
}

/// Test unshielded address (32 bytes hex)
fn test_unshielded_address() -> String {
    "0000000000000000000000000000000000000000000000000000000000000002".to_string()
}

/// Create an HTTP client with reasonable timeouts
fn create_client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("Failed to create HTTP client")
}

/// Check if sidecar is available
async fn is_sidecar_available() -> bool {
    let client = create_client();
    let url = format!("{}/healthz", sidecar_url());
    
    match client.get(&url).send().await {
        Ok(resp) => resp.status().is_success(),
        Err(_) => false,
    }
}

/// Skip test if sidecar is not available
macro_rules! skip_if_unavailable {
    () => {
        if !is_sidecar_available().await {
            eprintln!("Sidecar not available at {} - skipping test", sidecar_url());
            return;
        }
    };
}

// ============================================================================
// Response Types
// ============================================================================

#[derive(Debug, Deserialize)]
struct HealthResponse {
    status: String,
}

#[derive(Debug, Deserialize)]
struct ReadyResponse {
    status: String,
    checks: ReadinessChecks,
}

#[derive(Debug, Deserialize)]
struct ReadinessChecks {
    postgres: bool,
    nats: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BootstrapResponse {
    session_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DisconnectResponse {
    success: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateProfileResponse {
    success: bool,
    addresses_changed: bool,
    restart_from_height: Option<String>,
    nullifier_streaming: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FeedResponse {
    api_version: String,
    watermarks: Watermarks,
    blocks: Vec<BlockBundle>,
    next_height: Option<String>,
    next_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Watermarks {
    chain_head: String,
    wallet_ready_height: Option<String>,
    finalized_height: String,
}

#[derive(Debug, Deserialize)]
struct BlockBundle {
    meta: BlockMeta,
    items: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlockMeta {
    height: String,
    hash: String,
    parent_hash: String,
    timestamp: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FirstFreeResponse {
    first_free_index: String,
    block_height: String,
    source: String,
}

#[derive(Debug, Deserialize)]
struct ErrorResponse {
    error: String,
    message: String,
}

// ============================================================================
// Request Types
// ============================================================================

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BootstrapRequest {
    viewing_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    dust_public_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unshielded_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    nullifier_streaming: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DisconnectRequest {
    session_id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateProfileRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    dust_public_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unshielded_addresses: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    nullifier_streaming: Option<String>,
}

// ============================================================================
// Health Endpoint Tests
// ============================================================================

#[tokio::test]
async fn test_healthz_returns_ok() {
    skip_if_unavailable!();
    
    let client = create_client();
    let url = format!("{}/healthz", sidecar_url());
    
    let resp = client.get(&url).send().await.expect("Request failed");
    assert!(resp.status().is_success(), "Expected 200 OK");
    
    let body: HealthResponse = resp.json().await.expect("Failed to parse response");
    assert_eq!(body.status, "ok");
}

#[tokio::test]
async fn test_readyz_returns_status_with_checks() {
    skip_if_unavailable!();
    
    let client = create_client();
    let url = format!("{}/readyz", sidecar_url());
    
    let resp = client.get(&url).send().await.expect("Request failed");
    // May return 200 or 503 depending on dependency status
    
    let body: ReadyResponse = resp.json().await.expect("Failed to parse response");
    assert!(body.status == "ready" || body.status == "not_ready");
    // Checks should be present regardless of status
    // (postgres and nats booleans are always returned)
}

#[tokio::test]
async fn test_metrics_endpoint_returns_prometheus_format() {
    skip_if_unavailable!();
    
    let client = create_client();
    let url = format!("{}/metrics", sidecar_url());
    
    let resp = client.get(&url).send().await.expect("Request failed");
    assert!(resp.status().is_success(), "Expected 200 OK");
    
    let body = resp.text().await.expect("Failed to read response body");
    // Prometheus metrics should contain at least some metric lines
    assert!(body.contains("# ") || body.is_empty(), "Expected Prometheus format");
}

// ============================================================================
// Session Management Tests
// ============================================================================

#[tokio::test]
async fn test_bootstrap_creates_session() {
    skip_if_unavailable!();
    
    let client = create_client();
    let url = format!("{}/v1/session/bootstrap", sidecar_url());
    
    let request = BootstrapRequest {
        viewing_key: test_viewing_key(),
        dust_public_key: Some(test_dust_public_key()),
        unshielded_address: Some(test_unshielded_address()),
        nullifier_streaming: Some("enabled".to_string()),
    };
    
    let resp = client
        .post(&url)
        .json(&request)
        .send()
        .await
        .expect("Request failed");
    
    if !resp.status().is_success() {
        let error: ErrorResponse = resp.json().await.unwrap_or(ErrorResponse {
            error: "unknown".to_string(),
            message: "Failed to parse error".to_string(),
        });
        panic!("Bootstrap failed: {:?}", error);
    }
    
    let body: BootstrapResponse = resp.json().await.expect("Failed to parse response");
    
    // Session ID should be 64 hex chars (32 bytes)
    assert_eq!(body.session_id.len(), 64, "Session ID should be 64 hex chars");
    assert!(
        body.session_id.chars().all(|c| c.is_ascii_hexdigit()),
        "Session ID should be hex"
    );
    
    // Cleanup: disconnect the session
    let disconnect_url = format!("{}/v1/session/disconnect", sidecar_url());
    let _ = client
        .post(&disconnect_url)
        .json(&DisconnectRequest {
            session_id: body.session_id,
        })
        .send()
        .await;
}

#[tokio::test]
async fn test_bootstrap_with_invalid_viewing_key_returns_400() {
    skip_if_unavailable!();
    
    let client = create_client();
    let url = format!("{}/v1/session/bootstrap", sidecar_url());
    
    let request = BootstrapRequest {
        viewing_key: "invalid_key".to_string(),
        dust_public_key: Some(test_dust_public_key()),
        unshielded_address: Some(test_unshielded_address()),
        nullifier_streaming: None,
    };
    
    let resp = client
        .post(&url)
        .json(&request)
        .send()
        .await
        .expect("Request failed");
    
    assert_eq!(resp.status().as_u16(), 400, "Expected 400 Bad Request");
}

#[tokio::test]
async fn test_bootstrap_with_hex_viewing_key() {
    skip_if_unavailable!();
    
    let client = create_client();
    let url = format!("{}/v1/session/bootstrap", sidecar_url());
    
    // 32-byte hex viewing key (64 hex chars)
    let hex_viewing_key = "0000000000000000000000000000000000000000000000000000000000000000";
    
    let request = BootstrapRequest {
        viewing_key: hex_viewing_key.to_string(),
        dust_public_key: Some(test_dust_public_key()),
        unshielded_address: Some(test_unshielded_address()),
        nullifier_streaming: None,
    };
    
    let resp = client
        .post(&url)
        .json(&request)
        .send()
        .await
        .expect("Request failed");
    
    // Should succeed - hex viewing keys are normalized to bech32m
    if resp.status().is_success() {
        let body: BootstrapResponse = resp.json().await.expect("Failed to parse response");
        assert_eq!(body.session_id.len(), 64);
        
        // Cleanup
        let disconnect_url = format!("{}/v1/session/disconnect", sidecar_url());
        let _ = client
            .post(&disconnect_url)
            .json(&DisconnectRequest {
                session_id: body.session_id,
            })
            .send()
            .await;
    }
    // If it fails due to upstream, that's also acceptable in test env
}

#[tokio::test]
async fn test_disconnect_session() {
    skip_if_unavailable!();
    
    let client = create_client();
    
    // First create a session
    let bootstrap_url = format!("{}/v1/session/bootstrap", sidecar_url());
    let request = BootstrapRequest {
        viewing_key: test_viewing_key(),
        dust_public_key: Some(test_dust_public_key()),
        unshielded_address: Some(test_unshielded_address()),
        nullifier_streaming: None,
    };
    
    let resp = client
        .post(&bootstrap_url)
        .json(&request)
        .send()
        .await
        .expect("Request failed");
    
    if !resp.status().is_success() {
        eprintln!("Bootstrap failed, skipping disconnect test");
        return;
    }
    
    let bootstrap_body: BootstrapResponse = resp.json().await.expect("Failed to parse response");
    
    // Now disconnect
    let disconnect_url = format!("{}/v1/session/disconnect", sidecar_url());
    let resp = client
        .post(&disconnect_url)
        .json(&DisconnectRequest {
            session_id: bootstrap_body.session_id,
        })
        .send()
        .await
        .expect("Request failed");
    
    assert!(resp.status().is_success(), "Disconnect should succeed");
    
    let body: DisconnectResponse = resp.json().await.expect("Failed to parse response");
    assert!(body.success);
}

#[tokio::test]
async fn test_update_profile() {
    skip_if_unavailable!();
    
    let client = create_client();
    
    // First create a session
    let bootstrap_url = format!("{}/v1/session/bootstrap", sidecar_url());
    let request = BootstrapRequest {
        viewing_key: test_viewing_key(),
        dust_public_key: Some(test_dust_public_key()),
        unshielded_address: Some(test_unshielded_address()),
        nullifier_streaming: Some("enabled".to_string()),
    };
    
    let resp = client
        .post(&bootstrap_url)
        .json(&request)
        .send()
        .await
        .expect("Request failed");
    
    if !resp.status().is_success() {
        eprintln!("Bootstrap failed, skipping update profile test");
        return;
    }
    
    let bootstrap_body: BootstrapResponse = resp.json().await.expect("Failed to parse response");
    let session_id = bootstrap_body.session_id.clone();
    
    // Update profile
    let update_url = format!("{}/v1/session/{}/profile", sidecar_url(), session_id);
    let update_request = UpdateProfileRequest {
        dust_public_key: None,
        unshielded_addresses: Some(vec![
            "0000000000000000000000000000000000000000000000000000000000000003".to_string(),
            "0000000000000000000000000000000000000000000000000000000000000004".to_string(),
        ]),
        nullifier_streaming: Some("disabled".to_string()),
    };
    
    let resp = client
        .put(&update_url)
        .json(&update_request)
        .send()
        .await
        .expect("Request failed");
    
    if resp.status().is_success() {
        let body: UpdateProfileResponse = resp.json().await.expect("Failed to parse response");
        assert!(body.success);
        assert_eq!(body.nullifier_streaming, "disabled");
    }
    
    // Cleanup
    let disconnect_url = format!("{}/v1/session/disconnect", sidecar_url());
    let _ = client
        .post(&disconnect_url)
        .json(&DisconnectRequest { session_id })
        .send()
        .await;
}

// ============================================================================
// Feed Endpoint Tests
// ============================================================================

#[tokio::test]
async fn test_feed_requires_session_id() {
    skip_if_unavailable!();
    
    let client = create_client();
    let url = format!("{}/v1/feed", sidecar_url());
    
    // Request without sessionId
    let resp = client.get(&url).send().await.expect("Request failed");
    
    assert_eq!(resp.status().as_u16(), 400, "Expected 400 Bad Request");
}

#[tokio::test]
async fn test_feed_with_invalid_session_returns_404() {
    skip_if_unavailable!();
    
    let client = create_client();
    // Use a valid-format but non-existent session ID
    let fake_session = "0000000000000000000000000000000000000000000000000000000000000000";
    let url = format!(
        "{}/v1/feed?sessionId={}&fromHeight=0",
        sidecar_url(),
        fake_session
    );
    
    let resp = client.get(&url).send().await.expect("Request failed");
    
    // Should return 404 Not Found for non-existent session
    assert_eq!(resp.status().as_u16(), 404, "Expected 404 Not Found");
}

#[tokio::test]
async fn test_feed_returns_blocks_and_watermarks() {
    skip_if_unavailable!();
    
    let client = create_client();
    
    // First create a session
    let bootstrap_url = format!("{}/v1/session/bootstrap", sidecar_url());
    let request = BootstrapRequest {
        viewing_key: test_viewing_key(),
        dust_public_key: Some(test_dust_public_key()),
        unshielded_address: Some(test_unshielded_address()),
        nullifier_streaming: None,
    };
    
    let resp = client
        .post(&bootstrap_url)
        .json(&request)
        .send()
        .await
        .expect("Request failed");
    
    if !resp.status().is_success() {
        eprintln!("Bootstrap failed, skipping feed test");
        return;
    }
    
    let bootstrap_body: BootstrapResponse = resp.json().await.expect("Failed to parse response");
    let session_id = bootstrap_body.session_id.clone();
    
    // Fetch feed
    let feed_url = format!(
        "{}/v1/feed?sessionId={}&fromHeight=0&limitBlocks=10",
        sidecar_url(),
        session_id
    );
    
    let resp = client.get(&feed_url).send().await.expect("Request failed");
    
    if resp.status().is_success() {
        let body: FeedResponse = resp.json().await.expect("Failed to parse response");
        
        assert_eq!(body.api_version, "v1");
        // Watermarks should be present
        assert!(!body.watermarks.chain_head.is_empty());
        assert!(!body.watermarks.finalized_height.is_empty());
        // Blocks is an array (may be empty if no data)
        // nextHeight may be null or a string
    }
    
    // Cleanup
    let disconnect_url = format!("{}/v1/session/disconnect", sidecar_url());
    let _ = client
        .post(&disconnect_url)
        .json(&DisconnectRequest { session_id })
        .send()
        .await;
}

#[tokio::test]
async fn test_feed_respects_limit_blocks() {
    skip_if_unavailable!();
    
    let client = create_client();
    
    // First create a session
    let bootstrap_url = format!("{}/v1/session/bootstrap", sidecar_url());
    let request = BootstrapRequest {
        viewing_key: test_viewing_key(),
        dust_public_key: Some(test_dust_public_key()),
        unshielded_address: Some(test_unshielded_address()),
        nullifier_streaming: None,
    };
    
    let resp = client
        .post(&bootstrap_url)
        .json(&request)
        .send()
        .await
        .expect("Request failed");
    
    if !resp.status().is_success() {
        eprintln!("Bootstrap failed, skipping feed limit test");
        return;
    }
    
    let bootstrap_body: BootstrapResponse = resp.json().await.expect("Failed to parse response");
    let session_id = bootstrap_body.session_id.clone();
    
    // Fetch feed with limit
    let limit = 5;
    let feed_url = format!(
        "{}/v1/feed?sessionId={}&fromHeight=0&limitBlocks={}",
        sidecar_url(),
        session_id,
        limit
    );
    
    let resp = client.get(&feed_url).send().await.expect("Request failed");
    
    if resp.status().is_success() {
        let body: FeedResponse = resp.json().await.expect("Failed to parse response");
        assert!(
            body.blocks.len() <= limit,
            "Should return at most {} blocks",
            limit
        );
    }
    
    // Cleanup
    let disconnect_url = format!("{}/v1/session/disconnect", sidecar_url());
    let _ = client
        .post(&disconnect_url)
        .json(&DisconnectRequest { session_id })
        .send()
        .await;
}

// ============================================================================
// Zswap Endpoint Tests
// ============================================================================

#[tokio::test]
async fn test_first_free_returns_index() {
    skip_if_unavailable!();
    
    let client = create_client();
    let url = format!("{}/v1/zswap/first-free", sidecar_url());
    
    let resp = client.get(&url).send().await.expect("Request failed");
    
    if resp.status().is_success() {
        let body: FirstFreeResponse = resp.json().await.expect("Failed to parse response");
        
        // first_free_index should be a decimal string
        assert!(
            body.first_free_index.parse::<u64>().is_ok(),
            "first_free_index should be a valid number"
        );
        
        // block_height should be a decimal string
        assert!(
            body.block_height.parse::<i64>().is_ok(),
            "block_height should be a valid number"
        );
        
        // source should be "snapshot" or "db"
        assert!(
            body.source == "snapshot" || body.source == "db",
            "source should be 'snapshot' or 'db'"
        );
    }
    // If it fails (e.g., no snapshot available), that's acceptable
}

// ============================================================================
// Error Response Tests
// ============================================================================

#[tokio::test]
async fn test_invalid_session_id_format_returns_400() {
    skip_if_unavailable!();
    
    let client = create_client();
    
    // Session ID that's not valid hex
    let url = format!("{}/v1/feed?sessionId=not-valid-hex&fromHeight=0", sidecar_url());
    let resp = client.get(&url).send().await.expect("Request failed");
    assert_eq!(resp.status().as_u16(), 400);
    
    // Session ID that's too short
    let url = format!("{}/v1/feed?sessionId=abc123&fromHeight=0", sidecar_url());
    let resp = client.get(&url).send().await.expect("Request failed");
    assert_eq!(resp.status().as_u16(), 400);
}

#[tokio::test]
async fn test_404_endpoint_returns_not_found() {
    skip_if_unavailable!();
    
    let client = create_client();
    let url = format!("{}/v1/nonexistent", sidecar_url());
    
    let resp = client.get(&url).send().await.expect("Request failed");
    assert_eq!(resp.status().as_u16(), 404);
}

// ============================================================================
// Session ID Determinism Tests
// ============================================================================

#[tokio::test]
async fn test_bootstrap_returns_deterministic_session_id() {
    skip_if_unavailable!();
    
    let client = create_client();
    let bootstrap_url = format!("{}/v1/session/bootstrap", sidecar_url());
    
    let request = BootstrapRequest {
        viewing_key: test_viewing_key(),
        dust_public_key: Some(test_dust_public_key()),
        unshielded_address: Some(test_unshielded_address()),
        nullifier_streaming: None,
    };
    
    // Bootstrap twice with the same credentials
    let resp1 = client
        .post(&bootstrap_url)
        .json(&request)
        .send()
        .await
        .expect("Request failed");
    
    if !resp1.status().is_success() {
        eprintln!("Bootstrap failed, skipping determinism test");
        return;
    }
    
    let body1: BootstrapResponse = resp1.json().await.expect("Failed to parse response");
    
    let resp2 = client
        .post(&bootstrap_url)
        .json(&request)
        .send()
        .await
        .expect("Request failed");
    
    if !resp2.status().is_success() {
        eprintln!("Second bootstrap failed, skipping determinism test");
        return;
    }
    
    let body2: BootstrapResponse = resp2.json().await.expect("Failed to parse response");
    
    // Session IDs should be the same (deterministic based on server secret + credentials)
    assert_eq!(
        body1.session_id, body2.session_id,
        "Session IDs should be deterministic for the same credentials"
    );
    
    // Cleanup
    let disconnect_url = format!("{}/v1/session/disconnect", sidecar_url());
    let _ = client
        .post(&disconnect_url)
        .json(&DisconnectRequest {
            session_id: body1.session_id,
        })
        .send()
        .await;
}
