use metrics::gauge;
use metrics_exporter_prometheus::PrometheusBuilder;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::sleep;
use tracing::{Level, info};
use tracing_subscriber::FmtSubscriber;

use crate::telemetry::{UnixMillis, unix_now_ms};

/// Shared telemetry state populated by the main loop.
/// Freshness is computed at scrape/export time from [`Self::telemetry_acquired_at`].
#[derive(Debug, Clone, Default)]
pub struct RelayMetrics {
    pub telemetry_acquired_at: Option<UnixMillis>,
}

/// Age of the last sample in seconds. `None` acquired_at is 0 (no sample yet).
#[must_use]
pub fn freshness_seconds(acquired_at: Option<UnixMillis>, now: UnixMillis) -> f64 {
    acquired_at
        .map(|ts| now.saturating_sub(ts) as f64 / 1000.0)
        .unwrap_or(0.0)
}

/// Sets up our logging and metrics engines.
/// Binds the Prometheus HTTP listener on `metrics_addr`.
/// RUST_LOG (or future log-level arg) still controls tracing via env filter where applicable.
pub fn init_telemetry(metrics_addr: std::net::SocketAddr) {
    // 1. Initialize 'tracing' for our structured logs
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .finish();

    // We use try_set_global_default in case it's already set
    let _ = tracing::subscriber::set_global_default(subscriber);

    // 2. Initialize the Prometheus exporter for our metrics.
    // Port is always 9000 (compliance); host/IP may be customized via CLI/env.
    PrometheusBuilder::new()
        .with_http_listener(metrics_addr)
        .install()
        .expect("Failed to install Prometheus recorder");

    info!(
        "Telemetry initialized. Prometheus metrics available on http://{}/metrics",
        metrics_addr
    );
}

/// Spawns a background task to track relay telemetry metrics.
/// Reads from shared state populated by the main loop.
pub async fn run_metrics_collector(metrics: Arc<Mutex<RelayMetrics>>) {
    info!("Starting Metrics Collector...");

    loop {
        // Read from shared state populated by the main loop
        let snapshot = {
            let guard = metrics.lock().unwrap();
            guard.clone()
        };

        gauge!("telemetry_freshness_s").set(freshness_seconds(
            snapshot.telemetry_acquired_at,
            unix_now_ms(),
        ));

        // Simulate tick rate
        sleep(Duration::from_secs(2)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_metrics_default_values() {
        let m = RelayMetrics::default();
        assert_eq!(m.telemetry_acquired_at, None);
        assert_eq!(freshness_seconds(None, 1_000), 0.0);
    }

    #[test]
    fn freshness_seconds_grows_when_export_time_advances() {
        let acquired = Some(1_000);
        let early = freshness_seconds(acquired, 2_500);
        let later = freshness_seconds(acquired, 5_000);
        assert!((early - 1.5).abs() < f64::EPSILON);
        assert!((later - 4.0).abs() < f64::EPSILON);
        assert!(later > early);
    }
}
