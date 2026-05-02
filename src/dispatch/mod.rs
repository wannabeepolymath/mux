pub mod connect;
pub mod queue;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use bytes::Bytes;
use tokio::sync::{mpsc, oneshot};

use crate::backend::circuit::CircuitState;
use crate::backend::connection::BackendConn;
use crate::backend::state::BackendState;
use crate::config::Config;
use crate::forward::{self, ForwardOutcome, ForwardResult};
use crate::health::{BackendHealth, HealthSnapshot};
use crate::metrics::Metrics;
use crate::protocol::client::ServerMessage;
use crate::types::{BackendId, StreamId};

use queue::HasStreamId;

// ── Events sent TO the Dispatcher ───────────────────────────────────────

pub enum DispatchEvent {
    /// A client submitted a new TTS request.
    NewRequest(PendingRequest),
    /// A forwarding task completed (success or failure).
    StreamCompleted(ForwardResult),
    /// A backend has recovered from a post-failure cooldown and is ready again.
    BackendRecovered(BackendId),
    /// A client cancelled a stream.
    CancelStream { stream_id: StreamId },
    /// Health endpoint requested a snapshot of the dispatcher's state.
    QueryHealth(oneshot::Sender<HealthSnapshot>),
    /// Connect task succeeded — hand over the warm ws.
    ConnectionEstablished {
        backend_id: BackendId,
        ws: crate::backend::BackendWs,
    },
    /// Connect task failed — schedule retry (subject to circuit state).
    ConnectionFailed {
        backend_id: BackendId,
        error: String,
    },
}

// ── Request types ───────────────────────────────────────────────────────

pub struct PendingRequest {
    pub stream_id: StreamId,
    pub text: String,
    pub speaker_id: u32,
    pub client_tx: mpsc::Sender<ClientEvent>,
    pub retries_remaining: u32,
    pub created_at: Instant,
    /// Shared counter for active streams on the client connection.
    /// Decremented by the dispatcher when the stream reaches a terminal state.
    pub active_count: Arc<AtomicUsize>,
    /// Backends already tried for this request (to avoid retrying the same one).
    pub tried_backends: Vec<BackendId>,
}

impl HasStreamId for PendingRequest {
    fn stream_id(&self) -> &StreamId {
        &self.stream_id
    }
}

// ── Events sent FROM Dispatcher/forward TO client writer ────────────────

pub enum ClientEvent {
    /// JSON text frame to send to the client.
    Text(String),
    /// Pre-encoded binary frame (stream_id header + PCM payload).
    Binary(Bytes),
}

// ── Dispatcher actor ────────────────────────────────────────────────────

pub struct Dispatcher {
    config: Arc<Config>,
    metrics: Arc<Metrics>,
    backends: Vec<BackendConn>,
    queue: queue::BoundedQueue<PendingRequest>,
    dispatch_tx: mpsc::Sender<DispatchEvent>,
    dispatch_rx: mpsc::Receiver<DispatchEvent>,
    active_streams: usize,
    next_backend_idx: usize,
    /// In-flight streams with their cancellation handles.
    in_flight: HashMap<StreamId, oneshot::Sender<()>>,
}

impl Dispatcher {
    pub fn new(config: Arc<Config>, metrics: Arc<Metrics>) -> Self {
        let (tx, rx) = mpsc::channel(256);

        let backends: Vec<BackendConn> = config
            .backend_addrs
            .iter()
            .enumerate()
            .map(|(i, &addr)| {
                let mut conn = BackendConn::new(
                    BackendId(i),
                    addr,
                    config.circuit_threshold,
                    config.circuit_cooldown,
                );
                conn.state = BackendState::Ready;
                conn
            })
            .collect();

        for backend in &backends {
            metrics.set_backend_state(&backend.addr.to_string(), backend.state.name());
        }

        let queue = queue::BoundedQueue::new(config.max_queue_depth);

        Self {
            config,
            metrics,
            backends,
            queue,
            dispatch_tx: tx,
            dispatch_rx: rx,
            active_streams: 0,
            next_backend_idx: 0,
            in_flight: HashMap::new(),
        }
    }

