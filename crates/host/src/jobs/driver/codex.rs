//! One private app-server process per delegated worker.
mod policy;
#[cfg(test)]
mod tests;
mod turn;
use super::{
    build_worker_prompt, kill_pg, ProgressCallback, WorkerDriver, WorkerOutcome, WorkerSpec,
};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::{ffi::OsString, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    time::timeout,
};
use tokio_util::sync::CancellationToken;

pub struct CodexWorkerDriver {
    executable: OsString,
    default_model: Option<String>,
}
impl CodexWorkerDriver {
    pub fn new(default_model: Option<String>) -> Self {
        Self {
            executable: std::env::var_os("CANT_CODEX_BIN").unwrap_or_else(|| "codex".into()),
            default_model,
        }
    }
}
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ActiveTurn {
    thread_id: String,
    turn_id: String,
}

struct Connection {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    active: Option<ActiveTurn>,
}
impl Drop for Connection {
    fn drop(&mut self) {
        // Flow siblings may be aborted instead of cancelled cooperatively.
        if let Some(pid) = self.child.id() {
            kill_pg(pid);
        }
    }
}
impl Connection {
    async fn send(&mut self, msg: Value) -> Result<(), String> {
        let mut bytes = serde_json::to_vec(&msg).map_err(|e| e.to_string())?;
        bytes.push(b'\n');
        self.stdin
            .write_all(&bytes)
            .await
            .map_err(|e| format!("writing Codex request: {e}"))
    }
    async fn message(&mut self) -> Result<Value, String> {
        loop {
            let mut bytes = Vec::new();
            (&mut self.stdout)
                .take(8 * 1024 * 1024)
                .read_until(b'\n', &mut bytes)
                .await
                .map_err(|e| format!("reading Codex message: {e}"))?;
            if bytes.last() != Some(&b'\n') {
                return Err("Codex closed stdout or exceeded frame limit".into());
            }
            let msg: Value = serde_json::from_slice(&bytes)
                .map_err(|e| format!("invalid Codex protocol message: {e}"))?;
            if let (Some(id), Some(method)) = (msg.get("id"), msg.get("method")) {
                self.send(json!({"id":id,"error":{"code":-32601,"message":format!("Cant worker does not support {method}")}})).await?;
            } else {
                return Ok(msg);
            }
        }
    }
    async fn request(
        &mut self,
        id: u32,
        method: &str,
        params: Value,
        mut notify: impl FnMut(Value) -> Result<(), String>,
    ) -> Result<Value, String> {
        self.send(json!({"id":id,"method":method,"params":params}))
            .await?;
        loop {
            let msg = self.message().await?;
            if msg.get("method").is_some() {
                notify(msg)?;
                continue;
            }
            if msg["id"].as_u64() != Some(id.into()) {
                return Err(format!("unexpected response waiting for Codex {method}"));
            }
            if let Some(e) = msg.get("error") {
                return Err(format!("Codex {method}: {e}"));
            }
            return msg
                .get("result")
                .cloned()
                .ok_or_else(|| format!("Codex {method}: missing result"));
        }
    }
    async fn execute(
        &mut self,
        spec: &WorkerSpec,
        model: Option<&str>,
        progress: &ProgressCallback,
    ) -> Result<WorkerOutcome, String> {
        let client = json!({"name": "cant", "title": "Cant", "version": env!("CARGO_PKG_VERSION")});
        self.request(1, "initialize", json!({"clientInfo": client}), |_| Ok(()))
            .await?;
        self.send(json!({"method":"initialized"})).await?;
        // Codex merges MCP tables across configuration layers. An empty override
        // doesn't disable inherited registrations; even Cant may be registered
        // under another name. Inspect the effective config for this worker cwd.
        let config = self
            .request(
                5,
                "config/read",
                json!({"cwd": spec.cwd, "includeLayers": false}),
                |_| Ok(()),
            )
            .await?;
        let servers = config
            .pointer("/config/mcp_servers")
            .and_then(Value::as_object)
            .ok_or("Codex config/read: cannot verify effective MCP configuration")?;
        if let Some((name, _)) = servers
            .iter()
            .find(|(_, server)| server["enabled"] != false)
        {
            return Err(format!("Codex worker found enabled MCP server {name:?}; use a worker config without external MCP servers"));
        }
        let res = self
            .request(
                2,
                "thread/start",
                policy::thread_params(spec, model),
                |_| Ok(()),
            )
            .await?;
        let thread = res["thread"]["id"]
            .as_str()
            .ok_or("Codex thread/start: missing thread id")?
            .to_owned();
        let mut params =
            json!({"threadId":thread,"input":[{"type":"text","text":build_worker_prompt(spec)}]});
        if let Some(schema) = &spec.output_schema {
            params["outputSchema"] = schema.clone();
        }
        let mut early = Vec::new();
        let res = self
            .request(3, "turn/start", params, |msg| {
                if early.len() >= 32 {
                    return Err("too many early Codex notifications".into());
                }
                early.push(msg);
                Ok(())
            })
            .await?;
        let id = res["turn"]["id"]
            .as_str()
            .ok_or("Codex turn/start: missing turn id")?
            .to_owned();
        self.active = Some(ActiveTurn {
            thread_id: thread.clone(),
            turn_id: id.clone(),
        });
        let mut turn = turn::Turn::new(thread, id, spec.output_schema.is_some());
        let mut early = early.into_iter();
        loop {
            let msg = match early.next() {
                Some(msg) => msg,
                None => self.message().await?,
            };
            if let Some(method) = msg["method"].as_str() {
                if let Some(outcome) = turn.event(method, &msg["params"], progress)? {
                    return Ok(outcome);
                }
            }
        }
    }
    async fn interrupt(&mut self) {
        let Some(active) = self.active.take() else {
            return;
        };
        let interrupt = async {
            self.send(json!({"id": 4, "method": "turn/interrupt", "params": active}))
                .await?;
            loop {
                let msg = self.message().await?;
                if msg["id"] == 4 || msg["method"] == "turn/completed" {
                    return Ok::<_, String>(());
                }
            }
        };
        let _ = timeout(Duration::from_millis(500), interrupt).await;
    }
}
#[async_trait]
impl WorkerDriver for CodexWorkerDriver {
    fn id(&self) -> &'static str {
        "codex"
    }
    async fn run(
        &self,
        spec: WorkerSpec,
        progress: ProgressCallback,
        cancel: CancellationToken,
    ) -> Result<WorkerOutcome, String> {
        let mut cmd = Command::new(&self.executable);
        cmd.args(["app-server", "--stdio"])
            .current_dir(&spec.cwd)
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        policy::configure(&mut cmd, &spec)?;
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("starting Codex ({:?}): {e}", self.executable))?;
        let mut stderr = child.stderr.take().expect("piped stderr");
        let diagnostics = tokio::spawn(async move {
            let mut buf = [0; 4096];
            let mut log = Vec::new();
            while let Ok(n) = stderr.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                log.extend_from_slice(&buf[..n]);
                if log.len() > 16384 {
                    log.drain(..log.len() - 16384);
                }
            }
            String::from_utf8_lossy(&log).trim().to_owned()
        });
        let mut conn = Connection {
            stdin: child.stdin.take().expect("piped stdin"),
            stdout: BufReader::new(child.stdout.take().expect("piped stdout")),
            child,
            active: None,
        };
        let deadline = async {
            match spec.timeout_secs.filter(|n| *n > 0) {
                Some(n) => tokio::time::sleep(Duration::from_secs(n)).await,
                None => std::future::pending().await,
            }
        };
        let model = spec.effective_model(self.default_model.as_deref());
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                conn.interrupt().await;
                Err("Codex worker cancelled".into())
            }
            _ = deadline => {
                conn.interrupt().await;
                Err("Codex worker timed out".into())
            }
            result = conn.execute(&spec, model, &progress) => result,
        };
        if let Some(pid) = conn.child.id() {
            kill_pg(pid);
        }
        let _ = conn.child.kill().await;
        let log = timeout(Duration::from_secs(1), diagnostics)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        result.map_err(|e| {
            if log.is_empty() || !e.starts_with("Codex closed stdout") {
                e
            } else {
                format!("{e}\nCodex stderr: {log}")
            }
        })
    }
}
