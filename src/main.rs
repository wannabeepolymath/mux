mod backend;
mod client;
mod config;
mod dispatch;
mod error;
mod forward;
mod health;
mod metrics;
mod protocol;
mod server;
mod types;

use std::sync::Arc;

use clap::Parser;
use config::{CliArgs, Config};
use dispatch::Dispatcher;
use metrics::Metrics;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() {
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
        "  backends: {}",
        config
            .backend_addrs
            .iter()
            .map(|a| a.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    tracing::info!("  hang_timeout: {:?}", config.hang_timeout);
    tracing::info!("  max_retries: {}", config.max_retries);
    tracing::info!("  max_queue_depth: {}", config.max_queue_depth);
    tracing::info!("  max_streams_per_conn: {}", config.max_streams_per_conn);

    let config = Arc::new(config);

    let metrics = match Metrics::new() {
        Ok(m) => Arc::new(m),
        Err(e) => {
            tracing::error!("failed to create metrics: {e}");
            std::process::exit(1);
        }
    };

    let listener = match TcpListener::bind(config.listen_addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("failed to bind {}: {e}", config.listen_addr);
            std::process::exit(1);
        }
    };

    let dispatcher = Dispatcher::new(config.clone(), metrics.clone());
    let dispatch_tx = dispatcher.sender();

    tokio::spawn(dispatcher.run());

    tokio::spawn(health::run_http_server(
        config.metrics_addr,
        dispatch_tx.clone(),
        metrics.clone(),
    ));

    server::run_server(listener, dispatch_tx, config, metrics).await;
}
