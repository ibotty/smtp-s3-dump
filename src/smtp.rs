use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use arc_swap::ArcSwap;
use mail_parser::{Message, MessageParser};
use smtp_server::{
    Envelope, ForwardPath, Handler, Hostname, MessageSize, Recipient, Rejection, ReversePath,
    Sender,
};
use sqlx::PgPool;
use tokio_rustls::rustls::ServerConfig;
use tracing::{debug, error, instrument, warn};

use crate::db;
use crate::s3;

pub struct SmtpBackend {
    pub config: Arc<ArcSwap<Config>>,
    pub server_config: Arc<smtp_server::Config>,
}

impl SmtpBackend {
    #[allow(clippy::too_many_arguments)]
    #[instrument(skip(s3_config, pg_pool, tls_config))]
    pub fn new(
        s3_config: aws_sdk_s3::Config,
        pg_pool: PgPool,
        tls_config: Arc<ServerConfig>,
        domain: &str,
        bucket: &str,
        allowed_rcpts: Option<HashSet<String>>,
        allowed_froms: Option<HashSet<String>>,
        check_db: bool,
    ) -> Result<SmtpBackend> {
        let bucket = bucket.to_string();
        let domain =
            Hostname::new(domain).map_err(|e| anyhow!("could not parse SMTP_DOMAIN: {}", e))?;
        let mut server_config = smtp_server::Config::new(domain.clone());
        server_config.max_message_size = MessageSize::new(100_000_000);

        let config = Arc::new(ArcSwap::from_pointee(Config {
            s3_config,
            pg_pool,
            tls_config,
            domain,
            bucket,
            allowed_rcpts,
            allowed_froms,
            check_db,
        }));
        debug!("got config");
        Ok(SmtpBackend {
            config,
            server_config: Arc::new(server_config),
        })
    }

    #[instrument(skip_all)]
    pub fn new_session(&self) -> Result<SmtpSession> {
        Ok(SmtpSession {
            config: self.config.load_full(),
        })
    }
}

pub struct Config {
    pub s3_config: aws_sdk_s3::Config,
    pub pg_pool: PgPool,
    pub tls_config: Arc<ServerConfig>,
    pub domain: Hostname,
    pub bucket: String,
    pub allowed_rcpts: Option<HashSet<String>>,
    /// Spam filter on the client-asserted `MAIL FROM`; there is no SMTP AUTH, SPF or DKIM, so
    /// this is not a security boundary.
    pub allowed_froms: Option<HashSet<String>>,
    pub check_db: bool,
}

pub struct SmtpSession {
    pub config: Arc<Config>,
}

impl Handler for SmtpSession {
    #[instrument(skip_all)]
    async fn mail(&mut self, sender: &Sender) -> Result<(), Rejection> {
        match sender.path() {
            ReversePath::Null => Err(Rejection::not_authorized("null sender not accepted")),
            ReversePath::Mailbox(_) => Ok(()),
        }
    }

    #[instrument(skip_all)]
    async fn rcpt(&mut self, sender: &Sender, rcpt: &Recipient) -> Result<(), Rejection> {
        debug!("handle RCPT");
        let rcpt = match rcpt.path() {
            ForwardPath::Postmaster => format!("postmaster@{}", self.config.domain),
            ForwardPath::Mailbox(m) => m.to_string(),
        };
        let from = sender.path().to_string();
        let unavailable = || Rejection::mailbox_unavailable("mailbox unavailable");

        if self
            .config
            .allowed_rcpts
            .as_ref()
            .is_some_and(|c| !c.contains(&rcpt))
        {
            warn!("rejected mail due to RCPT address");
            return Err(unavailable());
        }

        if self
            .config
            .allowed_froms
            .as_ref()
            .is_some_and(|c| !c.contains(&from))
        {
            warn!("rejected mail due to FROM address");
            return Err(unavailable());
        }

        if self.config.check_db {
            match db::check_address(&self.config.pg_pool, &from, &rcpt).await {
                Ok(true) => {}
                Ok(false) => {
                    warn!("rejected mail due to DB check");
                    return Err(unavailable());
                }
                Err(e) => {
                    error!("could not handle request: {}", e);
                    return Err(Rejection::transient("could not handle request"));
                }
            }
        }
        Ok(())
    }

    #[instrument(skip_all)]
    async fn data_end(&mut self, env: &Envelope, message: Vec<u8>) -> Result<String, Rejection> {
        debug!("handle DATA");
        let from = env.sender().path().to_string();
        let reply = format!("Received {} bytes.", message.len());
        let parsed = MessageParser::default()
            .parse(&message)
            .ok_or_else(|| Rejection::invalid_content("cannot parse message"))?;
        validate(&parsed).map_err(Rejection::invalid_content)?;

        for rcpt in env.rcpts().iter() {
            let rcpt = match rcpt.path() {
                ForwardPath::Postmaster => format!("postmaster@{}", self.config.domain),
                ForwardPath::Mailbox(m) => m.to_string(),
            };
            self.store(&from, &rcpt, &parsed).await.map_err(|e| {
                error!("could not handle request: {:?}", e);
                Rejection::transient("could not handle request")
            })?;
        }
        Ok(reply)
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

impl SmtpSession {
    async fn store(&self, from: &str, rcpt: &str, message: &Message<'_>) -> Result<()> {
        s3::upload_message(
            &self.config.s3_config,
            &self.config.pg_pool,
            &self.config.bucket,
            from,
            rcpt,
            message.clone(),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
