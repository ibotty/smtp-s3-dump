use std::collections::HashSet;
use std::env;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use ipnet::IpNet;
use smtp_server::{
    read_proxy_peer, reject_busy, shutdown_signal, Hostname, MessageSize, SessionGuard,
    SessionLimiter, Shutdown, TlsMode,
};
use sqlx::postgres::PgPoolOptions;
use tokio::io::AsyncRead;
use tokio::net::{TcpListener, TcpStream};
use tokio::signal::unix::{signal, Signal, SignalKind};
use tokio::task::JoinSet;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;
use tracing::instrument;
use tracing::{debug, error, info, warn};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use crate::smtp::{Config, SmtpSession};

mod attachment;
mod db;
mod notify;
mod s3;
mod smtp;
mod tls;

/// How long in-flight sessions get to finish on shutdown; keep below the orchestrator's
/// kill timeout (Kubernetes `terminationGracePeriodSeconds` defaults to 30s).
const SHUTDOWN_GRACE: Duration = Duration::from_secs(25);

const DEFAULT_MAX_SESSIONS: usize = 100;
const DEFAULT_MAX_SESSIONS_PER_IP: usize = 10;
const MAX_MESSAGE_SIZE: usize = 100_000_000;

/// How long a trusted proxy gets to send its PROXY header after connecting.
const PROXY_HEADER_TIMEOUT: Duration = Duration::from_secs(5);

/// Parse a positive integer limit; unset falls back to `default`, invalid/zero is an error.
fn parse_limit(name: &str, value: Option<String>, default: usize) -> Result<usize> {
    match value {
        None => Ok(default),
        Some(v) => match v.trim().parse::<usize>() {
            Ok(n) if n > 0 => Ok(n),
            _ => anyhow::bail!("env variable {name} must be a positive integer, got {v:?}"),
        },
    }
}

/// Parse the trusted proxy list (CIDRs or bare IPs). It is only used, and then required to be
/// non-empty, when a PROXY listener is configured.
fn parse_trusted(value: Option<String>, proxy_enabled: bool) -> Result<Option<Arc<[IpNet]>>> {
    if !proxy_enabled {
        if value.is_some() {
            warn!("PROXY_TRUSTED is set but SMTP_PROXY_BIND_ADDR is not: ignoring it");
        }
        return Ok(None);
    }
    let nets = value
        .iter()
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<IpNet>()
                .or_else(|_| s.parse::<IpAddr>().map(IpNet::from))
                .map_err(|_| anyhow!("env variable PROXY_TRUSTED: invalid CIDR {s:?}"))
        })
        .collect::<Result<Arc<[IpNet]>>>()?;
    anyhow::ensure!(
        !nets.is_empty(),
        "env variable PROXY_TRUSTED must list the trusted proxies when SMTP_PROXY_BIND_ADDR is set"
    );
    Ok(Some(nets))
}

fn required(name: &str) -> Result<String> {
    env::var(name).with_context(|| format!("env variable {name} not provided"))
}

fn list(name: &str) -> Option<HashSet<String>> {
    env::var(name)
        .ok()
        .map(|s| s.split(',').map(str::to_string).collect())
}

