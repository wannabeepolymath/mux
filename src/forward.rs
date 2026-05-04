use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

use crate::backend::BackendWs;
use crate::backend::shard::{BackendOutcome, LifecycleEvent};
use crate::client::{ClientEvent, ClientStreams};
use crate::dispatch::PendingRequest;
use crate::metrics::Metrics;
use crate::protocol::backend::BackendMessage;
use crate::protocol::frame::encode_binary_frame;
use crate::types::{BackendId, StreamId};

/// Outcome of a forwarding attempt.
#[derive(Debug)]
pub enum ForwardOutcome {
    Success {
        ttfc: Duration,
        audio_duration: f64,
    },
    BackendCrashed,
    BackendHung,
    MalformedResponse(String),
    /// Backend accepted the connection but refused the start (contention).
    BackendBusy,
    BackendError(String),
    ClientGone,
    Cancelled,
}

impl ForwardOutcome {
    /// Translate the outcome into the slimmer per-backend signal that the
    /// lifecycle task uses to update scoring/circuit/state.
    fn to_backend_outcome(&self) -> BackendOutcome {
        match self {
            ForwardOutcome::Success { ttfc, .. } => BackendOutcome::Success {
                ttfc_ms: ttfc.as_secs_f64() * 1000.0,
            },
            ForwardOutcome::BackendCrashed
            | ForwardOutcome::BackendHung
            | ForwardOutcome::MalformedResponse(_)
            | ForwardOutcome::BackendError(_) => BackendOutcome::Failure,
            ForwardOutcome::BackendBusy => BackendOutcome::Busy,
            ForwardOutcome::ClientGone | ForwardOutcome::Cancelled => {
                BackendOutcome::ClientGoneOrCancel
            }
        }
    }
}

/// Full result returned to the Dispatcher, carrying request data back for retries.
pub struct ForwardResult {
    pub backend_id: BackendId,
    pub stream_id: StreamId,
    pub outcome: ForwardOutcome,
    pub client_streams: Arc<ClientStreams>,
    pub text: String,
    pub speaker_id: u32,
    pub retries_remaining: u32,
    pub created_at: Instant,
    pub active_count: Arc<AtomicUsize>,
    pub tried_backends: Vec<BackendId>,
}

/// Execute a single forwarding attempt using the warm `ws` connection passed in.
///
/// On any terminal outcome:
///   1. Spawn a detached cleanup task for the WS (drain, active-close, or drop
///      depending on outcome).
///   2. Send a `LifecycleEvent::ForwardDone(...)` to the per-backend lifecycle
///      task so it can update scoring/circuit and reconnect.
///   3. Return a `ForwardResult` to the caller (the dispatch task that spawned
///      us). The caller is responsible for sending it on as
///      `DispatchEvent::StreamCompleted`.
pub async fn run_forwarding(
    backend_id: BackendId,
    mut ws: BackendWs,
    request: PendingRequest,
    hang_timeout: Duration,
    drain_timeout: Duration,
    cancel_rx: oneshot::Receiver<()>,
    metrics: Arc<Metrics>,
    lifecycle_tx: mpsc::Sender<LifecycleEvent>,
) -> ForwardResult {
    let stream_id = request.stream_id.clone();

    let outcome = do_forward(
        &mut ws,
        &stream_id,
        &request.text,
        request.speaker_id,
        &request.client_streams,
        hang_timeout,
        cancel_rx,
        &metrics,
    )
    .await;

    // Detach TCP cleanup so the dispatcher learns of completion immediately.
    let cleanup = cleanup_kind(&outcome);
    spawn_ws_cleanup(ws, drain_timeout, cleanup);

    // Notify the per-backend lifecycle task (single writer for backend state).
    // try_send because the lifecycle channel is bounded and we don't want to
    // back-pressure the forward task; in the worst case we block briefly on
    // the await fallback.
    let backend_outcome = outcome.to_backend_outcome();
    if let Err(mpsc::error::TrySendError::Full(ev)) =
        lifecycle_tx.try_send(LifecycleEvent::ForwardDone(backend_outcome))
    {
        // Channel full (rare). Fall back to await to ensure delivery.
        let _ = lifecycle_tx.send(ev).await;
    }

    ForwardResult {
        backend_id,
        stream_id,
        outcome,
        client_streams: request.client_streams,
        text: request.text,
        speaker_id: request.speaker_id,
        retries_remaining: request.retries_remaining,
        created_at: request.created_at,
        active_count: request.active_count,
        tried_backends: request.tried_backends,
    }
}

/// What style of TCP cleanup the connection needs after the request resolves.
#[derive(Debug, Clone, Copy)]
enum CleanupKind {
    Drain,
    ActiveClose,
    Drop,
}

fn cleanup_kind(outcome: &ForwardOutcome) -> CleanupKind {
    match outcome {
        ForwardOutcome::BackendHung => CleanupKind::Drop,
        ForwardOutcome::Cancelled | ForwardOutcome::ClientGone => CleanupKind::ActiveClose,
        _ => CleanupKind::Drain,
    }
}

fn spawn_ws_cleanup(ws: BackendWs, drain_timeout: Duration, kind: CleanupKind) {
    if matches!(kind, CleanupKind::Drop) {
        drop(ws);
        return;
    }
    tokio::spawn(async move {
        let mut ws = ws;
        if matches!(kind, CleanupKind::ActiveClose) {
            let _ = tokio::time::timeout(Duration::from_millis(50), ws.close(None)).await;
        }
        let _ = tokio::time::timeout(drain_timeout, async {
            while ws.next().await.is_some() {}
        })
        .await;
    });
}

