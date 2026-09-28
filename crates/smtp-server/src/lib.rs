//! A typesafe Sans-IO SMTP server core built on [`smtp_proto`].
//!
//! The state machine ([`Session`]) does no I/O; drive it with [`Session::feed`] and
//! [`Session::poll`]. A Tokio driver ([`serve`]) is provided by default.

mod reply;
mod session;
mod types;

#[cfg(feature = "tokio")]
mod driver;
#[cfg(feature = "tokio")]
mod limit;
#[cfg(feature = "tokio")]
mod shutdown;

pub use reply::Rejection;
pub use session::{
    Cleartext, DataChunkRequest, DataEndRequest, DataStartRequest, EhloRequest, Event, HeloRequest,
    MailRequest, Plain, Poll, QuitToken, RcptRequest, ResumeToken, Session, StartTlsToken, Tls,
    Transport,
};
pub use types::{
    Domain, Envelope, ForwardPath, Hostname, InvalidHostname, InvalidLocalPart, LocalPart, Mailbox,
    MessageSize, NonEmpty, Recipient, ReversePath, Sender,
};

#[cfg(feature = "tokio")]
pub use driver::{Handler, TlsMode, serve};
#[cfg(feature = "tokio")]
pub use limit::{SessionGuard, SessionLimiter, reject_busy};
#[cfg(feature = "tokio")]
pub use shutdown::{Shutdown, ShutdownTrigger, shutdown_signal};

use std::num::NonZeroU32;
use std::time::Duration;

/// Server configuration: identity, advertised extensions, limits, and driver timeouts.
#[derive(Debug, Clone)]
pub struct Config {
    /// Advertised in the greeting and EHLO response.
    pub hostname: Hostname,
    /// `SIZE=` advertised (if set) and enforced against `MAIL FROM SIZE=` and the actual message.
    /// `None` means unlimited, which is only safe with a streaming [`Handler::data_chunk`]:
    /// the default handler buffers the whole message in memory. [`Config::new`] sets 25 MiB.
    pub max_message_size: Option<MessageSize>,
    /// Advertise `PIPELINING`.
    pub pipelining: bool,
    /// Advertise `CHUNKING` (`BDAT`).
    pub chunking: bool,
    /// Advertise `SMTPUTF8`.
    pub smtputf8: bool,
    /// Advertise `8BITMIME`.
    pub eightbitmime: bool,
    /// Advertise DSN parameters (`RET=`, `ENVID=`, `NOTIFY=`, `ORCPT=`).
    pub dsn: bool,
    /// Unrecognized/malformed commands allowed before the connection is closed with `421`.
    pub max_bad_commands: NonZeroU32,
    /// Maximum recipients per transaction; further `RCPT TO` get `452 4.5.3`.
    pub max_rcpts: NonZeroU32,
    /// Maximum `NOOP`/`HELP`/`VRFY` commands per session before it is closed with `421`
    /// (these never count as bad commands, so they would otherwise keep a session open forever).
    pub max_idle_commands: NonZeroU32,
    /// Idle timeout while waiting for the next command.
    pub command_timeout: Duration,
    /// Idle timeout for a single read while inside `DATA`/`BDAT`.
    pub data_timeout: Duration,
    /// Overall deadline for the whole `DATA`/`BDAT` phase (anti-slowloris).
    pub data_deadline: Duration,
}

impl Config {
    /// A reasonable default configuration for `hostname`; messages are limited to 25 MiB.
    pub fn new(hostname: Hostname) -> Self {
        Self {
            hostname,
            max_message_size: MessageSize::new(25 * 1024 * 1024),
            pipelining: true,
            chunking: true,
            smtputf8: true,
            eightbitmime: true,
            dsn: true,
            max_bad_commands: NonZeroU32::new(10).expect("10 != 0"),
            max_rcpts: NonZeroU32::new(10).expect("10 != 0"),
            max_idle_commands: NonZeroU32::new(100).expect("100 != 0"),
            command_timeout: Duration::from_secs(5 * 60),
            data_timeout: Duration::from_secs(3 * 60),
            data_deadline: Duration::from_secs(10 * 60),
        }
    }
}
