use std::{
    future::Future,
    io,
    pin::Pin,
    sync::Arc,
    task::{Context as TaskContext, Poll},
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use fast_socks5::{
    server::Socks5ServerProtocol, util::target_addr::TargetAddr, ReplyError, Socks5Command,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{watch, Semaphore},
    task::JoinSet,
    time::timeout,
};
use tracing::{field, info, info_span, warn, Instrument};

use crate::config::Config;

/// Serve until shutdown resolves, then drain existing connections for the configured grace period.
pub async fn run_until(
    listener: TcpListener,
    config: Config,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    config.validate()?;
    let config = Arc::new(config);
    let permits = Arc::new(Semaphore::new(config.max_connections));
    let (force_shutdown, _) = watch::channel(false);
    let mut tasks = JoinSet::new();
    let mut next_id = 0_u64;
    info!(event = "server_started", listen = %listener.local_addr()?, auth_enabled = config.auth.enabled,
        ip_filter_enabled = config.ip_filter.enabled, max_connections = config.max_connections, "server listening");
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => break,
            Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                if let Err(error) = result { warn!(event = "connection_task_failed", %error); }
            }
            accepted = listener.accept() => {
                let (socket, peer) = accepted.context("accepting client connection")?;
                next_id += 1;
                let span = info_span!("connection", connection_id = next_id,
                    client_ip = %peer.ip(), client_port = peer.port(), destination = field::Empty, username = field::Empty);
                if !config.ip_filter.allows(peer.ip()) {
                    warn!(parent: &span, event = "connection_rejected", reason = "ip_denied", "client IP denied");
                    continue;
                }
                let Ok(permit) = permits.clone().try_acquire_owned() else {
                    warn!(parent: &span, event = "connection_rejected", reason = "connection_limit", "connection limit reached");
                    continue;
                };
                let config = config.clone();
                let mut force = force_shutdown.subscribe();
                tasks.spawn(async move {
                    let _permit = permit;
                    let started = Instant::now();
                    let mut session = Session::default();
                    info!(event = "connection_accepted", "client connected");
                    let result = tokio::select! {
                        result = serve(socket, &config, &mut session) => result,
                        _ = force.changed() => {
                            session.stage = "shutdown";
                            Err(anyhow::anyhow!("shutdown grace period expired"))
                        }
                    };
                    let elapsed_ms = started.elapsed().as_millis() as u64;
                    match result {
                        Ok(()) => info!(event = "connection_closed", outcome = "success", elapsed_ms,
                            bytes_up = session.traffic.up, bytes_down = session.traffic.down, "connection closed"),
                        Err(error) => warn!(event = "connection_closed", outcome = "failed", stage = session.stage,
                            error = %format!("{error:#}"), elapsed_ms, bytes_up = session.traffic.up,
                            bytes_down = session.traffic.down, "connection failed"),
                    }
                }.instrument(span));
            }
        }
    }
    drop(listener);
    info!(
        event = "server_stopping",
        active_connections = tasks.len(),
        "draining connections"
    );
    if timeout(Duration::from_secs(config.shutdown_timeout_secs), async {
        while let Some(result) = tasks.join_next().await {
            if let Err(error) = result {
                warn!(event = "connection_task_failed", %error);
            }
        }
    })
    .await
    .is_err()
    {
        let _ = force_shutdown.send(true);
        while let Some(result) = tasks.join_next().await {
            if let Err(error) = result {
                warn!(event = "connection_task_failed", %error);
            }
        }
    }
    info!(event = "server_stopped", "server stopped");
    Ok(())
}

#[derive(Default)]
struct Session {
    stage: &'static str,
    traffic: Traffic,
}

