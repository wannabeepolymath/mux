use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use prometheus::{
    Histogram, HistogramOpts, IntCounterVec, IntGauge, IntGaugeVec, Opts, Registry,
};

/// All Prometheus metrics exposed by the multiplexer.
pub struct Metrics {
    pub registry: Registry,

    /// Counter of completed requests, labelled by terminal status.
    /// Status values: success | error | retry
    pub requests_total: IntCounterVec,

    /// Time-to-first-chunk for successful streams, in seconds.
    pub ttfc_seconds: Histogram,

    /// Multiplexer's per-chunk forwarding overhead, in seconds.
    /// Buckets are tight (microsecond range) since this should be tiny.
    pub chunk_forward_seconds: Histogram,

    /// Current depth of the global request queue.
    pub queue_depth: IntGauge,

    /// Number of streams currently being forwarded.
    pub active_streams: IntGauge,

    /// Per-backend state gauge. Labels: backend (addr), state.
    /// Value is 1 if backend is currently in that state, 0 otherwise.
    pub backend_state: IntGaugeVec,

    /// Number of connected client WebSocket sessions.
    pub active_connections: IntGauge,
    pub active_connections_counter: Arc<AtomicUsize>,
}

impl Metrics {
    pub fn new() -> Result<Self, prometheus::Error> {
        let registry = Registry::new();

        let requests_total = IntCounterVec::new(
            Opts::new("mux_requests_total", "Total requests by terminal status"),
            &["status"],
        )?;
        registry.register(Box::new(requests_total.clone()))?;

        let ttfc_seconds = Histogram::with_opts(
            HistogramOpts::new("mux_ttfc_seconds", "Time-to-first-chunk in seconds")
                .buckets(vec![0.1, 0.25, 0.5, 1.0, 2.5, 5.0]),
        )?;
        registry.register(Box::new(ttfc_seconds.clone()))?;

        let chunk_forward_seconds = Histogram::with_opts(
            HistogramOpts::new(
                "mux_chunk_forward_seconds",
                "Multiplexer's overhead per forwarded chunk, in seconds",
            )
            .buckets(vec![
                0.0001, 0.0005, 0.001, 0.002, 0.005, 0.01, 0.025, 0.1,
            ]),
        )?;
        registry.register(Box::new(chunk_forward_seconds.clone()))?;

        let queue_depth = IntGauge::with_opts(Opts::new(
            "mux_queue_depth",
            "Current depth of the global request queue",
        ))?;
        registry.register(Box::new(queue_depth.clone()))?;

        let active_streams = IntGauge::with_opts(Opts::new(
            "mux_active_streams",
            "Number of streams currently being forwarded",
        ))?;
        registry.register(Box::new(active_streams.clone()))?;

        let backend_state = IntGaugeVec::new(
            Opts::new(
                "mux_backend_state",
                "Per-backend state (1 if active, 0 otherwise)",
            ),
            &["backend", "state"],
        )?;
        registry.register(Box::new(backend_state.clone()))?;

        let active_connections = IntGauge::with_opts(Opts::new(
            "mux_active_connections",
            "Number of client WebSocket connections currently open",
        ))?;
        registry.register(Box::new(active_connections.clone()))?;

        Ok(Self {
            registry,
            requests_total,
            ttfc_seconds,
            chunk_forward_seconds,
            queue_depth,
            active_streams,
            backend_state,
            active_connections,
            active_connections_counter: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub fn inc_active_connections(&self) {
        let n = self.active_connections_counter.fetch_add(1, Ordering::Relaxed) + 1;
        self.active_connections.set(n as i64);
    }

    pub fn dec_active_connections(&self) {
        let n = self
            .active_connections_counter
            .fetch_sub(1, Ordering::Relaxed)
            .saturating_sub(1);
        self.active_connections.set(n as i64);
    }

    /// Publish state for a single backend: set the active state gauge to 1
    /// and all others to 0.
    pub fn set_backend_state(&self, backend_label: &str, current_state: &str) {
        for state in &["ready", "busy", "connecting", "draining", "disconnected"] {
            let value = if *state == current_state { 1 } else { 0 };
            if let Ok(g) = self.backend_state.get_metric_with_label_values(&[backend_label, state]) {
                g.set(value);
            }
        }
    }
}
