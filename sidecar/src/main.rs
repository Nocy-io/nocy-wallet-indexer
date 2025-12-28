use axum::{
    extract::State,
    http::{HeaderName, Request, StatusCode},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use serde::Serialize;
use sqlx::{postgres::PgPoolOptions, PgPool};
use std::{sync::Arc, time::Duration};
use tokio::signal;
use tower_http::{
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    trace::TraceLayer,
};
use tracing::{info, info_span, warn, Span};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use nocy_wallet_feed::config::{Config, SERVER_SECRET_BYTES};
use nocy_wallet_feed::graphql_ws::SubscriptionManager;
use nocy_wallet_feed::keepalive::KeepaliveManager;
use nocy_wallet_feed::merkle_cache::MerkleCacheManager;
use nocy_wallet_feed::metrics;
use nocy_wallet_feed::routes::feed::get_feed;
use nocy_wallet_feed::routes::session::{session_routes, SessionState};
use nocy_wallet_feed::routes::subscribe::subscribe_feed;
use nocy_wallet_feed::routes::zswap::{zswap_routes, ZswapState};
use nocy_wallet_feed::schema;
use nocy_wallet_feed::snapshot::SnapshotClient;
use nocy_wallet_feed::upstream::UpstreamClient;

const X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db_pool: PgPool,
    pub nats_client: async_nats::Client,
    pub upstream_client: UpstreamClient,
    pub keepalive_manager: KeepaliveManager,
    pub snapshot_client: Arc<SnapshotClient>,
    pub subscription_manager: Arc<SubscriptionManager>,
    pub merkle_cache_manager: Option<Arc<MerkleCacheManager>>,
    pub metrics_handle: metrics_exporter_prometheus::PrometheusHandle,
    /// Server secret for deterministic session ID generation.
    /// This is initialized once at startup and reused for all sessions.
    pub server_secret: [u8; SERVER_SECRET_BYTES],
}

// Allow extracting SessionState from AppState
impl axum::extract::FromRef<AppState> for SessionState {
    fn from_ref(app_state: &AppState) -> Self {
        SessionState {
            db_pool: app_state.db_pool.clone(),
            upstream_client: app_state.upstream_client.clone(),
            keepalive_manager: app_state.keepalive_manager.clone(),
            subscription_manager: app_state.subscription_manager.clone(),
            server_secret: app_state.server_secret,
            limit_blocks_max: app_state.config.limit_blocks_max,
            heartbeat_interval: app_state.config.heartbeat_interval(),
            poll_interval: app_state.config.poll_interval(),
            sse_send_timeout: app_state.config.sse_send_timeout(),
            merkle_cache_manager: app_state.merkle_cache_manager.clone(),
        }
    }
}

