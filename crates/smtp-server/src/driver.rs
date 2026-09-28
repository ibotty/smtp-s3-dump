//! Tokio driver for [`crate::Session`]: the only part of this crate that does I/O.

use std::future::{Future, pending};
use std::io;
use std::panic::AssertUnwindSafe;
use std::pin::pin;
use std::sync::Arc;
use tokio::time::Instant;

use futures_util::FutureExt;
use futures_util::future::{Either, select};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_rustls::TlsAcceptor;

use crate::{
    Cleartext, Config, Envelope, Plain, Poll, Recipient, Rejection, Sender, Session, Shutdown, Tls,
    Transport,
};

/// Reacts to protocol events. All methods default to accepting; override what you need.
///
/// Handler futures are wrapped in `catch_unwind`: a panic closes the connection with `421`
/// (`tracing::error!` is logged if the `tracing` feature is enabled) instead of taking the
/// whole process down. Don't reuse `&mut Self` after [`serve`] returned due to a panic.
pub trait Handler: Send {
    /// React to `EHLO`.
    fn ehlo(
        &mut self,
        _host: &crate::Domain,
    ) -> impl Future<Output = Result<(), Rejection>> + Send {
        async { Ok(()) }
    }
    /// React to `HELO`.
    fn helo(
        &mut self,
        _host: &crate::Domain,
    ) -> impl Future<Output = Result<(), Rejection>> + Send {
        async { Ok(()) }
    }
    /// React to `MAIL FROM`.
    fn mail(&mut self, _sender: &Sender) -> impl Future<Output = Result<(), Rejection>> + Send {
        async { Ok(()) }
    }
    /// React to `RCPT TO`.
    fn rcpt(
        &mut self,
        _sender: &Sender,
        _rcpt: &Recipient,
    ) -> impl Future<Output = Result<(), Rejection>> + Send {
        async { Ok(()) }
    }
    /// React to `DATA`/first `BDAT` (the envelope is complete).
    fn data_start(
        &mut self,
        _env: &Envelope,
    ) -> impl Future<Output = Result<(), Rejection>> + Send {
        async { Ok(()) }
    }
    /// A chunk of message bytes arrived. Default: buffer into `message`. Override to stream;
    /// `message` then stays empty and [`Handler::data_end`] receives it empty too.
    fn data_chunk(
        &mut self,
        chunk: &[u8],
        message: &mut Vec<u8>,
    ) -> impl Future<Output = Result<(), Rejection>> + Send {
        message.extend_from_slice(chunk);
        async { Ok(()) }
    }
    /// The message is complete. `message` is whatever [`Handler::data_chunk`] left in the buffer
    /// (the whole message, unless streaming). The returned string becomes part of the `250` reply.
    fn data_end(
        &mut self,
        env: &Envelope,
        message: Vec<u8>,
    ) -> impl Future<Output = Result<String, Rejection>> + Send;
    /// The in-progress message was aborted (oversize, or a rejected chunk); clean up.
    fn data_abort(&mut self) -> impl Future<Output = ()> + Send {
        async {}
    }
    /// `RSET`, or entry into a fresh transaction after `STARTTLS`; clean up.
    fn rset(&mut self) -> impl Future<Output = ()> + Send {
        async {}
    }
}

async fn catch<F: Future<Output = Result<R, Rejection>>, R>(fut: F) -> Result<R, Rejection> {
    AssertUnwindSafe(fut)
        .catch_unwind()
        .await
        .unwrap_or_else(|payload| {
            #[cfg(feature = "tracing")]
            tracing::error!(?payload, "SMTP handler panicked");
            #[cfg(not(feature = "tracing"))]
            let _ = payload;
            Err(Rejection::closing("internal error"))
        })
}

/// How a connection is (or is not) encrypted.
#[derive(Clone)]
pub enum TlsMode {
    /// Plaintext only; `STARTTLS` is not offered.
    None,
    /// Plaintext, with `STARTTLS` on offer (submission/relay ports 25 and 587).
    StartTls(TlsAcceptor),
    /// The TLS handshake happens before any SMTP bytes (SMTPS, port 465); no `STARTTLS`.
    Implicit(TlsAcceptor),
}

