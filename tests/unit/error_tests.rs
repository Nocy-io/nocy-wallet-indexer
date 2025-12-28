//! Unit tests for the error module
//!
//! Run with: cargo test --test error_tests

use nocy_wallet_feed::error::{AppError, ErrorCode, ErrorResponse};

#[test]
fn test_bad_request_error() {
    let error = AppError::bad_request("Invalid input");
    assert!(matches!(error, AppError::BadRequest(_)));
    assert_eq!(error.to_string(), "Bad request: Invalid input");
}

#[test]
fn test_not_found_error() {
    let error = AppError::not_found("Session not found");
    assert!(matches!(error, AppError::NotFound(_)));
    assert_eq!(error.to_string(), "Not found: Session not found");
}

#[test]
fn test_upstream_error() {
    let error = AppError::upstream_error("Connection refused");
    assert!(matches!(error, AppError::UpstreamError(_)));
    assert_eq!(error.to_string(), "Upstream error: Connection refused");
}

#[test]
fn test_internal_error() {
    let error = AppError::internal_error("Database failure");
    assert!(matches!(error, AppError::InternalError(_)));
    assert_eq!(error.to_string(), "Internal error: Database failure");
}

#[test]
fn test_service_unavailable_error() {
    let error = AppError::service_unavailable("NATS disconnected");
    assert!(matches!(error, AppError::ServiceUnavailable(_)));
    assert_eq!(error.to_string(), "Service unavailable: NATS disconnected");
}

#[test]
fn test_error_response_serialization() {
    let response = ErrorResponse {
        error: ErrorCode::BadRequest,
        message: "Invalid session ID".to_string(),
        details: None,
    };

    let json = serde_json::to_string(&response).unwrap();
    assert!(json.contains("\"error\":\"BAD_REQUEST\""));
    assert!(json.contains("\"message\":\"Invalid session ID\""));
    assert!(!json.contains("details"));
}

#[test]
fn test_error_response_with_details() {
    let response = ErrorResponse {
        error: ErrorCode::InternalError,
        message: "Database error".to_string(),
        details: Some("Connection pool exhausted".to_string()),
    };

    let json = serde_json::to_string(&response).unwrap();
    assert!(json.contains("\"error\":\"INTERNAL_ERROR\""));
    assert!(json.contains("\"details\":\"Connection pool exhausted\""));
}

#[test]
fn test_error_code_bad_request_serialization() {
    assert_eq!(
        serde_json::to_string(&ErrorCode::BadRequest).unwrap(),
        "\"BAD_REQUEST\""
    );
}

#[test]
fn test_error_code_not_found_serialization() {
    assert_eq!(
        serde_json::to_string(&ErrorCode::NotFound).unwrap(),
        "\"NOT_FOUND\""
    );
}

#[test]
fn test_error_code_upstream_error_serialization() {
    assert_eq!(
        serde_json::to_string(&ErrorCode::UpstreamError).unwrap(),
        "\"UPSTREAM_ERROR\""
    );
}

#[test]
fn test_error_code_internal_error_serialization() {
    assert_eq!(
        serde_json::to_string(&ErrorCode::InternalError).unwrap(),
        "\"INTERNAL_ERROR\""
    );
}

#[test]
fn test_error_code_service_unavailable_serialization() {
    assert_eq!(
        serde_json::to_string(&ErrorCode::ServiceUnavailable).unwrap(),
        "\"SERVICE_UNAVAILABLE\""
    );
}

#[test]
fn test_from_anyhow_error() {
    let anyhow_err = anyhow::anyhow!("Something went wrong");
    let app_err: AppError = anyhow_err.into();

    assert!(matches!(app_err, AppError::InternalError(_)));
    assert!(app_err.to_string().contains("Something went wrong"));
}