#[tokio::main]
#[instrument]
async fn main() -> Result<()> {
    // install global default tracing subscriber using RUST_LOG env variable
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    tokio_rustls::rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install default rustls crypto provider");

    let smtp_bind_addr = env::var("SMTP_BIND_ADDR").unwrap_or("0.0.0.0:2525".to_string());
    let max_sessions = parse_limit(
        "MAX_SESSIONS",
        env::var("MAX_SESSIONS").ok(),
        DEFAULT_MAX_SESSIONS,
    )?;
    let max_sessions_per_ip = parse_limit(
        "MAX_SESSIONS_PER_IP",
        env::var("MAX_SESSIONS_PER_IP").ok(),
        DEFAULT_MAX_SESSIONS_PER_IP,
    )?;
    let proxy_bind_addr = env::var("SMTP_PROXY_BIND_ADDR").ok();
    let proxy_trusted = parse_trusted(env::var("PROXY_TRUSTED").ok(), proxy_bind_addr.is_some())?;
    let smtp_domain = required("SMTP_DOMAIN")?;
    let bucket = required("BUCKET_NAME")?;
    let cert_path = required("SMTP_CERT_FILE")?;
    let key_path = required("SMTP_KEY_FILE")?;
    let database_url = required("DATABASE_URL")?;

    let allowed_rcpts = list("ALLOWED_RCPTS");
    let allowed_froms = list("ALLOWED_FROMS");
    let check_db = env::var("CHECK_ALLOWED_IN_DB").is_ok_and(|s| s == "true");

    if allowed_rcpts.is_none() && allowed_froms.is_none() && !check_db {
        warn!(
            "ALLOWED_RCPTS, ALLOWED_FROMS and CHECK_ALLOWED_IN_DB are all unset: \
             anyone who can reach this port can store mail"
        );
    }

    let resolver = tls::CertificateResolver::new(&cert_path, &key_path)?;
    // start certificate change watcher
    notify::watch_certs(resolver.clone()).await?;
    let acceptor = TlsAcceptor::from(Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_cert_resolver(resolver),
    ));

    let aws_config = aws_config::load_from_env().await;
    let s3_config = aws_sdk_s3::config::Builder::from(&aws_config)
        .force_path_style(true)
        .build();

    let pg_pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await?;

    let domain =
        Hostname::new(&smtp_domain).map_err(|e| anyhow!("could not parse SMTP_DOMAIN: {}", e))?;
    let mut server = smtp_server::Config::new(domain);
    server.max_message_size = MessageSize::new(MAX_MESSAGE_SIZE);
    info!(
        smtp_domain,
        bind_addr = smtp_bind_addr,
        proxy_bind_addr,
        proxy_trusted = proxy_trusted.as_ref().map(|t| t.len()),
        cert_path,
        key_path,
        bucket,
        s3_endpoint = aws_config.endpoint_url(),
        s3_region = aws_config.region().map(|r| r.as_ref()),
        check_db,
        allowed_rcpts = allowed_rcpts.as_ref().map(HashSet::len),
        allowed_froms = allowed_froms.as_ref().map(HashSet::len),
        max_sessions,
        max_sessions_per_ip,
        max_message_size = MAX_MESSAGE_SIZE,
        "configuration"
    );
    let config = Arc::new(Config {
        s3: aws_sdk_s3::Client::from_conf(s3_config),
        pg_pool,
        server: Arc::new(server),
        bucket,
        allowed_rcpts,
        allowed_froms,
        check_db,
    });

    let mut sigint = signal(SignalKind::interrupt()).context("failed to install SIGINT handler")?;
    let mut sigterm =
        signal(SignalKind::terminate()).context("failed to install SIGTERM handler")?;

    let (trigger, stop_rx) = shutdown_signal();
    let ctx = Arc::new(Ctx {
        config,
        acceptor,
        // guards the PROXY header read, when the real client is not known yet
        pending: SessionLimiter::new(max_sessions, None),
        clients: SessionLimiter::new(max_sessions, Some(max_sessions_per_ip)),
        stop: stop_rx,
    });
    let mut servers = JoinSet::new();
    servers.spawn(start_smtp_server(smtp_bind_addr, None, ctx.clone()));
    if let (Some(addr), Some(trusted)) = (proxy_bind_addr, proxy_trusted) {
        servers.spawn(start_smtp_server(addr, Some(trusted), ctx));
    }

    tokio::select! {
        _ = next_signal(&mut sigint, &mut sigterm) => {},
        // a server only ends on its own if it failed (e.g. bind error)
        Some(res) = servers.join_next() => return res.context("smtp server task failed")?,
    }
    info!(
        grace_secs = SHUTDOWN_GRACE.as_secs(),
        "shutting down, waiting for open sessions"
    );

    trigger.trigger();
    let drained = async {
        while let Some(res) = servers.join_next().await {
            res.context("smtp server task failed")??;
        }
        Ok::<_, anyhow::Error>(())
    };
    tokio::select! {
        res = drained => res?,
        _ = tokio::time::sleep(SHUTDOWN_GRACE) => warn!("grace period over, aborting open sessions"),
        _ = next_signal(&mut sigint, &mut sigterm) => warn!("second signal, aborting open sessions"),
    }
    // dropping the servers' JoinSets aborts whatever is still running
    servers.abort_all();

    Ok(())
}

