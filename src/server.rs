use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::accept_async;

use crate::client;
use crate::config::Config;
use crate::dispatch::DispatchEvent;
use crate::metrics::Metrics;

/// Accept client WebSocket connections and spawn a handler for each.
pub async fn run_server(
    listener: TcpListener,
    dispatch_tx: mpsc::Sender<DispatchEvent>,
    config: Arc<Config>,
    metrics: Arc<Metrics>,
) {
    tracing::info!("WebSocket server listening on {}", config.listen_addr);

    loop {
        let (tcp_stream, addr) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                tracing::error!("tcp accept error: {e}");
                continue;
            }
        };
        if let Err(e) = tcp_stream.set_nodelay(true) {
            tracing::debug!(client = %addr, "set_nodelay on client: {e}");
        }

        let dispatch_tx = dispatch_tx.clone();
        let config = config.clone();
        let metrics = metrics.clone();

        tokio::spawn(async move {
            match accept_async(tcp_stream).await {
                Ok(ws) => {
                    client::handle_client(ws, dispatch_tx, config, metrics, addr).await;
                }
                Err(e) => {
                    tracing::debug!(client = %addr, "ws handshake failed: {e}");
                }
            }
        });
    }
}
