pub mod connect;
pub mod lifecycle;
pub mod queue;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use tokio::sync::{mpsc, oneshot};

use crate::backend::shard::BackendShard;
use crate::backend::state::BackendState;
use crate::client::{ClientEvent, ClientStreams};
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
    /// A forwarding task completed (success or failure). Lifecycle handles
    /// per-backend state in parallel; this carries the request payload so
    /// the dispatcher can decide retry / done frame / error frame.
    StreamCompleted(ForwardResult),
    /// A backend just transitioned to Ready (slot freshly filled by its
    /// lifecycle task). Re-runs try_dispatch.
    BackendReady(BackendId),
    /// A client cancelled a stream.
    CancelStream { stream_id: StreamId },
    /// Health endpoint requested a snapshot of the dispatcher's state.
    QueryHealth(oneshot::Sender<HealthSnapshot>),
}

// ── Request types ───────────────────────────────────────────────────────

pub struct PendingRequest {
    pub stream_id: StreamId,
    pub text: String,
    pub speaker_id: u32,
    pub client_streams: Arc<ClientStreams>,
    pub retries_remaining: u32,
    pub created_at: Instant,
    pub active_count: Arc<AtomicUsize>,
    pub tried_backends: Vec<BackendId>,
}

impl HasStreamId for PendingRequest {
    fn stream_id(&self) -> &StreamId {
        &self.stream_id
    }
}

// ── Dispatcher actor ────────────────────────────────────────────────────

pub struct Dispatcher {
    config: Arc<Config>,
    metrics: Arc<Metrics>,
    shards: Vec<Arc<BackendShard>>,
    queue: queue::BoundedQueue<PendingRequest>,
    dispatch_tx: mpsc::Sender<DispatchEvent>,
    dispatch_rx: mpsc::Receiver<DispatchEvent>,
    active_streams: usize,
    next_backend_idx: usize,
    in_flight: HashMap<StreamId, oneshot::Sender<()>>,
    /// Receivers paired with `shards`; consumed when `run()` spawns lifecycle tasks.
    lifecycle_receivers: Option<Vec<mpsc::Receiver<crate::backend::shard::LifecycleEvent>>>,
}

impl Dispatcher {
    pub fn new(config: Arc<Config>, metrics: Arc<Metrics>) -> Self {
        let (tx, rx) = mpsc::channel(256);

        let mut shards = Vec::with_capacity(config.backend_addrs.len());
        let mut receivers = Vec::with_capacity(config.backend_addrs.len());
        for (i, &addr) in config.backend_addrs.iter().enumerate() {
            let (shard, lifecycle_rx) = BackendShard::new(
                BackendId(i),
                addr,
                config.circuit_threshold,
                config.circuit_cooldown,
            );
            metrics.set_backend_state(&shard.addr_str, shard.state().name());
            shards.push(shard);
            receivers.push(lifecycle_rx);
        }

        let queue = queue::BoundedQueue::new(config.max_queue_depth);

        Self {
            config,
            metrics,
            shards,
            queue,
            dispatch_tx: tx,
            dispatch_rx: rx,
            active_streams: 0,
            next_backend_idx: 0,
            in_flight: HashMap::new(),
            lifecycle_receivers: Some(receivers),
        }
    }

    pub fn sender(&self) -> mpsc::Sender<DispatchEvent> {
        self.dispatch_tx.clone()
    }

    pub async fn run(mut self) {
        // Spawn one lifecycle task per backend. Each performs its own initial
        // connect on startup and sends BackendReady when the slot fills.
        let receivers = self
            .lifecycle_receivers
            .take()
            .expect("receivers consumed once");
        lifecycle::spawn_lifecycles(
            &self.shards,
            receivers,
            self.dispatch_tx.clone(),
            self.metrics.clone(),
            self.config.clone(),
        );

        tracing::info!("dispatcher started with {} backends", self.shards.len());

        while let Some(event) = self.dispatch_rx.recv().await {
            match event {
                DispatchEvent::NewRequest(req) => self.handle_new_request(req),
                DispatchEvent::StreamCompleted(result) => self.handle_stream_completed(result),
                DispatchEvent::BackendReady(id) => self.handle_backend_ready(id),
                DispatchEvent::CancelStream { stream_id } => self.handle_cancel(&stream_id),
                DispatchEvent::QueryHealth(reply) => {
                    let _ = reply.send(self.snapshot());
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
            let _ = req
                .client_streams
                .enqueue(&req.stream_id, ClientEvent::Text(err.to_json()));
            return;
        }

        let queue_depth = self.queue.len();

        let ack = ServerMessage::Queued {
            stream_id: req.stream_id.0.clone(),
            queue_depth,
        };
        let _ = req
            .client_streams
            .enqueue(&req.stream_id, ClientEvent::Text(ack.to_json()));

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
            let _ = req
                .client_streams
                .enqueue(&req.stream_id, ClientEvent::Text(err.to_json()));
            return;
        }

        self.metrics.queue_depth.set(self.queue.len() as i64);
        self.try_dispatch();
    }

    fn handle_stream_completed(&mut self, result: ForwardResult) {
        // Stream is no longer in-flight (succeeded, failed, or cancelled).
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
                    .client_streams
                    .enqueue(&result.stream_id, ClientEvent::Text(done.to_json()));

                tracing::debug!(
                    stream = %result.stream_id,
                    backend = %result.backend_id,
                    ttfc_ms = ttfc.as_millis(),
                    "stream completed"
                );
            }

            ForwardOutcome::BackendBusy => {
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
                    client_streams: result.client_streams,
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
                self.active_streams = self.active_streams.saturating_sub(1);
                self.metrics.active_streams.set(self.active_streams as i64);

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
                        client_streams: result.client_streams,
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
                        .client_streams
                        .enqueue(&result.stream_id, ClientEvent::Text(err.to_json()));
                }
            }

