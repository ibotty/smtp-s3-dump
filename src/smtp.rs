use std::collections::HashSet;
use std::error::Error;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use mail_parser::{Message, MessageParser};
use smtp_server::{Envelope, ForwardPath, Handler, Recipient, Rejection, ReversePath, Sender};
use sqlx::PgPool;
use tracing::{info, warn};

use crate::db;
use crate::s3;

pub struct Config {
    pub s3: aws_sdk_s3::Client,
    pub pg_pool: PgPool,
    pub server: Arc<smtp_server::Config>,
    pub bucket: String,
    pub allowed_rcpts: Option<HashSet<String>>,
    /// Spam filter on the client-asserted `MAIL FROM`; there is no SMTP AUTH, SPF or DKIM, so
    /// this is not a security boundary.
    pub allowed_froms: Option<HashSet<String>>,
    pub check_db: bool,
}

struct MailEvent {
    started: Instant,
    peer: SocketAddr,
    from: String,
    rcpts: Vec<String>,
    rejected: Vec<String>,
    size: Option<usize>,
    message_id: Option<String>,
    stored: usize,
    error: Option<String>,
}

impl MailEvent {
    fn new(peer: SocketAddr, from: String) -> Self {
        MailEvent {
            started: Instant::now(),
            peer,
            from,
            rcpts: vec![],
            rejected: vec![],
            size: None,
            message_id: None,
            stored: 0,
            error: None,
        }
    }

    fn emit(self, outcome: &'static str) {
        macro_rules! log {
            ($level:ident) => {
                $level!(
                    peer = %self.peer,
                    from = %self.from,
                    rcpts = ?self.rcpts,
                    rejected = ?self.rejected,
                    size = self.size,
                    message_id = self.message_id.as_deref(),
                    stored = self.stored,
                    outcome,
                    error = self.error.as_deref(),
                    duration_ms = self.started.elapsed().as_millis() as u64,
                    "mail"
                )
            };
        }
        match outcome {
            "rejected" | "reset" | "failed" | "invalid" => log!(warn),
            _ => log!(info),
        }
    }
}

pub struct SmtpSession {
    config: Arc<Config>,
    peer: SocketAddr,
    event: Option<MailEvent>,
}

impl SmtpSession {
    pub fn new(config: Arc<Config>, peer: SocketAddr) -> Self {
        SmtpSession {
            config,
            peer,
            event: None,
        }
    }
}

impl Drop for SmtpSession {
    fn drop(&mut self) {
        if let Some(ev) = self.event.take() {
            let outcome = if ev.rcpts.is_empty() {
                "rejected"
            } else {
                "abandoned"
            };
            ev.emit(outcome);
        }
    }
}

impl Handler for SmtpSession {
    async fn mail(&mut self, sender: &Sender) -> Result<(), Rejection> {
        if let Some(ev) = self.event.take() {
            ev.emit("reset");
        }
        let mut ev = MailEvent::new(self.peer, sender.path().to_string());
        match sender.path() {
            ReversePath::Null => {
                ev.error = Some("null sender".to_string());
                ev.emit("rejected");
                Err(Rejection::not_authorized("null sender not accepted"))
            }
            ReversePath::Mailbox(_) => {
                self.event = Some(ev);
                Ok(())
            }
        }
    }

    async fn rcpt(&mut self, sender: &Sender, rcpt: &Recipient) -> Result<(), Rejection> {
        let rcpt = self.rcpt_addr(rcpt.path());
        let result = self.check_rcpt(&sender.path().to_string(), &rcpt).await;
        if let Some(ev) = &mut self.event {
            match &result {
                Ok(()) => ev.rcpts.push(rcpt),
                Err(r) => ev.rejected.push(format!("{rcpt}:{}", cause(r))),
            }
        }
        result
    }

    async fn data_end(&mut self, env: &Envelope, message: Vec<u8>) -> Result<String, Rejection> {
        let mut ev = self
            .event
            .take()
            .unwrap_or_else(|| MailEvent::new(self.peer, env.sender().path().to_string()));
        ev.size = Some(message.len());
        match self.store_all(&mut ev, env, &message).await {
            Ok(reply) => {
                ev.emit("stored");
                Ok(reply)
            }
            Err((outcome, rejection)) => {
                ev.error = Some(cause(&rejection));
                ev.emit(outcome);
                Err(rejection)
            }
        }
    }

