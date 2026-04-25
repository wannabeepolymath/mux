use std::net::SocketAddr;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::dispatch::{ClientEvent, PendingRequest};
use crate::protocol::backend::BackendMessage;
use crate::protocol::frame::encode_binary_frame;
use crate::types::{BackendId, StreamId};

/// Outcome of a forwarding attempt.
#[derive(Debug)]
pub enum ForwardOutcome {
    Success {
        ttfc: Duration,
        audio_duration: f64,
        total_time: f64,
        rtf: f64,
        total_bytes: usize,
    },
    BackendCrashed,
    BackendHung,
    MalformedResponse(String),
    BackendError(String),
    ClientGone,
}

/// Full result returned to the Dispatcher, carrying request data back for retries.
pub struct ForwardResult {
    pub backend_id: BackendId,
    pub stream_id: StreamId,
    pub outcome: ForwardOutcome,
    pub client_tx: mpsc::Sender<ClientEvent>,
    pub text: String,
    pub speaker_id: u32,
    pub priority: u32,
    pub retries_remaining: u32,
    pub created_at: Instant,
}

/// Execute a single forwarding attempt: connect to backend, send request,
/// stream audio chunks back to the client.
pub async fn run_forwarding(
    backend_id: BackendId,
    backend_addr: SocketAddr,
    request: PendingRequest,
    hang_timeout: Duration,
) -> ForwardResult {
    let stream_id = request.stream_id.clone();

    let outcome = do_forward(
        backend_addr,
        &stream_id,
        &request.text,
        request.speaker_id,
        &request.client_tx,
        hang_timeout,
    )
    .await;

    ForwardResult {
        backend_id,
        stream_id,
        outcome,
        client_tx: request.client_tx,
        text: request.text,
        speaker_id: request.speaker_id,
        priority: request.priority,
        retries_remaining: request.retries_remaining,
        created_at: request.created_at,
    }
}

async fn do_forward(
    backend_addr: SocketAddr,
    stream_id: &StreamId,
    text: &str,
    speaker_id: u32,
    client_tx: &mpsc::Sender<ClientEvent>,
    hang_timeout: Duration,
) -> ForwardOutcome {
    let url = format!("ws://{}/v1/ws/speech", backend_addr);
    let mut ws = match tokio_tungstenite::connect_async(&url).await {
        Ok((ws, _)) => ws,
        Err(e) => {
            tracing::warn!(backend = %backend_addr, "connect failed: {e}");
            return ForwardOutcome::BackendError(format!("connect failed: {e}"));
        }
    };

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
    let mut total_bytes: usize = 0;

    loop {
        let read = tokio::time::timeout(hang_timeout, ws.next()).await;

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
                    total_bytes += data.len();

                    let tagged = encode_binary_frame(&stream_id.0, &data);
                    if send_to_client(client_tx, ClientEvent::Binary(tagged)).is_err() {
                        return ForwardOutcome::ClientGone;
                    }
                }
                Message::Text(text_data) => {
                    let backend_msg = BackendMessage::from_text(&text_data);
                    match backend_msg {
                        BackendMessage::Queued { .. } => {}
                        BackendMessage::Done {
                            audio_duration,
                            total_time,
                            rtf,
                            ..
                        } => {
                            let ttfc = first_chunk_time
                                .map(|t| t.duration_since(request_start))
                                .unwrap_or_default();

                            return ForwardOutcome::Success {
                                ttfc,
                                audio_duration,
                                total_time,
                                rtf,
                                total_bytes,
                            };
                        }
                        BackendMessage::Error { message } => {
                            return ForwardOutcome::BackendError(message);
                        }
                        BackendMessage::Unknown(raw) => {
                            return ForwardOutcome::MalformedResponse(raw);
                        }
                        BackendMessage::AudioData(_) => {}
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

fn send_to_client(tx: &mpsc::Sender<ClientEvent>, event: ClientEvent) -> Result<(), ()> {
    match tx.try_send(event) {
        Ok(()) => Ok(()),
        Err(mpsc::error::TrySendError::Closed(_)) => {
            tracing::debug!("client disconnected, stopping forward");
            Err(())
        }
        Err(mpsc::error::TrySendError::Full(_)) => {
            tracing::warn!("client buffer full, dropping chunk");
            Ok(())
        }
    }
}
