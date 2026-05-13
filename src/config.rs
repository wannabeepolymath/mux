use std::net::SocketAddr;
use std::time::Duration;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "tts-multiplexer", about = "Chaos-resilient TTS streaming multiplexer")]
pub struct CliArgs {
    /// Port to listen on for client WebSocket connections
    #[arg(long, default_value = "9000")]
    pub port: u16,

    /// Port for health/metrics HTTP server
    #[arg(long, default_value = "9001")]
    pub metrics_port: u16,

    /// Backend addresses (host:port), comma-separated
    #[arg(long, value_delimiter = ',', required = true)]
    pub backends: Vec<String>,

    /// Hang detection timeout in seconds
    #[arg(long, default_value = "5")]
    pub hang_timeout_secs: u64,

    /// Circuit breaker consecutive failure threshold
    #[arg(long, default_value = "3")]
    pub circuit_threshold: u32,

    /// Circuit breaker cooldown in seconds
    #[arg(long, default_value = "1")]
    pub circuit_cooldown_secs: u64,

    /// Max global queue depth
    #[arg(long, default_value = "64")]
    pub max_queue_depth: usize,

    /// Max concurrent streams per client connection
    #[arg(long, default_value = "4")]
    pub max_streams_per_conn: usize,

    /// Max retry attempts per request (so total attempts = 1 + max_retries)
    #[arg(long, default_value = "3")]
    pub max_retries: u32,

    /// Connection drain timeout in ms. Bounded read-to-EOF window after
    /// the terminal frame so the multiplexer ends as the passive TCP closer.
    #[arg(long, default_value = "100")]
    pub drain_timeout_ms: u64,

    /// Connect timeout in seconds for backend WebSocket handshake.
    #[arg(long, default_value = "3")]
    pub connect_timeout_secs: u64,

    /// Per-stream client-egress buffer size in bytes (drop-oldest at cap).
    #[arg(long, default_value = "262144")]
    pub client_buffer_bytes: usize,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub listen_addr: SocketAddr,
    pub metrics_addr: SocketAddr,
    pub backend_addrs: Vec<SocketAddr>,
    pub hang_timeout: Duration,
    pub circuit_threshold: u32,
    pub circuit_cooldown: Duration,
    pub max_queue_depth: usize,
    pub max_streams_per_conn: usize,
    pub max_retries: u32,
    pub drain_timeout: Duration,
    pub connect_timeout: Duration,
    pub client_buffer_bytes: usize,
}

impl Config {
    pub fn from_cli(args: CliArgs) -> Result<Self, String> {
        let backend_addrs: Result<Vec<SocketAddr>, _> = args
            .backends
            .iter()
            .map(|s| {
                s.parse::<SocketAddr>()
                    .map_err(|e| format!("invalid backend address '{}': {}", s, e))
            })
            .collect();

        Ok(Self {
            listen_addr: SocketAddr::from(([0, 0, 0, 0], args.port)),
            metrics_addr: SocketAddr::from(([0, 0, 0, 0], args.metrics_port)),
            backend_addrs: backend_addrs?,
            hang_timeout: Duration::from_secs(args.hang_timeout_secs),
            circuit_threshold: args.circuit_threshold,
            circuit_cooldown: Duration::from_secs(args.circuit_cooldown_secs),
            max_queue_depth: args.max_queue_depth,
            max_streams_per_conn: args.max_streams_per_conn,
            max_retries: args.max_retries,
            drain_timeout: Duration::from_millis(args.drain_timeout_ms),
            connect_timeout: Duration::from_secs(args.connect_timeout_secs),
            client_buffer_bytes: args.client_buffer_bytes,
        })
    }
}
