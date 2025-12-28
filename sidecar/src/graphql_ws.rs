//! GraphQL WebSocket subscription client for upstream indexer
//!
//! Implements the graphql-transport-ws protocol to subscribe to shieldedTransactions
//! and receive collapsed merkle tree updates.

use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::timeout;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        client::IntoClientRequest,
        http::{header::SEC_WEBSOCKET_PROTOCOL, HeaderValue},
        protocol::Message,
    },
};
use tracing::{debug, warn};

use crate::error::AppError;

/// Collapsed merkle tree update data
#[derive(Debug, Clone)]
pub struct CollapsedMerkleUpdate {
    pub start_index: i64,
    pub end_index: i64,
    pub update: String, // Hex-encoded collapsed update bytes
    pub protocol_version: u32,
}

/// Cache for storing collapsed merkle updates per session
#[derive(Debug, Default)]
pub struct MerkleUpdateCache {
    /// Map of session_id -> list of collapsed updates
    updates: RwLock<HashMap<String, Vec<CollapsedMerkleUpdate>>>,
}

impl MerkleUpdateCache {
    pub fn new() -> Self {
        Self {
            updates: RwLock::new(HashMap::new()),
        }
    }

    /// Add a collapsed merkle update for a session
    pub async fn add_update(&self, session_id: &str, update: CollapsedMerkleUpdate) {
        let mut cache = self.updates.write().await;
        cache
            .entry(session_id.to_string())
            .or_default()
            .push(update);
    }

