pub mod stream;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

use crate::config::Config;
use crate::dispatch::{ClientEvent, DispatchEvent, PendingRequest};
use crate::protocol::client::ClientMessage;
use crate::types::StreamId;

/// Handle a single client WebSocket connection.
pub async fn handle_client(
    ws: WebSocketStream<TcpStream>,
    dispatch_tx: mpsc::Sender<DispatchEvent>,
    config: Arc<Config>,
    client_addr: std::net::SocketAddr,
) {
    let (ws_sink, mut ws_stream) = ws.split();
    let (client_tx, client_rx) = mpsc::channel::<ClientEvent>(64);

    let writer_handle = tokio::spawn(client_writer(ws_sink, client_rx));

    tracing::debug!(client = %client_addr, "client connected");

    let active_count = Arc::new(AtomicUsize::new(0));

    while let Some(msg_result) = ws_stream.next().await {
        let msg = match msg_result {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(client = %client_addr, "ws read error: {e}");
                break;
            }
        };

        match msg {
            Message::Text(text) => {
                let parsed: ClientMessage = match serde_json::from_str(&text) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!(client = %client_addr, "invalid message: {e}");
                        let err_json = serde_json::json!({
                            "type": "error",
                            "stream_id": "",
                            "message": format!("invalid message: {e}"),
                        });
                        let _ = client_tx.try_send(ClientEvent::Text(err_json.to_string()));
                        continue;
                    }
                };

                match parsed {
                    ClientMessage::Start {
                        stream_id,
                        text,
                        speaker_id,
                        priority,
                    } => {
                        let current = active_count.load(Ordering::Relaxed);
                        if current >= config.max_streams_per_conn {
                            let err_json = serde_json::json!({
                                "type": "error",
                                "stream_id": stream_id,
                                "message": format!(
                                    "stream limit exceeded ({} per connection)",
                                    config.max_streams_per_conn
                                ),
                            });
                            let _ =
                                client_tx.try_send(ClientEvent::Text(err_json.to_string()));
                            continue;
                        }

                        active_count.fetch_add(1, Ordering::Relaxed);

                        let req = PendingRequest {
                            stream_id: StreamId(stream_id),
                            text,
                            speaker_id,
                            priority,
                            client_tx: client_tx.clone(),
                            retries_remaining: config.max_retries,
                            created_at: Instant::now(),
                            active_count: active_count.clone(),
                            tried_backends: Vec::new(),
                        };

                        if dispatch_tx
                            .send(DispatchEvent::NewRequest(req))
                            .await
                            .is_err()
                        {
                            tracing::error!("dispatcher channel closed");
                            break;
                        }
                    }
                    ClientMessage::Cancel { stream_id } => {
                        if dispatch_tx
                            .send(DispatchEvent::CancelStream {
                                stream_id: StreamId(stream_id),
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    ClientMessage::Close => {
                        tracing::debug!(client = %client_addr, "client sent close");
                        break;
                    }
                }
            }
            Message::Close(_) => {
                tracing::debug!(client = %client_addr, "client closed connection");
                break;
            }
            _ => {}
        }
    }

    drop(client_tx);
    let _ = writer_handle.await;
    tracing::debug!(client = %client_addr, "client handler finished");
}

/// Dedicated writer task: reads ClientEvents and sends them over the WebSocket.
async fn client_writer(
    mut sink: futures_util::stream::SplitSink<WebSocketStream<TcpStream>, Message>,
    mut rx: mpsc::Receiver<ClientEvent>,
) {
    while let Some(event) = rx.recv().await {
        let msg = match event {
            ClientEvent::Text(text) => Message::Text(text.into()),
            ClientEvent::Binary(data) => Message::Binary(data),
        };

        if let Err(e) = sink.send(msg).await {
            tracing::debug!("client write error: {e}");
            break;
        }
    }
}
