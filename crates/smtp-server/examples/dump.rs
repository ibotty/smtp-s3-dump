//! What `smtp-s3-dump` would look like built on `smtp-server`: accept mail for one domain,
//! write one copy of each message per recipient. Run with `cargo run --example dump`.

use std::io::{self, BufReader};
use std::sync::Arc;

use mail_parser::{MessageParser, MimeHeaders};
use smtp_server::{
    Config, Domain, Envelope, ForwardPath, Handler, Hostname, Recipient, Rejection, ReversePath,
    Sender, TlsMode,
};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;

const MAX_CONNECTIONS: usize = 64;

struct Dump {
    domain: Domain,
}

impl Handler for Dump {
    async fn rcpt(&mut self, _sender: &Sender, rcpt: &Recipient) -> Result<(), Rejection> {
        match rcpt.path() {
            ForwardPath::Postmaster => Ok(()),
            ForwardPath::Mailbox(m) if m.domain() == &self.domain => Ok(()),
            ForwardPath::Mailbox(_) => Err(Rejection::mailbox_unavailable("no such user here")),
        }
    }

    // One message, N recipients: store one copy per recipient. SMTP (unlike LMTP) has a single
    // reply for all recipients: if any store fails, the client retries the whole transaction, so
    // stores must be idempotent (key = hash of message + rcpt).
    async fn data_end(&mut self, env: &Envelope, msg: Vec<u8>) -> Result<String, Rejection> {
        let from = match env.sender().path() {
            ReversePath::Null => "<>".to_owned(),
            ReversePath::Mailbox(m) => m.to_string(),
        };
        let id = content_hash(&msg);
        let message = MessageParser::default()
            .parse(&msg)
            .ok_or_else(|| Rejection::transient("could not parse message"))?;
        let headers: String = message
            .headers_raw()
            .map(|(k, v)| format!("{k}:{v}"))
            .collect();
        let text_body = message.body_text(0).map(|b| b.into_owned());
        let html_body = message.body_html(0).map(|b| b.into_owned());
        let attachments: Vec<(String, &[u8])> = message
            .attachments()
            .enumerate()
            .map(|(ix, part)| {
                let name = part.attachment_name().unwrap_or("attachment");
                (format!("{ix:02}-{name}"), part.contents())
            })
            .collect();

        for rcpt in env.rcpts().iter() {
            let dir = match rcpt.path() {
                ForwardPath::Postmaster => "postmaster".to_owned(),
                ForwardPath::Mailbox(m) => m.to_string(),
            };
            let base = format!("{dir}/{id}");
            tokio::fs::create_dir_all(&base)
                .await
                .map_err(|_| Rejection::transient("could not store message"))?;
            tokio::fs::write(format!("{base}/headers.txt"), &headers)
                .await
                .map_err(|_| Rejection::transient("could not store message"))?;
            if let Some(body) = &text_body {
                tokio::fs::write(format!("{base}/body.txt"), body)
                    .await
                    .map_err(|_| Rejection::transient("could not store message"))?;
            }
            if let Some(body) = &html_body {
                tokio::fs::write(format!("{base}/body.html"), body)
                    .await
                    .map_err(|_| Rejection::transient("could not store message"))?;
            }
            if !attachments.is_empty() {
                tokio::fs::create_dir_all(format!("{base}/attachments"))
                    .await
                    .map_err(|_| Rejection::transient("could not store message"))?;
                for (name, contents) in &attachments {
                    tokio::fs::write(format!("{base}/attachments/{name}"), contents)
                        .await
                        .map_err(|_| Rejection::transient("could not store message"))?;
                }
            }
        }
        Ok(format!(
            "{id}: {} bytes, {} attachment(s) from {from} for {} rcpt(s)",
            msg.len(),
            attachments.len(),
            env.rcpts().len()
        ))
    }
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let hostname = Hostname::new("mx.example.org").expect("valid hostname");
    let cfg = Arc::new(Config::new(hostname.clone()));
    let acceptor = TlsAcceptor::from(Arc::new(load_tls_config()?));
    let listener = TcpListener::bind("[::]:2525").await?;
    // Buffered mode holds up to max_message_size per connection: bound concurrency.
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let permit = slots
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore never closed");
        let (socket, peer) = listener.accept().await?;
        let cfg = cfg.clone();
        let acceptor = acceptor.clone();
        let mut handler = Dump {
            domain: Domain::Name(hostname.clone()),
        };
        tokio::spawn(async move {
            if let Err(e) =
                smtp_server::serve(socket, &mut handler, cfg, TlsMode::StartTls(acceptor), None)
                    .await
            {
                eprintln!("{peer}: {e}");
            }
            drop(permit);
        });
    }
}

/// Loads the self-signed cert/key checked in next to this example (`examples/tls.{crt,key}`) to
/// offer STARTTLS. Regenerate with:
/// `openssl req -x509 -newkey rsa:2048 -days 3650 -nodes -subj "/CN=mx.example.org" \
///   -keyout examples/tls.key -out examples/tls.crt`
fn load_tls_config() -> io::Result<ServerConfig> {
    let mut cert_file = BufReader::new(std::fs::File::open("examples/tls.crt")?);
    let certs = rustls_pemfile::certs(&mut cert_file).collect::<Result<Vec<_>, _>>()?;
    let mut key_file = BufReader::new(std::fs::File::open("examples/tls.key")?);
    let key = rustls_pemfile::private_key(&mut key_file)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "no private key in examples/tls.key",
        )
    })?;
    ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// A deterministic content hash (not cryptographic): `std`'s `DefaultHasher` is unsuitable here
/// because its seed is randomized per process, so the same message would hash differently across
/// runs. FNV-1a is small, dependency-free, and stable.
fn content_hash(msg: &[u8]) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &byte in msg {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}
