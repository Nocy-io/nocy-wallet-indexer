//! Metrics instrumentation for the sidecar service
//!
//! Provides Prometheus-compatible metrics for:
//! - DB query latency
//! - Feed assembly time
//! - SSE client count

use metrics::{counter, gauge, histogram};
use std::time::Instant;

/// Metric names as constants for consistency
pub mod names {
    pub const DB_QUERY_DURATION: &str = "db_query_duration_seconds";
    pub const FEED_ASSEMBLY_DURATION: &str = "feed_assembly_duration_seconds";
    pub const SSE_CLIENTS_ACTIVE: &str = "sse_clients_active";
    pub const FEED_BLOCKS_RETURNED: &str = "feed_blocks_returned_total";
    pub const REORG_DETECTED: &str = "reorg_detected_total";
    pub const UPSTREAM_REQUESTS: &str = "upstream_requests_total";
    pub const EVENT_ORDERING_ANOMALIES: &str = "event_ordering_anomalies_total";
}

/// Record a DB query duration
pub fn record_db_query(query_name: &'static str, duration: std::time::Duration) {
    histogram!(names::DB_QUERY_DURATION, "query" => query_name).record(duration.as_secs_f64());
}

/// Record feed assembly duration
pub fn record_feed_assembly(duration: std::time::Duration) {
    histogram!(names::FEED_ASSEMBLY_DURATION).record(duration.as_secs_f64());
}

/// Increment active SSE client count
pub fn sse_client_connected() {
    gauge!(names::SSE_CLIENTS_ACTIVE).increment(1.0);
}

/// Decrement active SSE client count
pub fn sse_client_disconnected() {
    gauge!(names::SSE_CLIENTS_ACTIVE).decrement(1.0);
}

/// Record blocks returned in a feed response
pub fn record_blocks_returned(count: u64) {
    counter!(names::FEED_BLOCKS_RETURNED).increment(count);
}

/// Record a reorg detection
pub fn record_reorg_detected(reorg_type: &'static str) {
    counter!(names::REORG_DETECTED, "type" => reorg_type).increment(1);
}

/// Record an upstream request
pub fn record_upstream_request(endpoint: &'static str, success: bool) {
    counter!(
        names::UPSTREAM_REQUESTS,
        "endpoint" => endpoint,
        "success" => if success { "true" } else { "false" }
    )
    .increment(1);
}

/// Record an event ordering anomaly (gap in ledger_event_id sequence)
pub fn record_event_ordering_anomaly(anomaly_type: &'static str) {
    counter!(
        names::EVENT_ORDERING_ANOMALIES,
        "type" => anomaly_type
    )
    .increment(1);
}

/// Helper struct for timing operations
pub struct Timer {
    start: Instant,
}

impl Timer {
    /// Start a new timer
    pub fn start() -> Self {
        Self {
            start: Instant::now(),
        }
    }

    /// Get elapsed duration
    pub fn elapsed(&self) -> std::time::Duration {
        self.start.elapsed()
    }
}

/// Initialize the Prometheus metrics exporter
/// Returns a handle to the metrics endpoint
pub fn init_metrics() -> metrics_exporter_prometheus::PrometheusHandle {
    let builder = metrics_exporter_prometheus::PrometheusBuilder::new();
    builder
        .install_recorder()
        .expect("Failed to install Prometheus recorder")
}
