use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

use crate::dispatch::{ClientEvent, PendingRequest};
use crate::metrics::Metrics;
use crate::protocol::backend::BackendMessage;
use crate::protocol::frame::encode_binary_frame;
use crate::types::{BackendId, StreamId};
use tokio_tungstenite::MaybeTlsStream;

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
    /// Not a failure for circuit/scoring; request should be retried elsewhere.
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

/// Execute a single forwarding attempt: connect to backend, send request,
/// stream audio chunks back to the client.
///
/// `cancel_rx` lets the dispatcher abort an in-flight stream on client request.
pub async fn run_forwarding(
    backend_id: BackendId,
    backend_addr: SocketAddr,
    request: PendingRequest,
    hang_timeout: Duration,
    cancel_rx: oneshot::Receiver<()>,
    metrics: Arc<Metrics>,
) -> ForwardResult {
    let stream_id = request.stream_id.clone();

    let outcome = do_forward(
        backend_addr,
        &stream_id,
        &request.text,
        request.speaker_id,
        &request.client_tx,
        hang_timeout,
        cancel_rx,
        &metrics,
    )
    .await;

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

async fn do_forward(
    backend_addr: SocketAddr,
    stream_id: &StreamId,
    text: &str,
    speaker_id: u32,
    client_tx: &mpsc::Sender<ClientEvent>,
    hang_timeout: Duration,
    mut cancel_rx: oneshot::Receiver<()>,
    metrics: &Arc<Metrics>,
) -> ForwardOutcome {
    let url = format!("ws://{}/v1/ws/speech", backend_addr);
    let connect_timeout = Duration::from_secs(3);
    let mut ws = tokio::select! {
        result = tokio::time::timeout(connect_timeout, tokio_tungstenite::connect_async(&url)) => {
            match result {
                Ok(Ok((ws, _))) => ws,
                Ok(Err(e)) => {
                    tracing::warn!(backend = %backend_addr, "connect failed: {e}");
                    return ForwardOutcome::BackendError(format!("connect failed: {e}"));
                }
                Err(_) => {
                    tracing::warn!(backend = %backend_addr, "connect timed out after {connect_timeout:?}");
                    return ForwardOutcome::BackendError("connect timeout".into());
                }
            }
        }
        _ = &mut cancel_rx => {
            return ForwardOutcome::Cancelled;
        }
    };
    nodelay_backend_stream(ws.get_ref());

    let start_json = serde_json::json!({
        "type": "start",
        "text": text,
        "speaker_id": speaker_id,
    });
    if let Err(e) = ws
        .send(Message::Text(start_json.to_string().into()))
        .await
    {
        tracing::warn!(backend = %backend_addr, "send start failed: {e}");
        return ForwardOutcome::BackendCrashed;
    }

    let request_start = Instant::now();
    let mut first_chunk_time: Option<Instant> = None;

    loop {
        tokio::select! {
            biased;
            _ = &mut cancel_rx => {
                tracing::debug!(
                    backend = %backend_addr,
                    stream = %stream_id,
                    "stream cancelled, closing backend connection"
                );
                let _ = ws.close(None).await;
                return ForwardOutcome::Cancelled;
            }
            read = tokio::time::timeout(hang_timeout, ws.next()) => {
                match read {
                    Err(_elapsed) => {
                        tracing::warn!(
                            backend = %backend_addr,
                            stream = %stream_id,
                            "hung — no data in {hang_timeout:?}"
                        );
                        return ForwardOutcome::BackendHung;
                    }
                    Ok(None) => {
                        tracing::warn!(
                            backend = %backend_addr,
                            stream = %stream_id,
                            "connection closed without done"
                        );
                        return ForwardOutcome::BackendCrashed;
                    }
                    Ok(Some(Err(e))) => {
                        tracing::warn!(
                            backend = %backend_addr,
                            stream = %stream_id,
                            "ws error: {e}"
                        );
                        return ForwardOutcome::BackendCrashed;
                    }
                    Ok(Some(Ok(msg))) => match msg {
                        Message::Binary(data) => {
                            if first_chunk_time.is_none() {
                                first_chunk_time = Some(Instant::now());
                            }

                            let chunk_start = Instant::now();
                            let tagged = encode_binary_frame(&stream_id.0, &data);
                            // Backpressure: wait for client without dropping chunks.
                            tokio::select! {
                                biased;
                                _ = &mut cancel_rx => {
                                    let _ = ws.close(None).await;
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

/// Disable Nagle on the backend TCP connection to avoid small-packet delays on TTFC.
fn nodelay_backend_stream(stream: &MaybeTlsStream<tokio::net::TcpStream>) {
    if let MaybeTlsStream::Plain(tcp) = stream {
        let _ = tcp.set_nodelay(true);
    }
}