async fn next_signal(sigint: &mut Signal, sigterm: &mut Signal) {
    tokio::select! {
        _ = sigint.recv() => {},
        _ = sigterm.recv() => {},
    }
}

/// State shared by all listeners.
struct Ctx {
    config: Arc<Config>,
    acceptor: TlsAcceptor,
    /// Every accepted connection, keyed by nothing: bounds what is in flight before the client
    /// is known.
    pending: Arc<SessionLimiter<()>>,
    /// Keyed by the real client IP (from the PROXY header on the PROXY listener).
    clients: Arc<SessionLimiter<IpAddr>>,
    stop: Shutdown,
}

/// Accept loop for one listener. With `proxy_trusted` set, every connection must come from one of
/// these networks and start with a PROXY header; without it the listener never speaks PROXY.
#[instrument(skip_all)]
async fn start_smtp_server(
    bind_addr: String,
    proxy_trusted: Option<Arc<[IpNet]>>,
    ctx: Arc<Ctx>,
) -> Result<()> {
    let listener = TcpListener::bind(&bind_addr)
        .await
        .with_context(|| format!("cannot listen on {bind_addr}"))?;
    info!(bind_addr, proxy = proxy_trusted.is_some(), "listening");

    let mut stopped = ctx.stop.clone();
    let mut sessions = JoinSet::new();
    loop {
        tokio::select! {
            _ = stopped.requested() => break,
            // reap finished sessions so the set does not grow
            Some(res) = sessions.join_next() => {
                if let Err(e) = res {
                    error!(error = %e, "session task failed");
                }
            }
            accepted = listener.accept() => match accepted {
                Ok((socket, addr)) => {
                    let Some(guard) = ctx.pending.try_acquire(()) else {
                        warn!(peer = %addr, "session limit reached, refusing connection");
                        sessions.spawn(refuse(socket, addr));
                        continue;
                    };
                    sessions.spawn(handle_connection(
                        socket,
                        addr,
                        proxy_trusted.clone(),
                        ctx.clone(),
                        guard,
                    ));
                }
                Err(e) => {
                    // e.g. out of file descriptors: keep serving, don't spin
                    error!(error = %e, "accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    drop(listener);
    info!(open_sessions = sessions.len(), "waiting for open sessions");
    while sessions.join_next().await.is_some() {}
    Ok(())
}

async fn refuse(socket: TcpStream, peer: SocketAddr) {
    if let Err(e) = reject_busy(socket).await {
        warn!(peer = %peer, error = %e, "could not send busy reply");
    }
}

async fn handle_connection(
    mut socket: TcpStream,
    tcp_addr: SocketAddr,
    proxy_trusted: Option<Arc<[IpNet]>>,
    ctx: Arc<Ctx>,
    _pending: SessionGuard<()>,
) {
    let peer = match proxy_trusted {
        None => tcp_addr,
        Some(trusted) => match proxied_peer(&mut socket, tcp_addr, &trusted).await {
            Some(peer) => peer,
            None => return,
        },
    };
    let Some(_guard) = ctx.clients.try_acquire(peer.ip().to_canonical()) else {
        warn!(peer = %peer, "session limit reached, refusing connection");
        refuse(socket, peer).await;
        return;
    };

    let mut session = SmtpSession::new(ctx.config.clone(), peer);
    if let Err(e) = smtp_server::serve(
        socket,
        &mut session,
        ctx.config.server.clone(),
        TlsMode::StartTls(ctx.acceptor.clone()),
        Some(ctx.stop.clone()),
    )
    .await
    {
        warn!(peer = %peer, error = %e, "connection ended abnormally");
    }
}

/// The real client address of a connection on the PROXY listener, or `None` (after logging) if
/// the connection has to be dropped.
async fn proxied_peer<S: AsyncRead + Unpin>(
    socket: &mut S,
    tcp_addr: SocketAddr,
    trusted: &[IpNet],
) -> Option<SocketAddr> {
    if !trusted
        .iter()
        .any(|n| n.contains(&tcp_addr.ip().to_canonical()))
    {
        warn!(peer = %tcp_addr, "untrusted peer on PROXY listener, dropping connection");
        return None;
    }
    match read_proxy_peer(socket, tcp_addr, PROXY_HEADER_TIMEOUT).await {
        Ok(peer) => {
            debug!(peer = %peer, proxy = %tcp_addr, "PROXY header accepted");
            Some(peer)
        }
        Err(e) => {
            warn!(peer = %tcp_addr, error = %e, "bad PROXY header, dropping connection");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_limit_cases() {
        assert_eq!(parse_limit("X", None, 5).unwrap(), 5);
        assert_eq!(parse_limit("X", Some("7".into()), 5).unwrap(), 7);
        assert!(parse_limit("X", Some("0".into()), 5).is_err());
        assert!(parse_limit("X", Some("abc".into()), 5).is_err());
    }

    #[test]
    fn parse_trusted_cases() {
        let t = parse_trusted(Some("10.0.0.0/8, 192.0.2.1,2001:db8::/32".into()), true)
            .unwrap()
            .unwrap();
        assert_eq!(t.len(), 3);
        assert!(t[0].contains(&"10.1.2.3".parse::<IpAddr>().unwrap()));
        assert!(t[1].contains(&"192.0.2.1".parse::<IpAddr>().unwrap()));
        assert!(!t[1].contains(&"192.0.2.2".parse::<IpAddr>().unwrap()));
        assert!(parse_trusted(None, false).unwrap().is_none());
        assert!(parse_trusted(Some("10.0.0.0/8".into()), false)
            .unwrap()
            .is_none());
        assert!(parse_trusted(None, true).is_err());
        assert!(parse_trusted(Some(" , ".into()), true).is_err());
        assert!(parse_trusted(Some("nope".into()), true).is_err());
    }

    async fn proxied(header: &[u8], tcp_addr: &str, trusted: &str) -> Option<SocketAddr> {
        let mut socket = header;
        let trusted = parse_trusted(Some(trusted.into()), true).unwrap().unwrap();
        proxied_peer(&mut socket, tcp_addr.parse().unwrap(), &trusted).await
    }

    #[tokio::test]
    async fn proxied_peer_cases() {
        let hdr = b"PROXY TCP4 192.0.2.1 198.51.100.7 56324 25\r\n";
        let client = Some("192.0.2.1:56324".parse().unwrap());
        assert_eq!(proxied(hdr, "10.1.2.3:4000", "10.0.0.0/8").await, client);
        // IPv4-mapped peer address on a dual-stack listener
        assert_eq!(
            proxied(hdr, "[::ffff:10.1.2.3]:4000", "10.0.0.0/8").await,
            client
        );
        // untrusted proxy
        assert_eq!(proxied(hdr, "192.0.2.9:4000", "10.0.0.0/8").await, None);
        // not a PROXY header
        assert_eq!(
            proxied(b"EHLO example.org\r\n", "10.1.2.3:4000", "10.0.0.0/8").await,
            None
        );
        // nothing announced: the proxy is the peer
        let local = b"\r\n\r\n\0\r\nQUIT\n\x20\x00\x00\x00";
        assert_eq!(
            proxied(local, "10.1.2.3:4000", "10.0.0.0/8").await,
            Some("10.1.2.3:4000".parse().unwrap())
        );
    }
}
