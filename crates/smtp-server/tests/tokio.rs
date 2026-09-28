//! Driver integration tests, running the real Tokio event loop over `tokio::io::duplex`.

use std::sync::Arc;
use std::time::Duration;

use smtp_server::{Config, Envelope, Handler, Hostname, Recipient, Rejection, Sender};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Echo;

impl Handler for Echo {
    async fn data_end(&mut self, _env: &Envelope, msg: Vec<u8>) -> Result<String, Rejection> {
        Ok(format!("{} bytes", msg.len()))
    }
}

fn cfg() -> Arc<Config> {
    let mut c = Config::new(Hostname::new("mx.example.org").unwrap());
    c.command_timeout = Duration::from_secs(5);
    c.data_timeout = Duration::from_secs(5);
    c.data_deadline = Duration::from_secs(5);
    Arc::new(c)
}

async fn read(client: &mut tokio::io::DuplexStream, buf: &mut [u8]) -> usize {
    client.read(buf).await.expect("read")
}

#[tokio::test]
async fn buffered_transaction_with_two_recipients() {
    let (mut client, server) = tokio::io::duplex(4096);
    let cfg = cfg();
    let mut handler = Echo;
    let task =
        tokio::spawn(async move { smtp_server::serve(server, &mut handler, cfg, None).await });

    let mut buf = vec![0u8; 4096];
    let n = read(&mut client, &mut buf).await;
    assert!(buf[..n].starts_with(b"220"));

    client.write_all(b"EHLO client\r\n").await.unwrap();
    let n = read(&mut client, &mut buf).await;
    assert!(buf[..n].starts_with(b"250"));

    client.write_all(b"MAIL FROM:<a@b>\r\n").await.unwrap();
    let n = read(&mut client, &mut buf).await;
    assert!(buf[..n].starts_with(b"250"));

    client.write_all(b"RCPT TO:<c@d>\r\n").await.unwrap();
    let n = read(&mut client, &mut buf).await;
    assert!(buf[..n].starts_with(b"250"));

    client.write_all(b"RCPT TO:<e@f>\r\n").await.unwrap();
    let n = read(&mut client, &mut buf).await;
    assert!(buf[..n].starts_with(b"250"));

    client.write_all(b"DATA\r\n").await.unwrap();
    let n = read(&mut client, &mut buf).await;
    assert!(buf[..n].starts_with(b"354"));

    client.write_all(b"hello world\r\n.\r\n").await.unwrap();
    let n = read(&mut client, &mut buf).await;
    assert!(buf[..n].starts_with(b"250"));
    assert!(buf[..n].windows(7).any(|w| w == b"13 byte"));

    client.write_all(b"QUIT\r\n").await.unwrap();
    let n = read(&mut client, &mut buf).await;
    assert!(buf[..n].starts_with(b"221"));

    task.await.expect("task panicked").expect("serve failed");
}

#[tokio::test]
async fn command_timeout_closes_with_421() {
    let (mut client, server) = tokio::io::duplex(4096);
    let mut c = Config::new(Hostname::new("mx.example.org").unwrap());
    c.command_timeout = Duration::from_millis(100);
    let mut handler = Echo;
    let task =
        tokio::spawn(
            async move { smtp_server::serve(server, &mut handler, Arc::new(c), None).await },
        );

    let mut buf = vec![0u8; 4096];
    read(&mut client, &mut buf).await; // greeting
    let n = read(&mut client, &mut buf).await;
    assert!(buf[..n].starts_with(b"421"));
    assert_eq!(
        read(&mut client, &mut buf).await,
        0,
        "connection must close"
    );

    task.await.expect("task panicked").expect("serve failed");
}

#[tokio::test]
async fn data_deadline_closes_with_421() {
    let (mut client, server) = tokio::io::duplex(4096);
    let mut c = Config::new(Hostname::new("mx.example.org").unwrap());
    c.data_deadline = Duration::from_millis(100);
    let mut handler = Echo;
    let task =
        tokio::spawn(
            async move { smtp_server::serve(server, &mut handler, Arc::new(c), None).await },
        );

    let mut buf = vec![0u8; 4096];
    read(&mut client, &mut buf).await; // greeting
    client
        .write_all(b"EHLO client\r\nMAIL FROM:<a@b>\r\nRCPT TO:<c@d>\r\nDATA\r\n")
        .await
        .unwrap();
    // Drain replies until the "354" that starts the DATA phase (the deadline clock starts then).
    let mut got = Vec::new();
    while !got.windows(3).any(|w| w == b"354") {
        let n = read(&mut client, &mut buf).await;
        got.extend_from_slice(&buf[..n]);
    }

    let n = read(&mut client, &mut buf).await;
    assert!(buf[..n].starts_with(b"421"));
    assert_eq!(
        read(&mut client, &mut buf).await,
        0,
        "connection must close"
    );

    task.await.expect("task panicked").expect("serve failed");
}

struct Panicky;

