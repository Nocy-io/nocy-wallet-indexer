use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::{debug, error, instrument};

use crate::error::AppError;

/// Client for communicating with the upstream indexer API (GraphQL).
#[derive(Clone)]
pub struct UpstreamClient {
    client: Client,
    base_url: String,
}

const CONNECT_MUTATION: &str = r#"
mutation($viewingKey: ViewingKey!) {
  connect(viewingKey: $viewingKey)
}
"#;

const DISCONNECT_MUTATION: &str = r#"
mutation($sessionId: HexEncoded!) {
  disconnect(sessionId: $sessionId)
}
"#;

#[derive(Debug, Serialize)]
struct GraphQLRequest<V> {
    query: &'static str,
    variables: V,
}

#[derive(Debug, Deserialize)]
struct GraphQLResponse<D> {
    data: Option<D>,
    errors: Option<Vec<GraphQLError>>,
}

#[derive(Debug, Deserialize)]
struct GraphQLError {
    message: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectVariables<'a> {
    viewing_key: &'a str,
}

#[derive(Debug, Deserialize)]
struct ConnectData {
    connect: String,
}

/// Response from upstream connect endpoint
/// Session ID is a 32-byte SHA256 hash, hex-encoded
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectResponse {
    pub session_id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DisconnectVariables<'a> {
    session_id: &'a str,
}

#[derive(Debug, Deserialize)]
struct DisconnectData {
    #[allow(dead_code)]
    disconnect: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct DisconnectResponse {
    pub success: bool,
}

impl UpstreamClient {
    /// Create a new upstream client with the given base URL and timeout
    pub fn new(base_url: String, timeout: Duration) -> Result<Self, AppError> {
        let client = Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| AppError::InternalError(format!("Failed to create HTTP client: {}", e)))?;

        Ok(Self { client, base_url })
    }

    fn graphql_endpoint(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        // Prefer the indexer-api v3 GraphQL endpoint, but allow callers to pass
        // an explicit GraphQL endpoint (e.g. ".../api/v3/graphql" or ".../graphql").
        if base.ends_with("/graphql") {
            base.to_string()
        } else if base.ends_with("/api/v3") {
            format!("{}/graphql", base)
        } else {
            format!("{}/api/v3/graphql", base)
        }
    }

    /// Connect to the upstream indexer with a viewing key
    /// Returns the session ID on success
    #[instrument(skip(self, viewing_key), fields(base_url = %self.base_url))]
    pub async fn connect(&self, viewing_key: &str) -> Result<ConnectResponse, AppError> {
        let url = self.graphql_endpoint();

        debug!("Connecting to upstream indexer");

        let response = self
            .client
            .post(&url)
            .json(&GraphQLRequest {
                query: CONNECT_MUTATION,
                variables: ConnectVariables { viewing_key },
            })
            .send()
            .await
            .map_err(|e| {
                error!(error = %e, "Failed to connect to upstream indexer");
                AppError::UpstreamError(format!("Connection failed: {}", e))
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            error!(status = %status, body = %body, "Upstream connect returned error");
            return Err(AppError::UpstreamError(format!(
                "Upstream returned {}: {}",
                status, body
            )));
        }

        let gql: GraphQLResponse<ConnectData> = response.json().await.map_err(|e| {
            error!(error = %e, "Failed to parse upstream connect response");
            AppError::UpstreamError(format!("Invalid response: {}", e))
        })?;

        if let Some(errors) = gql.errors {
            let msg = errors
                .into_iter()
                .map(|e| e.message)
                .collect::<Vec<_>>()
                .join("; ");
            return Err(AppError::UpstreamError(format!("Upstream GraphQL error: {}", msg)));
        }

        let data = gql
            .data
            .ok_or_else(|| AppError::UpstreamError("Upstream GraphQL response missing data".into()))?;

        Ok(ConnectResponse {
            session_id: data.connect,
        })
    }

    /// Disconnect a session from the upstream indexer
    #[instrument(skip(self), fields(base_url = %self.base_url))]
    pub async fn disconnect(&self, session_id: &str) -> Result<DisconnectResponse, AppError> {
        let url = self.graphql_endpoint();

        debug!(%session_id, "Disconnecting from upstream indexer");

        let response = self
            .client
            .post(&url)
            .json(&GraphQLRequest {
                query: DISCONNECT_MUTATION,
                variables: DisconnectVariables { session_id },
            })
            .send()
            .await
            .map_err(|e| {
                error!(error = %e, "Failed to disconnect from upstream indexer");
                AppError::UpstreamError(format!("Disconnect failed: {}", e))
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            error!(status = %status, body = %body, "Upstream disconnect returned error");
            return Err(AppError::UpstreamError(format!(
                "Upstream returned {}: {}",
                status, body
            )));
        }

        let gql: GraphQLResponse<DisconnectData> = response.json().await.map_err(|e| {
            error!(error = %e, "Failed to parse upstream disconnect response");
            AppError::UpstreamError(format!("Invalid response: {}", e))
        })?;

        if let Some(errors) = gql.errors {
            let msg = errors
                .into_iter()
                .map(|e| e.message)
                .collect::<Vec<_>>()
                .join("; ");
            return Err(AppError::UpstreamError(format!("Upstream GraphQL error: {}", msg)));
        }

        if gql.data.is_none() {
            return Err(AppError::UpstreamError(
                "Upstream GraphQL response missing data".into(),
            ));
        }

        Ok(DisconnectResponse { success: true })
    }
}