    async fn rejected(&mut self, rejection: &Rejection) {
        match &mut self.event {
            Some(ev) => ev.rejected.push(rejection.to_string()),
            None => warn!(peer = %self.peer, rejection = %rejection, "command rejected"),
        }
    }

    async fn data_abort(&mut self) {
        if let Some(ev) = self.event.take() {
            ev.emit("aborted");
        }
    }

    async fn rset(&mut self) {
        if let Some(ev) = self.event.take() {
            ev.emit("rset");
        }
    }
}

fn validate(message: &Message<'_>) -> Result<(), &'static str> {
    if message.message_id().is_none() {
        return Err("message has no Message-ID");
    }
    if message.date().is_none() {
        return Err("message has no Date");
    }
    Ok(())
}

fn cause(r: &Rejection) -> String {
    let mut parts = Vec::new();
    let mut e = r.source();
    while let Some(x) = e {
        parts.push(x.to_string());
        e = x.source();
    }
    parts.join(": ")
}

fn denied(list: &Option<HashSet<String>>, addr: &str) -> bool {
    list.as_ref().is_some_and(|l| !l.contains(addr))
}

impl SmtpSession {
    fn rcpt_addr(&self, path: &ForwardPath) -> String {
        match path {
            ForwardPath::Postmaster => format!("postmaster@{}", self.config.server.hostname),
            ForwardPath::Mailbox(m) => m.to_string(),
        }
    }

    async fn check_rcpt(&self, from: &str, rcpt: &str) -> Result<(), Rejection> {
        let unavailable = |reason: &str| {
            Rejection::mailbox_unavailable("mailbox unavailable").with_source(reason)
        };

        if denied(&self.config.allowed_rcpts, rcpt) {
            return Err(unavailable("rcpt_not_allowed"));
        }

        if denied(&self.config.allowed_froms, from) {
            return Err(unavailable("from_not_allowed"));
        }

        if self.config.check_db {
            match db::check_address(&self.config.pg_pool, from, rcpt).await {
                Ok(true) => {}
                Ok(false) => return Err(unavailable("db_denied")),
                Err(e) => {
                    return Err(Rejection::transient("could not handle request")
                        .with_source(e.context("db_error")))
                }
            }
        }
        Ok(())
    }

    async fn store_all(
        &self,
        ev: &mut MailEvent,
        env: &Envelope,
        message: &[u8],
    ) -> Result<String, (&'static str, Rejection)> {
        let invalid = |e: &str| {
            (
                "invalid",
                Rejection::invalid_content(e.to_string()).with_source(e.to_string()),
            )
        };
        let parsed = MessageParser::default()
            .parse(message)
            .ok_or_else(|| invalid("cannot parse message"))?;
        ev.message_id = parsed.message_id().map(str::to_string);
        validate(&parsed).map_err(invalid)?;

        for rcpt in env.rcpts().iter() {
            let rcpt = self.rcpt_addr(rcpt.path());
            s3::upload_message(&self.config, &ev.from, &rcpt, &parsed)
                .await
                .map_err(|e| {
                    (
                        "failed",
                        Rejection::transient("could not handle request").with_source(e),
                    )
                })?;
            ev.stored += 1;
        }
        Ok(format!("Received {} bytes.", message.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cause_joins_the_source_chain() {
        let e = anyhow::anyhow!("connection refused").context("db_error");
        let r = Rejection::transient("could not handle request").with_source(e);
        assert_eq!(cause(&r), "db_error: connection refused");
        assert_eq!(cause(&Rejection::transient("x")), "");
    }

    fn check(eml: &str) -> Result<(), &'static str> {
        validate(&MessageParser::default().parse(eml.as_bytes()).unwrap())
    }

    const HEAD: &str = "Message-ID: <a@b>\r\nDate: Mon, 1 Jan 2024 00:00:00 +0000\r\nFrom: a@b\r\n";

    #[test]
    fn accepts_complete_message() {
        assert_eq!(check(&format!("{HEAD}\r\nhi\r\n")), Ok(()));
    }

    #[test]
    fn rejects_missing_message_id() {
        let eml = "Date: Mon, 1 Jan 2024 00:00:00 +0000\r\nFrom: a@b\r\n\r\nhi\r\n";
        assert_eq!(check(eml), Err("message has no Message-ID"));
    }

    #[test]
    fn rejects_missing_date() {
        let eml = "Message-ID: <a@b>\r\nFrom: a@b\r\n\r\nhi\r\n";
        assert_eq!(check(eml), Err("message has no Date"));
    }
}
