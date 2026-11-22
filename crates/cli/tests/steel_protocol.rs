use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::{fd::OwnedFd, unix::net::UnixStream};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct Runner {
    child: Child,
    wire: BufReader<UnixStream>,
}

impl Runner {
    fn start() -> Self {
        let (host, guest) = UnixStream::pair().unwrap();
        host.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_cant-steel-runner"))
            .stdin(Stdio::from(OwnedFd::from(guest.try_clone().unwrap())))
            .stdout(Stdio::from(OwnedFd::from(guest)))
            .spawn()
            .unwrap();
        let mut runner = Self {
            child,
            wire: BufReader::new(host),
        };
        assert_eq!(
            runner.receive(),
            json!({
                "type": "ready", "protocolVersion": 1,
                "runner": {"id": "steel-repl", "programFormat": "scheme"}
            })
        );
        runner
    }

    fn send(&mut self, frame: Value) {
        writeln!(self.wire.get_mut(), "{frame}").unwrap();
    }

    fn eval(&mut self, id: &str, source: &str) {
        self.send(json!({
            "type": "execute", "runId": id,
            "program": {"format": "scheme", "source": source, "repl": {"op": "eval"}},
            "inputs": {}, "bindings": {"apiPaths": [["probe", "call"]]}
        }));
    }

    fn receive(&mut self) -> Value {
        let mut line = String::new();
        assert_ne!(self.wire.read_line(&mut line).unwrap(), 0);
        serde_json::from_str(&line).unwrap()
    }

    fn finish(mut self, success: bool) {
        let mut line = String::new();
        assert_eq!(self.wire.read_line(&mut line).unwrap(), 0, "{line}");
        assert_eq!(self.child.wait().unwrap().success(), success);
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn no_flags_runs_a_persistent_workspace() {
    let mut runner = Runner::start();
    runner.eval("1", "(define repl_counter 41) (error \"partial\")");
    let reply = runner.receive();
    assert_eq!(reply["type"], "complete");
    assert_eq!(reply["ok"], false);
    runner.eval("2", "(+ repl_counter 1)");
    assert_eq!(
        runner.receive(),
        json!({
            "type": "complete", "runId": "2", "ok": true,
            "value": {"kind": "scheme", "preview": "42", "type": "number", "truncated": false}
        })
    );
    runner.send(json!({"type": "shutdown"}));
    runner.finish(true);
}

#[test]
fn protocol_faults_are_uncatchable() {
    let malformed = "Unexpected or malformed host response";
    let first_result =
        r#"{"type":"tool_result","runId":"r","callId":"call_1","ok":true,"value":0}"#;
    for (replies, expected) in [
        (&["not-json"][..], malformed),
        (&[][..], malformed),
        (
            &[r#"{"type":"cancel","runId":"r"}"#][..],
            "Invocation aborted",
        ),
        (
            &[r#"{"type":"tool_result","runId":"stale","callId":"call_1","ok":true,"value":0}"#][..],
            malformed,
        ),
        (
            &[r#"{"type":"tool_result","runId":"r","callId":"unknown","ok":true,"value":0}"#][..],
            malformed,
        ),
        (
            &[r#"{"type":"tool_result","runId":"r","callId":"call_1","ok":true}"#][..],
            malformed,
        ),
        (
            &[r#"{"type":"tool_result","runId":"r","callId":"call_1","ok":false}"#][..],
            "Invalid host error response",
        ),
        (&[first_result, first_result][..], malformed),
    ] {
        let mut runner = Runner::start();
        runner.eval(
            "r",
            r#"
            (with-handler (lambda (e) (let loop () (loop)))
              (parallel-calls (tools.probe.call (obj)) (tools.probe.call (obj))))
        "#,
        );
        for call_id in ["call_1", "call_2"] {
            let call = runner.receive();
            assert_eq!(call["type"], "tool_call");
            assert_eq!(call["callId"], call_id);
        }
        if replies.is_empty() {
            runner.wire.get_ref().shutdown(Shutdown::Write).unwrap();
        }
        for reply in replies {
            writeln!(runner.wire.get_mut(), "{reply}").unwrap();
        }
        let fatal = runner.receive();
        assert_eq!(fatal["type"], "fatal", "{replies:?}");
        assert_eq!(fatal["error"]["message"], expected, "{replies:?}");
        runner.finish(false);
    }
}