/// Serves one connection.
///
/// With `shutdown`, the connection stops gracefully once it is triggered: a connection that is
/// idle (waiting for a command) gets `421 4.3.2` and is closed right away. A message being
/// received (`DATA`/`BDAT` payload) or a running handler is never interrupted: the transaction
/// finishes and is answered as usual, and the connection is closed with `421` once the server
/// would wait for the client again. Clone one [`Shutdown`] per connection; it is level-triggered,
/// so connections accepted after the trigger are closed immediately. Dropping the
/// [`crate::ShutdownTrigger`] without triggering disables shutdown.
pub async fn serve<S, H>(
    mut stream: S,
    handler: &mut H,
    cfg: Arc<Config>,
    tls: TlsMode,
    mut shutdown: Option<Shutdown>,
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
    H: Handler,
{
    match tls {
        TlsMode::None => {
            let session = Session::<Cleartext>::new(cfg.clone());
            run(&mut stream, session, handler, &cfg, &mut shutdown).await?;
            Ok(())
        }
        TlsMode::StartTls(acceptor) => {
            let session = Session::<Plain>::new(cfg.clone());
            let Some(mut start_tls) =
                run(&mut stream, session, handler, &cfg, &mut shutdown).await?
            else {
                return Ok(());
            };
            stream.write_all(&start_tls.output()).await?;
            let mut stream = acceptor.accept(stream).await?;
            handler.rset().await;
            run(
                &mut stream,
                start_tls.established(),
                handler,
                &cfg,
                &mut shutdown,
            )
            .await?;
            stream.shutdown().await
        }
        TlsMode::Implicit(acceptor) => {
            let mut stream = acceptor.accept(stream).await?;
            let session = Session::<Tls>::new(cfg.clone());
            run(&mut stream, session, handler, &cfg, &mut shutdown).await?;
            stream.shutdown().await
        }
    }
}

/// Resolves once shutdown was requested; never resolves without a (live) signal.
async fn shutdown_requested(shutdown: &mut Option<Shutdown>) {
    match shutdown {
        Some(s) => s.requested().await,
        None => pending().await,
    }
}

/// Drives `session` to completion; returns `Some(start_tls)` only when the client asked for
/// (and this transport offers) `STARTTLS`.
async fn run<S, T, H>(
    stream: &mut S,
    mut session: Session<T>,
    h: &mut H,
    cfg: &Config,
    shutdown: &mut Option<Shutdown>,
) -> io::Result<Option<T::StartTls>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
    T: Transport,
    H: Handler,
{
    let mut buf = vec![0u8; 16 * 1024];
    let mut message = Vec::new();
    let mut data_since: Option<Instant> = None;
    loop {
        session = match session.poll() {
            crate::Poll::NeedInput(mut s) => {
                stream.write_all(&s.take_output()).await?;
                let limit = if s.in_data() {
                    let since = *data_since.get_or_insert_with(Instant::now);
                    cfg.data_timeout
                        .min(cfg.data_deadline.saturating_sub(since.elapsed()))
                } else {
                    data_since = None;
                    cfg.command_timeout
                };
                let read = tokio::time::timeout(limit, stream.read(&mut buf));
                // Never interrupt a message in flight; only an idle connection is closed early.
                let read = if s.in_data() {
                    Some(read.await)
                } else {
                    match select(pin!(read), pin!(shutdown_requested(shutdown))).await {
                        Either::Left((res, _)) => Some(res),
                        Either::Right(_) => None,
                    }
                };
                match read {
                    None => {
                        stream.write_all(&s.shutting_down()).await?;
                        return Ok(None);
                    }
                    Some(Err(_elapsed)) => {
                        stream.write_all(&s.timed_out()).await?;
                        return Ok(None);
                    }
                    Some(Ok(Ok(0))) => return Ok(None),
                    Some(Ok(Ok(n))) => {
                        s.feed(&buf[..n]);
                        s
                    }
                    Some(Ok(Err(e))) => return Err(e),
                }
            }
            Poll::Closed(out) => {
                stream.write_all(&out).await?;
                return Ok(None);
            }
            Poll::Event(ev) => match ev {
                crate::Event::Ehlo(req) => {
                    let r = catch(h.ehlo(req.host())).await;
                    req.decide(r)
                }
                crate::Event::Helo(req) => {
                    let r = catch(h.helo(req.host())).await;
                    req.decide(r)
                }
                crate::Event::Mail(req) => {
                    let r = catch(h.mail(req.sender())).await;
                    req.decide(r)
                }
                crate::Event::Rcpt(req) => {
                    let r = catch(h.rcpt(req.sender(), req.recipient())).await;
                    req.decide(r)
                }
                crate::Event::DataStart(req) => {
                    let r = catch(h.data_start(req.envelope())).await;
                    req.decide(r)
                }
                crate::Event::DataChunk(req) => {
                    let r = catch(h.data_chunk(req.chunk(), &mut message)).await;
                    req.decide(r)
                }
                crate::Event::DataEnd(mut req) => {
                    // The trailing bytes right before the terminator never went through a
                    // `DataChunk` event; route them through `data_chunk` too so buffering and
                    // streaming handlers alike see the complete message.
                    let last = req.take_message();
                    let r = if last.is_empty() {
                        Ok(())
                    } else {
                        catch(h.data_chunk(&last, &mut message)).await
                    };
                    let env = req.envelope().clone();
                    let r = match r {
                        Ok(()) => catch(h.data_end(&env, std::mem::take(&mut message))).await,
                        Err(e) => Err(e),
                    };
                    req.decide(r)
                }
                crate::Event::DataAbort(n) => {
                    message.clear();
                    h.data_abort().await;
                    n.resume()
                }
                crate::Event::Rset(n) => {
                    message.clear();
                    h.rset().await;
                    n.resume()
                }
                crate::Event::Quit(q) => {
                    stream.write_all(&q.close()).await?;
                    return Ok(None);
                }
                crate::Event::StartTls(t) => return Ok(Some(t)),
            },
        };
    }
}
