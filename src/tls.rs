use anyhow::Result;
use tokio::{fs::File, io::AsyncReadExt, try_join};
use tokio_rustls::rustls::{Certificate, PrivateKey, ServerConfig};
use tracing::instrument;

#[instrument]
pub async fn safe_tls_config(cert_path: &str, key_path: &str) -> Result<ServerConfig> {
    let (cert, key) = try_join!(load_file(cert_path), load_file(key_path))?;

    Ok(ServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth()
        .with_single_cert(vec![Certificate(cert)], PrivateKey(key))?)
}

#[instrument]
async fn load_file(path: &str) -> Result<Vec<u8>> {
    let mut file = File::open(path).await?;
    let mut data = vec![];
    file.read_to_end(&mut data).await?;
    Ok(data)
}
