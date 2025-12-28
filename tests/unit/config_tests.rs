//! Unit tests for the config module
//!
//! Run with: cargo test --test config_tests

use nocy_wallet_feed::config::{Config, SERVER_SECRET_BYTES};
use std::time::Duration;

fn create_test_config() -> Config {
    Config {
        database_url: "postgres://localhost/test".to_string(),
        server_host: "127.0.0.1".to_string(),
        server_port: 9090,
        upstream_indexer_url: "http://localhost:8080".to_string(),
        nats_url: "nats://localhost:4222".to_string(),
        server_secret: None,
        limit_blocks_max: 50,
        upstream_timeout_secs: 60,
        heartbeat_interval_secs: 30,
        poll_interval_secs: 2,
        sse_send_timeout_secs: 10,
        db_acquire_timeout_secs: 15,
        merkle_cache_enabled: true,
    }
}

#[test]
fn test_server_secret_bytes_constant() {
    assert_eq!(SERVER_SECRET_BYTES, 32);
}

#[test]
fn test_socket_addr() {
    let config = create_test_config();
    let addr = config.socket_addr();
    assert_eq!(addr.ip().to_string(), "127.0.0.1");
    assert_eq!(addr.port(), 9090);
}

#[test]
fn test_upstream_timeout() {
    let config = create_test_config();
    assert_eq!(config.upstream_timeout(), Duration::from_secs(60));
}

#[test]
fn test_heartbeat_interval() {
    let config = create_test_config();
    assert_eq!(config.heartbeat_interval(), Duration::from_secs(30));
}

#[test]
fn test_poll_interval() {
    let config = create_test_config();
    assert_eq!(config.poll_interval(), Duration::from_secs(2));
}

#[test]
fn test_sse_send_timeout() {
    let config = create_test_config();
    assert_eq!(config.sse_send_timeout(), Duration::from_secs(10));
}

#[test]
fn test_db_acquire_timeout() {
    let config = create_test_config();
    assert_eq!(config.db_acquire_timeout(), Duration::from_secs(15));
}

#[test]
fn test_server_secret_bytes_from_valid_hex() {
    let mut config = create_test_config();
    config.server_secret = Some(
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string(),
    );

    let (secret, was_generated) = config.server_secret_bytes().unwrap();
    assert!(!was_generated);
    assert_eq!(secret.len(), 32);
    assert_eq!(secret[0], 0x01);
    assert_eq!(secret[1], 0x23);
}

#[test]
fn test_server_secret_bytes_generates_random_when_none() {
    let config = create_test_config();

    let (secret1, was_generated1) = config.server_secret_bytes().unwrap();
    let (secret2, was_generated2) = config.server_secret_bytes().unwrap();

    assert!(was_generated1);
    assert!(was_generated2);
    assert_eq!(secret1.len(), 32);
    assert_eq!(secret2.len(), 32);
    // Random secrets should be different (with overwhelming probability)
    assert_ne!(secret1, secret2);
}

#[test]
fn test_server_secret_bytes_invalid_hex() {
    let mut config = create_test_config();
    config.server_secret = Some("not-valid-hex".to_string());

    let result = config.server_secret_bytes();
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("valid hex"));
}

#[test]
fn test_server_secret_bytes_wrong_length() {
    let mut config = create_test_config();
    config.server_secret = Some("0123456789abcdef0123456789abcdef".to_string());

    let result = config.server_secret_bytes();
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("32 bytes"));
}

#[test]
fn test_server_secret_bytes_too_long() {
    let mut config = create_test_config();
    config.server_secret = Some(
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123".to_string(),
    );

    let result = config.server_secret_bytes();
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("32 bytes"));
}

#[test]
fn test_server_secret_bytes_uppercase_hex() {
    let mut config = create_test_config();
    config.server_secret = Some(
        "0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF".to_string(),
    );

    let (secret, was_generated) = config.server_secret_bytes().unwrap();
    assert!(!was_generated);
    assert_eq!(secret.len(), 32);
}

#[test]
fn test_server_secret_bytes_mixed_case_hex() {
    let mut config = create_test_config();
    config.server_secret = Some(
        "0123456789AbCdEf0123456789aBcDeF0123456789ABcdef0123456789abCDEF".to_string(),
    );

    let (secret, was_generated) = config.server_secret_bytes().unwrap();
    assert!(!was_generated);
    assert_eq!(secret.len(), 32);
}
