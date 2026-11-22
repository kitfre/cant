use super::*;
use std::{fs, os::unix::fs::PermissionsExt, sync::Arc};

// A real pipe and child process, but no Codex account, network, or model.
struct Fixture(std::path::PathBuf);
impl Fixture {
    fn new(script: &str) -> Self {
        let dir = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        fs::create_dir(&dir).unwrap();
        let path = dir.join("codex");
        fs::write(&path, format!("#!/bin/sh\nset -eu\n{script}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(dir)
    }
    async fn run(
        &self,
        cancel: CancellationToken,
        progress: ProgressCallback,
    ) -> Result<WorkerOutcome, String> {
        let spec = WorkerSpec {
            name: "test".into(),
            task: "ping".into(),
            cwd: self.0.clone(),
            timeout_secs: Some(1),
            input: None,
            model: Some(" \t".into()),
            output_schema: None,
            capabilities: None,
            session_id: None,
            channels: Vec::new(),
        };
        let driver = CodexWorkerDriver {
            executable: self.0.join("codex").into_os_string(),
            default_model: Some("driver-default".into()),
        };
        driver.run(spec, progress, cancel).await
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn timeout_includes_handshake() {
    let f = Fixture::new("exec sleep 60");
    let err = tokio::time::timeout(
        std::time::Duration::from_secs(4),
        f.run(CancellationToken::new(), Arc::new(|_| {})),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(err.contains("timed out"), "{err}");
}

#[tokio::test]
async fn cancellation_interrupts_active_turn() {
    let f = Fixture::new(
        r#"
read -r init
echo '{"id":1,"result":{}}'
read -r initialized
read -r config
echo '{"id":5,"result":{"config":{"mcp_servers":{"cant":{"enabled":false}}}}}'
read -r thread
echo '{"id":2,"result":{"thread":{"id":"th"}}}'
read -r turn
echo '{"id":3,"result":{"turn":{"id":"tu"}}}'
echo '{"method":"item/agentMessage/delta","params":{"threadId":"th","turnId":"tu","itemId":"i","delta":"ready"}}'
read -r interrupt
printf '%s' "$interrupt" > interrupted
"#,
    );
    let token = CancellationToken::new();
    let cancel = token.clone();
    let res = tokio::time::timeout(
        Duration::from_secs(4),
        f.run(token, Arc::new(move |_| cancel.cancel())),
    )
    .await
    .unwrap();
    assert!(res.unwrap_err().contains("cancelled"));
    let msg: serde_json::Value =
        serde_json::from_slice(&fs::read(f.0.join("interrupted")).unwrap()).unwrap();
    assert_eq!(msg["method"], "turn/interrupt");
    assert_eq!(
        msg["params"],
        serde_json::json!({"threadId":"th","turnId":"tu"})
    );
}

#[tokio::test]
async fn unexpected_server_request_does_not_block_the_turn() {
    let f = Fixture::new(
        r#"
read -r init
echo '{"id":1,"result":{}}'
read -r initialized
read -r config
echo '{"id":5,"result":{"config":{"mcp_servers":{"cant":{"enabled":false}}}}}'
read -r thread
echo '{"id":2,"result":{"thread":{"id":"th"}}}'
read -r turn
printf '%s' "$thread" > thread-request
echo '{"id":"authz","method":"unknown/approval","params":{}}'
read -r deny
printf '%s' "$deny" > denial
echo '{"method":"item/completed","params":{"threadId":"th","turnId":"tu","item":{"type":"agentMessage","phase":"final_answer","id":"i","text":"pong"}}}'
echo '{"id":3,"result":{"turn":{"id":"tu"}}}'
echo '{"method":"turn/completed","params":{"threadId":"th","turn":{"id":"tu","status":"completed","items":[]}}}'
"#,
    );
    assert_eq!(
        f.run(CancellationToken::new(), Arc::new(|_| {}))
            .await
            .unwrap()
            .text,
        "pong"
    );
    let thread: serde_json::Value =
        serde_json::from_slice(&fs::read(f.0.join("thread-request")).unwrap()).unwrap();
    assert_eq!(thread["params"]["model"], "driver-default");
    let denial: serde_json::Value =
        serde_json::from_slice(&fs::read(f.0.join("denial")).unwrap()).unwrap();
    assert_eq!(denial["id"], "authz");
    assert_eq!(denial["error"]["code"], -32601);
}

#[tokio::test]
async fn enabled_mcp_alias_is_rejected_before_thread_start() {
    let f = Fixture::new(
        r#"
read -r init
echo '{"id":1,"result":{}}'
read -r initialized
read -r config
echo '{"id":5,"result":{"config":{"mcp_servers":{"anything":{"command":"cant"}}}}}'
read -r unexpected && touch launched
"#,
    );
    let err = f
        .run(CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap_err();
    assert!(err.contains("enabled MCP server"), "{err}");
    assert!(!f.0.join("launched").exists());
}

#[tokio::test]
async fn cancellation_reaps_child_while_turn_id_is_unknown() {
    let f = Fixture::new(
        r#"
read -r init
echo '{"id":1,"result":{}}'
read -r initialized
read -r config
echo '{"id":5,"result":{"config":{"mcp_servers":{}}}}'
read -r thread
echo '{"id":2,"result":{"thread":{"id":"th"}}}'
read -r turn
printf '%s' "$$" > pid.tmp && mv pid.tmp waiting
exec sleep 30
"#,
    );
    let token = CancellationToken::new();
    let cancel = async {
        let waiting = f.0.join("waiting");
        while !waiting.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        token.cancel();
        fs::read_to_string(waiting).unwrap().parse::<i32>().unwrap()
    };
    let (pid, result) = timeout(Duration::from_secs(4), async {
        tokio::join!(cancel, f.run(token.clone(), Arc::new(|_| {})))
    })
    .await
    .unwrap();
    assert!(result.unwrap_err().contains("cancelled"));
    // Signal zero checks for existence without sending a signal. A zombie
    // would also still exist, so ESRCH verifies both termination and reaping.
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}
