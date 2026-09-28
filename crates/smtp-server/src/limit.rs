//! Concurrent-connection limiting for accept loops, and the reply for refused connections.

use std::collections::HashMap;
use std::hash::Hash;
use std::io;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::Rejection;

/// How long [`reject_busy`] may take to write the reply and close.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Caps concurrent sessions: at most `max_total` overall and, optionally, at most `max_per_key`
/// per key (for example the peer IP; use `()` when there is no meaningful key).
pub struct SessionLimiter<K> {
    max_total: usize,
    max_per_key: Option<usize>,
    state: Mutex<State<K>>,
}

struct State<K> {
    total: usize,
    per_key: HashMap<K, usize>,
}

/// A held session slot; released on drop.
pub struct SessionGuard<K: Hash + Eq + Clone> {
    limiter: Arc<SessionLimiter<K>>,
    key: K,
}

impl<K: Hash + Eq + Clone> SessionLimiter<K> {
    pub fn new(max_total: usize, max_per_key: Option<usize>) -> Arc<Self> {
        Arc::new(Self {
            max_total,
            max_per_key,
            state: Mutex::new(State {
                total: 0,
                per_key: HashMap::new(),
            }),
        })
    }

    /// Takes a slot for `key`, or returns `None` if a limit is reached.
    pub fn try_acquire(self: &Arc<Self>, key: K) -> Option<SessionGuard<K>> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let n = state.per_key.get(&key).copied().unwrap_or(0);
        if state.total >= self.max_total || self.max_per_key.is_some_and(|max| n >= max) {
            return None;
        }
        state.total += 1;
        state.per_key.insert(key.clone(), n + 1);
        Some(SessionGuard {
            limiter: self.clone(),
            key,
        })
    }
}

impl<K: Hash + Eq + Clone> Drop for SessionGuard<K> {
    fn drop(&mut self) {
        let mut state = self
            .limiter
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        state.total -= 1;
        if let Some(n) = state.per_key.get_mut(&self.key) {
            *n -= 1;
            if *n == 0 {
                state.per_key.remove(&self.key);
            }
        }
    }
}

/// Answers a connection that was refused for lack of capacity with `421`, then closes it.
///
/// Gives up after a few seconds, so a client that does not read cannot hold the task.
pub async fn reject_busy<S: AsyncWrite + Unpin>(mut stream: S) -> io::Result<()> {
    let mut reply = Vec::new();
    Rejection::closing("too many connections").write(&mut reply);
    tokio::time::timeout(BUSY_TIMEOUT, async {
        stream.write_all(&reply).await?;
        stream.shutdown().await
    })
    .await
    .unwrap_or_else(|_| Err(io::ErrorKind::TimedOut.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[test]
    fn enforces_per_key_and_total_and_releases() {
        let l = SessionLimiter::new(3, Some(2));
        let a1 = l.try_acquire(1u8).unwrap();
        let _a2 = l.try_acquire(1).unwrap();
        assert!(l.try_acquire(1).is_none(), "per-key cap");
        let _b = l.try_acquire(2).unwrap();
        assert!(l.try_acquire(3).is_none(), "total cap");
        drop(a1);
        assert!(l.try_acquire(3).is_some(), "slot released");
    }

    #[test]
    fn no_per_key_cap() {
        let l = SessionLimiter::new(3, None);
        let _g: Vec<_> = (0..3).map(|_| l.try_acquire(()).unwrap()).collect();
        assert!(l.try_acquire(()).is_none());
    }

    #[test]
    fn leaves_no_entries_behind() {
        let l = SessionLimiter::new(1, Some(1));
        let held = l.try_acquire(1u8).unwrap();
        assert!(l.try_acquire(2).is_none());
        assert_eq!(l.state.lock().unwrap().per_key.len(), 1);
        drop(held);
        assert!(l.state.lock().unwrap().per_key.is_empty());
    }

    #[tokio::test]
    async fn busy_reply_is_421_then_close() {
        let (mut client, server) = tokio::io::duplex(1024);
        reject_busy(server).await.unwrap();
        let mut out = String::new();
        client.read_to_string(&mut out).await.unwrap();
        assert_eq!(out, "421 4.3.0 too many connections\r\n");
    }

    #[tokio::test(start_paused = true)]
    async fn busy_reply_gives_up_on_a_stalled_peer() {
        // Nothing reads the other end and the pipe is smaller than the reply.
        let (_client, server) = tokio::io::duplex(4);
        let err = reject_busy(server).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }
}
