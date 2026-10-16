#![forbid(unsafe_code)]

use anyhow::{Context, Result};
use clap::Parser;
use socks_server::{cli::Args, config::LogFormat};
use tokio::net::TcpListener;
use tracing_subscriber::{filter::filter_fn, layer::SubscriberExt, util::SubscriberInitExt, Layer};

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let config = args.load_config()?;
    if args.check {
        println!("configuration valid");
        return Ok(());
    }
    // Only our own events are enabled. Upstream fast-socks5 debug logs contain password bytes.
    // Do not accept RUST_LOG overrides that could accidentally expose them.
    let level = config.logging.level_filter()?;
    // Keep connection spans enabled even at warn/error so those events retain client metadata.
    let filter = filter_fn(move |metadata| {
        metadata.target().starts_with("socks_server")
            && (metadata.is_span() || level >= *metadata.level())
    });
    match config.logging.format {
        LogFormat::Json => tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_writer(std::io::stderr)
                    .with_filter(filter),
            )
            .try_init()?,
        LogFormat::Text => tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(std::io::stderr)
                    .with_filter(filter),
            )
            .try_init()?,
    }
    let listener = TcpListener::bind(config.listen)
        .await
        .context("binding SOCKS5 listener")?;
    let shutdown = shutdown_signal()?;
    socks_server::server::run_until(listener, config, shutdown).await
}

fn shutdown_signal() -> Result<impl std::future::Future<Output = ()>> {
    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("registering SIGTERM handler")?;
    Ok(async move {
        #[cfg(unix)]
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                if let Err(error) = result { tracing::warn!(%error, "Ctrl-C handler failed"); }
            }
            _ = terminate.recv() => {}
        }
        #[cfg(not(unix))]
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::warn!(%error, "Ctrl-C handler failed");
        }
    })
}