    /// Get and remove collapsed updates for a range
    /// Returns updates that cover indices from `from_index` to `to_index`
    pub async fn get_updates_for_range(
        &self,
        session_id: &str,
        from_index: i64,
        to_index: i64,
    ) -> Vec<CollapsedMerkleUpdate> {
        let cache = self.updates.read().await;
        if let Some(updates) = cache.get(session_id) {
            updates
                .iter()
                .filter(|u| u.start_index >= from_index && u.end_index <= to_index)
                .cloned()
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Clear all updates for a session
    pub async fn clear_session(&self, session_id: &str) {
        let mut cache = self.updates.write().await;
        cache.remove(session_id);
    }
}

// === GraphQL WebSocket Protocol Types ===

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum ClientMessage {
    #[serde(rename = "connection_init")]
    ConnectionInit { payload: serde_json::Value },
    #[serde(rename = "subscribe")]
    Subscribe { id: String, payload: SubscribePayload },
    #[serde(rename = "complete")]
    Complete { id: String },
}

#[derive(Debug, Serialize)]
struct SubscribePayload {
    query: String,
    variables: serde_json::Value,
}

#[allow(dead_code)] // Fields used by serde
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ServerMessage {
    #[serde(rename = "connection_ack")]
    ConnectionAck,
    #[serde(rename = "next")]
    Next { id: String, payload: NextPayload },
    #[serde(rename = "error")]
    Error { id: String, payload: Vec<GraphQLError> },
    #[serde(rename = "complete")]
    Complete { id: String },
    #[serde(rename = "ping")]
    Ping,
    #[serde(rename = "pong")]
    Pong,
}

#[allow(dead_code)] // Fields used by serde
#[derive(Debug, Deserialize)]
struct NextPayload {
    data: Option<ShieldedTransactionsData>,
    errors: Option<Vec<GraphQLError>>,
}

#[derive(Debug, Deserialize)]
struct GraphQLError {
    message: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShieldedTransactionsData {
    shielded_transactions: ShieldedTransactionsEvent,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "__typename")]
enum ShieldedTransactionsEvent {
    RelevantTransaction(RelevantTransaction),
    ShieldedTransactionsProgress(ShieldedTransactionsProgress),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RelevantTransaction {
    transaction: RegularTransactionRef,
    collapsed_merkle_tree: Option<CollapsedMerkleTree>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RegularTransactionRef {
    id: i64,
    hash: String,
    start_index: i64,
    end_index: i64,
    protocol_version: i32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CollapsedMerkleTree {
    start_index: i64,
    end_index: i64,
    update: String,
    protocol_version: i32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShieldedTransactionsProgress {
    highest_end_index: i64,
    highest_checked_end_index: i64,
    highest_relevant_end_index: i64,
}

/// Subscription manager for fetching collapsed merkle updates
pub struct SubscriptionManager {
    ws_url: String,
    cache: Arc<MerkleUpdateCache>,
}

impl SubscriptionManager {
    pub fn new(upstream_indexer_url: &str) -> Self {
        // Convert HTTP URL to WebSocket URL
        let ws_url = upstream_indexer_url
            .replace("https://", "wss://")
            .replace("http://", "ws://")
            .trim_end_matches('/')
            .to_string()
            + "/api/v3/graphql/ws";

        Self {
            ws_url,
            cache: Arc::new(MerkleUpdateCache::new()),
        }
    }

    /// Get the cache for external access
    pub fn cache(&self) -> Arc<MerkleUpdateCache> {
        self.cache.clone()
    }

    /// Fetch collapsed merkle updates for a session starting at a given index
    /// This opens a temporary subscription, collects updates until we reach the target,
    /// and then closes the subscription.
    pub async fn fetch_merkle_updates(
        &self,
        session_id: &str,
        from_index: i64,
        to_index: i64,
    ) -> Result<Vec<CollapsedMerkleUpdate>, AppError> {
        if from_index >= to_index {
            return Ok(Vec::new());
        }

        // Collapsed merkle updates use inclusive indices (start/end are commitment indices),
        // while callers pass `to_index` as an exclusive "next free index".
        let required_end_index = to_index - 1;

        // Connect to WebSocket.
        //
        // The upstream indexer uses the graphql-transport-ws subprotocol; without it
        // the handshake is rejected (HTTP 400).
        let mut request = self.ws_url.clone().into_client_request().map_err(|e| {
            AppError::UpstreamError(format!("Invalid WebSocket URL '{}': {}", self.ws_url, e))
        })?;
        request.headers_mut().insert(
            SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("graphql-transport-ws"),
        );

        let (ws_stream, _) = connect_async(request)
            .await
            .map_err(|e| AppError::UpstreamError(format!("WebSocket connection failed: {}", e)))?;

        let (mut write, mut read) = ws_stream.split();

        // Send connection_init
        let init_msg = ClientMessage::ConnectionInit {
            payload: serde_json::json!({}),
        };
        write
            .send(Message::Text(serde_json::to_string(&init_msg).unwrap()))
            .await
            .map_err(|e| AppError::UpstreamError(format!("Failed to send init: {}", e)))?;

        // Wait for connection_ack with timeout
        let ack_timeout = Duration::from_secs(10);
        let ack_msg = timeout(ack_timeout, read.next())
            .await
            .map_err(|_| AppError::UpstreamError("Connection ack timeout".into()))?
            .ok_or_else(|| AppError::UpstreamError("Connection closed before ack".into()))?
            .map_err(|e| AppError::UpstreamError(format!("WebSocket read error: {}", e)))?;

        if let Message::Text(text) = ack_msg {
            let msg: ServerMessage = serde_json::from_str(&text)
                .map_err(|e| AppError::UpstreamError(format!("Failed to parse ack: {}", e)))?;
            match msg {
                ServerMessage::ConnectionAck => {
                    debug!("WebSocket connection acknowledged");
                }
                _ => {
                    return Err(AppError::UpstreamError(
                        "Expected connection_ack".into(),
                    ));
                }
            }
        }

        // Subscribe to shieldedTransactions
        let subscription_id = "merkle-fetch-1";
        let subscribe_msg = ClientMessage::Subscribe {
            id: subscription_id.to_string(),
            payload: SubscribePayload {
                query: SHIELDED_TRANSACTIONS_SUBSCRIPTION.to_string(),
                variables: serde_json::json!({
                    "sessionId": session_id,
                    "index": from_index
                }),
            },
        };
        write
            .send(Message::Text(serde_json::to_string(&subscribe_msg).unwrap()))
            .await
            .map_err(|e| AppError::UpstreamError(format!("Failed to send subscribe: {}", e)))?;

        // Collect updates
        let collect_timeout = Duration::from_secs(30);

        let result: Result<Vec<CollapsedMerkleUpdate>, AppError> = timeout(collect_timeout, async {
            while let Some(msg_result) = read.next().await {
                match msg_result {
                    Ok(Message::Text(text)) => {
                        if let Ok(server_msg) = serde_json::from_str::<ServerMessage>(&text) {
                            match server_msg {
                                ServerMessage::Next { id: _, payload } => {
                                    if let Some(errors) = payload.errors {
                                        let msg = errors
                                            .into_iter()
                                            .map(|e| e.message)
                                            .collect::<Vec<_>>()
                                            .join("; ");
                                        return Err(AppError::UpstreamError(format!(
                                            "GraphQL subscription error: {}",
                                            msg
                                        )));
                                    }

                                    if let Some(data) = payload.data {
                                        match data.shielded_transactions {
                                            ShieldedTransactionsEvent::RelevantTransaction(rt) => {
                                                // We expect the upstream to return the next relevant transaction
                                                // at `to_index` (start index) and include a collapsed merkle tree
                                                // update that bridges `[from_index, to_index)`.
                                                let tx_start_index = rt.transaction.start_index;

                                                if tx_start_index != to_index {
                                                    debug!(
                                                        from_index,
                                                        to_index,
                                                        tx_start_index,
                                                        "No collapsed merkle update available for requested range (next relevant tx start index mismatch)"
                                                    );
                                                    return Ok(Vec::new());
                                                }

                                                let Some(tree) = rt.collapsed_merkle_tree else {
                                                    // No gap / no update.
                                                    return Ok(Vec::new());
                                                };

                                                if tree.start_index != from_index {
                                                    debug!(
                                                        from_index,
                                                        to_index,
                                                        tree_start_index = tree.start_index,
                                                        "No collapsed merkle update available for requested range (start index mismatch)"
                                                    );
                                                    return Ok(Vec::new());
                                                }

                                                if tree.end_index != required_end_index {
                                                    debug!(
                                                        from_index,
                                                        to_index,
                                                        required_end_index,
                                                        tree_end_index = tree.end_index,
                                                        "No collapsed merkle update available for requested range (end index mismatch)"
                                                    );
                                                    return Ok(Vec::new());
                                                }

                                                return Ok(vec![CollapsedMerkleUpdate {
                                                    start_index: tree.start_index,
                                                    end_index: tree.end_index,
                                                    update: tree.update,
                                                    protocol_version: tree.protocol_version as u32,
                                                }]);
                                            }
                                            ShieldedTransactionsEvent::ShieldedTransactionsProgress(progress) => {
                                                debug!(
                                                    highest_end_index = progress.highest_end_index,
                                                    highest_checked_end_index =
                                                        progress.highest_checked_end_index,
                                                    highest_relevant_end_index =
                                                        progress.highest_relevant_end_index,
                                                    "Shielded transactions progress"
                                                );
                                            }
                                        }
                                    }
                                }
                                ServerMessage::Complete { .. } => {
                                    return Ok(Vec::new());
                                }
                                ServerMessage::Error { payload, .. } => {
                                    let msg = payload
                                        .into_iter()
                                        .map(|e| e.message)
                                        .collect::<Vec<_>>()
                                        .join("; ");
                                    return Err(AppError::UpstreamError(format!(
                                        "GraphQL subscription error: {}",
                                        msg
                                    )));
                                }
                                ServerMessage::Ping => {
                                    // Respond with pong
                                    let _ = write
                                        .send(Message::Text(
                                            serde_json::to_string(&serde_json::json!({"type": "pong"}))
                                                .unwrap(),
                                        ))
                                        .await;
                                }
                                _ => {}
                            }
                        }
                    }
                    Ok(Message::Close(_)) => {
                        return Ok(Vec::new());
                    }
                    Err(e) => {
                        return Err(AppError::UpstreamError(format!("WebSocket error: {}", e)));
                    }
                    _ => {}
                }
            }
            Ok(Vec::new())
        })
        .await
        .unwrap_or_else(|_| {
            // Timeout - return what we have (none) so callers can recover/retry.
            warn!(
                from_index,
                to_index,
                "Merkle update fetch timed out"
            );
            Ok(Vec::new())
        });

        // Send complete message to close subscription
        let complete_msg = ClientMessage::Complete {
            id: subscription_id.to_string(),
        };
        let _ = write
            .send(Message::Text(serde_json::to_string(&complete_msg).unwrap()))
            .await;

        result
    }
}

const SHIELDED_TRANSACTIONS_SUBSCRIPTION: &str = r#"
subscription ShieldedTransactions($sessionId: HexEncoded!, $index: Int) {
  shieldedTransactions(sessionId: $sessionId, index: $index) {
    __typename
    ... on RelevantTransaction {
      transaction {
        id
        hash
        startIndex
        endIndex
        protocolVersion
      }
      collapsedMerkleTree {
        startIndex
        endIndex
        update
        protocolVersion
      }
    }
    ... on ShieldedTransactionsProgress {
      highestEndIndex
      highestCheckedEndIndex
      highestRelevantEndIndex
    }
  }
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ws_url_conversion() {
        let manager = SubscriptionManager::new("http://localhost:8080");
        assert_eq!(manager.ws_url, "ws://localhost:8080/api/v3/graphql/ws");

        let manager = SubscriptionManager::new("https://indexer.example.com");
        assert_eq!(
            manager.ws_url,
            "wss://indexer.example.com/api/v3/graphql/ws"
        );

        let manager = SubscriptionManager::new("http://localhost:8080/");
        assert_eq!(manager.ws_url, "ws://localhost:8080/api/v3/graphql/ws");
    }
}