// Allow extracting ZswapState from AppState
impl axum::extract::FromRef<AppState> for ZswapState {
    fn from_ref(app_state: &AppState) -> Self {
        ZswapState {
            db_pool: app_state.db_pool.clone(),
            snapshot_client: app_state.snapshot_client.clone(),
            subscription_manager: Some(app_state.subscription_manager.clone()),
            merkle_cache_manager: app_state.merkle_cache_manager.clone(),
        }
    }
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

#[derive(Serialize)]
struct ReadyResponse {
    status: &'static str,
    checks: ReadinessChecks,
}

#[derive(Serialize)]
struct ReadinessChecks {
    postgres: bool,
    nats: bool,
}

async fn healthz() -> impl IntoResponse {
    Json(HealthResponse { status: "ok" })
}

async fn metrics_endpoint(State(state): State<AppState>) -> impl IntoResponse {
    state.metrics_handle.render()
}

async fn readyz(State(state): State<AppState>) -> impl IntoResponse {
    // Check Postgres connectivity
    let postgres_ok = sqlx::query("SELECT 1")
        .fetch_one(&state.db_pool)
        .await
        .is_ok();

    if !postgres_ok {
        warn!("Postgres readiness check failed");
    }

    // Check NATS connectivity (client connection state)
    let nats_ok = state.nats_client.connection_state()
        == async_nats::connection::State::Connected;

    if !nats_ok {
        warn!("NATS readiness check failed");
    }

    let checks = ReadinessChecks {
        postgres: postgres_ok,
        nats: nats_ok,
    };

    let all_ready = checks.postgres && checks.nats;

    let status_code = if all_ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (
        status_code,
        Json(ReadyResponse {
            status: if all_ready { "ready" } else { "not_ready" },
            checks,
        }),
    )
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"))
        .add_directive("hyper=warn".parse().unwrap())
        .add_directive("tower_http=debug".parse().unwrap());

    tracing_subscriber::registry()
        .with(filter)
        .with(
            fmt::layer()
                .with_target(true)
                .with_level(true)
                .with_thread_ids(false)
                .json(),
        )
        .init();
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            info!("Received Ctrl+C, starting graceful shutdown");
        }
        _ = terminate => {
            info!("Received SIGTERM, starting graceful shutdown");
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    init_logging();

    let config = Config::from_env()?;
    let addr = config.socket_addr();

    // Initialize metrics
    info!("Initializing metrics...");
    let metrics_handle = metrics::init_metrics();
    info!("Metrics initialized");

    // Initialize server secret for session ID generation
    info!("Initializing server secret...");
    let (server_secret, was_generated) = config
        .server_secret_bytes()
        .map_err(|e| anyhow::anyhow!("Server secret configuration error: {}", e))?;

    if was_generated {
        warn!(
            "SERVER_SECRET not configured - generated random secret. \
             Sessions will not persist across restarts. \
             Set SERVER_SECRET env var for persistent sessions."
        );
    } else {
        info!("Using configured SERVER_SECRET for session ID generation");
    }

    info!("Starting nocy-wallet-feed service");
    info!(host = %config.server_host, port = %config.server_port, "Server configuration");
    info!(upstream_indexer_url = %config.upstream_indexer_url, "Upstream indexer");
    info!(nats_url = %config.nats_url, "NATS server");
    info!(limit_blocks_max = %config.limit_blocks_max, "Feed limits");

    // Initialize Postgres connection pool
    info!("Connecting to Postgres...");
    let db_pool = PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(config.db_acquire_timeout())
        .connect(&config.database_url)
        .await?;
    info!("Postgres connection pool established");

    // Ensure sidecar schema/tables exist without touching the shared `_sqlx_migrations` table.
    info!("Ensuring sidecar schema...");
    schema::ensure_schema(&db_pool).await?;
    info!("Sidecar schema ready");

    // Initialize NATS client.
    // Note: async-nats does not use username/password embedded in the URL for auth;
    // credentials must be provided via ConnectOptions.
    info!("Connecting to NATS...");
    let nats_server: async_nats::ServerAddr = config
        .nats_url
        .parse()
        .map_err(|e| anyhow::anyhow!("Invalid NATS_URL: {}", e))?;
    let mut nats_options = async_nats::ConnectOptions::new();
    if let (Some(user), Some(pass)) = (nats_server.username(), nats_server.password()) {
        nats_options = nats_options.user_and_password(user.to_string(), pass.to_string());
    }
    let nats_client = nats_options.connect(nats_server).await?;
    info!("NATS client connected");

    // Initialize upstream indexer client
    info!("Initializing upstream indexer client...");
    let upstream_client =
        UpstreamClient::new(config.upstream_indexer_url.clone(), config.upstream_timeout())?;
    info!("Upstream indexer client initialized");

    // Create keepalive manager
    let keepalive_manager = KeepaliveManager::new(db_pool.clone());

    // Create snapshot client for ledger state
    info!("Initializing snapshot client...");
    let snapshot_client = Arc::new(SnapshotClient::new(nats_client.clone()));

    // Start background subscriber to keep snapshot cache warm
    Arc::clone(&snapshot_client).start_background_subscriber().await;
    info!("Snapshot client initialized with background subscriber");

    // Create subscription manager for WebSocket subscriptions to upstream indexer
    info!("Initializing subscription manager for merkle updates...");
    let subscription_manager = Arc::new(SubscriptionManager::new(&config.upstream_indexer_url));
    info!("Subscription manager initialized");

    // Create merkle cache manager if enabled
    let merkle_cache_manager = if config.merkle_cache_enabled {
        info!("Initializing merkle cache manager...");
        let manager = Arc::new(MerkleCacheManager::new(
            db_pool.clone(),
            upstream_client.clone(),
            &config.upstream_indexer_url,
        ));
        // Start background subscription
        Arc::clone(&manager).start();
        info!("Merkle cache manager initialized with background subscription");
        Some(manager)
    } else {
        info!("Merkle cache disabled by configuration");
        None
    };

    // Create unified app state (SessionState is derived via FromRef)
    let state = AppState {
        config: Arc::new(config),
        db_pool,
        nats_client,
        upstream_client,
        keepalive_manager: keepalive_manager.clone(),
        snapshot_client,
        subscription_manager,
        merkle_cache_manager: merkle_cache_manager.clone(),
        metrics_handle,
        server_secret,
    };

    // Build unified router with AppState
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics_endpoint))
        .route("/v1/feed", get(get_feed))
        .route("/v1/feed/subscribe", get(subscribe_feed))
        .nest("/v1/session", session_routes())
        .nest("/v1/zswap", zswap_routes())
        .with_state(state)
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|request: &Request<_>| {
                    let request_id = request
                        .headers()
                        .get(&X_REQUEST_ID)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("unknown");

                    info_span!(
                        "http_request",
                        request_id = %request_id,
                        method = %request.method(),
                        uri = %request.uri(),
                        version = ?request.version(),
                    )
                })
                .on_response(|response: &axum::http::Response<_>, latency: Duration, _span: &Span| {
                    tracing::info!(
                        status = %response.status().as_u16(),
                        latency_ms = %latency.as_millis(),
                        "response"
                    );
                })
                .on_failure(|error: tower_http::classify::ServerErrorsFailureClass, latency: Duration, _span: &Span| {
                    tracing::error!(
                        error = %error,
                        latency_ms = %latency.as_millis(),
                        "request failed"
                    );
                }),
        )
        .layer(PropagateRequestIdLayer::new(X_REQUEST_ID))
        .layer(SetRequestIdLayer::new(X_REQUEST_ID, MakeRequestUuid));

    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(addr = %addr, "Listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    // Stop all keepalive tasks on shutdown
    info!("Stopping keepalive tasks...");
    keepalive_manager.stop_all().await;

    // Stop merkle cache subscription on shutdown
    if let Some(ref cache) = merkle_cache_manager {
        info!("Stopping merkle cache subscription...");
        cache.shutdown();
    }

    info!("Shutdown complete");
    Ok(())
}