    /// Get a cloneable sender handle for submitting events.
    pub fn sender(&self) -> mpsc::Sender<DispatchEvent> {
        self.dispatch_tx.clone()
    }

    /// Run the dispatcher event loop. Blocks until shutdown or all senders drop.
    pub async fn run(mut self) {
        tracing::info!("dispatcher started with {} backends", self.backends.len());

        while let Some(event) = self.dispatch_rx.recv().await {
            match event {
                DispatchEvent::NewRequest(req) => self.handle_new_request(req),
                DispatchEvent::StreamCompleted(result) => self.handle_stream_completed(result),
                DispatchEvent::BackendRecovered(id) => self.handle_backend_recovered(id),
                DispatchEvent::CancelStream { stream_id } => self.handle_cancel(&stream_id),
                DispatchEvent::QueryHealth(reply) => {
                    let _ = reply.send(self.snapshot());
                }
                DispatchEvent::ConnectionEstablished { backend_id, ws } => {
                    self.handle_connection_established(backend_id, ws);
                }
                DispatchEvent::ConnectionFailed { backend_id, error } => {
                    self.handle_connection_failed(backend_id, error);
                }
            }
        }
    }

    fn handle_new_request(&mut self, req: PendingRequest) {
        if req.text.is_empty() {
            req.active_count.fetch_sub(1, Ordering::Relaxed);
            let err = ServerMessage::Error {
                stream_id: req.stream_id.0.clone(),
                message: "Missing 'text' field".into(),
            };
            let _ = req.client_tx.try_send(ClientEvent::Text(err.to_json()));
            return;
        }

        let queue_depth = self.queue.len();

        let ack = ServerMessage::Queued {
            stream_id: req.stream_id.0.clone(),
            queue_depth,
        };
        let _ = req.client_tx.try_send(ClientEvent::Text(ack.to_json()));

        if let Err(req) = self.queue.push_back(req) {
            req.active_count.fetch_sub(1, Ordering::Relaxed);
            self.metrics
                .requests_total
                .with_label_values(&["error"])
                .inc();
            let err = ServerMessage::Error {
                stream_id: req.stream_id.0.clone(),
                message: format!(
                    "queue full ({}/{})",
                    self.queue.len(),
                    self.config.max_queue_depth
                ),
            };
            let _ = req.client_tx.try_send(ClientEvent::Text(err.to_json()));
            return;
        }

        self.metrics.queue_depth.set(self.queue.len() as i64);
        self.try_dispatch();
    }

