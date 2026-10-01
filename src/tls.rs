use std::sync::Arc;

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use tokio_rustls::rustls::{
    crypto::CryptoProvider,
    server::{ClientHello, ResolvesServerCert},
    pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer},
    sign::CertifiedKey,
};
use tracing::{instrument, trace};

#[derive(Debug)]
pub struct CertificateResolver {
    pub cert_path: String,
    pub key_path: String,
    pub certified_key: ArcSwap<CertifiedKey>,
}

impl CertificateResolver {
    #[instrument]
    fn load_certs_and_key(cert_path: &str, key_path: &str) -> Result<CertifiedKey> {
        trace!("loading certs from files");

        let crypto_provider =
            CryptoProvider::get_default().context("no default crypto provider")?;

        let certs = CertificateDer::pem_file_iter(cert_path)?.collect::<Result<Vec<_>, _>>()?;
        let key = PrivateKeyDer::from_pem_file(key_path).context("no private key found")?;
        let key = crypto_provider
            .key_provider
            .load_private_key(key)
            .context("cannot load signing key out of private key")?;
        let certified_key = CertifiedKey::new(certs, key);
        trace!("got certs from files");

        Ok(certified_key)
    }

    #[instrument]
    pub fn new(cert_path: &str, key_path: &str) -> Result<Arc<Self>> {
        let certified_key = ArcSwap::from_pointee(Self::load_certs_and_key(cert_path, key_path)?);

        let cert_path = cert_path.to_string();
        let key_path = key_path.to_string();
        Ok(Arc::new(Self {
            cert_path,
            key_path,
            certified_key,
        }))
    }

    #[instrument(skip_all)]
    pub fn refresh(&self) -> Result<()> {
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
