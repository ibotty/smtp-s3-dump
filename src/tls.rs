use anyhow::Result;
use tokio::{fs::File, io::AsyncReadExt};
use tracing::instrument;
use tokio_rustls::rustls::{Certificate, PrivateKey, ServerConfig};

#[instrument]
pub async fn safe_tls_config(cert_path: &str, key_path: &str) -> Result<ServerConfig> {
    let mut cert_file = File::open(cert_path).await?;
    let mut cert = vec!();
    cert_file.read_to_end(&mut cert).await?;

    let mut key_file = File::open(key_path).await?;
    let mut key = vec!();
    key_file.read_to_end(&mut key).await?;

    Ok(ServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth()
        .with_single_cert(vec!(Certificate(cert)), PrivateKey(key))?)
}

