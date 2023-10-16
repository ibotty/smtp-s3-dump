use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use arc_swap::ArcSwap;
use async_trait::async_trait;
use bytes::BytesMut;
use futures::{Stream, TryStreamExt};
use mail_parser::MessageParser;
use rustyknife::rfc5321::{ForwardPath, Param, ReversePath};
use rustyknife::types::{Domain, DomainPart, Mailbox};
use smtpbis::{EhloKeywords, Reply};
use tokio_rustls::rustls::ServerConfig;
use tracing::{error, instrument, trace, warn};

use crate::s3;

pub struct SmtpBackend {
    pub config: Arc<ArcSwap<Config>>,
}

impl SmtpBackend {
    #[instrument(skip(s3_config, tls_config))]
    pub fn new(
        s3_config: aws_sdk_s3::Config,
        tls_config: Arc<ServerConfig>,
        domain: &str,
        bucket: &str,
        allowed_rcpts: Option<HashSet<String>>,
        allowed_froms: Option<HashSet<String>>,
    ) -> Result<SmtpBackend> {
        let bucket = bucket.to_string();
        let domain: DomainPart = DomainPart::from_smtp(domain.as_bytes())
            .map_err(|e| anyhow!("could not parse SMTP_DOMAIN: {}", e))?;

        let config = Arc::new(ArcSwap::from_pointee(Config {
            s3_config,
            tls_config,
            domain,
            bucket,
            allowed_rcpts,
            allowed_froms,
        }));
        Ok(SmtpBackend { config })
    }

    #[instrument(skip_all)]
    pub fn new_session(&self) -> Result<SmtpSession> {
        let message_parser = MessageParser::default();
        let config = self.config.load_full();
        Ok(SmtpSession {
            message_parser,
            config,
            rcpt: None,
            from: None,
            data: vec![],
        })
    }
}

pub struct Config {
    pub s3_config: aws_sdk_s3::Config,
    pub tls_config: Arc<ServerConfig>,
    pub domain: DomainPart,
    pub bucket: String,
    pub allowed_rcpts: Option<HashSet<String>>,
    pub allowed_froms: Option<HashSet<String>>,
}

pub struct SmtpSession {
    pub config: Arc<Config>,
    pub message_parser: MessageParser,
    pub rcpt: Option<String>,
    pub from: Option<String>,
    pub data: Vec<u8>,
}

impl SmtpSession {
    #[instrument(skip(self))]
    fn reset(&mut self) {
        trace!("resetting session");
        self.from = None;
        self.rcpt = None;
        self.data = vec![];
    }

    async fn handle_data(&mut self) -> Result<()> {
        let from = self.from.take().unwrap();
        let rcpt = self.rcpt.take().unwrap();
        let message = self
            .message_parser
            .parse(&self.data)
            .ok_or_else(|| anyhow!("Cannot parse message"))?;

        s3::upload_message(
            &self.config.s3_config,
            &self.config.bucket,
            &from,
            &rcpt,
            message,
        )
        .await
        .map_err(|e| {
            error!("upload to s3 bucket failed: {:?}", e);
            e
        })?;

        self.reset();
        Ok(())
    }
}

#[async_trait]
impl smtpbis::Handler for SmtpSession {
    type TlsConfig = Arc<ServerConfig>;

    #[instrument(skip_all)]
    async fn tls_request(&mut self) -> Option<Self::TlsConfig> {
        Some(self.config.tls_config.clone())
    }

    #[instrument(skip_all)]
    async fn ehlo(
        &mut self,
        domain: DomainPart,
        mut initial_keywords: EhloKeywords,
    ) -> Result<(String, EhloKeywords), Reply> {
        trace!("handle EHLO");
        let max_message_size = 100_000_000;
        initial_keywords.insert("DSN".into(), None);
        initial_keywords.insert("8BITMIME".into(), None);
        initial_keywords.insert("SIZE".into(), Some(max_message_size.to_string()));

        let greet = format!("hello {}", domain);
        self.reset();

        Ok((greet, initial_keywords))
    }

    #[instrument(skip(self))]
    async fn helo(&mut self, _domain: Domain) -> Option<Reply> {
        self.reset();
        None
    }

    #[instrument(skip_all)]
    async fn mail(&mut self, from: ReversePath, _params: Vec<Param>) -> Option<Reply> {
        trace!("handle MAIL");

        if let Some((mailbox, domain)) =
            std::convert::Into::<Option<Mailbox>>::into(from).map(Mailbox::into_parts)
        {
            let from = format!("{}@{}", mailbox, domain);
            self.from = Some(from);
        }
        None
    }

    #[instrument(skip_all, fields(from=self.from))]
    async fn rcpt(&mut self, rcpt: ForwardPath, _params: Vec<Param>) -> Option<Reply> {
        trace!("handle RCPT");
        let (mailbox, domain) = rcpt.into_mailbox(&self.config.domain).into_parts();
        let rcpt = format!("{}@{}", mailbox, domain);

        if self
            .config
            .allowed_rcpts
            .as_ref()
            .is_some_and(|c| !c.contains(&rcpt))
        {
            warn!("rejected mail due to RCPT address");
            return Some(Reply::new(550, None, "mailbox unavailable"));
        };

        if self
            .config
            .allowed_froms
            .as_ref()
            .is_some_and(|c| !c.contains(self.from.as_ref().unwrap()))
        {
            warn!("rejected mail due to FROM address");
            return Some(Reply::new(550, None, "mailbox unavailable"));
        };

        self.rcpt = Some(rcpt);
        None
    }

    #[instrument(skip_all)]
    async fn data_start(&mut self) -> Option<Reply> {
        None
    }

    #[instrument(skip_all, fields(from=self.from, rcpt=self.rcpt))]
    async fn data<S>(&mut self, stream: &mut S) -> Result<Option<Reply>, smtpbis::ServerError>
    where
        S: Stream<Item = Result<BytesMut, smtpbis::LineError>> + Unpin + Send,
    {
        trace!("handle DATA");

        let mut nb_lines: usize = 0;

        self.data = Vec::new();
        while let Some(line) = stream.try_next().await? {
            self.data.extend(line);
            nb_lines += 1
        }

        let reply_txt = format!("Received {} bytes in {} lines.", self.data.len(), nb_lines);

        match self.handle_data().await {
            Ok(_) => Ok(Some(Reply::new(250, None, reply_txt))),
            Err(e) => {
                error!("could not handle request: {}", e);
                Ok(Some(Reply::new(451, None, "could not handle request")))
            }
        }
    }

    #[instrument(skip_all)]
    async fn bdat<S>(
        &mut self,
        stream: &mut S,
        _size: u64,
        last: bool,
    ) -> Result<Option<Reply>, smtpbis::ServerError>
    where
        S: Stream<Item = Result<BytesMut, smtpbis::LineError>> + Unpin + Send,
    {
        while let Some(chunk) = stream.try_next().await? {
            self.data.extend(chunk)
        }
        if last {
            match self.handle_data().await {
                Ok(_) => Ok(None),
                Err(e) => {
                    error!("could not handle request: {}", e);
                    Ok(Some(Reply::new(451, None, "could not handle request")))
                }
            }
        } else {
            Ok(None)
        }
    }

    #[instrument(skip_all)]
    async fn rset(&mut self) {
        self.reset();
    }
}
