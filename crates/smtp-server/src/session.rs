//! The Sans-IO SMTP core: a state machine driven by [`Session::feed`] and
//! [`Session::poll`]. No I/O happens here; see [`crate::serve`] for a driver.

use std::convert::Infallible;
use std::io::Write;
use std::marker::PhantomData;
use std::sync::Arc;

use smtp_proto::request::receiver::{BdatReceiver, DataReceiver, RequestReceiver};
use smtp_proto::{
    EXT_8BIT_MIME, EXT_CHUNKING, EXT_DSN, EXT_ENHANCED_STATUS_CODES, EXT_PIPELINING, EXT_SIZE,
    EXT_SMTP_UTF8, EXT_START_TLS, EhloResponse, Error as ParseError, Request,
};

use crate::Config;
use crate::reply::Rejection;
use crate::types::{Domain, Envelope, NonEmpty, Recipient, Sender};

/// Bytes accumulated while receiving `DATA`/`BDAT`, and the receiver that
/// unstuffs/counts them. Abstracts over the two wire formats smtp-proto
/// exposes so the rest of the core doesn't care which one is in progress.
enum AnyReceiver {
    Data(DataReceiver),
    Bdat(BdatReceiver),
}

impl AnyReceiver {
    /// Ingests as much of `bytes` as available; returns `true` once the
    /// message (or the current BDAT chunk, if last) is complete.
    fn ingest(&mut self, bytes: &mut std::slice::Iter<'_, u8>, buf: &mut Vec<u8>) -> bool {
        match self {
            AnyReceiver::Data(r) => r.ingest(bytes, buf),
            AnyReceiver::Bdat(r) => r.ingest(bytes, buf),
        }
    }

    fn is_last(&self) -> bool {
        match self {
            AnyReceiver::Data(_) => true,
            AnyReceiver::Bdat(r) => r.is_last,
        }
    }
}

/// The transaction's runtime phase. Unrepresentable states (DATA without
/// RCPT, RCPT without MAIL, ...) are simply not variants here.
enum Phase {
    /// No EHLO/HELO yet.
    Fresh,
    /// Greeted, ready for a new transaction.
    Greeted,
    /// `MAIL FROM` accepted, waiting for `RCPT TO`.
    Mail { sender: Sender },
    /// At least one `RCPT TO` accepted, waiting for more, `DATA`/`BDAT`, or `RSET`.
    Rcpt { envelope: Envelope },
    /// Receiving message bytes (via `DATA` or `BDAT`).
    Receiving {
        envelope: Envelope,
        receiver: AnyReceiver,
        size: usize,
    },
    /// Between `BDAT` chunks (non-last chunk finished): only `BDAT`/`RSET`/`QUIT` make sense.
    Chunking { envelope: Envelope, size: usize },
    /// Swallowing the rest of a rejected/oversize `DATA`/`BDAT` before replying.
    Discard {
        receiver: AnyReceiver,
        reply: Rejection,
    },
    /// A `421` reply was queued; the next [`Session::poll`] closes the connection.
    Closing,
}

mod sealed {
    pub trait Sealed {}
}

/// Sealed marker for the connection's TLS status, carried as a type
/// parameter on [`Session`] so STARTTLS availability is checked at compile time.
pub trait Transport: sealed::Sealed + Sized + Send + 'static {
    /// Payload of [`Event::StartTls`]: [`StartTlsToken`] for [`Plain`],
    /// uninhabited for [`Tls`]/[`Cleartext`] (STARTTLS is never offered there).
    type StartTls: Send;

    /// Whether EHLO advertises `STARTTLS` for this transport.
    const OFFERS_STARTTLS: bool;

    #[doc(hidden)]
    fn start_tls_event(session: Session<Self>) -> Event<Self>;
}

/// Cleartext connection with STARTTLS on offer.
#[derive(Debug)]
pub struct Plain(Infallible);
/// Connection encrypted (after STARTTLS or implicit TLS). STARTTLS is not offered again.
#[derive(Debug)]
pub struct Tls(Infallible);
/// Cleartext connection with no TLS available at all.
#[derive(Debug)]
pub struct Cleartext(Infallible);

impl sealed::Sealed for Plain {}
impl sealed::Sealed for Tls {}
impl sealed::Sealed for Cleartext {}

impl Transport for Plain {
    type StartTls = StartTlsToken;
    const OFFERS_STARTTLS: bool = true;

    fn start_tls_event(session: Session<Self>) -> Event<Self> {
        Event::StartTls(StartTlsToken { session })
    }
}

macro_rules! no_starttls {
    ($($t:ident),*) => {$(
        impl Transport for $t {
            type StartTls = Infallible;
            const OFFERS_STARTTLS: bool = false;

            fn start_tls_event(_session: Session<Self>) -> Event<Self> {
                unreachable!(concat!("STARTTLS is never offered on ", stringify!($t)))
            }
        }
    )*};
}
no_starttls!(Tls, Cleartext);

/// A single SMTP connection. Fed bytes via [`Session::feed`], driven forward
/// via [`Session::poll`]; never does I/O itself.
pub struct Session<T: Transport> {
    cfg: Arc<Config>,
    phase: Phase,
    command: RequestReceiver,
    input: Vec<u8>,
    output: Vec<u8>,
    bad_commands: u32,
    idle_commands: u32,
    _transport: PhantomData<T>,
}

/// The result of [`Session::poll`].
// `Session` is moved by value through every state transition on purpose (no I/O, no allocation
// beyond the buffers it already owns); boxing the large variant would add a heap allocation to
// the hot path it exists to avoid.
#[allow(clippy::large_enum_variant)]
#[must_use]
pub enum Poll<T: Transport> {
    /// No complete command/event is available; flush `NeedInput`'s buffered
    /// output, read more bytes, [`Session::feed`] them, and poll again.
    NeedInput(Session<T>),
    /// A decision is required; see the token inside for details.
    Event(Event<T>),
    /// The session decided to close on its own (e.g. too many bad commands).
    /// Write the bytes and stop.
    Closed(Vec<u8>),
}

