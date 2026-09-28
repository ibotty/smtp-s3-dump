use std::env;
use std::time::Duration;

use anyhow::{Context, Result};
use smtp_server::{shutdown_signal, Shutdown, TlsMode};
use sqlx::postgres::PgPoolOptions;
use tokio::net::TcpListener;
use tokio::signal::unix::{signal, Signal, SignalKind};
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use tracing::instrument;
use tracing::{error, info, warn};
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use crate::smtp::SmtpBackend;

mod attachment;
mod db;
mod notify;
mod s3;
mod smtp;
mod tls;

/// How long in-flight sessions get to finish on shutdown; keep below the orchestrator's
/// kill timeout (Kubernetes `terminationGracePeriodSeconds` defaults to 30s).
const SHUTDOWN_GRACE: Duration = Duration::from_secs(25);

#[tokio::main]
#[instrument]
async fn main() -> Result<()> {
    // install global default tracing subscriber using RUST_LOG env variable
    tracing_subscriber::registry()
        .with(fmt::layer().with_span_events(FmtSpan::NEW))
        .with(EnvFilter::from_default_env())
        .init();

    tokio_rustls::rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install default rustls crypto provider");

    let smtp_bind_addr = env::var("STMP_BIND_ADDR").unwrap_or("0.0.0.0:2525".to_string());
    let smtp_domain = env::var("SMTP_DOMAIN").context("env variable SMTP_DOMAIN not provided")?;
    let bucket: String =
        env::var("BUCKET_NAME").context("env variable BUCKET_NAME not provided")?;
    let aws_endpoint_url: Option<String> = env::var("AWS_ENDPOINT_URL").ok();
    let cert_path =
        env::var("SMTP_CERT_FILE").context("env variable SMTP_CERT_FILE not provided")?;
    let key_path = env::var("SMTP_KEY_FILE").context("env variable SMTP_KEY_FILE not provided")?;
    let database_url =
        env::var("DATABASE_URL").context("env variable DATABASE_URL not provided")?;

    let allowed_rcpts = env::var("ALLOWED_RCPTS")
        .map(|s| s.split(',').map(str::to_string).collect())
        .ok();
    let allowed_froms = env::var("ALLOWED_FROMS")
        .map(|s| s.split(',').map(str::to_string).collect())
        .ok();
    let check_db: bool = env::var("CHECK_ALLOWED_IN_DB")
        .map(|s| s == "true")
        .unwrap_or(false);

    let resolver = tls::CertificateResolver::new(&cert_path, &key_path)?;
    // start certificate change watcher
    notify::watch_certs(resolver.clone()).await?;
    let tls_config = tls::safe_tls_config(resolver)?;

    let aws_config = aws_config::from_env();
    // remove once https://github.com/awslabs/smithy-rs/issues/2863 lands
    let aws_config = if let Some(endpoint) = aws_endpoint_url {
        aws_config.endpoint_url(endpoint)
    } else {
        aws_config
    };
    let aws_config = aws_config.load().await;

    let s3_config = aws_sdk_s3::config::Builder::from(&aws_config)
        .force_path_style(true)
        .build();

    let pg_pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await?;

    let backend = SmtpBackend::new(
        s3_config,
        pg_pool,
        tls_config,
        &smtp_domain,
        &bucket,
        allowed_rcpts,
        allowed_froms,
        check_db,
    )?;

    let mut sigint = signal(SignalKind::interrupt()).context("failed to install SIGINT handler")?;
    let mut sigterm =
        signal(SignalKind::terminate()).context("failed to install SIGTERM handler")?;

    let (trigger, stop_rx) = shutdown_signal();
    let mut server = tokio::spawn(start_smtp_server(smtp_bind_addr, backend, stop_rx));

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
    smtp_backend: SmtpBackend,
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
                    let mut session = smtp_backend.new_session()?;
                    let server_config = smtp_backend.server_config.clone();
                    let stop = stop.clone();
                    sessions.spawn(async move {
                        let acceptor = TlsAcceptor::from(session.config.tls_config.clone());
                        if let Err(e) = smtp_server::serve(
                            socket,
                            &mut session,
                            server_config,
                            TlsMode::StartTls(acceptor),
                            Some(stop),

                        )
                        .await
                        {
                            warn!("could not handle connection from {}: {}", addr, e);
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
