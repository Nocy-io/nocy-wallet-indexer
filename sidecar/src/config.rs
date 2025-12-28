use rand::Rng;
use serde::Deserialize;
use std::net::SocketAddr;
use std::time::Duration;

/// Server secret size in bytes (256-bit)
pub const SERVER_SECRET_BYTES: usize = 32;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// PostgreSQL connection string
    pub database_url: String,

    #[serde(default = "default_host")]
    pub server_host: String,

    #[serde(default = "default_port")]
    pub server_port: u16,

    /// URL of the upstream wallet-indexer service
    pub upstream_indexer_url: String,

    /// NATS server URL for ledger_state snapshots and BlockIndexed notifications
    pub nats_url: String,

    /// Server secret for session ID generation (hex-encoded 32 bytes).
    /// If not provided, a random secret is generated on startup.
    ///
    /// SECURITY: This secret MUST be kept private. It's used to derive session IDs
    /// from wallet credentials in a way that's deterministic per-server but not
    /// correlatable across servers.
    ///
    /// For persistent session IDs across restarts, set SERVER_SECRET env var.
    /// For ephemeral sessions (cleared on restart), leave unset.
    #[serde(default)]
    pub server_secret: Option<String>,

    /// Maximum number of blocks to return in a single feed request
    #[serde(default = "default_limit_blocks_max")]
    pub limit_blocks_max: u32,

    /// Timeout for upstream indexer requests in seconds
    #[serde(default = "default_upstream_timeout_secs")]
    pub upstream_timeout_secs: u64,

    /// SSE heartbeat interval in seconds
    #[serde(default = "default_heartbeat_interval_secs")]
    pub heartbeat_interval_secs: u64,

    /// SSE poll interval in seconds (how often to check for new blocks)
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: u64,

    /// Timeout for sending SSE events to slow clients in seconds
    #[serde(default = "default_sse_send_timeout_secs")]
    pub sse_send_timeout_secs: u64,

    /// Database connection acquire timeout in seconds
    #[serde(default = "default_db_acquire_timeout_secs")]
    pub db_acquire_timeout_secs: u64,

    /// Enable background merkle cache subscription
    /// When enabled, the sidecar maintains a persistent subscription to cache
    /// all merkle tree collapsed updates for serving historical requests
    #[serde(default = "default_merkle_cache_enabled")]
    pub merkle_cache_enabled: bool,
}

fn default_host() -> String {
    "0.0.0.0".to_string()
}

fn default_port() -> u16 {
    8080
}

fn default_limit_blocks_max() -> u32 {
    100
}

fn default_upstream_timeout_secs() -> u64 {
    30
}

fn default_heartbeat_interval_secs() -> u64 {
    15
}

fn default_poll_interval_secs() -> u64 {
    1
}

fn default_sse_send_timeout_secs() -> u64 {
    5
}

fn default_db_acquire_timeout_secs() -> u64 {
    5
}

fn default_merkle_cache_enabled() -> bool {
    true // Enabled by default for production use
}

impl Config {
    pub fn from_env() -> Result<Self, config::ConfigError> {
        let cfg = config::Config::builder()
            // Use double-underscore for nesting so single underscores remain part of the key,
            // allowing env vars like DATABASE_URL / SERVER_HOST to map to snake_case fields.
            .add_source(config::Environment::default().separator("__"))
            .build()?;

        cfg.try_deserialize()
    }

    pub fn socket_addr(&self) -> SocketAddr {
        format!("{}:{}", self.server_host, self.server_port)
            .parse()
            .expect("Invalid server address")
    }

    /// Get upstream timeout as Duration
    pub fn upstream_timeout(&self) -> Duration {
        Duration::from_secs(self.upstream_timeout_secs)
    }

    /// Get SSE heartbeat interval as Duration
    pub fn heartbeat_interval(&self) -> Duration {
        Duration::from_secs(self.heartbeat_interval_secs)
    }

    /// Get SSE poll interval as Duration
    pub fn poll_interval(&self) -> Duration {
        Duration::from_secs(self.poll_interval_secs)
    }

    /// Get SSE send timeout as Duration
    pub fn sse_send_timeout(&self) -> Duration {
        Duration::from_secs(self.sse_send_timeout_secs)
    }

    /// Get database acquire timeout as Duration
    pub fn db_acquire_timeout(&self) -> Duration {
        Duration::from_secs(self.db_acquire_timeout_secs)
    }

    /// Get server secret as bytes, parsing from hex if configured or generating random.
    /// Returns (secret_bytes, was_generated) where was_generated indicates if a new
    /// random secret was created (useful for logging).
    ///
    /// SECURITY: The returned secret is used for session ID derivation.
    /// If was_generated is true, sessions will not persist across restarts.
    pub fn server_secret_bytes(&self) -> Result<([u8; SERVER_SECRET_BYTES], bool), String> {
        match &self.server_secret {
            Some(hex_secret) => {
                let bytes = hex::decode(hex_secret)
                    .map_err(|e| format!("SERVER_SECRET must be valid hex: {}", e))?;

                if bytes.len() != SERVER_SECRET_BYTES {
                    return Err(format!(
                        "SERVER_SECRET must be {} bytes ({} hex chars), got {} bytes",
                        SERVER_SECRET_BYTES,
                        SERVER_SECRET_BYTES * 2,
                        bytes.len()
                    ));
                }

                let mut arr = [0u8; SERVER_SECRET_BYTES];
                arr.copy_from_slice(&bytes);
                Ok((arr, false))
            }
            None => {
                let mut rng = rand::thread_rng();
                let mut secret = [0u8; SERVER_SECRET_BYTES];
                rng.fill(&mut secret);
                Ok((secret, true))
            }
        }
    }
}
