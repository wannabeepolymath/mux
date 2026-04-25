mod backend;
mod client;
mod config;
mod dispatch;
mod error;
mod protocol;
mod types;

use clap::Parser;
use config::{CliArgs, Config};

fn main() {
    let args = CliArgs::parse();
    let config = match Config::from_cli(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("configuration error: {e}");
            std::process::exit(1);
        }
    };

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!("TTS Multiplexer starting");
    tracing::info!("  listen: {}", config.listen_addr);
    tracing::info!("  metrics: {}", config.metrics_addr);
    tracing::info!(
        "  backends: {:?}",
        config
            .backend_addrs
            .iter()
            .map(|a| a.to_string())
            .collect::<Vec<_>>()
    );
    tracing::info!("  hang_timeout: {:?}", config.hang_timeout);
    tracing::info!("  circuit_threshold: {}", config.circuit_threshold);
    tracing::info!("  circuit_cooldown: {:?}", config.circuit_cooldown);
    tracing::info!("  max_queue_depth: {}", config.max_queue_depth);
    tracing::info!("  max_streams_per_conn: {}", config.max_streams_per_conn);
    tracing::info!("  max_retries: {}", config.max_retries);

    // TODO: Phase 3+ — start the tokio runtime and run the server
    tracing::info!("Configuration validated. Server not yet implemented.");
}