            ForwardOutcome::ClientGone => {
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

    fn handle_backend_ready(&mut self, _backend_id: BackendId) {
        self.try_dispatch();
    }

    fn handle_cancel(&mut self, stream_id: &StreamId) {
        if let Some(cancel_tx) = self.in_flight.remove(stream_id) {
            let _ = cancel_tx.send(());
            tracing::debug!(stream = %stream_id, "cancelling in-flight stream");
            return;
        }

        if let Some(req) = self.queue.remove_by_stream_id(stream_id) {
            req.active_count.fetch_sub(1, Ordering::Relaxed);
            self.metrics.queue_depth.set(self.queue.len() as i64);
            tracing::debug!(stream = %stream_id, "cancelled queued request");
        }
    }

    fn try_dispatch(&mut self) {
        let total_backends = self.shards.len();
        let mut idx = 0;

        while idx < self.queue.len() {
            let exclude = match self.queue.peek_at(idx) {
                Some(r) => r.tried_backends.clone(),
                None => break,
            };

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
        }
    }

    fn dispatch_to(&mut self, backend_id: BackendId, req: PendingRequest) {
        let shard = self.shards[backend_id.0].clone();
        let ws = match shard.take_for_dispatch() {
            Some(ws) => ws,
            None => {
                tracing::error!(
                    backend = %backend_id,
                    "dispatch_to called but no warm slot — re-queueing"
                );
                let _ = self.queue.push_front(req);
                self.metrics.queue_depth.set(self.queue.len() as i64);
                return;
            }
        };

        self.metrics
            .set_backend_state(&shard.addr_str, BackendState::Busy.name());
        self.active_streams += 1;
        self.metrics.active_streams.set(self.active_streams as i64);

        let dispatch_tx = self.dispatch_tx.clone();
        let lifecycle_tx = shard.lifecycle_sender();
        let hang_timeout = self.config.hang_timeout;
        let drain_timeout = self.config.drain_timeout;
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
                ws,
                req,
                hang_timeout,
                drain_timeout,
                cancel_rx,
                metrics,
                lifecycle_tx,
            )
            .await;
            let _ = dispatch_tx
                .send(DispatchEvent::StreamCompleted(result))
                .await;
        });
    }

    fn find_best_backend(&mut self, exclude: &[BackendId]) -> Option<BackendId> {
        self.find_best_backend_filtered(exclude, true)
            .or_else(|| self.find_best_backend_filtered(exclude, false))
    }

    fn find_best_backend_filtered(
        &mut self,
        exclude: &[BackendId],
        prefer_low_error: bool,
    ) -> Option<BackendId> {
        let n = self.shards.len();
        let mut best: Option<(BackendId, f64)> = None;

        for offset in 0..n {
            let i = (self.next_backend_idx + offset) % n;
            let shard = &self.shards[i];

            if exclude.contains(&shard.id) {
                continue;
            }
            if !shard.is_available() {
                continue;
            }
            if prefer_low_error && shard.error_rate() >= 0.30 {
                continue;
            }

            let score = shard.ttfc_ewma_ms();

            let dominated = match &best {
                None => false,
                Some((_, best_score)) => {
                    if score == 0.0 && *best_score == 0.0 {
                        true
                    } else if score == 0.0 {
                        true
                    } else if *best_score == 0.0 {
                        false
                    } else {
                        score >= *best_score
                    }
                }
            };

            if !dominated {
                best = Some((shard.id, score));
            }
        }

        if let Some((id, _)) = best {
            self.next_backend_idx = (id.0 + 1) % n;
        }

        best.map(|(id, _)| id)
    }

    fn snapshot(&self) -> HealthSnapshot {
        let backends = self
            .shards
            .iter()
            .map(|s| BackendHealth {
                addr: s.addr_str.to_string(),
                state: s.state().name().to_string(),
                ttfc_ewma_ms: round_2(s.ttfc_ewma_ms()),
                error_rate: round_2(s.error_rate()),
                total_requests: s.total_requests(),
                consecutive_failures: s.consecutive_failures(),
                circuit: match s.circuit_state() {
                    crate::backend::circuit::CircuitState::Closed => "closed".to_string(),
                    crate::backend::circuit::CircuitState::Open => "open".to_string(),
                    crate::backend::circuit::CircuitState::HalfOpen => "half_open".to_string(),
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
