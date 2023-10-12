use std::{sync::Arc, time::Duration};

use anyhow::Result;
use rust_smtp_server::server::Server;
use rustls::server::ServerConfig;
use tokio::signal::unix::{signal, SignalKind};
use tokio_rustls::TlsAcceptor;
use tracing::instrument;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

mod smtp;

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
    let smtp_domain = "smtp-upload.example.com".to_string();
    let aws_endpoint_url: Option<String> = None;

    // let tls_config = safe_tls_config().await?;
    // let tls_acceptor = TlsAcceptor::from(Arc::new(tls_config));

    let aws_config = aws_config::from_env();

    // remove once https://github.com/awslabs/smithy-rs/issues/2863 lands
    let aws_config = if let Some(endpoint) = aws_endpoint_url {
        aws_config.endpoint_url(endpoint)
    } else {
        aws_config
    };

    let aws_config = aws_config.load().await;

    let _s3 = aws_sdk_s3::Client::new(&aws_config);

    let mut server = Server::new(smtp::SmtpBackend {});
    // server.tls_acceptor = Some(tls_acceptor);
    server.addr = smtp_bind_addr;
    server.domain = smtp_domain;
    server.read_timeout = Duration::from_secs(10);
    server.write_timeout = Duration::from_secs(10);
    server.max_line_length = smtp_max_line_length;
    server.max_message_bytes = smtp_max_messages_bytes;
    server.max_recipients = 5;

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
    tracing::info!("starting graceful shutdown");

    Ok(())
}

#[instrument]
async fn safe_tls_config() -> Result<ServerConfig> {
    Ok(ServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth()
        .with_single_cert(vec![], rustls::PrivateKey(vec![]))?)
}