impl Handler for Panicky {
    async fn rcpt(&mut self, _sender: &Sender, _rcpt: &Recipient) -> Result<(), Rejection> {
        panic!("boom");
    }

    async fn data_end(&mut self, _env: &Envelope, _msg: Vec<u8>) -> Result<String, Rejection> {
        unreachable!("rcpt always panics first")
    }
}

#[tokio::test]
async fn panicking_handler_closes_with_421() {
    let (mut client, server) = tokio::io::duplex(4096);
    let cfg = cfg();
    let mut handler = Panicky;
    let task =
        tokio::spawn(async move { smtp_server::serve(server, &mut handler, cfg, None).await });

    let mut buf = vec![0u8; 4096];
    read(&mut client, &mut buf).await; // greeting
    client
        .write_all(b"EHLO client\r\nMAIL FROM:<a@b>\r\n")
        .await
        .unwrap();
    let mut got = Vec::new();
    while got.iter().filter(|&&b| b == b'\n').count() < 2 {
        let n = read(&mut client, &mut buf).await;
        got.extend_from_slice(&buf[..n]);
    }

    client.write_all(b"RCPT TO:<c@d>\r\n").await.unwrap();
    let n = read(&mut client, &mut buf).await;
    assert!(buf[..n].starts_with(b"421"));
    assert_eq!(
        read(&mut client, &mut buf).await,
        0,
        "connection must close"
    );

    task.await.expect("task panicked").expect("serve failed");
}

async fn expect(client: &mut tokio::io::DuplexStream, prefix: &[u8]) {
    let mut buf = vec![0u8; 4096];
    let n = read(client, &mut buf).await;
    assert!(
        buf[..n].starts_with(prefix),
        "expected {prefix:?}, got {:?}",
        String::from_utf8_lossy(&buf[..n])
    );
}

async fn rest(client: &mut tokio::io::DuplexStream) -> String {
    let mut out = Vec::new();
    client.read_to_end(&mut out).await.expect("read to EOF");
    String::from_utf8(out).unwrap()
}

#[tokio::test]
async fn shutdown_closes_idle_connection_with_421() {
    let (mut client, server) = tokio::io::duplex(4096);
    let (trigger, rx) = smtp_server::shutdown_signal();
    let mut handler = Echo;
    let task = tokio::spawn(async move {
        smtp_server::serve_until(server, &mut handler, cfg(), None, rx).await
    });

    expect(&mut client, b"220").await;
    trigger.trigger();
    assert!(rest(&mut client).await.starts_with("421 4.3.2"));
    task.await.expect("task panicked").expect("serve failed");
}

#[tokio::test]
async fn shutdown_lets_message_in_flight_finish() {
    let (mut client, server) = tokio::io::duplex(4096);
    let (trigger, rx) = smtp_server::shutdown_signal();
    let mut handler = Echo;
    let task = tokio::spawn(async move {
        smtp_server::serve_until(server, &mut handler, cfg(), None, rx).await
    });

    expect(&mut client, b"220").await;
    for (cmd, reply) in [
        (&b"EHLO client\r\n"[..], &b"250"[..]),
        (b"MAIL FROM:<a@b>\r\n", b"250"),
        (b"RCPT TO:<c@d>\r\n", b"250"),
        (b"DATA\r\n", b"354"),
    ] {
        client.write_all(cmd).await.unwrap();
        expect(&mut client, reply).await;
    }
    client.write_all(b"partial\r\n").await.unwrap();
    trigger.trigger();
    tokio::time::sleep(Duration::from_millis(100)).await;
    client.write_all(b"hello\r\n.\r\n").await.unwrap();
    let tail = rest(&mut client).await;
    assert!(tail.starts_with("250"), "{tail:?}");
    assert!(tail.contains("\r\n421 4.3.2"), "{tail:?}");
    task.await.expect("task panicked").expect("serve failed");
}

#[tokio::test]
async fn connection_after_shutdown_gets_421_after_greeting() {
    let (mut client, server) = tokio::io::duplex(4096);
    let (trigger, rx) = smtp_server::shutdown_signal();
    trigger.trigger();
    let mut handler = Echo;
    let task = tokio::spawn(async move {
        smtp_server::serve_until(server, &mut handler, cfg(), None, rx).await
    });

    let all = rest(&mut client).await;
    assert!(
        all.starts_with("220") && all.contains("\r\n421 4.3.2"),
        "{all:?}"
    );
    task.await.expect("task panicked").expect("serve failed");
}

#[tokio::test]
async fn dropped_trigger_does_not_shut_down() {
    let (mut client, server) = tokio::io::duplex(4096);
    let (trigger, rx) = smtp_server::shutdown_signal();
    drop(trigger);
    let mut handler = Echo;
    let task = tokio::spawn(async move {
        smtp_server::serve_until(server, &mut handler, cfg(), None, rx).await
    });

    expect(&mut client, b"220").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    client.write_all(b"QUIT\r\n").await.unwrap();
    expect(&mut client, b"221").await;
    task.await.expect("task panicked").expect("serve failed");
}