    fn handle_stream_completed(&mut self, result: ForwardResult) {
        let backend_idx = result.backend_id.0;
        // Stream is no longer in-flight (either succeeded, failed, or cancelled).
        // Remove the cancel handle now to free resources.
        self.in_flight.remove(&result.stream_id);

        match &result.outcome {
            ForwardOutcome::Success {
                ttfc,
                audio_duration,
            } => {
                self.metrics.ttfc_seconds.observe(ttfc.as_secs_f64());
                self.metrics
                    .requests_total
                    .with_label_values(&["success"])
                    .inc();

                let backend = &mut self.backends[backend_idx];
                backend.scoring.record_ttfc(ttfc.as_secs_f64() * 1000.0);
                backend.scoring.record_result(true);
                backend.circuit.record_success();
                backend.state = BackendState::Ready;
                self.metrics
                    .set_backend_state(&backend.addr.to_string(), backend.state.name());
                self.active_streams = self.active_streams.saturating_sub(1);
                self.metrics.active_streams.set(self.active_streams as i64);

                let elapsed = result.created_at.elapsed();
                let total_time = elapsed.as_secs_f64();
                let rtf = if total_time > 0.0 {
                    audio_duration / total_time
                } else {
                    0.0
                };

                result.active_count.fetch_sub(1, Ordering::Relaxed);

                let done = ServerMessage::Done {
                    stream_id: result.stream_id.0.clone(),
                    audio_duration: *audio_duration,
                    total_time: round_2(total_time),
                    rtf: round_2(rtf),
                };
                let _ = result
                    .client_tx
                    .try_send(ClientEvent::Text(done.to_json()));

                tracing::debug!(
                    stream = %result.stream_id,
                    backend = %result.backend_id,
                    ttfc_ms = ttfc.as_millis(),
                    "stream completed"
                );
            }

            ForwardOutcome::BackendBusy => {
                let backend = &mut self.backends[backend_idx];
                backend.state = BackendState::Ready;
                self.metrics
                    .set_backend_state(&backend.addr.to_string(), backend.state.name());
                self.active_streams = self.active_streams.saturating_sub(1);
                self.metrics.active_streams.set(self.active_streams as i64);

                tracing::debug!(
                    stream = %result.stream_id,
                    backend = %result.backend_id,
                    "backend busy, requeue without penalty"
                );

                let mut tried = result.tried_backends;
                if !tried.contains(&result.backend_id) {
                    tried.push(result.backend_id);
                }
                let retry = PendingRequest {
                    stream_id: result.stream_id,
                    text: result.text,
                    speaker_id: result.speaker_id,
                    client_tx: result.client_tx,
                    retries_remaining: result.retries_remaining,
                    created_at: result.created_at,
                    active_count: result.active_count,
                    tried_backends: tried,
                };
                let _ = self.queue.push_front(retry);
                self.metrics.queue_depth.set(self.queue.len() as i64);
            }

            ForwardOutcome::BackendCrashed
            | ForwardOutcome::BackendHung
            | ForwardOutcome::MalformedResponse(_)
            | ForwardOutcome::BackendError(_) => {
                let backend = &mut self.backends[backend_idx];
                backend.scoring.record_result(false);
                let penalty_ms = self.config.hang_timeout.as_secs_f64() * 1000.0;
                backend.scoring.record_ttfc(penalty_ms);
                backend.circuit.record_failure();
                // Don't mark Ready immediately — cooldown prevents retries
                // hitting the same (possibly still-hung) backend.
                backend.state = BackendState::Disconnected;
                self.metrics
                    .set_backend_state(&backend.addr.to_string(), backend.state.name());
                self.active_streams = self.active_streams.saturating_sub(1);
                self.metrics.active_streams.set(self.active_streams as i64);

                let dispatch_tx = self.dispatch_tx.clone();
                let bid = result.backend_id;
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    let _ = dispatch_tx.send(DispatchEvent::BackendRecovered(bid)).await;
                });

                let reason = match &result.outcome {
                    ForwardOutcome::BackendCrashed => "crashed".to_string(),
                    ForwardOutcome::BackendHung => "hung".to_string(),
                    ForwardOutcome::MalformedResponse(s) => format!("malformed: {s}"),
                    ForwardOutcome::BackendError(s) => format!("error: {s}"),
                    _ => "unknown".to_string(),
                };

                tracing::warn!(
                    stream = %result.stream_id,
                    backend = %result.backend_id,
                    retries_left = result.retries_remaining,
                    reason = %reason,
                    "forwarding failed"
                );

                if result.retries_remaining > 0 {
                    self.metrics
                        .requests_total
                        .with_label_values(&["retry"])
                        .inc();
                    let mut tried = result.tried_backends;
                    if !tried.contains(&result.backend_id) {
                        tried.push(result.backend_id);
                    }
                    let retry = PendingRequest {
                        stream_id: result.stream_id,
                        text: result.text,
                        speaker_id: result.speaker_id,
                        client_tx: result.client_tx,
                        retries_remaining: result.retries_remaining - 1,
                        created_at: result.created_at,
                        active_count: result.active_count,
                        tried_backends: tried,
                    };
                    let _ = self.queue.push_front(retry);
                    self.metrics.queue_depth.set(self.queue.len() as i64);
                } else {
                    self.metrics
                        .requests_total
                        .with_label_values(&["error"])
                        .inc();
                    result.active_count.fetch_sub(1, Ordering::Relaxed);
                    let err = ServerMessage::Error {
                        stream_id: result.stream_id.0.clone(),
                        message: format!(
                            "backend unavailable after {} retries",
                            self.config.max_retries
                        ),
                    };
                    let _ = result
                        .client_tx
                        .try_send(ClientEvent::Text(err.to_json()));
                }
            }

