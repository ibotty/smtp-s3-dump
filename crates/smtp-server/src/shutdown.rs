//! A graceful-shutdown signal for [`crate::serve`].

use std::future::pending;

use tokio::sync::watch;

/// The sending half: call [`ShutdownTrigger::trigger`] to ask all connections to wind down.
#[derive(Debug)]
pub struct ShutdownTrigger(watch::Sender<bool>);

impl ShutdownTrigger {
    /// Requests shutdown. Idempotent; also reaches connections that start afterwards.
    pub fn trigger(&self) {
        self.0.send_replace(true);
    }
}

/// The receiving half. Clone one per connection (or pass it to your accept loop).
#[derive(Debug, Clone)]
pub struct Shutdown(watch::Receiver<bool>);

impl Shutdown {
    /// Resolves once shutdown was triggered (immediately if it already was). If the
    /// [`ShutdownTrigger`] is dropped without triggering, this never resolves.
    pub async fn requested(&mut self) {
        if self.0.wait_for(|stop| *stop).await.is_ok() {
            return;
        }
        pending().await
    }
}

/// Creates a linked trigger and signal.
pub fn shutdown_signal() -> (ShutdownTrigger, Shutdown) {
    let (tx, rx) = watch::channel(false);
    (ShutdownTrigger(tx), Shutdown(rx))
}
