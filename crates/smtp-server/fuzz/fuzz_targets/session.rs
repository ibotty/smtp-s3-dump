#![no_main]

use std::sync::Arc;

use libfuzzer_sys::fuzz_target;
use smtp_server::{Config, Event, Hostname, Plain, Poll, Rejection, Session};

const MAX_ITERATIONS: usize = 10_000;

fn config() -> Arc<Config> {
    Arc::new(Config::new(Hostname::new("mx.example.org").unwrap()))
}

/// Drive a session on one chunk of input, deciding every request token
/// deterministically so the state machine always makes progress. Returns
/// `Some` if the caller should keep feeding more bytes, `None` if the
/// session ended (closed, STARTTLS, or QUIT).
fn drive(mut session: Session<Plain>, data: &[u8]) -> Option<Session<Plain>> {
    session.feed(data);
    for _ in 0..MAX_ITERATIONS {
        session = match session.poll() {
            Poll::NeedInput(s) => return Some(s),
            Poll::Closed(..) => return None,
            Poll::Event(Event::Ehlo(req)) => req.accept(),
            Poll::Event(Event::Helo(req)) => req.accept(),
            Poll::Event(Event::Mail(req)) => req.decide(Err(Rejection::transient("no"))),
            Poll::Event(Event::Rcpt(req)) => req.decide(Err(Rejection::transient("no"))),
            Poll::Event(Event::DataStart(req)) => req.decide(Err(Rejection::transient("no"))),
            Poll::Event(Event::DataChunk(req)) => req.accept(),
            Poll::Event(Event::DataEnd(req)) => req.decide(Err(Rejection::transient("no"))),
            Poll::Event(Event::DataAbort(token)) => token.resume(),
            Poll::Event(Event::Rset(token)) => token.resume(),
            Poll::Event(Event::Rejected(token)) => token.resume(),
            Poll::Event(Event::Quit(token)) => {
                let _ = token.close();
                return None;
            }
            Poll::Event(Event::StartTls(mut token)) => {
                // No real TLS layer in this byte-only harness; drain the
                // reply and stop, matching how QUIT/Closed end the run.
                let _ = token.output();
                return None;
            }
        };
    }
    panic!("session did not terminate within {MAX_ITERATIONS} iterations");
}

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    let split = usize::from(data[0]) % data.len();
    let (first, second) = data.split_at(split);

    let session = Session::<Plain>::new(config());
    if let Some(session) = drive(session, first) {
        drive(session, second);
    }
});