/// Something a [`crate::Handler`] must react to.
#[must_use]
pub enum Event<T: Transport> {
    /// `EHLO`.
    Ehlo(EhloRequest<T>),
    /// `HELO`.
    Helo(HeloRequest<T>),
    /// `MAIL FROM`.
    Mail(MailRequest<T>),
    /// `RCPT TO`.
    Rcpt(RcptRequest<T>),
    /// `DATA`/first `BDAT`: the envelope is complete, decide whether to accept the message.
    DataStart(DataStartRequest<T>),
    /// A chunk of message bytes became available.
    DataChunk(DataChunkRequest<T>),
    /// The message is complete.
    DataEnd(DataEndRequest<T>),
    /// The in-progress message was aborted (oversize, or a rejected chunk);
    /// clean up any partial state, then call `.resume()`.
    DataAbort(ResumeToken<T>),
    /// `RSET`; clean up any partial state, then call `.resume()`.
    Rset(ResumeToken<T>),
    /// `QUIT`; call `.close()` to get the final bytes to write, then stop.
    Quit(QuitToken<T>),
    /// `STARTTLS` (only reachable when `T = Plain`).
    StartTls(T::StartTls),
}

fn parse_error_reply(e: ParseError) -> Rejection {
    match e {
        ParseError::UnknownCommand => Rejection::unknown_command(),
        ParseError::SyntaxError { syntax } => Rejection::command_syntax(syntax),
        ParseError::InvalidParameter { param } => Rejection::unsupported_param(param),
        ParseError::UnsupportedParameter { param } => Rejection::unsupported_param(&param),
        ParseError::InvalidSenderAddress => Rejection::invalid_mailbox("<sender>"),
        ParseError::InvalidRecipientAddress => Rejection::invalid_mailbox("<recipient>"),
        ParseError::ResponseTooLong => Rejection::line_too_long(),
        ParseError::InvalidResponse { .. } => Rejection::unknown_command(),
        ParseError::NeedsMoreData { .. } => unreachable!("handled by the caller"),
    }
}

fn push_ok(out: &mut Vec<u8>, code: u16, esc: (u8, u8, u8), text: &str) {
    smtp_proto::Response::new(code, esc.0, esc.1, esc.2, text)
        .write(out)
        .expect("Vec<u8> writes are infallible");
}

impl<T: Transport> Session<T> {
    /// Starts a new session; the greeting (`220 <hostname> ESMTP`) is queued immediately
    /// (take it via [`Session::take_output`]).
    pub fn new(cfg: Arc<Config>) -> Self {
        Self::with_greeting(cfg, "ESMTP")
    }

    /// Like [`Session::new`], with `text` following `220 <hostname> ` in the greeting.
    /// Control characters in `text` (notably CR/LF) are replaced by spaces.
    pub fn with_greeting(cfg: Arc<Config>, text: &str) -> Self {
        let mut output = Vec::new();
        use std::io::Write;
        let _ = write!(
            output,
            "220 {} {}\r\n",
            cfg.hostname,
            crate::reply::sanitize_text(text)
        );
        Self {
            cfg,
            phase: Phase::Fresh,
            command: RequestReceiver::default(),
            input: Vec::new(),
            output,
            bad_commands: 0,
            idle_commands: 0,
            _transport: PhantomData,
        }
    }

    /// Buffers newly received bytes for the next [`Session::poll`].
    pub fn feed(&mut self, bytes: &[u8]) {
        self.input.extend_from_slice(bytes);
    }

