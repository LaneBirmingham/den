//! The terminal client exits on SIGTERM even while its stdin stays open
//! (a pipe that never sends EOF, as under `tail -f /dev/null | den …`).

#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn sigterm_exits_with_stdin_open() {
    let home = std::env::temp_dir().join(format!("den-exit-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join("config")).unwrap();
    // Loopback only, no relays, nothing asked of the router.
    std::fs::write(home.join("config/config.toml"), "bind = \"localhost\"\n[net]\nrelays = []\nport_mapping = false\n").unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_den"))
        .args(["--nick", "tester", "chat", "--host", "--name", "exit-test"])
        .env("DEN_HOME", &home)
        .env("DEN_LOG", "warn")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // Hold stdin open for the whole test.
    let _stdin = child.stdin.take().unwrap();

    // Wait until it hosts, so the signal lands in the running client.
    let (tx, rx) = std::sync::mpsc::channel();
    let stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line.contains("connected to") {
                let _ = tx.send(());
            }
        }
    });
    rx.recv_timeout(Duration::from_secs(20)).expect("client never connected to its own room");

    let status = Command::new("kill").args(["-TERM", &child.id().to_string()]).status().unwrap();
    assert!(status.success());

    let start = Instant::now();
    let exited = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if start.elapsed() > Duration::from_secs(10) {
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    if exited.is_none() {
        let _ = child.kill();
    }
    let _ = std::fs::remove_dir_all(&home);
    let status = exited.expect("still running 10 s after SIGTERM");
    assert!(status.success(), "exit status {status}");
}
