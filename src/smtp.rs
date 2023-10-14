use anyhow::{anyhow, Result};
use async_trait::async_trait;
use mail_parser::MessageParser;
use rust_smtp_server::backend::{Backend, MailOptions, Session};
use tokio::io::{self, AsyncRead, AsyncReadExt};
use tracing::{error, instrument, trace};

use crate::s3;

#[derive(Clone, Debug)]
pub struct SmtpBackend {
    pub s3_config: aws_sdk_s3::Config,
    pub bucket: String,
}

#[derive(Clone, Debug)]
pub struct SmtpSession {
    pub message_parser: MessageParser,
    pub s3_config: aws_sdk_s3::Config,
    pub rcpt: Option<String>,
    pub from: Option<String>,
    pub bucket: String,
}

impl SmtpSession {
    fn new(s3_config: aws_sdk_s3::Config, bucket: &str) -> SmtpSession {
        let message_parser = MessageParser::default();
        SmtpSession {
            message_parser,
            s3_config,
            rcpt: None,
            from: None,
            bucket: bucket.to_string(),
        }
    }
}

impl Backend for SmtpBackend {
    type S = SmtpSession;

    fn new_session(&self) -> Result<SmtpSession> {
        Ok(SmtpSession::new(self.s3_config.clone(), &self.bucket))
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

        s3::upload_message(&self.s3_config, &self.bucket, &from, &rcpt, message)
            .await
            .map_err(|e| {
                error!("upload to s3 bucket failed: {:?}", e);
                e
            })
    }
}