    /// Takes the bytes ready to be written to the peer, if any.
    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.output)
    }

    /// Whether the session is currently receiving (or discarding a rejected) `DATA`/`BDAT`
    /// payload (used by drivers to pick an appropriate read timeout and overall deadline).
    pub fn in_data(&self) -> bool {
        matches!(self.phase, Phase::Receiving { .. } | Phase::Discard { .. })
    }

    /// Anti-slowloris/command-timeout expiry: returns the final `421` bytes to write, then close.
    pub fn timed_out(mut self) -> Vec<u8> {
        push_ok(
            &mut self.output,
            421,
            (4, 4, 2),
            "timeout waiting for input",
        );
        self.output
    }

    /// The server is shutting down: returns the final `421` bytes to write, then close.
    pub fn shutting_down(mut self) -> Vec<u8> {
        push_ok(&mut self.output, 421, (4, 3, 2), "service shutting down");
        self.output
    }

    fn push_reply(&mut self, r: &Rejection) {
        r.write(&mut self.output);
    }

    /// Sends `r`, then either continues in `otherwise` or, for a `421`, arms the connection
    /// to close on the next [`Session::poll`].
    fn reply_and_continue(&mut self, r: &Rejection, otherwise: Phase) {
        self.push_reply(r);
        self.phase = if r.closes_session() {
            Phase::Closing
        } else {
            otherwise
        };
    }

    /// Sends `r` and leaves the current phase untouched (unless `r` closes the session).
    fn reject_keep_phase(&mut self, r: &Rejection) {
        let phase = std::mem::replace(&mut self.phase, Phase::Fresh);
        self.reply_and_continue(r, phase);
    }

    fn into_transport<U: Transport>(self) -> Session<U> {
        Session {
            cfg: self.cfg,
            phase: Phase::Fresh,
            command: RequestReceiver::default(),
            // Deliberately dropped: any bytes pipelined before STARTTLS completes must not
            // survive the upgrade (defense against STARTTLS command injection).
            input: Vec::new(),
            output: Vec::new(),
            bad_commands: self.bad_commands,
            idle_commands: self.idle_commands,
            _transport: PhantomData,
        }
    }

    fn ehlo_response(&self) -> Vec<u8> {
        let mut caps = EXT_ENHANCED_STATUS_CODES;
        if self.cfg.pipelining {
            caps |= EXT_PIPELINING;
        }
        if self.cfg.eightbitmime {
            caps |= EXT_8BIT_MIME;
        }
        if self.cfg.dsn {
            caps |= EXT_DSN;
        }
        if self.cfg.smtputf8 {
            caps |= EXT_SMTP_UTF8;
        }
        if self.cfg.chunking {
            caps |= EXT_CHUNKING;
        }
        if self.cfg.max_message_size.is_some() {
            caps |= EXT_SIZE;
        }
        if T::OFFERS_STARTTLS {
            caps |= EXT_START_TLS;
        }
        let mut resp = EhloResponse::new(self.cfg.hostname.as_str());
        resp.capabilities = caps;
        if let Some(size) = self.cfg.max_message_size {
            resp.size = size.get();
        }
        let mut out = Vec::new();
        resp.write(&mut out).expect("Vec<u8> writes are infallible");
        out
    }

    /// Advances the state machine as far as possible with the bytes fed so far.
    pub fn poll(mut self) -> Poll<T> {
        match std::mem::replace(&mut self.phase, Phase::Fresh) {
            Phase::Closing => Poll::Closed(self.take_output()),
            Phase::Receiving {
                envelope,
                receiver,
                size,
            } => self.poll_receiving(envelope, receiver, size),
            Phase::Discard { receiver, reply } => self.poll_discard(receiver, reply),
            other => {
                self.phase = other;
                self.poll_commands()
            }
        }
    }

    fn poll_commands(mut self) -> Poll<T> {
        loop {
            let mut it = self.input.iter();
            let res = self.command.ingest(&mut it);
            let consumed = self.input.len() - it.len();
            match res {
                Ok(req) => {
                    let req = req.into_owned();
                    self.input.drain(..consumed);
                    match dispatch(self, req) {
                        Dispatch::Continue(s) => self = s,
                        Dispatch::Stop(p) => return p,
                    }
                }
                Err(ParseError::NeedsMoreData { .. }) => {
                    self.input.clear();
                    return Poll::NeedInput(self);
                }
                Err(e) => {
                    self.input.drain(..consumed);
                    self.push_reply(&parse_error_reply(e));
                    self.bad_commands += 1;
                    if self.bad_commands >= self.cfg.max_bad_commands.get() {
                        self.push_reply(&Rejection::too_many_bad_commands());
                        return Poll::Closed(self.take_output());
                    }
                }
            }
        }
    }

    /// Feeds buffered input to `receiver`, dropping what it consumed; returns whether the
    /// payload finished and the bytes extracted.
    fn ingest_payload(&mut self, receiver: &mut AnyReceiver) -> (bool, Vec<u8>) {
        let mut out = Vec::new();
        let mut it = self.input.iter();
        let complete = receiver.ingest(&mut it, &mut out);
        let consumed = self.input.len() - it.len();
        self.input.drain(..consumed);
        (complete, out)
    }

    fn poll_receiving(
        mut self,
        envelope: Envelope,
        mut receiver: AnyReceiver,
        mut size: usize,
    ) -> Poll<T> {
        if self.input.is_empty() {
            self.phase = Phase::Receiving {
                envelope,
                receiver,
                size,
            };
            return Poll::NeedInput(self);
        }
        let (complete, chunk) = self.ingest_payload(&mut receiver);
        size += chunk.len();

        if let Some(limit) = self.cfg.max_message_size
            && size > limit.get()
        {
            self.phase = Phase::Discard {
                receiver,
                reply: Rejection::too_big(),
            };
            return Poll::Event(Event::DataAbort(ResumeToken {
                session: self,
                ack: false,
            }));
        }

        if complete && receiver.is_last() {
            return Poll::Event(Event::DataEnd(DataEndRequest {
                session: self,
                envelope,
                message: chunk,
            }));
        }

        if complete {
            // Non-last BDAT chunk finished: surface it, then wait for the next command.
            return Poll::Event(Event::DataChunk(DataChunkRequest {
                session: self,
                envelope,
                receiver: None,
                chunk,
                size,
            }));
        }

        if chunk.is_empty() {
            self.phase = Phase::Receiving {
                envelope,
                receiver,
                size,
            };
            return Poll::NeedInput(self);
        }

        Poll::Event(Event::DataChunk(DataChunkRequest {
            session: self,
            envelope,
            receiver: Some(receiver),
            chunk,
            size,
        }))
    }

    fn poll_discard(mut self, mut receiver: AnyReceiver, reply: Rejection) -> Poll<T> {
        if self.input.is_empty() {
            self.phase = Phase::Discard { receiver, reply };
            return Poll::NeedInput(self);
        }
        let (complete, _) = self.ingest_payload(&mut receiver);
        if complete && receiver.is_last() {
            self.reply_and_continue(&reply, Phase::Greeted);
        } else {
            self.phase = Phase::Discard { receiver, reply };
        }
        self.poll()
    }
}

#[allow(clippy::large_enum_variant)] // see the note on `Poll`
enum Dispatch<T: Transport> {
    Continue(Session<T>),
    Stop(Poll<T>),
}

#[derive(Clone, Copy)]
enum Greeting {
    Ehlo,
    Helo,
}

fn hello<T: Transport>(mut session: Session<T>, host: &str, kind: Greeting) -> Dispatch<T> {
    match Domain::parse_client(host) {
        Ok(host) => {
            let req = EhloRequest {
                session,
                host,
                ehlo: matches!(kind, Greeting::Ehlo),
            };
            Dispatch::Stop(Poll::Event(match kind {
                Greeting::Ehlo => Event::Ehlo(req),
                Greeting::Helo => Event::Helo(req),
            }))
        }
        Err(_) => {
            let name = match kind {
                Greeting::Ehlo => "EHLO",
                Greeting::Helo => "HELO",
            };
            session.push_reply(&Rejection::syntax_error(format!("invalid {name} domain")));
            Dispatch::Continue(session)
        }
    }
}

