use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// The owned WebSocket connection type held in a backend's warm slot.
pub type BackendWs = WebSocketStream<MaybeTlsStream<TcpStream>>;
