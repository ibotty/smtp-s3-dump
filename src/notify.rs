use anyhow::{Context, Result};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::{spawn, sync::mpsc::Receiver};

use notify_debouncer_mini::{
    new_debouncer,
    notify::{RecommendedWatcher, RecursiveMode},
    DebounceEventResult, Debouncer,
};
use tracing::{error, info, instrument, trace};

use crate::tls;

#[instrument(skip_all)]
pub async fn watch_certs(resolver: Arc<tls::CertificateResolver>) -> Result<()> {
    let (mut debouncer, mut rx) = setup_watcher()?;

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
        while let Some(res) = rx.recv().await {
            match res {
                Ok(event) => {
                    trace!("got inotify event {:?}", event);
                    match resolver.refresh().await {
                        Ok(s) => info!("refreshed certificates successfully. {:?}", s),
                        Err(e) => error!("could not refresh certificates: {:?}", e),
                    };
                }
                Err(e) => {
                    error!("inotify error: {:?}", e);
                }
            }
        }
    });
    Ok(())
}

#[instrument]
pub fn setup_watcher() -> Result<(Debouncer<RecommendedWatcher>, Receiver<DebounceEventResult>)> {
    let (tx, rx) = tokio::sync::mpsc::channel(1);

    let debouncer = new_debouncer(Duration::from_secs(2), move |res: DebounceEventResult| {
        if let Err(e) = tx.try_send(res) {
            error!("could not send event {:?}", e);
        }
    })?;

    Ok((debouncer, rx))
}
