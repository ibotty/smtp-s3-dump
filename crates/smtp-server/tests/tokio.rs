//! Driver integration tests, running the real Tokio event loop over `tokio::io::duplex`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use smtp_server::{
    Config, Envelope, Handler, Hostname, Recipient, Rejection, Sender, Shutdown, TlsMode,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

struct Echo;

impl Handler for Echo {
    async fn data_end(&mut self, _env: &Envelope, msg: Vec<u8>) -> Result<String, Rejection> {
        Ok(format!("{} bytes", msg.len()))
    }
}

type Serve = tokio::task::JoinHandle<std::io::Result<()>>;

fn cfg_with(f: impl FnOnce(&mut Config)) -> Arc<Config> {
    let mut c = Config::new(Hostname::new("mx.example.org").unwrap());
    f(&mut c);
    Arc::new(c)
}

fn cfg() -> Arc<Config> {
    cfg_with(|c| {
        c.command_timeout = Duration::from_secs(5);
        c.data_timeout = Duration::from_secs(5);
        c.data_deadline = Duration::from_secs(5);
    })
}

fn spawn(
    mut handler: impl Handler + 'static,
    config: Arc<Config>,
    shutdown: Option<Shutdown>,
) -> (DuplexStream, Serve) {
    let (client, server) = tokio::io::duplex(4096);
    let task = tokio::spawn(async move {
        smtp_server::serve(server, &mut handler, config, TlsMode::None, shutdown).await
    });
    (client, task)
}

async fn join(task: Serve) {
    task.await.expect("task panicked").expect("serve failed");
}

async fn read(client: &mut DuplexStream, buf: &mut [u8]) -> usize {
    client.read(buf).await.expect("read")
}

async fn expect(client: &mut DuplexStream, prefix: &[u8]) {
    let mut buf = vec![0u8; 4096];
    let n = read(client, &mut buf).await;
    assert!(
        buf[..n].starts_with(prefix),
        "expected {prefix:?}, got {:?}",
        String::from_utf8_lossy(&buf[..n])
    );
}

async fn rest(client: &mut DuplexStream) -> String {
    let mut out = Vec::new();
    client.read_to_end(&mut out).await.expect("read to EOF");
    String::from_utf8(out).unwrap()
}

async fn send(client: &mut DuplexStream, cmd: &[u8], reply: &[u8]) {
    client.write_all(cmd).await.unwrap();
    expect(client, reply).await;
}

/// Greeting, EHLO, MAIL FROM and RCPT TO, each expected to succeed.
async fn envelope(client: &mut DuplexStream) {
    expect(client, b"220").await;
    for cmd in [
        &b"EHLO client\r\n"[..],
        b"MAIL FROM:<a@b>\r\n",
        b"RCPT TO:<c@d>\r\n",
    ] {
        send(client, cmd, b"250").await;
    }
}

#[tokio::test]
async fn buffered_transaction_with_two_recipients() {
    let (mut client, task) = spawn(Echo, cfg(), None);
    expect(&mut client, b"220").await;
    send(&mut client, b"EHLO client\r\n", b"250").await;
    send(&mut client, b"MAIL FROM:<a@b>\r\n", b"250").await;
    send(&mut client, b"RCPT TO:<c@d>\r\n", b"250").await;
    send(&mut client, b"RCPT TO:<e@f>\r\n", b"250").await;
    send(&mut client, b"DATA\r\n", b"354").await;
    client.write_all(b"hello world\r\n.\r\n").await.unwrap();
    let mut buf = vec![0u8; 4096];
    let n = read(&mut client, &mut buf).await;
    assert!(buf[..n].starts_with(b"250"));
    assert!(buf[..n].windows(7).any(|w| w == b"13 byte"));
    send(&mut client, b"QUIT\r\n", b"221").await;
    join(task).await;
}

#[tokio::test]
async fn command_timeout_closes_with_421() {
    let config = cfg_with(|c| c.command_timeout = Duration::from_millis(100));
    let (mut client, task) = spawn(Echo, config, None);
    expect(&mut client, b"220").await;
    assert!(rest(&mut client).await.starts_with("421"));
    join(task).await;
}

#[tokio::test]
async fn data_deadline_closes_with_421() {
    let config = cfg_with(|c| c.data_deadline = Duration::from_millis(100));
    let (mut client, task) = spawn(Echo, config, None);
    envelope(&mut client).await;
    send(&mut client, b"DATA\r\n", b"354").await;
    assert!(rest(&mut client).await.starts_with("421"));
    join(task).await;
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
    let (mut client, task) = spawn(Panicky, cfg(), None);
    expect(&mut client, b"220").await;
    send(&mut client, b"EHLO client\r\n", b"250").await;
    send(&mut client, b"MAIL FROM:<a@b>\r\n", b"250").await;
    client.write_all(b"RCPT TO:<c@d>\r\n").await.unwrap();
    assert!(rest(&mut client).await.starts_with("421"));
    join(task).await;
}

