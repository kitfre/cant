use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

struct TestDir(PathBuf);
impl TestDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("cant-{name}-{}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for TestDir {
    fn drop(&mut self) {
        let socket = self.0.join("cant.sock");
        if let Ok(mut stream) = UnixStream::connect(socket) {
            let _ = stream.write_all(b"{\"type\":\"shutdown\"}\n");
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn cli(socket: &Path, cmd: &str) {
    let output = Command::new(env!("CARGO_BIN_EXE_cant"))
        .env("TOKIO_WORKER_THREADS", "2")
        .arg("--socket")
        .arg(socket)
        .arg(cmd)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{cmd}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
fn request(socket: &Path, kind: &str) -> (serde_json::Value, BufReader<UnixStream>) {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    writeln!(stream, "{{\"type\":\"{kind}\"}}").unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    (serde_json::from_str(&line).unwrap(), reader)
}
#[test]
fn shutdown_idle_creates_nothing() {
    let dir = TestDir::new("idle");
    let socket = dir.0.join("missing/cant.sock");
    cli(&socket, "shutdown");
    assert!(!socket.parent().unwrap().exists());
}
#[test]
fn shutdown_retries_acknowledged_but_missed_wakeup() {
    let dir = TestDir::new("missed");
    let socket = dir.0.join("cant.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&line).unwrap()["type"],
                "shutdown"
            );
            stream.write_all(b"{\"type\":\"ok\"}\n").unwrap();
        }
        // Leave a stale socket behind, as if the daemon exited abruptly.
    });
    cli(&socket, "shutdown");
    assert!(server.is_finished());
    server.join().unwrap();
    cli(&socket, "restart");
    assert_eq!(request(&socket, "ping").0["type"], "pong");
    cli(&socket, "shutdown");
}

#[test]
fn single_shutdown_frame_with_subscribers() {
    let dir = TestDir::new("broadcast");
    let socket = dir.0.join("cant.sock");
    cli(&socket, "ping");
    let subscribers = subscribers(&socket);
    assert_eq!(request(&socket, "shutdown").0["type"], "ok");
    for mut reader in subscribers {
        assert_eq!(reader.read_line(&mut String::new()).unwrap(), 0);
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while socket.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "accept loop still running"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn subscribers(socket: &Path) -> Vec<BufReader<UnixStream>> {
    (0..3)
        .map(|_| {
            let (reply, reader) = request(socket, "subscribe");
            assert_eq!(reply["type"], "ok");
            reader
        })
        .collect()
}

#[test]
fn restart_idle_then_live_with_subscribers() {
    let dir = TestDir::new("restart");
    let socket = dir.0.join("cant.sock");
    cli(&socket, "restart");
    let pid = || request(&socket, "ping").0["pid"].as_u64().unwrap();
    let old = pid();
    let subscribers = subscribers(&socket);
    cli(&socket, "restart");
    assert_ne!(old, pid());
    for mut reader in subscribers {
        assert_eq!(reader.read_line(&mut String::new()).unwrap(), 0);
    }
    cli(&socket, "shutdown");
    assert!(!socket.exists());
    cli(&socket, "shutdown");
}
