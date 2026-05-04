use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

use crate::backend::BackendWs;
use crate::dispatch::{ClientEvent, PendingRequest};
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

/// Full result returned to the Dispatcher, carrying request data back for retries.
pub struct ForwardResult {
    pub backend_id: BackendId,
    pub stream_id: StreamId,
    pub outcome: ForwardOutcome,
    pub client_tx: mpsc::Sender<ClientEvent>,
    pub text: String,
    pub speaker_id: u32,
    pub retries_remaining: u32,
    pub created_at: Instant,
    pub active_count: Arc<AtomicUsize>,
    pub tried_backends: Vec<BackendId>,
}

/// Execute a single forwarding attempt using the warm `ws` connection passed in.
///
/// `cancel_rx` lets the dispatcher abort an in-flight stream on client request.
/// `drain_timeout` bounds the post-terminal read-to-EOF window.
///
/// On any terminal outcome, this function returns the `ForwardResult` to the
/// caller *immediately* and spawns a detached task to handle TCP cleanup
/// (drain to EOF + drop). The dispatcher learns about the completion without
/// waiting for the cleanup tail — this removes ~drain_timeout (default 100ms)
/// of "backend dark time" before reconnect can begin.
pub async fn run_forwarding(
    backend_id: BackendId,
    mut ws: BackendWs,
    request: PendingRequest,
    hang_timeout: Duration,
    drain_timeout: Duration,
    cancel_rx: oneshot::Receiver<()>,
    metrics: Arc<Metrics>,
) -> ForwardResult {
    let stream_id = request.stream_id.clone();

    let outcome = do_forward(
        &mut ws,
        &stream_id,
        &request.text,
        request.speaker_id,
        &request.client_tx,
        hang_timeout,
        cancel_rx,
        &metrics,
    )
    .await;

    // Decide on cleanup style based on outcome, then detach. The drain has
    // no semantic role in the request flow — it's purely TCP hygiene
    // (consume peer's FIN before sending our own = passive closer).
    let cleanup = cleanup_kind(&outcome);
    spawn_ws_cleanup(ws, drain_timeout, cleanup);

    ForwardResult {
        backend_id,
        stream_id,
        outcome,
        client_tx: request.client_tx,
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
    /// Drain reads to EOF (or timeout). Use when the peer is expected to close.
    Drain,
    /// Send our own Close (capped 50ms), then drain. Use on Cancel/ClientGone:
    /// peer doesn't support cancel and will keep producing — be polite.
    ActiveClose,
    /// Skip the drain entirely. Use for hung backends: server is sleeping
    /// and will not FIN, so the drain would just sit idle for the timeout.
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
        // Just drop the ws on this thread; no need for a task.
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
    client_tx: &mpsc::Sender<ClientEvent>,
    hang_timeout: Duration,
    mut cancel_rx: oneshot::Receiver<()>,
    metrics: &Arc<Metrics>,
) -> ForwardOutcome {
    // Liveness check: if the warm slot already has buffered data (pre-start
    // error from REFUSE chaos, server crashed during the gap, etc.), handle it
    // before sending start.
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

    loop {
        tokio::select! {
            biased;
            _ = &mut cancel_rx => {
                tracing::debug!(stream = %stream_id, "stream cancelled");
                return ForwardOutcome::Cancelled;
            }
            read = tokio::time::timeout(hang_timeout, ws.next()) => {
                match read {
                    Err(_elapsed) => {
                        tracing::warn!(stream = %stream_id, "hung — no data in {hang_timeout:?}");
                        return ForwardOutcome::BackendHung;
                    }
                    Ok(None) => {
                        tracing::warn!(stream = %stream_id, "connection closed without done");
                        return ForwardOutcome::BackendCrashed;
                    }
                    Ok(Some(Err(e))) => {
                        tracing::warn!(stream = %stream_id, "ws error: {e}");
                        return ForwardOutcome::BackendCrashed;
                    }
                    Ok(Some(Ok(msg))) => match msg {
                        Message::Binary(data) => {
                            if first_chunk_time.is_none() {
                                first_chunk_time = Some(Instant::now());
                            }

                            let chunk_start = Instant::now();
                            let tagged = encode_binary_frame(&stream_id.0, &data);
                            tokio::select! {
                                biased;
                                _ = &mut cancel_rx => {
                                    return ForwardOutcome::Cancelled;
                                }
                                send = client_tx.send(ClientEvent::Binary(tagged)) => {
                                    if send.is_err() {
                                        return ForwardOutcome::ClientGone;
                                    }
                                }
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

/// Non-blocking poll of the warm slot before sending start.
/// Returns Some(outcome) if the connection is dead or has buffered data,
/// None if Pending (slot is alive — proceed with start).
async fn check_warm_slot_liveness(ws: &mut BackendWs) -> Option<ForwardOutcome> {
    use futures_util::future::poll_immediate;
    match poll_immediate(ws.next()).await {
        None => None, // Pending: alive
        Some(None) => Some(ForwardOutcome::BackendCrashed),
        Some(Some(Err(_))) => Some(ForwardOutcome::BackendCrashed),
        Some(Some(Ok(Message::Close(_)))) => Some(ForwardOutcome::BackendCrashed),
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