fn dispatch<T: Transport>(mut session: Session<T>, req: Request<String>) -> Dispatch<T> {
    match req {
        Request::Ehlo { host } | Request::Lhlo { host } => hello(session, &host, Greeting::Ehlo),
        Request::Helo { host } => hello(session, &host, Greeting::Helo),
        Request::Mail { from } => {
            if !matches!(session.phase, Phase::Greeted) {
                session.push_reply(&Rejection::bad_sequence(
                    "EHLO/HELO required, or MAIL already in progress",
                ));
                return Dispatch::Continue(session);
            }
            match Sender::from_smtp(from, &session.cfg) {
                Ok(sender) => {
                    if let (Some(limit), Some(size)) = (session.cfg.max_message_size, sender.size())
                        && size.get() > limit.get()
                    {
                        session.push_reply(&Rejection::too_big());
                        return Dispatch::Continue(session);
                    }
                    Dispatch::Stop(Poll::Event(Event::Mail(MailRequest { session, sender })))
                }
                Err(e) => {
                    session.push_reply(&e);
                    Dispatch::Continue(session)
                }
            }
        }
        Request::Rcpt { to } => {
            if !matches!(session.phase, Phase::Mail { .. } | Phase::Rcpt { .. }) {
                session.push_reply(&Rejection::bad_sequence("MAIL FROM required first"));
                return Dispatch::Continue(session);
            }
            if let Phase::Rcpt { envelope } = &session.phase
                && envelope.rcpts().len().get() >= session.cfg.max_rcpts.get() as usize
            {
                session.push_reply(&Rejection::too_many_rcpts());
                return Dispatch::Continue(session);
            }
            match Recipient::from_smtp(to, &session.cfg) {
                Ok(recipient) => {
                    Dispatch::Stop(Poll::Event(Event::Rcpt(RcptRequest { session, recipient })))
                }
                Err(e) => {
                    session.push_reply(&e);
                    Dispatch::Continue(session)
                }
            }
        }
        Request::Data => {
            let Phase::Rcpt { envelope } = std::mem::replace(&mut session.phase, Phase::Fresh)
            else {
                session.push_reply(&Rejection::bad_sequence("RCPT TO required first"));
                session.phase = Phase::Greeted;
                return Dispatch::Continue(session);
            };
            Dispatch::Stop(Poll::Event(Event::DataStart(DataStartRequest {
                session,
                envelope,
                first_bdat: None,
            })))
        }
        Request::Bdat {
            chunk_size,
            is_last,
        } => {
            if !session.cfg.chunking {
                session.push_reply(&Rejection::not_implemented());
                return Dispatch::Continue(session);
            }
            match std::mem::replace(&mut session.phase, Phase::Fresh) {
                Phase::Rcpt { envelope } => {
                    Dispatch::Stop(Poll::Event(Event::DataStart(DataStartRequest {
                        session,
                        envelope,
                        first_bdat: Some((chunk_size, is_last)),
                    })))
                }
                Phase::Chunking { envelope, size } => {
                    session.phase = Phase::Receiving {
                        envelope,
                        receiver: AnyReceiver::Bdat(BdatReceiver::new(chunk_size, is_last)),
                        size,
                    };
                    // The chunk's payload bytes (still in `input`) are not a command; route
                    // them through `poll_receiving` instead of back through the command loop.
                    Dispatch::Stop(session.poll())
                }
                other => {
                    session.phase = other;
                    session.push_reply(&Rejection::bad_sequence("RCPT TO required first"));
                    Dispatch::Continue(session)
                }
            }
        }
        Request::Rset => {
            session.phase = if matches!(session.phase, Phase::Fresh) {
                Phase::Fresh
            } else {
                Phase::Greeted
            };
            Dispatch::Stop(Poll::Event(Event::Rset(ResumeToken { session, ack: true })))
        }
        Request::Quit => Dispatch::Stop(Poll::Event(Event::Quit(QuitToken { session }))),
        Request::StartTls => {
            if T::OFFERS_STARTTLS && matches!(session.phase, Phase::Fresh | Phase::Greeted) {
                Dispatch::Stop(Poll::Event(T::start_tls_event(session)))
            } else {
                session.push_reply(&Rejection::not_implemented());
                Dispatch::Continue(session)
            }
        }
        Request::Noop { .. } | Request::Vrfy { .. } | Request::Help { .. } => {
            if session.idle_commands >= session.cfg.max_idle_commands.get() {
                session.push_reply(&Rejection::closing("too many commands"));
                return Dispatch::Stop(Poll::Closed(session.take_output()));
            }
            session.idle_commands += 1;
            let (code, esc, text) = match req {
                Request::Vrfy { .. } => {
                    (252, (2, 5, 0), "cannot VRFY user, but will accept message")
                }
                Request::Help { .. } => (214, (2, 0, 0), "OK"),
                _ => (250, (2, 0, 0), "OK"),
            };
            push_ok(&mut session.output, code, esc, text);
            Dispatch::Continue(session)
        }
        Request::Expn { .. }
        | Request::Auth { .. }
        | Request::Burl { .. }
        | Request::Atrn { .. }
        | Request::Etrn { .. } => {
            session.push_reply(&Rejection::not_implemented());
            Dispatch::Continue(session)
        }
    }
}

macro_rules! decide {
    ($($t:ident),*) => {$(
        impl<T: Transport> $t<T> {
            /// Accepts or rejects based on the handler's decision.
            pub fn decide(self, r: Result<(), Rejection>) -> Session<T> {
                match r {
                    Ok(()) => self.accept(),
                    Err(e) => self.reject(e),
                }
            }
        }
    )*};
}
decide!(
    EhloRequest,
    MailRequest,
    RcptRequest,
    DataStartRequest,
    DataChunkRequest
);

/// `EHLO` (or `HELO`) request; decide whether to accept it.
pub struct EhloRequest<T: Transport> {
    session: Session<T>,
    host: Domain,
    ehlo: bool,
}

/// `HELO` request; the same type as [`EhloRequest`].
pub type HeloRequest<T> = EhloRequest<T>;

impl<T: Transport> EhloRequest<T> {
    /// The peer-supplied domain (hostname or address literal).
    pub fn host(&self) -> &Domain {
        &self.host
    }

    /// Accepts the EHLO (advertises capabilities) or HELO, (re)starting the transaction.
    pub fn accept(mut self) -> Session<T> {
        if self.ehlo {
            let ehlo = self.session.ehlo_response();
            self.session.output.extend_from_slice(&ehlo);
        } else {
            let _ = write!(self.session.output, "250 {}\r\n", self.session.cfg.hostname);
        }
        self.session.phase = Phase::Greeted;
        self.session
    }

    /// Rejects the request; any transaction already in progress is left untouched.
    pub fn reject(mut self, r: Rejection) -> Session<T> {
        self.session.reject_keep_phase(&r);
        self.session
    }
}

