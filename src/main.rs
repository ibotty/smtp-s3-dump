use std::env;
use std::net::SocketAddr;
// use std::time::Duration;

use anyhow::{Context, Result};
use futures::{FutureExt, TryFutureExt};
use smtpbis::{smtp_server, LoopExit};
use sqlx::postgres::PgPoolOptions;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::signal::unix::{signal, SignalKind};
use tokio_rustls::TlsAcceptor;
use tracing::instrument;
use tracing::{error, info, trace, warn};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use crate::smtp::{SmtpBackend, SmtpSession};

mod db;
mod notify;
mod s3;
mod smtp;
mod tls;

#[tokio::main]
#[instrument]
async fn main() -> Result<()> {
    // install global default tracing subscriber using RUST_LOG env variable
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

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
    let check_db: bool = env::var("CHECK_ALLOWED_IN_DB").and_then(|s| Ok(s == "true")).unwrap_or(false);

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

    let server = start_smtp_server(smtp_bind_addr, backend);

    let smtp_handler = tokio::spawn(server);

    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler")
    };

    let terminate = async {
        signal(SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
        _ = smtp_handler => {},
    }
    tracing::info!("shutting down");

    Ok(())
}

#[instrument(skip_all)]
async fn start_smtp_server(smtp_bind_addr: String, smtp_backend: SmtpBackend) -> Result<()> {
    info!("listening on {}", smtp_bind_addr);
    let listener = TcpListener::bind(smtp_bind_addr).await?;

    // ignore smtpbis' shutdown
    let (_shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let shutdown_rx = shutdown_rx.map_err(|_| ()).shared();

    while let Ok((socket, addr)) = listener.accept().await {
        let session = smtp_backend.new_session()?;
        let mut shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_smtp_connection(socket, addr, session, &mut shutdown_rx).await {
                warn!("could not handle connection: {}", e);
            }
        });
    }
    Ok(())
}

#[instrument(skip_all)]
async fn handle_smtp_connection(
    mut socket: TcpStream,
    _addr: SocketAddr,
    mut session: SmtpSession,
    shutdown: &mut smtpbis::ShutdownSignal,
) -> Result<()> {
    let mut smtp_config = smtpbis::Config::default();
    match smtp_server(&mut socket, &mut session, &smtp_config, shutdown, true).await {
        Ok(LoopExit::Done) => trace!("session done"),
        Ok(LoopExit::STARTTLS(tls_config)) => {
            let acceptor = TlsAcceptor::from(tls_config);
            let mut tls_socket = acceptor.accept(socket).await?;
            smtp_config.enable_starttls = false;
            // handler.tls_started(tls_socket.get_ref().1).await;
            match smtp_server(&mut tls_socket, &mut session, &smtp_config, shutdown, false).await {
                Ok(_) => trace!("TLS session done"),
                Err(e) => error!("TLS session error: {:?}", e),
            }
            tls_socket.shutdown().await?;
        }
        Err(_e) => {}
    }
    Ok(())
}