#[tokio::test]
async fn shutdown_closes_idle_connection_with_421() {
    let (trigger, rx) = smtp_server::shutdown_signal();
    let (mut client, task) = spawn(Echo, cfg(), Some(rx));
    expect(&mut client, b"220").await;
    trigger.trigger();
    assert!(rest(&mut client).await.starts_with("421 4.3.2"));
    join(task).await;
}

#[tokio::test]
async fn shutdown_lets_message_in_flight_finish() {
    let (trigger, rx) = smtp_server::shutdown_signal();
    let (mut client, task) = spawn(Echo, cfg(), Some(rx));
    envelope(&mut client).await;
    send(&mut client, b"DATA\r\n", b"354").await;
    client.write_all(b"partial\r\n").await.unwrap();
    trigger.trigger();
    tokio::time::sleep(Duration::from_millis(100)).await;
    client.write_all(b"hello\r\n.\r\n").await.unwrap();
    let tail = rest(&mut client).await;
    assert!(tail.starts_with("250"), "{tail:?}");
    assert!(tail.contains("\r\n421 4.3.2"), "{tail:?}");
    join(task).await;
}

#[tokio::test]
async fn connection_after_shutdown_gets_421_after_greeting() {
    let (trigger, rx) = smtp_server::shutdown_signal();
    trigger.trigger();
    let (mut client, task) = spawn(Echo, cfg(), Some(rx));
    let all = rest(&mut client).await;
    assert!(
        all.starts_with("220") && all.contains("\r\n421 4.3.2"),
        "{all:?}"
    );
    join(task).await;
}

#[tokio::test]
async fn dropped_trigger_does_not_shut_down() {
    let (trigger, rx) = smtp_server::shutdown_signal();
    drop(trigger);
    let (mut client, task) = spawn(Echo, cfg(), Some(rx));
    expect(&mut client, b"220").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    send(&mut client, b"QUIT\r\n", b"221").await;
    join(task).await;
}

#[tokio::test(start_paused = true)]
async fn data_deadline_applies_while_discarding_oversize_message() {
    let config = cfg_with(|c| {
        c.max_message_size = smtp_server::MessageSize::new(5);
        c.command_timeout = Duration::from_secs(3600);
        c.data_timeout = Duration::from_secs(30);
        c.data_deadline = Duration::from_secs(60);
    });
    let (mut client, task) = spawn(Echo, config, None);
    envelope(&mut client).await;
    send(&mut client, b"DATA\r\n", b"354").await;
    // Exceed the limit, then trickle a byte every 20s: each read is within `data_timeout`, but
    // the overall deadline (60s) must still end the session.
    client.write_all(b"way more than five bytes").await.unwrap();
    let mut closed = false;
    for _ in 0..10 {
        tokio::time::sleep(Duration::from_secs(20)).await;
        if client.write_all(b"x").await.is_err() {
            closed = true;
            break;
        }
        tokio::task::yield_now().await;
    }
    let all = rest(&mut client).await;
    assert!(closed, "deadline must close the session while discarding");
    assert!(all.contains("421"), "{all:?}");
    join(task).await;
}