/// `MAIL FROM` request; decide whether to accept it.
pub struct MailRequest<T: Transport> {
    session: Session<T>,
    sender: Sender,
}

impl<T: Transport> MailRequest<T> {
    /// The parsed sender.
    pub fn sender(&self) -> &Sender {
        &self.sender
    }

    /// Accepts: the transaction now has a sender, waiting for `RCPT TO`.
    pub fn accept(mut self) -> Session<T> {
        push_ok(&mut self.session.output, 250, (2, 1, 0), "OK");
        self.session.phase = Phase::Mail {
            sender: self.sender,
        };
        self.session
    }

    /// Rejects the sender; the transaction is not started.
    pub fn reject(mut self, r: Rejection) -> Session<T> {
        self.session.reply_and_continue(&r, Phase::Greeted);
        self.session
    }
}

/// `RCPT TO` request; decide whether to accept it.
pub struct RcptRequest<T: Transport> {
    session: Session<T>,
    recipient: Recipient,
}

impl<T: Transport> RcptRequest<T> {
    /// The transaction's sender.
    pub fn sender(&self) -> &Sender {
        match &self.session.phase {
            Phase::Mail { sender } => sender,
            Phase::Rcpt { envelope } => envelope.sender(),
            _ => unreachable!("RcptRequest is only built from Mail/Rcpt"),
        }
    }

    /// The parsed recipient.
    pub fn recipient(&self) -> &Recipient {
        &self.recipient
    }

    /// Accepts: adds the recipient to the envelope.
    pub fn accept(mut self) -> Session<T> {
        push_ok(&mut self.session.output, 250, (2, 1, 5), "OK");
        self.session.phase = match std::mem::replace(&mut self.session.phase, Phase::Fresh) {
            Phase::Mail { sender } => Phase::Rcpt {
                envelope: Envelope::new(sender, NonEmpty::new(self.recipient)),
            },
            Phase::Rcpt { mut envelope } => {
                envelope.push_rcpt(self.recipient);
                Phase::Rcpt { envelope }
            }
            _ => unreachable!("RcptRequest is only built from Mail/Rcpt"),
        };
        self.session
    }

    /// Rejects the recipient; the transaction (and any prior recipients) is kept.
    pub fn reject(mut self, r: Rejection) -> Session<T> {
        self.session.reject_keep_phase(&r);
        self.session
    }
}

/// `DATA`/first `BDAT` chunk; the envelope is complete, decide whether to accept the message.
pub struct DataStartRequest<T: Transport> {
    session: Session<T>,
    envelope: Envelope,
    /// `Some((chunk_size, is_last))` for the `BDAT` that triggered this event, `None` for `DATA`.
    first_bdat: Option<(usize, bool)>,
}

impl<T: Transport> DataStartRequest<T> {
    /// The complete envelope (sender + all recipients accepted so far).
    pub fn envelope(&self) -> &Envelope {
        &self.envelope
    }

    /// Accepts: starts receiving message bytes.
    pub fn accept(mut self) -> Session<T> {
        let receiver = match self.first_bdat {
            Some((chunk_size, is_last)) => {
                AnyReceiver::Bdat(BdatReceiver::new(chunk_size, is_last))
            }
            None => {
                self.session
                    .output
                    .extend_from_slice(b"354 Start mail input; end with <CRLF>.<CRLF>\r\n");
                AnyReceiver::Data(DataReceiver::new())
            }
        };
        self.session.phase = Phase::Receiving {
            envelope: self.envelope,
            receiver,
            size: 0,
        };
        self.session
    }

    /// Rejects the message; the transaction is discarded (client must `MAIL FROM` again).
    pub fn reject(mut self, r: Rejection) -> Session<T> {
        self.session.reply_and_continue(&r, Phase::Greeted);
        self.session
    }
}

/// A chunk of message bytes became available; decide whether to keep receiving.
pub struct DataChunkRequest<T: Transport> {
    session: Session<T>,
    envelope: Envelope,
    receiver: Option<AnyReceiver>,
    chunk: Vec<u8>,
    size: usize,
}

impl<T: Transport> DataChunkRequest<T> {
    /// The newly received (dot-unstuffed) bytes.
    pub fn chunk(&self) -> &[u8] {
        &self.chunk
    }

    /// Keeps receiving.
    pub fn accept(mut self) -> Session<T> {
        self.session.phase = match self.receiver {
            Some(receiver) => Phase::Receiving {
                envelope: self.envelope,
                receiver,
                size: self.size,
            },
            None => {
                // A non-last BDAT chunk just finished: ack it, then wait for the next command.
                push_ok(
                    &mut self.session.output,
                    250,
                    (2, 6, 0),
                    "message chunk accepted",
                );
                Phase::Chunking {
                    envelope: self.envelope,
                    size: self.size,
                }
            }
        };
        self.session
    }

    /// Rejects: the rest of the message is swallowed, then `r` is sent.
    pub fn reject(mut self, r: Rejection) -> Session<T> {
        match self.receiver {
            // Sent once swallowing completes; poll_discard routes it through reply_and_continue.
            Some(receiver) => self.session.phase = Phase::Discard { receiver, reply: r },
            // Between BDAT chunks: nothing left to swallow, abort the transaction now.
            None => self.session.reply_and_continue(&r, Phase::Greeted),
        }
        self.session
    }
}

/// The message is complete; decide the final reply.
pub struct DataEndRequest<T: Transport> {
    session: Session<T>,
    envelope: Envelope,
    message: Vec<u8>,
}

impl<T: Transport> DataEndRequest<T> {
    /// The complete envelope.
    pub fn envelope(&self) -> &Envelope {
        &self.envelope
    }

    /// Accepts the message: `text` becomes the `250` reply.
    pub fn accept(mut self, text: impl AsRef<str>) -> Session<T> {
        push_ok(
            &mut self.session.output,
            250,
            (2, 0, 0),
            &crate::reply::sanitize_text(text.as_ref()),
        );
        self.session.phase = Phase::Greeted;
        self.session
    }

    /// Rejects the message.
    pub fn reject(mut self, r: Rejection) -> Session<T> {
        self.session.reply_and_continue(&r, Phase::Greeted);
        self.session
    }

