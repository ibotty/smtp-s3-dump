use std::sync::Arc;

use anyhow::{anyhow, Result};
use arc_swap::ArcSwap;
use async_trait::async_trait;
use mail_parser::MessageParser;
use rust_smtp_server::backend::{Backend, MailOptions, Session};
use tokio::io::{self, AsyncRead, AsyncReadExt};
use tracing::{error, instrument, trace};

use crate::s3;

#[derive(Clone, Debug)]
pub struct SmtpBackend {
    pub config: Arc<ArcSwap<Config>>,
}

impl SmtpBackend {
    pub fn new(s3_config: aws_sdk_s3::Config, bucket: &str, allowed_rcpts: Vec<String>, allowed_from: Vec<String>) -> SmtpBackend {
        let bucket = bucket.to_string();

        let config = Arc::new(ArcSwap::from_pointee(Config{s3_config, bucket, allowed_rcpts, allowed_from}));
        SmtpBackend{config}
    }

}

#[derive(Clone, Debug)]
pub struct Config {
    pub s3_config: aws_sdk_s3::Config,
    pub bucket: String,
    pub allowed_rcpts: Vec<String>,
    pub allowed_from: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct SmtpSession {
    pub config: Arc<Config>,
    pub message_parser: MessageParser,
    pub rcpt: Option<String>,
    pub from: Option<String>,
}

impl Backend for SmtpBackend {
    type S = SmtpSession;

    fn new_session(&self) -> Result<SmtpSession> {
        let message_parser = MessageParser::default();
        let config = self.config.load_full();
        Ok(SmtpSession {
            message_parser,
            rcpt: None,
            from: None,
            config,
        })
    }
}

#[async_trait]
impl Session for SmtpSession {
    #[instrument(skip(self, _opts))]
    async fn mail(&mut self, from: &str, _opts: &MailOptions) -> Result<()> {
        trace!("got mail");
        self.from = Some(from.to_string());
        Ok(())
    }

    #[instrument(skip(self))]
    fn reset(&mut self) {
        trace!("got reset");
        self.from = None;
        self.rcpt = None;
    }

    #[instrument(skip(self))]
    fn logout(&mut self) -> Result<()> {
        Ok(())
    }

    #[instrument(skip(self), fields(from = self.from))]
    async fn rcpt(&mut self, rcpt: &str) -> Result<()> {
        trace!("got rcpt: {}", rcpt);
        self.rcpt = Some(rcpt.to_string());
        Ok(())
    }

    #[instrument(skip(self, r), fields(from = self.from, rcpt = self.rcpt))]
    async fn data<R: AsyncRead + Send + Unpin>(&mut self, r: R) -> Result<()> {
        trace!("got data");

        let from = self.from.take().unwrap();
        let rcpt = self.rcpt.take().unwrap();

        let mut data = Vec::new();
        let mut reader = io::BufReader::new(r);

        while reader.read_to_end(&mut data).await? > 0 {
            data.push(b'\n');
        }

        let message = self
            .message_parser
            .parse(&data)
            .ok_or_else(|| anyhow!("Cannot parse message"))?;

        s3::upload_message(&self.config.s3_config, &self.config.bucket, &from, &rcpt, message)
            .await
            .map_err(|e| {
                error!("upload to s3 bucket failed: {:?}", e);
                e
            })
    }
}