struct Greeter(&'static str);

impl Handler for Greeter {
    async fn data_end(&mut self, _env: &Envelope, _msg: Vec<u8>) -> Result<String, Rejection> {
        Ok(String::new())
    }
    async fn greeting(&mut self) -> String {
        self.0.to_owned()
    }
}

async fn greeting_of(handler: impl Handler + 'static) -> Vec<u8> {
    let (mut client, task) = spawn(handler, cfg(), None);
    let mut buf = vec![0u8; 4096];
    let n = read(&mut client, &mut buf).await;
    client.write_all(b"QUIT\r\n").await.unwrap();
    rest(&mut client).await;
    join(task).await;
    buf[..n].to_vec()
}

#[tokio::test]
async fn handler_supplies_greeting_text() {
    assert_eq!(
        greeting_of(Greeter("welcome")).await,
        b"220 mx.example.org welcome\r\n"
    );
    assert_eq!(
        greeting_of(Greeter("hi\r\n250 injected")).await,
        b"220 mx.example.org hi  250 injected\r\n"
    );
    assert_eq!(greeting_of(Echo).await, b"220 mx.example.org ESMTP\r\n");
}

struct Boom {
    greeting: bool,
}

impl Handler for Boom {
    async fn data_end(&mut self, _: &Envelope, _: Vec<u8>) -> Result<String, Rejection> {
        Ok(String::new())
    }
    async fn greeting(&mut self) -> String {
        if self.greeting {
            panic!("boom");
        }
        String::new()
    }
    async fn rset(&mut self) {
        panic!("boom")
    }
}

#[tokio::test]
async fn panicking_greeting_closes_with_421() {
    let (mut client, task) = spawn(Boom { greeting: true }, cfg(), None);
    assert!(rest(&mut client).await.starts_with("421 "));
    join(task).await;
}

#[tokio::test]
async fn panicking_rset_closes_with_421() {
    let (mut client, task) = spawn(Boom { greeting: false }, cfg(), None);
    expect(&mut client, b"220").await;
    client.write_all(b"RSET\r\n").await.unwrap();
    assert!(rest(&mut client).await.starts_with("421 "));
    join(task).await;
}

#[derive(Clone, Default)]
struct Counts {
    abort: Arc<AtomicUsize>,
    rset: Arc<AtomicUsize>,
    end: Arc<AtomicUsize>,
}

struct Streaming(Counts);

impl Handler for Streaming {
    async fn data_chunk(&mut self, _: &[u8], _: &mut Vec<u8>) -> Result<(), Rejection> {
        Ok(())
    }
    async fn data_end(&mut self, _: &Envelope, _: Vec<u8>) -> Result<String, Rejection> {
        self.0.end.fetch_add(1, Ordering::SeqCst);
        Ok(String::new())
    }
    async fn data_abort(&mut self) {
        self.0.abort.fetch_add(1, Ordering::SeqCst);
    }
    async fn rset(&mut self) {
        self.0.rset.fetch_add(1, Ordering::SeqCst);
    }
}

async fn streaming_session(
    config: Arc<Config>,
    shutdown: Option<Shutdown>,
) -> (DuplexStream, Serve, Counts) {
    let counts = Counts::default();
    let (mut client, task) = spawn(Streaming(counts.clone()), config, shutdown);
    envelope(&mut client).await;
    (client, task, counts)
}

async fn finish(task: Serve, counts: &Counts) -> usize {
    join(task).await;
    counts.abort.load(Ordering::SeqCst)
}

#[tokio::test]
async fn abort_on_client_drop_mid_data() {
    let (mut client, task, counts) = streaming_session(cfg(), None).await;
    client.write_all(b"DATA\r\n").await.unwrap();
    expect(&mut client, b"354").await;
    client.write_all(b"partial\r\n").await.unwrap();
    drop(client);
    assert_eq!(finish(task, &counts).await, 1);
}

#[tokio::test]
async fn abort_on_client_drop_between_bdat_chunks() {
    let (mut client, task, counts) = streaming_session(cfg(), None).await;
    client.write_all(b"BDAT 5\r\nhello").await.unwrap();
    expect(&mut client, b"250").await;
    drop(client);
    assert_eq!(finish(task, &counts).await, 1);
}

#[tokio::test(start_paused = true)]
async fn abort_on_data_deadline_mid_data() {
    let config = cfg_with(|c| c.data_deadline = Duration::from_secs(60));
    let (mut client, task, counts) = streaming_session(config, None).await;
    client.write_all(b"DATA\r\n").await.unwrap();
    expect(&mut client, b"354").await;
    client.write_all(b"partial\r\n").await.unwrap();
    assert!(rest(&mut client).await.starts_with("421"));
    assert_eq!(finish(task, &counts).await, 1);
}

#[tokio::test]
async fn abort_on_shutdown_between_bdat_chunks() {
    let (trigger, rx) = smtp_server::shutdown_signal();
    let (mut client, task, counts) = streaming_session(cfg(), Some(rx)).await;
    client.write_all(b"BDAT 5\r\nhello").await.unwrap();
    expect(&mut client, b"250").await;
    trigger.trigger();
    assert!(rest(&mut client).await.starts_with("421 4.3.2"));
    assert_eq!(finish(task, &counts).await, 1);
}

#[tokio::test]
async fn no_abort_after_normal_data_end() {
    let (mut client, task, counts) = streaming_session(cfg(), None).await;
    client.write_all(b"DATA\r\n").await.unwrap();
    expect(&mut client, b"354").await;
    client.write_all(b"hello\r\n.\r\n").await.unwrap();
    expect(&mut client, b"250").await;
    drop(client);
    assert_eq!(finish(task, &counts).await, 0);
    assert_eq!(counts.end.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn no_abort_after_rset_mid_message() {
    let (mut client, task, counts) = streaming_session(cfg(), None).await;
    client.write_all(b"BDAT 5\r\nhello").await.unwrap();
    expect(&mut client, b"250").await;
    client.write_all(b"RSET\r\n").await.unwrap();
    expect(&mut client, b"250").await;
    drop(client);
    assert_eq!(finish(task, &counts).await, 0);
    assert_eq!(counts.rset.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn single_abort_after_oversize_then_disconnect() {
    let config = cfg_with(|c| c.max_message_size = smtp_server::MessageSize::new(5));
    let (mut client, task, counts) = streaming_session(config, None).await;
    client.write_all(b"DATA\r\n").await.unwrap();
    expect(&mut client, b"354").await;
    client
        .write_all(b"way more than five bytes\r\n")
        .await
        .unwrap();
    while counts.abort.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    drop(client);
    assert_eq!(finish(task, &counts).await, 1);
}
