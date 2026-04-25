pub mod queue;

use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use tokio::sync::mpsc;

use crate::backend::connection::BackendConn;
use crate::backend::state::BackendState;
use crate::config::Config;
use crate::forward::{self, ForwardOutcome, ForwardResult};
use crate::protocol::client::ServerMessage;
use crate::types::{BackendId, StreamId};

use queue::HasStreamId;

// ── Events sent TO the Dispatcher ───────────────────────────────────────

pub enum DispatchEvent {
    /// A client submitted a new TTS request.
    NewRequest(PendingRequest),
    /// A forwarding task completed (success or failure).
    StreamCompleted(ForwardResult),
    /// A client cancelled a stream.
    CancelStream { stream_id: StreamId },
    /// Graceful shutdown.
    Shutdown,
}

// ── Request types ───────────────────────────────────────────────────────

pub struct PendingRequest {
    pub stream_id: StreamId,
    pub text: String,
    pub speaker_id: u32,
    pub priority: u32,
    pub client_tx: mpsc::Sender<ClientEvent>,
    pub retries_remaining: u32,
    pub created_at: Instant,
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
    backends: Vec<BackendConn>,
    queue: queue::BoundedQueue<PendingRequest>,
    dispatch_tx: mpsc::Sender<DispatchEvent>,
    dispatch_rx: mpsc::Receiver<DispatchEvent>,
    active_streams: usize,
    next_backend_idx: usize,
}

impl Dispatcher {
    pub fn new(config: Arc<Config>) -> Self {
        let (tx, rx) = mpsc::channel(256);

        let backends = config
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
                conn.state = BackendState::Ready {
                    since: Instant::now(),
                };
                conn
            })
            .collect();

        let queue = queue::BoundedQueue::new(config.max_queue_depth);

        Self {
            config,
            backends,
            queue,
            dispatch_tx: tx,
            dispatch_rx: rx,
            active_streams: 0,
            next_backend_idx: 0,
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
                DispatchEvent::CancelStream { stream_id } => self.handle_cancel(&stream_id),
                DispatchEvent::Shutdown => {
                    tracing::info!("dispatcher shutting down");
                    break;
                }
            }
        }
    }

    fn handle_new_request(&mut self, req: PendingRequest) {
        if req.text.is_empty() {
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

        self.try_dispatch();
    }

    fn handle_stream_completed(&mut self, result: ForwardResult) {
        let backend_idx = result.backend_id.0;

        match &result.outcome {
            ForwardOutcome::Success {
                ttfc,
                audio_duration,
                total_time: _,
                rtf: _,
                ..
            } => {
                let backend = &mut self.backends[backend_idx];
                backend.scoring.record_ttfc(ttfc.as_secs_f64() * 1000.0);
                backend.scoring.record_result(true);
                backend.circuit.record_success();
                backend.state = BackendState::Ready {
                    since: Instant::now(),
                };
                self.active_streams = self.active_streams.saturating_sub(1);

                let elapsed = result.created_at.elapsed();
                let total_time = elapsed.as_secs_f64();
                let rtf = if total_time > 0.0 {
                    audio_duration / total_time
                } else {
                    0.0
                };

                let done = ServerMessage::Done {
                    stream_id: result.stream_id.0.clone(),
                    audio_duration: *audio_duration,
                    total_time: (total_time * 100.0).round() / 100.0,
                    rtf: (rtf * 100.0).round() / 100.0,
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

            ForwardOutcome::BackendCrashed
            | ForwardOutcome::BackendHung
            | ForwardOutcome::MalformedResponse(_)
            | ForwardOutcome::BackendError(_) => {
                let backend = &mut self.backends[backend_idx];
                backend.scoring.record_result(false);
                backend.circuit.record_failure();
                backend.state = BackendState::Ready {
                    since: Instant::now(),
                };
                self.active_streams = self.active_streams.saturating_sub(1);

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
                    let retry = PendingRequest {
                        stream_id: result.stream_id,
                        text: result.text,
                        speaker_id: result.speaker_id,
                        priority: result.priority,
                        client_tx: result.client_tx,
                        retries_remaining: result.retries_remaining - 1,
                        created_at: result.created_at,
                    };
                    let _ = self.queue.push_front(retry);
                } else {
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
                backend.state = BackendState::Ready {
                    since: Instant::now(),
                };
                self.active_streams = self.active_streams.saturating_sub(1);
                tracing::debug!(
                    stream = %result.stream_id,
                    backend = %result.backend_id,
                    "client gone, released backend"
                );
            }
        }

        self.try_dispatch();
    }

    fn handle_cancel(&mut self, stream_id: &StreamId) {
        if self.queue.remove_by_stream_id(stream_id).is_some() {
            tracing::debug!(stream = %stream_id, "cancelled queued request");
        }
    }

    /// Attempt to match queued requests with available backends.
    fn try_dispatch(&mut self) {
        while !self.queue.is_empty() {
            let Some(backend_id) = self.find_best_backend() else {
                break;
            };

            let Some(req) = self.queue.pop_front() else {
                break;
            };

            self.backends[backend_id.0].state = BackendState::Busy {
                since: Instant::now(),
                stream_id: req.stream_id.clone(),
            };
            self.active_streams += 1;

            let backend_addr = self.backends[backend_id.0].addr;
            let dispatch_tx = self.dispatch_tx.clone();
            let hang_timeout = self.config.hang_timeout;

            tracing::debug!(
                stream = %req.stream_id,
                backend = %backend_id,
                "dispatching"
            );

            tokio::spawn(async move {
                let result =
                    forward::run_forwarding(backend_id, backend_addr, req, hang_timeout).await;
                let _ = dispatch_tx
                    .send(DispatchEvent::StreamCompleted(result))
                    .await;
            });
        }
    }

    /// Pick the best available backend: lowest TTFC EWMA among those with
    /// error rate < 30% and a closed/half-open circuit.
    /// Uses round-robin as tiebreaker when scores are equal.
    fn find_best_backend(&mut self) -> Option<BackendId> {
        let n = self.backends.len();
        let mut best: Option<(BackendId, f64)> = None;

        for offset in 0..n {
            let i = (self.next_backend_idx + offset) % n;
            let backend = &mut self.backends[i];

            if !backend.is_available() {
                continue;
            }
            if backend.scoring.error_rate() >= 0.30 {
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
}