            ForwardOutcome::ClientGone => {
                let backend = &mut self.backends[backend_idx];
                backend.state = BackendState::Ready;
                self.metrics
                    .set_backend_state(&backend.addr.to_string(), backend.state.name());
                self.active_streams = self.active_streams.saturating_sub(1);
                self.metrics.active_streams.set(self.active_streams as i64);
                result.active_count.fetch_sub(1, Ordering::Relaxed);
                tracing::debug!(
                    stream = %result.stream_id,
                    backend = %result.backend_id,
                    "client gone, released backend"
                );
            }

            ForwardOutcome::Cancelled => {
                // Client-initiated cancellation. Don't penalize the backend.
                // The backend connection has been closed; mark Ready so it can
                // accept the next dispatch (a fresh connection will be opened).
                let backend = &mut self.backends[backend_idx];
                backend.state = BackendState::Ready;
                self.metrics
                    .set_backend_state(&backend.addr.to_string(), backend.state.name());
                self.active_streams = self.active_streams.saturating_sub(1);
                self.metrics.active_streams.set(self.active_streams as i64);
                result.active_count.fetch_sub(1, Ordering::Relaxed);
                tracing::debug!(
                    stream = %result.stream_id,
                    backend = %result.backend_id,
                    "stream cancelled, released backend"
                );
            }
        }

        self.try_dispatch();
    }

    fn handle_connection_established(
        &mut self,
        backend_id: BackendId,
        _ws: crate::backend::BackendWs,
    ) {
        // Real implementation lands in Task 8.
        tracing::debug!(backend = %backend_id, "stub: connection established (drop ws)");
    }

    fn handle_connection_failed(&mut self, backend_id: BackendId, error: String) {
        // Real implementation lands in Task 8.
        tracing::debug!(backend = %backend_id, error = %error, "stub: connection failed");
    }

    fn handle_backend_recovered(&mut self, backend_id: BackendId) {
        let backend = &mut self.backends[backend_id.0];
        if matches!(backend.state, BackendState::Disconnected) {
            backend.state = BackendState::Ready;
            self.metrics
                .set_backend_state(&backend.addr.to_string(), backend.state.name());
            tracing::debug!(backend = %backend_id, "backend recovered after cooldown");
            self.try_dispatch();
        }
    }

    fn handle_cancel(&mut self, stream_id: &StreamId) {
        // Try in-flight first: signal the forwarding task to abort.
        if let Some(cancel_tx) = self.in_flight.remove(stream_id) {
            let _ = cancel_tx.send(());
            tracing::debug!(stream = %stream_id, "cancelling in-flight stream");
            return;
        }

        // Otherwise, try removing from the queue.
        if let Some(req) = self.queue.remove_by_stream_id(stream_id) {
            req.active_count.fetch_sub(1, Ordering::Relaxed);
            self.metrics.queue_depth.set(self.queue.len() as i64);
            tracing::debug!(stream = %stream_id, "cancelled queued request");
        }
    }

    /// Attempt to match queued requests with available backends.
    ///
    /// Walks the queue (not just the head) so a request whose exclusion list
    /// can't be satisfied right now doesn't block requests behind it.
    fn try_dispatch(&mut self) {
        let total_backends = self.backends.len();
        let mut idx = 0;

        while idx < self.queue.len() {
            let exclude = match self.queue.peek_at(idx) {
                Some(r) => r.tried_backends.clone(),
                None => break,
            };

            // Only fall back to allowing previously-tried backends if EVERY
            // backend has been tried (otherwise we'd dispatch back to the same
            // failing backend while others were just temporarily busy).
            let all_tried = exclude.len() >= total_backends;

            let backend_id = if all_tried {
                self.find_best_backend(&[])
            } else {
                self.find_best_backend(&exclude)
            };

            let Some(backend_id) = backend_id else {
                idx += 1;
                continue;
            };

            let req = self
                .queue
                .remove_at(idx)
                .expect("peeked just above, must exist");

            self.metrics.queue_depth.set(self.queue.len() as i64);
            self.dispatch_to(backend_id, req);
            // Element shifted into our slot; don't increment idx.
        }
    }

    fn dispatch_to(&mut self, backend_id: BackendId, req: PendingRequest) {
        let backend_addr_str = self.backends[backend_id.0].addr.to_string();
        self.backends[backend_id.0].state = BackendState::Busy;
        self.metrics
            .set_backend_state(&backend_addr_str, self.backends[backend_id.0].state.name());
        self.active_streams += 1;
        self.metrics.active_streams.set(self.active_streams as i64);

        let backend_addr = self.backends[backend_id.0].addr;
        let dispatch_tx = self.dispatch_tx.clone();
        let hang_timeout = self.config.hang_timeout;
        let metrics = self.metrics.clone();

        let (cancel_tx, cancel_rx) = oneshot::channel();
        self.in_flight.insert(req.stream_id.clone(), cancel_tx);

        tracing::debug!(
            stream = %req.stream_id,
            backend = %backend_id,
            "dispatching"
        );

        tokio::spawn(async move {
            let result = forward::run_forwarding(
                backend_id,
                backend_addr,
                req,
                hang_timeout,
                cancel_rx,
                metrics,
            )
            .await;
            let _ = dispatch_tx
                .send(DispatchEvent::StreamCompleted(result))
                .await;
        });
    }

    /// Pick the best available backend: lowest TTFC EWMA among those with
    /// error rate < 30% and a closed/half-open circuit.
    /// If every backend in the window is "too hot" (common under chaos), falls
    /// back to the same score comparison without the error-rate filter so work
    /// is never left permanently undispatchable.
    /// Uses round-robin as tiebreaker when scores are equal.
    /// Excludes backends in the `exclude` list (already tried for this request).
    fn find_best_backend(&mut self, exclude: &[BackendId]) -> Option<BackendId> {
        self.find_best_backend_filtered(exclude, true)
            .or_else(|| self.find_best_backend_filtered(exclude, false))
    }

    /// `prefer_low_error`: when true, skip backends with recent error rate ≥ 30%.
    fn find_best_backend_filtered(
        &mut self,
        exclude: &[BackendId],
        prefer_low_error: bool,
    ) -> Option<BackendId> {
        let n = self.backends.len();
        let mut best: Option<(BackendId, f64)> = None;

        for offset in 0..n {
            let i = (self.next_backend_idx + offset) % n;
            let backend = &mut self.backends[i];

            if exclude.contains(&backend.id) {
                continue;
            }
            if !backend.is_available() {
                continue;
            }
            if prefer_low_error && backend.scoring.error_rate() >= 0.30 {
                continue;
            }

            let score = backend.scoring.ttfc_ewma_ms();

            let dominated = match &best {
                None => false,
                Some((_, best_score)) => {
                    if score == 0.0 && *best_score == 0.0 {
                        true // both have no data, keep first found (round-robin)
                    } else if score == 0.0 {
                        true // no data vs real data, prefer real data
                    } else if *best_score == 0.0 {
                        false // real data vs no data, prefer real data
                    } else {
                        score >= *best_score
                    }
                }
            };

            if !dominated {
                best = Some((backend.id, score));
            }
        }

        if let Some((id, _)) = best {
            self.next_backend_idx = (id.0 + 1) % n;
        }

        best.map(|(id, _)| id)
    }

    /// Build a snapshot of the dispatcher's state for the /health endpoint.
    fn snapshot(&self) -> HealthSnapshot {
        let backends = self
            .backends
            .iter()
            .map(|b| BackendHealth {
                addr: b.addr.to_string(),
                state: b.state.name().to_string(),
                ttfc_ewma_ms: round_2(b.scoring.ttfc_ewma_ms()),
                error_rate: round_2(b.scoring.error_rate()),
                total_requests: b.scoring.total_requests,
                consecutive_failures: b.circuit.consecutive_failures(),
                circuit: match b.circuit.state() {
                    CircuitState::Closed => "closed".to_string(),
                    CircuitState::Open => "open".to_string(),
                    CircuitState::HalfOpen => "half_open".to_string(),
                },
            })
            .collect();

        HealthSnapshot {
            status: "ok",
            backends,
            queue_depth: self.queue.len(),
            active_streams: self.active_streams,
            active_connections: self.metrics.active_connections.get(),
        }
    }
}

fn round_2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}
