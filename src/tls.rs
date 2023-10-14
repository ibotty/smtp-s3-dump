use std::{fs::File, io::BufReader, sync::Arc};

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
// use tokio::{fs::File, io::AsyncReadExt, try_join};
use tokio_rustls::rustls::{
    server::{ClientHello, ResolvesServerCert},
    sign::{self, CertifiedKey},
    Certificate, PrivateKey, ServerConfig,
};
use tracing::{instrument, trace};

#[instrument(skip_all)]
pub fn safe_tls_config(resolver: Arc<CertificateResolver>) -> Result<Arc<ServerConfig>> {
    Ok(Arc::new(
        ServerConfig::builder()
            .with_safe_defaults()
            .with_no_client_auth()
            .with_cert_resolver(resolver),
    ))
}

pub struct CertificateResolver {
    pub cert_path: String,
    pub key_path: String,
    pub certified_key: Arc<ArcSwap<CertifiedKey>>,
}

impl CertificateResolver {
    #[instrument]
    fn load_certs_and_key(cert_path: &str, key_path: &str) -> Result<CertifiedKey> {
        trace!("loading certs from files");

        let certs: Vec<Certificate> =
            rustls_pemfile::certs(&mut BufReader::new(File::open(cert_path)?))?
                .into_iter()
                .map(Certificate)
                .collect();
        let key = sign::any_supported_type(
            &rustls_pemfile::rsa_private_keys(&mut BufReader::new(File::open(key_path)?))?
                .into_iter()
                .map(PrivateKey)
                .next()
                .context("no private key found")?,
        )?;
        let certified_key = CertifiedKey::new(certs, key);
        trace!("got certs from files");

        Ok(certified_key)
    }

    #[instrument]
    pub fn new(cert_path: &str, key_path: &str) -> Result<Arc<Self>> {
        let certified_key = Arc::new(ArcSwap::from_pointee(Self::load_certs_and_key(
            cert_path, key_path,
        )?));

        let cert_path = cert_path.to_string();
        let key_path = key_path.to_string();
        Ok(Arc::new(Self {
            cert_path,
            key_path,
            certified_key,
        }))
    }

    #[instrument(skip_all)]
    pub async fn refresh(&self) -> Result<()> {
        trace!("refreshing certificates");
        let certified_key = Self::load_certs_and_key(&self.cert_path, &self.key_path)?;

        self.certified_key.store(Arc::new(certified_key));
        Ok(())
    }
}

impl ResolvesServerCert for CertificateResolver {
    #[instrument(skip_all)]
    fn resolve(&self, _client_hello: ClientHello) -> Option<Arc<CertifiedKey>> {
        trace!("loading certificate");
        Some(arc_swap::Guard::into_inner(self.certified_key.load()))
    }
}