async fn serve(socket: TcpStream, config: &Config, session: &mut Session) -> Result<()> {
    socket.set_nodelay(true)?;
    session.stage = "handshake";
    let (proto, command, destination) =
        timeout(Duration::from_secs(config.handshake_timeout_secs), async {
            let proto = if config.auth.enabled {
                session.stage = "authentication";
                Socks5ServerProtocol::accept_password_auth(socket, |username, password| {
                    tracing::Span::current().record("username", &username);
                    let accepted = config.auth.accepts(&username, &password);
                    if accepted {
                        info!(
                            event = "authentication_succeeded",
                            "password authentication accepted"
                        );
                    } else {
                        warn!(
                            event = "authentication_rejected",
                            "password authentication rejected"
                        );
                    }
                    accepted
                })
                .await?
                .0
            } else {
                Socks5ServerProtocol::accept_no_auth(socket).await?
            };
            session.stage = "request";
            proto.read_command().await
        })
        .await
        .context("SOCKS5 handshake/request timed out")??;

    tracing::Span::current().record("destination", field::display(&destination));
    info!(event = "proxy_requested", command = ?command, "proxy requested");
    if command != Socks5Command::TCPConnect {
        session.stage = "command";
        proto.reply_error(&ReplyError::CommandNotSupported).await?;
        bail!("only TCP CONNECT is supported");
    }

    session.stage = "connect";
    // Tokio resolves domains asynchronously and tries every returned address within one deadline.
    let connection = timeout(Duration::from_secs(config.connect_timeout_secs), async {
        match &destination {
            TargetAddr::Ip(address) => TcpStream::connect(address).await,
            TargetAddr::Domain(host, port) => TcpStream::connect((host.as_str(), *port)).await,
        }
    })
    .await;
    let mut outbound = match connection {
        Ok(Ok(socket)) => socket,
        result => {
            let (reply, error) = match result {
                Err(_) => (
                    ReplyError::TtlExpired,
                    anyhow::anyhow!("destination connection timed out"),
                ),
                Ok(Err(error)) => (connect_reply(&error), anyhow::Error::new(error)),
                Ok(Ok(_)) => unreachable!(),
            };
            proto
                .reply_error(&reply)
                .await
                .context("sending connection failure reply")?;
            return Err(error.context("connecting to destination"));
        }
    };
    outbound.set_nodelay(true)?;
    let client = proto.reply_success(outbound.local_addr()?).await?;
    info!(event = "proxy_connected", upstream = %outbound.peer_addr()?, "destination connected");
    session.stage = "relay";
    let mut client = Metered {
        stream: client,
        traffic: &mut session.traffic,
    };
    tokio::io::copy_bidirectional(&mut client, &mut outbound)
        .await
        .context("relaying TCP traffic")?;
    client.shutdown().await?;
    Ok(())
}

fn connect_reply(error: &io::Error) -> ReplyError {
    match error.kind() {
        io::ErrorKind::ConnectionRefused => ReplyError::ConnectionRefused,
        io::ErrorKind::PermissionDenied => ReplyError::ConnectionNotAllowed,
        io::ErrorKind::NetworkUnreachable => ReplyError::NetworkUnreachable,
        io::ErrorKind::HostUnreachable => ReplyError::HostUnreachable,
        io::ErrorKind::TimedOut => ReplyError::TtlExpired,
        _ => ReplyError::HostUnreachable,
    }
}

#[derive(Default)]
struct Traffic {
    up: u64,
    down: u64,
}

// Count only relay payload, excluding SOCKS5 negotiation. Counts survive errors and shutdown.
struct Metered<'a> {
    stream: TcpStream,
    traffic: &'a mut Traffic,
}

impl AsyncRead for Metered<'_> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let result = Pin::new(&mut this.stream).poll_read(cx, buf);
        this.traffic.up += (buf.filled().len() - before) as u64;
        result
    }
}

impl AsyncWrite for Metered<'_> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.stream).poll_write(cx, buf);
        if let Poll::Ready(Ok(count)) = result {
            this.traffic.down += count as u64;
        }
        result
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_shutdown(cx)
    }
}
