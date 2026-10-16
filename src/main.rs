#![forbid(unsafe_code)]

use anyhow::{Context, Result};
use clap::Parser;
use socks_server::{cli::Args, config::LogFormat, server::Shutdown};
use tokio::{net::TcpListener, sync::mpsc};
use tracing_subscriber::{filter::filter_fn, layer::SubscriberExt, util::SubscriberInitExt, Layer};

fn main() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("creating Tokio runtime")?;
    let result = runtime.block_on(run());
    // Cancelled DNS lookups can leave an OS resolver call in Tokio's blocking pool.
    // Connection tasks are already drained; do not let those calls hold process exit open.
    runtime.shutdown_background();
    result
}

async fn run() -> Result<()> {
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
    let (signal_task, shutdown) = shutdown_signals()?;
    let result = socks_server::server::run_with_shutdown(listener, config, shutdown).await;
    signal_task.abort();
    result
}

fn shutdown_signals() -> Result<(tokio::task::JoinHandle<()>, mpsc::Receiver<Shutdown>)> {
    // Register synchronously, before server_started, so an early signal cannot race registration.
    #[cfg(unix)]
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .context("registering SIGINT handler")?;
    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("registering SIGTERM handler")?;
    #[cfg(windows)]
    let mut interrupt = tokio::signal::windows::ctrl_c().context("registering Ctrl-C handler")?;
    let (sender, receiver) = mpsc::channel(2);
    let task = tokio::spawn(async move {
        #[cfg(unix)]
        let mut stopping = false;
        loop {
            #[cfg(unix)]
            let (mode, signal) = tokio::select! {
                _ = interrupt.recv() => (Shutdown::Immediate, "SIGINT"),
                _ = terminate.recv() => {
                    let mode = if stopping { Shutdown::Immediate } else { Shutdown::Graceful };
                    stopping = true;
                    (mode, "SIGTERM")
                }
            };
            #[cfg(windows)]
            let (mode, signal) = {
                interrupt.recv().await;
                (Shutdown::Immediate, "Ctrl-C")
            };
            #[cfg(not(any(unix, windows)))]
            let (mode, signal) = {
                if let Err(error) = tokio::signal::ctrl_c().await {
                    tracing::warn!(%error, "Ctrl-C handler failed");
                }
                (Shutdown::Immediate, "Ctrl-C")
            };
            tracing::info!(event = "shutdown_requested", signal, mode = ?mode, "shutdown requested");
            if sender.send(mode).await.is_err() || mode == Shutdown::Immediate {
                break;
            }
        }
    });
    Ok((task, receiver))
}