#[allow(clippy::too_many_arguments)]
async fn do_forward(
    ws: &mut BackendWs,
    stream_id: &StreamId,
    text: &str,
    speaker_id: u32,
    client_streams: &Arc<ClientStreams>,
    hang_timeout: Duration,
    mut cancel_rx: oneshot::Receiver<()>,
    metrics: &Arc<Metrics>,
) -> ForwardOutcome {
    if let Some(early) = check_warm_slot_liveness(ws).await {
        return early;
    }

    let start_json = serde_json::json!({
        "type": "start",
        "text": text,
        "speaker_id": speaker_id,
    });
    if let Err(e) = ws
        .send(Message::Text(start_json.to_string().into()))
        .await
    {
        tracing::warn!(stream = %stream_id, "send start failed: {e}");
        return ForwardOutcome::BackendCrashed;
    }

    let request_start = Instant::now();
    let mut first_chunk_time: Option<Instant> = None;

    // Single Sleep timer reused across iterations (#8 micro-fix). Each
    // successful read resets the deadline; firing means hang_timeout elapsed
    // without any progress.
    let sleep = tokio::time::sleep(hang_timeout);
    tokio::pin!(sleep);

    loop {
        tokio::select! {
            biased;
            _ = &mut cancel_rx => {
                tracing::debug!(stream = %stream_id, "stream cancelled");
                return ForwardOutcome::Cancelled;
            }
            _ = &mut sleep => {
                tracing::warn!(stream = %stream_id, "hung — no data in {hang_timeout:?}");
                return ForwardOutcome::BackendHung;
            }
            read = ws.next() => {
                // Reset the hang deadline before processing the message so
                // subsequent reads have the full hang_timeout window.
                sleep.as_mut().reset(tokio::time::Instant::now() + hang_timeout);
                match read {
                    None => {
                        tracing::warn!(stream = %stream_id, "connection closed without done");
                        return ForwardOutcome::BackendCrashed;
                    }
                    Some(Err(e)) => {
                        tracing::warn!(stream = %stream_id, "ws error: {e}");
                        return ForwardOutcome::BackendCrashed;
                    }
                    Some(Ok(msg)) => match msg {
                        Message::Binary(data) => {
                            if first_chunk_time.is_none() {
                                first_chunk_time = Some(Instant::now());
                            }

                            let chunk_start = Instant::now();
                            let tagged = encode_binary_frame(&stream_id.0, &data);
                            if client_streams
                                .enqueue(stream_id, ClientEvent::Binary(tagged))
                                .is_err()
                            {
                                return ForwardOutcome::ClientGone;
                            }
                            metrics
                                .chunk_forward_seconds
                                .observe(chunk_start.elapsed().as_secs_f64());
                        }
                        Message::Text(text_data) => {
                            let backend_msg = BackendMessage::from_text(&text_data);
                            match backend_msg {
                                BackendMessage::Queued => {}
                                BackendMessage::Done { audio_duration } => {
                                    let ttfc = first_chunk_time
                                        .map(|t| t.duration_since(request_start))
                                        .unwrap_or_default();
                                    return ForwardOutcome::Success {
                                        ttfc,
                                        audio_duration,
                                    };
                                }
                                BackendMessage::Error { message } => {
                                    if message.to_lowercase().contains("busy") {
                                        return ForwardOutcome::BackendBusy;
                                    }
                                    return ForwardOutcome::BackendError(message);
                                }
                                BackendMessage::Unknown(raw) => {
                                    return ForwardOutcome::MalformedResponse(raw);
                                }
                            }
                        }
                        Message::Close(_) => {
                            return ForwardOutcome::BackendCrashed;
                        }
                        _ => {}
                    },
                }
            }
        }
    }
}

async fn check_warm_slot_liveness(ws: &mut BackendWs) -> Option<ForwardOutcome> {
    use futures_util::future::poll_immediate;
    match poll_immediate(ws.next()).await {
        None => None,
        Some(None) => Some(ForwardOutcome::BackendCrashed),
        Some(Some(Err(_))) => Some(ForwardOutcome::BackendCrashed),
        Some(Some(Ok(Message::Close(_)))) => Some(ForwardOutcome::BackendCrashed),
        Some(Some(Ok(Message::Ping(_)))) | Some(Some(Ok(Message::Pong(_)))) => None,
        Some(Some(Ok(Message::Text(t)))) => {
            let m = BackendMessage::from_text(&t);
            Some(match m {
                BackendMessage::Error { message } => {
                    if message.to_lowercase().contains("busy") {
                        ForwardOutcome::BackendBusy
                    } else {
                        ForwardOutcome::BackendError(message)
                    }
                }
                BackendMessage::Unknown(raw) => ForwardOutcome::MalformedResponse(raw),
                BackendMessage::Done { .. } => {
                    tracing::warn!(
                        "pre-start `done` on warm slot — likely a warm-pool drain bug \
                         (previous request's done frame leaked into this slot)"
                    );
                    ForwardOutcome::MalformedResponse(format!("pre-start done: {t}"))
                }
                BackendMessage::Queued => {
                    ForwardOutcome::MalformedResponse(format!("pre-start queued: {t}"))
                }
            })
        }
        Some(Some(Ok(_))) => Some(ForwardOutcome::MalformedResponse(
            "pre-start binary".to_string(),
        )),
    }
}
