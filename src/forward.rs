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
        drain_timeout,
        cancel_rx,
        &metrics,
    )
    .await;

    // ws is dropped here — by this point either drain_close has consumed the
    // peer's FIN (passive-close path) or the peer never sent FIN (active-close
    // path: hang/cancel). Either way, the slot is consumed.
    drop(ws);

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

#[allow(clippy::too_many_arguments)]
async fn do_forward(
    ws: &mut BackendWs,
    stream_id: &StreamId,
    text: &str,
    speaker_id: u32,
    client_tx: &mpsc::Sender<ClientEvent>,
    hang_timeout: Duration,
    drain_timeout: Duration,
    mut cancel_rx: oneshot::Receiver<()>,
    metrics: &Arc<Metrics>,
) -> ForwardOutcome {
    // Liveness check: if the warm slot already has buffered data (pre-start
    // error from REFUSE chaos, server crashed during the gap, etc.), handle it
    // before sending start.
    if let Some(early) = check_warm_slot_liveness(ws).await {
        drain_close(ws, drain_timeout).await;
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
        drain_close(ws, drain_timeout).await;
        return ForwardOutcome::BackendCrashed;
    }

    let request_start = Instant::now();
    let mut first_chunk_time: Option<Instant> = None;

    loop {
        tokio::select! {
            biased;
            _ = &mut cancel_rx => {
                tracing::debug!(stream = %stream_id, "stream cancelled");
                // Best-effort: try to send WS Close, then short drain. Backend
                // doesn't support cancel, so we'll likely end as active closer.
                active_close(ws, drain_timeout).await;
                return ForwardOutcome::Cancelled;
            }
            read = tokio::time::timeout(hang_timeout, ws.next()) => {
                match read {
                    Err(_elapsed) => {
                        tracing::warn!(stream = %stream_id, "hung — no data in {hang_timeout:?}");
                        // No drain — server is sleeping and won't FIN.
                        return ForwardOutcome::BackendHung;
                    }
                    Ok(None) => {
                        tracing::warn!(stream = %stream_id, "connection closed without done");
                        // Peer already closed; drain is a no-op but cheap.
                        drain_close(ws, drain_timeout).await;
                        return ForwardOutcome::BackendCrashed;
                    }
                    Ok(Some(Err(e))) => {
                        tracing::warn!(stream = %stream_id, "ws error: {e}");
                        drain_close(ws, drain_timeout).await;
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
                                    active_close(ws, drain_timeout).await;
                                    return ForwardOutcome::Cancelled;
                                }
                                send = client_tx.send(ClientEvent::Binary(tagged)) => {
                                    if send.is_err() {
                                        // Client gone: backend keeps producing. Drain briefly
                                        // and drop. Active-close cost on this path is acceptable
                                        // (rare).
                                        active_close(ws, drain_timeout).await;
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
                                    drain_close(ws, drain_timeout).await;
                                    return ForwardOutcome::Success {
                                        ttfc,
                                        audio_duration,
                                    };
                                }
                                BackendMessage::Error { message } => {
                                    drain_close(ws, drain_timeout).await;
                                    if message.to_lowercase().contains("busy") {
                                        return ForwardOutcome::BackendBusy;
                                    }
                                    return ForwardOutcome::BackendError(message);
                                }
                                BackendMessage::Unknown(raw) => {
                                    drain_close(ws, drain_timeout).await;
                                    return ForwardOutcome::MalformedResponse(raw);
                                }
                            }
                        }
                        Message::Close(_) => {
                            drain_close(ws, drain_timeout).await;
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

/// Best-effort active close: send WS Close frame (capped at 50ms), then drain.
/// Used on Cancel/ClientGone paths where we want to be polite even though the
/// server doesn't support cancellation and will likely keep producing frames.
async fn active_close(ws: &mut BackendWs, drain_timeout: Duration) {
    let _ = tokio::time::timeout(Duration::from_millis(50), ws.close(None)).await;
    drain_close(ws, drain_timeout).await;
}

/// Drain reads to EOF (or timeout) so the peer's FIN arrives before our drop
/// sends ours — making us the passive TCP closer.
async fn drain_close(ws: &mut BackendWs, deadline: Duration) {
    let _ = tokio::time::timeout(deadline, async {
        while ws.next().await.is_some() {}
    })
    .await;
}
