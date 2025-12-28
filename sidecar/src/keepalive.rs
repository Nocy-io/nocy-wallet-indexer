use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

/// Default keepalive interval (must be less than wallet-indexer TTL)
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// Manages keepalive tasks for active sessions
/// Session IDs are 32-byte SHA256 hashes, hex-encoded
#[derive(Clone)]
pub struct KeepaliveManager {
    db_pool: PgPool,
    tasks: Arc<RwLock<HashMap<String, JoinHandle<()>>>>,
}

impl KeepaliveManager {
    /// Create a new keepalive manager
    pub fn new(db_pool: PgPool) -> Self {
        Self {
            db_pool,
            tasks: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Start a keepalive task for a session (session_id is hex-encoded)
    pub async fn start(&self, session_id: &str) {
        let mut tasks = self.tasks.write().await;

        // Stop existing task if any
        if let Some(handle) = tasks.remove(session_id) {
            handle.abort();
        }

        let db_pool = self.db_pool.clone();
        let session_id_owned = session_id.to_string();
        let handle = tokio::spawn(async move {
            keepalive_loop(db_pool, session_id_owned).await;
        });

        tasks.insert(session_id.to_string(), handle);
        info!(%session_id, "Keepalive started");
    }

    /// Stop the keepalive task for a session
    pub async fn stop(&self, session_id: &str) {
        let mut tasks = self.tasks.write().await;

        if let Some(handle) = tasks.remove(session_id) {
            handle.abort();
            info!(%session_id, "Keepalive stopped");
        } else {
            debug!(%session_id, "No keepalive task to stop");
        }
    }

    /// Stop all keepalive tasks (for graceful shutdown)
    pub async fn stop_all(&self) {
        let mut tasks = self.tasks.write().await;
        let count = tasks.len();

        for (session_id, handle) in tasks.drain() {
            handle.abort();
            debug!(%session_id, "Keepalive stopped during shutdown");
        }

        info!(count, "All keepalive tasks stopped");
    }
}

/// The keepalive loop that updates wallet activity
/// Session ID is a hex-encoded 32-byte SHA256 hash, stored as BYTEA
async fn keepalive_loop(db_pool: PgPool, session_id: String) {
    // Parse hex to bytes for DB query
    let session_id_bytes = match hex::decode(&session_id) {
        Ok(bytes) => bytes,
        Err(e) => {
            error!(%session_id, error = %e, "Invalid session_id hex, stopping keepalive");
            return;
        }
    };

    loop {
        tokio::time::sleep(KEEPALIVE_INTERVAL).await;

        // Update wallets.active and wallets.last_active for this session
        let result = sqlx::query(
            r#"
            UPDATE wallets
            SET active = true, last_active = NOW()
            WHERE session_id = $1
            "#,
        )
        .bind(&session_id_bytes)
        .execute(&db_pool)
        .await;

        match result {
            Ok(res) => {
                let rows = res.rows_affected();
                if rows > 0 {
                    debug!(%session_id, rows, "Keepalive ping sent");
                } else {
                    warn!(%session_id, "Keepalive ping: no wallet found for session");
                }
            }
            Err(e) => {
                error!(%session_id, error = %e, "Keepalive ping failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that stop removes a session from the task map
    /// This verifies that disconnect stops keepalive unconditionally
    #[tokio::test]
    async fn test_stop_removes_task_from_map() {
        // Create a mock pool (we won't actually connect, just test task management)
        // Using a dummy connection string that won't be used
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy("postgres://localhost/nonexistent")
            .unwrap();

        let manager = KeepaliveManager::new(pool);
        let session_id = "a".repeat(64); // Valid 32-byte hex

        // Start a keepalive task
        manager.start(&session_id).await;

        // Verify task exists
        {
            let tasks = manager.tasks.read().await;
            assert!(tasks.contains_key(&session_id), "Task should exist after start");
        }

        // Stop the task
        manager.stop(&session_id).await;

        // Verify task is removed
        {
            let tasks = manager.tasks.read().await;
            assert!(!tasks.contains_key(&session_id), "Task should be removed after stop");
        }
    }

    /// Test that stop is idempotent (can be called multiple times safely)
    #[tokio::test]
    async fn test_stop_is_idempotent() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy("postgres://localhost/nonexistent")
            .unwrap();

        let manager = KeepaliveManager::new(pool);
        let session_id = "b".repeat(64);

        // Start and stop
        manager.start(&session_id).await;
        manager.stop(&session_id).await;

        // Stop again - should not panic
        manager.stop(&session_id).await;

        // Verify still empty
        let tasks = manager.tasks.read().await;
        assert!(!tasks.contains_key(&session_id));
    }

    /// Test that stop_all clears all tasks
    #[tokio::test]
    async fn test_stop_all_clears_all_tasks() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy("postgres://localhost/nonexistent")
            .unwrap();

        let manager = KeepaliveManager::new(pool);

        // Start multiple tasks
        let session1 = "1".repeat(64);
        let session2 = "2".repeat(64);
        let session3 = "3".repeat(64);

        manager.start(&session1).await;
        manager.start(&session2).await;
        manager.start(&session3).await;

        // Verify all exist
        {
            let tasks = manager.tasks.read().await;
            assert_eq!(tasks.len(), 3);
        }

        // Stop all
        manager.stop_all().await;

        // Verify all removed
        {
            let tasks = manager.tasks.read().await;
            assert!(tasks.is_empty(), "All tasks should be removed after stop_all");
        }
    }

    /// Test that starting a session twice replaces the old task
    #[tokio::test]
    async fn test_start_replaces_existing_task() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy("postgres://localhost/nonexistent")
            .unwrap();

        let manager = KeepaliveManager::new(pool);
        let session_id = "c".repeat(64);

        // Start twice
        manager.start(&session_id).await;
        manager.start(&session_id).await;

        // Should still only have one task
        let tasks = manager.tasks.read().await;
        assert_eq!(tasks.len(), 1);
        assert!(tasks.contains_key(&session_id));
    }
}
