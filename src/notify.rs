use anyhow::{Context, Result};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::spawn;

use notify_debouncer_mini::{new_debouncer, notify::RecursiveMode, DebounceEventResult};
use tracing::{error, instrument, trace};

use crate::tls;

#[instrument(skip_all)]
pub async fn watch_certs(resolver: Arc<tls::CertificateResolver>) -> Result<()> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let mut debouncer = new_debouncer(Duration::from_secs(2), move |res: DebounceEventResult| {
        if let Err(e) = tx.try_send(res) {
            error!("could not send event {:?}", e);
        }
    })?;

    let binding = [&resolver.cert_path, &resolver.key_path];
    let mut dirs = binding
        .iter()
        .map(|p| Path::new(p).parent().context("path has no parent"))
        .collect::<Result<Vec<&Path>>>()?;
    dirs.dedup();

    for dir in dirs {
        debouncer
            .watcher()
            .watch(dir, RecursiveMode::NonRecursive)?;
    }

    spawn(async move {
        let _debouncer = debouncer; // dropping it stops the watcher
        while let Some(res) = rx.recv().await {
            match res {
                Ok(event) => {
                    trace!("got inotify event {:?}", event);
                    if let Err(e) = resolver.refresh() {
                        error!("could not refresh certificates: {:?}", e);
                    }
                }
                Err(e) => {
                    error!("inotify error: {:?}", e);
                }
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Instant;

    const A_CRT: &[u8] = include_bytes!("../tests/fixtures/a.crt");
    const A_KEY: &[u8] = include_bytes!("../tests/fixtures/a.key");
    const B_CRT: &[u8] = include_bytes!("../tests/fixtures/b.crt");
    const B_KEY: &[u8] = include_bytes!("../tests/fixtures/b.key");

    #[tokio::test]
    async fn watcher_reloads_certificate_on_file_change() {
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();

        let dir = std::env::temp_dir().join(format!("smtp-s3-dump-notify-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let (crt, key) = (dir.join("tls.crt"), dir.join("tls.key"));
        fs::write(&crt, A_CRT).unwrap();
        fs::write(&key, A_KEY).unwrap();

        let resolver =
            tls::CertificateResolver::new(crt.to_str().unwrap(), key.to_str().unwrap()).unwrap();
        watch_certs(resolver.clone()).await.unwrap();
        let before = resolver.certified_key.load().cert[0].clone();

        fs::write(&crt, B_CRT).unwrap();
        fs::write(&key, B_KEY).unwrap();

        // debounce is 2s
        let deadline = Instant::now() + Duration::from_secs(10);
        while resolver.certified_key.load().cert[0] == before {
            assert!(Instant::now() < deadline, "certificate was not reloaded");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        fs::remove_dir_all(&dir).unwrap();
    }
}
