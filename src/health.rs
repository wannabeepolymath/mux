use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use prometheus::Encoder;
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};

use crate::dispatch::DispatchEvent;
use crate::metrics::Metrics;

/// Snapshot of multiplexer health, returned to /health.
#[derive(Debug, Serialize)]
pub struct HealthSnapshot {
    pub status: &'static str,
    pub backends: Vec<BackendHealth>,
    pub queue_depth: usize,
    pub active_streams: usize,
    pub active_connections: i64,
}

#[derive(Debug, Serialize)]
pub struct BackendHealth {
    pub addr: String,
    pub state: String,
    pub ttfc_ewma_ms: f64,
    pub error_rate: f64,
    pub total_requests: u64,
    pub consecutive_failures: u32,
    pub circuit: String,
}

#[derive(Clone)]
struct AppState {
    dispatch_tx: mpsc::Sender<DispatchEvent>,
    metrics: Arc<Metrics>,
}

/// Run the HTTP server for /health and /metrics.
pub async fn run_http_server(
    addr: SocketAddr,
    dispatch_tx: mpsc::Sender<DispatchEvent>,
    metrics: Arc<Metrics>,
) {
    let state = AppState {
        dispatch_tx,
        metrics,
    };

    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/metrics", get(metrics_handler))
        .with_state(state);

    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("failed to bind metrics endpoint at {addr}: {e}");
            return;
        }
    };
    tracing::info!("HTTP server listening on {addr} (/health, /metrics)");

    if let Err(e) = axum::serve(listener, app).await {
        tracing::error!("axum server error: {e}");
    }
}

async fn health_handler(State(state): State<AppState>) -> Response {
    let (tx, rx) = oneshot::channel();
    if state
        .dispatch_tx
        .send(DispatchEvent::QueryHealth(tx))
        .await
        .is_err()
    {
        return (StatusCode::SERVICE_UNAVAILABLE, "dispatcher gone").into_response();
    }

    match tokio::time::timeout(Duration::from_secs(2), rx).await {
        Ok(Ok(snapshot)) => Json(snapshot).into_response(),
        _ => (StatusCode::SERVICE_UNAVAILABLE, "health query timed out").into_response(),
    }
}

async fn metrics_handler(State(state): State<AppState>) -> Response {
    let encoder = prometheus::TextEncoder::new();
    let metric_families = state.metrics.registry.gather();
    let mut buffer = Vec::new();
    if let Err(e) = encoder.encode(&metric_families, &mut buffer) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("encode error: {e}")).into_response();
    }

    (
        StatusCode::OK,
        [("Content-Type", encoder.format_type())],
        buffer,
    )
        .into_response()
}