    /// Accepts or rejects based on the handler's decision; the message bytes accumulated by
    /// the default [`crate::Handler::data_chunk`] have already been handed to the handler.
    pub fn decide(self, r: Result<String, Rejection>) -> Session<T> {
        match r {
            Ok(text) => self.accept(text),
            Err(e) => self.reject(e),
        }
    }

    /// The buffered message, if `data_chunk` was never overridden to stream it elsewhere.
    pub fn take_message(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.message)
    }
}

/// `RSET` or an internally-aborted transaction; call [`ResumeToken::resume`] after cleanup.
pub struct ResumeToken<T: Transport> {
    session: Session<T>,
    /// `RSET` needs an immediate `250` ack; `DataAbort` already left the session in
    /// `Phase::Discard`, whose stored rejection is sent once swallowing completes.
    ack: bool,
}

impl<T: Transport> ResumeToken<T> {
    /// Continues the session (the reply, if any, is already queued or will be sent once the
    /// aborted message has been swallowed).
    pub fn resume(mut self) -> Session<T> {
        if self.ack {
            push_ok(&mut self.session.output, 250, (2, 0, 0), "OK");
        }
        self.session
    }
}

/// `QUIT`; call [`QuitToken::close`] to get the final bytes to write.
pub struct QuitToken<T: Transport> {
    session: Session<T>,
}

impl<T: Transport> QuitToken<T> {
    /// Renders the `221` goodbye and returns the final bytes to write; the connection must close.
    pub fn close(mut self) -> Vec<u8> {
        push_ok(&mut self.session.output, 221, (2, 0, 0), "Bye");
        self.session.take_output()
    }
}

/// `STARTTLS`, offered only on [`Plain`]; call [`StartTlsToken::established`] after the TLS handshake.
pub struct StartTlsToken {
    session: Session<Plain>,
}

impl StartTlsToken {
    /// Queues (and returns) the `220 Ready to start TLS` reply; flush it before the handshake.
    pub fn output(&mut self) -> Vec<u8> {
        push_ok(
            &mut self.session.output,
            220,
            (2, 0, 0),
            "Ready to start TLS",
        );
        self.session.take_output()
    }

    /// Consumes the plain session, yielding a fresh encrypted one (EHLO required again).
    pub fn established(self) -> Session<Tls> {
        self.session.into_transport()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Hostname, MessageSize};

    fn cfg() -> Arc<Config> {
        cfg_with(|_| {})
    }

    fn cfg_with(f: impl FnOnce(&mut Config)) -> Arc<Config> {
        let mut c = Config::new(Hostname::new("mx.example.org").unwrap());
        f(&mut c);
        Arc::new(c)
    }

