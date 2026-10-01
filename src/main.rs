use std::collections::HashSet;
use std::env;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use smtp_server::{
    reject_busy, shutdown_signal, Hostname, MessageSize, SessionLimiter, Shutdown, TlsMode,
};
use sqlx::postgres::PgPoolOptions;
use tokio::net::TcpListener;
use tokio::signal::unix::{signal, Signal, SignalKind};
use tokio::task::JoinSet;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;
use tracing::instrument;
use tracing::{error, info, warn};
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
    let limiter = SessionLimiter::new(
        parse_limit(
            "MAX_SESSIONS",
            env::var("MAX_SESSIONS").ok(),
            DEFAULT_MAX_SESSIONS,
        )?,
        Some(parse_limit(
            "MAX_SESSIONS_PER_IP",
            env::var("MAX_SESSIONS_PER_IP").ok(),
            DEFAULT_MAX_SESSIONS_PER_IP,
        )?),
    );
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
    server.max_message_size = MessageSize::new(100_000_000);
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
    let mut server = tokio::spawn(start_smtp_server(
        smtp_bind_addr,
        config,
        acceptor,
        limiter,
        stop_rx,
    ));

    tokio::select! {
        _ = next_signal(&mut sigint, &mut sigterm) => {},
        // the server only ends on its own if it failed (e.g. bind error)
        res = &mut server => return res.context("smtp server task failed")?,
    }
    info!(
        "shutting down, waiting up to {:?} for open sessions",
        SHUTDOWN_GRACE
    );

    trigger.trigger();
    tokio::select! {
        res = &mut server => res.context("smtp server task failed")??,
        _ = tokio::time::sleep(SHUTDOWN_GRACE) => warn!("grace period over, aborting open sessions"),
        _ = next_signal(&mut sigint, &mut sigterm) => warn!("second signal, aborting open sessions"),
    }
    // dropping the server task's JoinSet aborts whatever is still running
    server.abort();

    Ok(())
}

async fn next_signal(sigint: &mut Signal, sigterm: &mut Signal) {
    tokio::select! {
        _ = sigint.recv() => {},
        _ = sigterm.recv() => {},
    }
}

#[instrument(skip_all)]
async fn start_smtp_server(
    smtp_bind_addr: String,
    config: Arc<Config>,
    acceptor: TlsAcceptor,
    limiter: Arc<SessionLimiter<IpAddr>>,
    stop: Shutdown,
) -> Result<()> {
    info!("listening on {}", smtp_bind_addr);
    let listener = TcpListener::bind(&smtp_bind_addr)
        .await
        .with_context(|| format!("cannot listen on {smtp_bind_addr}"))?;

    let mut stopped = stop.clone();
    let mut sessions = JoinSet::new();
    loop {
        tokio::select! {
            _ = stopped.requested() => break,
            // reap finished sessions so the set does not grow
            Some(res) = sessions.join_next() => {
                if let Err(e) = res {
                    error!("session task failed: {}", e);
                }
            }
            accepted = listener.accept() => match accepted {
                Ok((socket, addr)) => {
                    let Some(guard) = limiter.try_acquire(addr.ip()) else {
                        warn!("session limit reached, refusing connection from {}", addr);
                        sessions.spawn(async move {
                            if let Err(e) = reject_busy(socket).await {
                                warn!("could not send busy reply to {}: {}", addr, e);
                            }
                        });
                        continue;
                    };
                    let mut session = SmtpSession::new(config.clone(), addr);
                    let server_config = config.server.clone();
                    let acceptor = acceptor.clone();
                    let stop = stop.clone();
                    sessions.spawn(async move {
                        let _guard = guard;
                        if let Err(e) = smtp_server::serve(
                            socket,
                            &mut session,
                            server_config,
                            TlsMode::StartTls(acceptor),
                            Some(stop),
                        )
                        .await
                        {
                            warn!(peer = %addr, error = %e, "connection ended abnormally");
                        }
                    });
                }
                Err(e) => {
                    // e.g. out of file descriptors: keep serving, don't spin
                    error!("accept failed: {}", e);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    drop(listener);
    info!("waiting for {} open session(s)", sessions.len());
    while sessions.join_next().await.is_some() {}
    Ok(())
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
}
