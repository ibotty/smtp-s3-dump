use std::env;
use std::time::Duration;

use anyhow::{Context, Result};
use rust_smtp_server::server::Server;
use tokio::signal::unix::{signal, SignalKind};
use tokio_rustls::TlsAcceptor;
use tracing::info;
use tracing::instrument;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

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

    let smtp_bind_addr = "0.0.0.0:2525".to_string();
    let smtp_max_messages_bytes = 256 * 1024 * 1024;
    let smtp_max_line_length = 10000;
    let smtp_domain = env::var("SMTP_DOMAIN").context("env variable SMTP_DOMAIN not set")?;
    let bucket: String = env::var("BUCKET_NAME").context("env variable BUCKET_NAME not set")?;
    let aws_endpoint_url: Option<String> = env::var("AWS_ENDPOINT_URL").ok();
    let cert_path = env::var("SMTP_CERT_FILE");
    let key_path = env::var("SMTP_KEY_FILE");

    let tls_acceptor = if let (Ok(cert), Ok(key)) = (cert_path, key_path) {
        let resolver = tls::CertificateResolver::new(&cert, &key)?;

        // start certificate change watcher
        notify::watch_certs(resolver.clone()).await?;

        let tls_config = tls::safe_tls_config(resolver)?;
        Some(TlsAcceptor::from(tls_config))
    } else {
        None
    };

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

    let mut server = Server::new(smtp::SmtpBackend::new(s3_config, &bucket, vec![], vec![]));
    server.tls_acceptor = tls_acceptor;
    server.addr = smtp_bind_addr.clone();
    server.domain = smtp_domain;
    server.read_timeout = Duration::from_secs(10);
    server.write_timeout = Duration::from_secs(10);
    server.max_line_length = smtp_max_line_length;
    server.max_message_bytes = smtp_max_messages_bytes;
    server.max_recipients = 5;

    info!("listening on {}", smtp_bind_addr);
    // let smtp_handler = tokio::spawn(start_smtp_handler(&server));
    let smtp_handler = tokio::spawn(server.listen_and_serve());

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