    /// Feeds `input` to a fresh session, auto-accepting `EHLO`/`MAIL`/`RCPT` events, and returns
    /// the first other poll result. Output of accepted steps is discarded.
    fn drive(cfg: Arc<Config>, input: &[u8]) -> Poll<Cleartext> {
        let mut s = Session::<Cleartext>::new(cfg);
        s.take_output();
        s.feed(input);
        loop {
            s = match s.poll() {
                Poll::Event(Event::Ehlo(r)) => r.accept(),
                Poll::Event(Event::Mail(r)) => r.accept(),
                Poll::Event(Event::Rcpt(r)) => r.accept(),
                other => return other,
            };
            s.take_output();
        }
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn greeting_is_queued_immediately() {
        let mut s = Session::<Cleartext>::new(cfg());
        assert!(s.take_output().starts_with(b"220 mx.example.org ESMTP\r\n"));
    }

    fn ehlo_host(line: &[u8]) -> Option<Domain> {
        let mut s = Session::<Cleartext>::new(cfg());
        s.take_output();
        s.feed(line);
        match s.poll() {
            Poll::Event(Event::Ehlo(req)) => Some(req.host().clone()),
            Poll::NeedInput(mut s) => {
                assert!(s.take_output().starts_with(b"501"), "{line:?}");
                None
            }
            _ => panic!("unexpected poll result for {line:?}"),
        }
    }

    #[test]
    fn custom_greeting_text_is_sanitised() {
        let mut s = Session::<Cleartext>::with_greeting(cfg(), "hello there");
        assert_eq!(s.take_output(), b"220 mx.example.org hello there\r\n");

        let mut s = Session::<Cleartext>::with_greeting(cfg(), "hi\r\n250 injected");
        assert_eq!(s.take_output(), b"220 mx.example.org hi  250 injected\r\n");

        let mut s = Session::<Cleartext>::new(cfg());
        assert_eq!(s.take_output(), b"220 mx.example.org ESMTP\r\n");
    }

    #[test]
    fn ehlo_accepts_literals_and_underscores() {
        assert_eq!(
            ehlo_host(b"EHLO [1.2.3.4]\r\n"),
            Some(Domain::Literal("1.2.3.4".parse().unwrap()))
        );
        assert_eq!(
            ehlo_host(b"EHLO [IPv6:::1]\r\n"),
            Some(Domain::Literal("::1".parse().unwrap()))
        );
        assert_eq!(
            ehlo_host(b"EHLO [IPv6:2001:db8::1]\r\n"),
            Some(Domain::Literal("2001:db8::1".parse().unwrap()))
        );
        let h = ehlo_host(b"EHLO my_host.example\r\n").unwrap();
        assert_eq!(h.to_string(), "my_host.example");
    }

    #[test]
    fn helo_accepts_literals_and_underscores() {
        for line in [&b"HELO [1.2.3.4]\r\n"[..], b"HELO my_host.example\r\n"] {
            let mut s = Session::<Cleartext>::new(cfg());
            s.take_output();
            s.feed(line);
            assert!(matches!(s.poll(), Poll::Event(Event::Helo(_))), "{line:?}");
        }
    }

    #[test]
    fn ehlo_rejects_garbage_with_501() {
        for line in [
            &b"EHLO [1.2.3]\r\n"[..],
            b"EHLO [::1]\r\n",
            b"EHLO [IPv6:1.2.3.4]\r\n",
            b"EHLO a\x01b\r\n",
            b"EHLO a\x00\r\n",
            b"EHLO a\x7f\r\n",
            b"EHLO -a_b\r\n",
            b"EHLO a..b\r\n",
            b"EHLO \xc3\xa9.example\r\n",
        ] {
            assert_eq!(ehlo_host(line), None, "{line:?}");
        }
    }

    #[test]
    fn ehlo_advertises_starttls_only_for_plain() {
        let mut s = Session::<Plain>::new(cfg());
        s.take_output();
        s.feed(b"EHLO client.example\r\n");
        let Poll::Event(Event::Ehlo(req)) = s.poll() else {
            panic!("expected Ehlo event")
        };
        let mut s = req.accept();
        let out = s.take_output();
        assert!(contains(&out, b"STARTTLS"));
        assert!(contains(&out, b"ENHANCEDSTATUSCODES"));

        let mut s = Session::<Cleartext>::new(cfg());
        s.take_output();
        s.feed(b"EHLO client.example\r\n");
        let Poll::Event(Event::Ehlo(req)) = s.poll() else {
            panic!("expected Ehlo event")
        };
        let out = req.accept().take_output();
        assert!(!contains(&out, b"STARTTLS"));
    }

    #[test]
    fn rcpt_with_orcpt_and_mail_with_envid_are_accepted_and_decoded_once() {
        let Poll::Event(Event::DataStart(req)) = drive(
            cfg(),
            b"EHLO client\r\nMAIL FROM:<a@b> ENVID=x+2Bq\r\nRCPT TO:<c@d> ORCPT=rfc822;c@d NOTIFY=FAILURE\r\nDATA\r\n",
        ) else {
            panic!("expected DataStart event")
        };
        let env = req.envelope();
        assert_eq!(env.sender().env_id().unwrap().as_str(), "x+q");
        let orcpt = env.rcpts().first().orcpt().unwrap();
        assert_eq!((orcpt.addr_type(), orcpt.addr()), ("rfc822", "c@d"));
    }

    #[test]
    fn rcpt_before_mail_is_bad_sequence() {
        let Poll::NeedInput(mut s) = drive(cfg(), b"EHLO client\r\nRCPT TO:<a@b>\r\n") else {
            panic!("expected NeedInput")
        };
        assert!(s.take_output().starts_with(b"503"));
    }

    #[test]
    fn data_before_rcpt_is_bad_sequence() {
        let Poll::NeedInput(mut s) = drive(cfg(), b"EHLO client\r\nMAIL FROM:<a@b>\r\nDATA\r\n")
        else {
            panic!("expected NeedInput")
        };
        assert!(s.take_output().starts_with(b"503"));
    }

    #[test]
    fn full_transaction_with_dot_unstuffing_across_feed_boundaries() {
        let Poll::Event(Event::DataStart(req)) = drive(
            cfg(),
            b"EHLO client\r\nMAIL FROM:<a@b>\r\nRCPT TO:<c@d>\r\nDATA\r\n",
        ) else {
            panic!("expected DataStart event")
        };
        assert_eq!(req.envelope().rcpts().len().get(), 1);
        let mut s = req.accept();
        assert!(s.take_output().starts_with(b"354"));

        // ".." at the start of a line unstuffs to "."; split before the terminator is seen.
        // Chunks may arrive as they're parsed (mirroring the driver's `data_chunk` buffering).
        let mut message = Vec::new();
        s.feed(b"Subject: hi\r\n..still one dot\r\n");
        let Poll::Event(Event::DataChunk(chunk)) = s.poll() else {
            panic!("expected DataChunk event")
        };
        message.extend_from_slice(chunk.chunk());
        let s = chunk.accept();
        let Poll::NeedInput(mut s) = s.poll() else {
            panic!("expected NeedInput before terminator")
        };
        s.feed(b".\r\n");
        let Poll::Event(Event::DataEnd(mut req)) = s.poll() else {
            panic!("expected DataEnd event")
        };
        message.extend_from_slice(&req.take_message());
        assert_eq!(message, b"Subject: hi\r\n.still one dot\r\n");
        let mut s = req.accept("queued");
        assert!(s.take_output().starts_with(b"250"));
    }

    #[test]
    fn mail_from_size_over_limit_is_rejected_without_event() {
        let cfg = cfg_with(|c| c.max_message_size = MessageSize::new(10));
        let Poll::NeedInput(mut s) = drive(cfg, b"EHLO client\r\nMAIL FROM:<a@b> SIZE=1000\r\n")
        else {
            panic!("expected NeedInput")
        };
        assert!(s.take_output().starts_with(b"552"));
    }

    #[test]
    fn oversize_message_body_is_discarded_then_552() {
        let cfg = cfg_with(|c| c.max_message_size = MessageSize::new(5));
        let Poll::Event(Event::DataStart(req)) = drive(
            cfg,
            b"EHLO client\r\nMAIL FROM:<a@b>\r\nRCPT TO:<c@d>\r\nDATA\r\n",
        ) else {
            panic!("expected DataStart event")
        };
        let mut s = req.accept();
        s.take_output();
        s.feed(b"way more than five bytes");
        let Poll::Event(Event::DataAbort(n)) = s.poll() else {
            panic!("expected DataAbort event")
        };
        let s = n.resume();
        let Poll::NeedInput(mut s) = s.poll() else {
            panic!("expected NeedInput while discarding")
        };
        s.feed(b"\r\n.\r\n");
        let Poll::NeedInput(mut s) = s.poll() else {
            panic!("expected NeedInput after 552")
        };
        assert!(s.take_output().starts_with(b"552"));
    }

    #[test]
    fn bdat_last_chunk_completes_message() {
        let Poll::NeedInput(mut s) = drive(
            cfg(),
            b"EHLO client\r\nMAIL FROM:<a@b>\r\nRCPT TO:<c@d>\r\n",
        ) else {
            panic!("expected NeedInput")
        };

        s.feed(b"BDAT 5\r\nhello");
        let Poll::Event(Event::DataStart(req)) = s.poll() else {
            panic!("expected DataStart event")
        };
        let s = req.accept();
        let Poll::Event(Event::DataChunk(chunk)) = s.poll() else {
            panic!("expected DataChunk event")
        };
        assert_eq!(chunk.chunk(), b"hello");
        let mut s = chunk.accept();
        assert!(s.take_output().starts_with(b"250"));

        s.feed(b"BDAT 3 LAST\r\nbye");
        let Poll::Event(Event::DataEnd(mut req)) = s.poll() else {
            panic!("expected DataEnd event")
        };
        assert_eq!(req.take_message(), b"bye");
        let mut s = req.accept("queued");
        assert!(s.take_output().starts_with(b"250"));
    }

    #[test]
    fn pipelined_commands_are_processed_in_order() {
        let input = b"EHLO client\r\nMAIL FROM:<a@b>\r\nRCPT TO:<c@d>\r\nRCPT TO:<e@f>\r\n";
        let Poll::NeedInput(mut s) = drive(cfg(), input) else {
            panic!("expected NeedInput")
        };
        assert!(s.take_output().is_empty());
    }

    #[test]
    fn starttls_drops_pipelined_input_and_requires_reehlo() {
        let mut s = Session::<Plain>::new(cfg());
        s.take_output();
        s.feed(b"EHLO client\r\nSTARTTLS\r\nMAIL FROM:<injected@evil>\r\n");
        let Poll::Event(Event::Ehlo(req)) = s.poll() else {
            panic!("expected Ehlo event")
        };
        let Poll::Event(Event::StartTls(mut tls)) = req.accept().poll() else {
            panic!("expected StartTls event")
        };
        assert!(tls.output().ends_with(b"220 2.0.0 Ready to start TLS\r\n"));
        let s: Session<Tls> = tls.established();
        let Poll::NeedInput(mut s) = s.poll() else {
            panic!("injected MAIL must be gone")
        };
        s.feed(b"MAIL FROM:<a@b>\r\n");
        let Poll::NeedInput(mut s) = s.poll() else {
            panic!("expected NeedInput")
        };
        assert!(s.take_output().starts_with(b"503"));
    }

    #[test]
    fn starttls_is_not_offered_on_tls_or_cleartext() {
        let mut s = Session::<Cleartext>::new(cfg());
        s.take_output();
        s.feed(b"STARTTLS\r\n");
        let Poll::NeedInput(mut s) = s.poll() else {
            panic!("expected NeedInput")
        };
        assert!(s.take_output().starts_with(b"502"));
    }

    #[test]
    fn max_bad_commands_closes_with_421() {
        let cfg = cfg_with(|c| c.max_bad_commands = std::num::NonZeroU32::new(2).unwrap());
        let mut s = Session::<Cleartext>::new(cfg);
        s.take_output();
        s.feed(b"NOTACOMMAND\r\nNOTACOMMAND\r\n");
        let Poll::Closed(out) = s.poll() else {
            panic!("expected Closed")
        };
        assert!(out.starts_with(b"500"));
        assert!(contains(&out, b"421"));
    }

    #[test]
    fn noop_rset_quit() {
        let mut s = Session::<Cleartext>::new(cfg());
        s.take_output();
        s.feed(b"NOOP\r\n");
        let Poll::NeedInput(mut s) = s.poll() else {
            panic!("expected NeedInput")
        };
        assert!(s.take_output().starts_with(b"250"));

        s.feed(b"RSET\r\n");
        let Poll::Event(Event::Rset(n)) = s.poll() else {
            panic!("expected Rset event")
        };
        let mut s = n.resume();
        assert!(s.take_output().starts_with(b"250"));

        s.feed(b"QUIT\r\n");
        let Poll::Event(Event::Quit(q)) = s.poll() else {
            panic!("expected Quit event")
        };
        assert!(q.close().starts_with(b"221"));
    }

    #[test]
    fn rcpt_over_max_rcpts_is_452() {
        let cfg = cfg_with(|c| c.max_rcpts = std::num::NonZeroU32::new(2).unwrap());
        let input = b"EHLO client\r\nMAIL FROM:<a@b>\r\nRCPT TO:<c@d>\r\nRCPT TO:<e@f>\r\n";
        let Poll::NeedInput(mut s) = drive(cfg, input) else {
            panic!("expected NeedInput")
        };
        s.feed(b"RCPT TO:<g@h>\r\n");
        let Poll::NeedInput(mut s) = s.poll() else {
            panic!("third RCPT must be rejected without an event")
        };
        let out = s.take_output();
        assert!(
            out.starts_with(b"452 4.5.3"),
            "{:?}",
            String::from_utf8_lossy(&out)
        );
        // Envelope is intact: DATA still works with the two accepted recipients.
        s.feed(b"DATA\r\n");
        assert!(matches!(s.poll(), Poll::Event(Event::DataStart(_))));
    }

    #[test]
    fn discard_phase_counts_as_in_data() {
        let cfg = cfg_with(|c| c.max_message_size = MessageSize::new(5));
        let Poll::Event(Event::DataStart(req)) = drive(
            cfg,
            b"EHLO client\r\nMAIL FROM:<a@b>\r\nRCPT TO:<c@d>\r\nDATA\r\n",
        ) else {
            panic!("expected DataStart event")
        };
        let mut s = req.accept();
        s.feed(b"way more than five bytes");
        let Poll::Event(Event::DataAbort(n)) = s.poll() else {
            panic!("expected DataAbort event")
        };
        let Poll::NeedInput(s) = n.resume().poll() else {
            panic!("expected NeedInput while discarding")
        };
        assert!(
            s.in_data(),
            "discarding must be covered by the data deadline"
        );
    }

    #[test]
    fn max_idle_commands_closes_with_421() {
        let cfg = cfg_with(|c| c.max_idle_commands = std::num::NonZeroU32::new(2).unwrap());
        let mut s = Session::<Cleartext>::new(cfg);
        s.take_output();
        s.feed(b"NOOP\r\nHELP\r\n");
        let Poll::NeedInput(mut s) = s.poll() else {
            panic!("expected NeedInput")
        };
        assert!(s.take_output().starts_with(b"250"));
        s.feed(b"VRFY x\r\n");
        let Poll::Closed(out) = s.poll() else {
            panic!("expected Closed")
        };
        assert!(out.starts_with(b"421"));
    }
}
