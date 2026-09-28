//! Runs `tests/swaks.sh`, which drives `examples/dump.rs` over a real TCP socket with the
//! `swaks` SMTP test client (<https://www.jetmore.org/john/code/swaks/>). Skipped if `swaks`
//! isn't on `PATH` -- `cargo test` doesn't otherwise depend on it being installed.
use std::process::Command;

#[test]
fn swaks_smoke() {
    if Command::new("swaks").arg("--version").output().is_err() {
        eprintln!("skipping: `swaks` not found on PATH");
        return;
    }
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/swaks.sh");
    let status = Command::new("bash")
        .arg(script)
        .status()
        .expect("failed to run tests/swaks.sh");
    assert!(status.success(), "tests/swaks.sh failed");
}
