use anyhow::{anyhow, Result};
use async_trait::async_trait;
use mail_parser::MessageParser;
use rust_smtp_server::backend::{Backend, MailOptions, Session};
use tokio::io::{self, AsyncRead, AsyncReadExt};
use tracing::{debug, trace, instrument};

pub struct SmtpBackend;

#[derive(Debug, Clone)]
pub struct SmtpSession {
    pub message_parser: MessageParser,
}

impl SmtpSession {
    fn new() -> SmtpSession {
        let message_parser = MessageParser::default();
        SmtpSession { message_parser }
    }
}

impl Backend for SmtpBackend {
    type S = SmtpSession;

    fn new_session(&self) -> Result<SmtpSession> {
        Ok(SmtpSession::new())
    }
}

#[async_trait]
impl Session for SmtpSession {
    // #[instrument(skip(self))]
    // fn authenticators(&mut self) -> Vec<Box<dyn sasl::Server>> {
    //     vec!()
    // }

    #[instrument(skip(_opts))]
    async fn mail(&mut self, from: &str, _opts: &MailOptions) -> Result<()> {
        debug!("got mail from: {}", from);
        Ok(())
    }

    #[instrument(skip(self))]
    fn reset(&mut self) {}

    #[instrument(skip(self))]
    fn logout(&mut self) -> Result<()> {
        Ok(())
    }

    #[instrument(skip(self))]
    async fn rcpt(&mut self, to: &str) -> Result<()> {
        debug!("got rcpt: {}", to);
        Ok(())
    }

    #[instrument(skip(self, r))]
    async fn data<R: AsyncRead + Send + Unpin>(&mut self, r: R) -> Result<()> {
        let mut data = Vec::new();
        let mut reader = io::BufReader::new(r);

        while reader.read_to_end(&mut data).await? > 0 {
            data.push(b'\n');
        }

        let message = self
            .message_parser
            .parse(&data)
            .ok_or_else(|| anyhow!("Cannot parse message"))?;

        trace!("raw message {:?}", String::from_utf8_lossy(&data));
        trace!("parsed message {:?}", message.message_id());
        trace!("parsed message {:?}", message);
        Ok(())
    }
}
