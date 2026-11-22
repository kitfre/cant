//! Pluggable LLM worker driver trait + built-in `AgyWorkerDriver` (`agy --print`).
//!
//! Additional drivers (e.g. `claude -p` or reverse-RPC SDK workers)
//! simply implement `WorkerDriver` and register into `WorkerDriverRegistry`.

pub mod codex;

use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct WorkerSpec {
    pub name: String,
    pub task: String,
    pub input: Option<Value>,
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub timeout_secs: Option<u64>,
    pub output_schema: Option<Value>,
    pub capabilities: Option<Vec<String>>,
    pub session_id: Option<String>,
    pub channels: Vec<String>,
}

impl WorkerSpec {
    fn effective_model<'a>(&'a self, default: Option<&'a str>) -> Option<&'a str> {
        self.model.as_deref().into_iter().chain(default)
            .map(str::trim).find(|name| !name.is_empty())
    }
}

#[derive(Debug, Clone)]
pub struct WorkerOutcome {
    pub text: String,
    pub data: Option<Value>,
    pub usage: Option<Value>,
}

pub type ProgressCallback = Arc<dyn Fn(String) + Send + Sync>;

#[async_trait]
pub trait WorkerDriver: Send + Sync {
    fn id(&self) -> &'static str;
    async fn run(
        &self,
        spec: WorkerSpec,
        on_progress: ProgressCallback,
        cancel: CancellationToken,
    ) -> Result<WorkerOutcome, String>;
}

#[derive(Default)]
pub struct WorkerDriverRegistry {
    drivers: HashMap<String, Arc<dyn WorkerDriver>>,
}

impl WorkerDriverRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, driver: Arc<dyn WorkerDriver>) {
        self.drivers.insert(driver.id().to_string(), driver);
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn WorkerDriver>> {
        self.drivers.get(id).cloned()
    }

    pub fn ids(&self) -> Vec<String> {
        let mut out: Vec<_> = self.drivers.keys().cloned().collect();
        out.sort();
        out
    }
}

/// Runs delegated workers via Antigravity CLI (`agy --output-format stream-json --dangerously-skip-permissions`)
/// so delegated jobs and DAG flows run on your Google subscription.
#[derive(Debug)]
pub struct AgyWorkerDriver {
    pub executable: String,
    pub default_model: Option<String>,
}

impl Default for AgyWorkerDriver {
    fn default() -> Self {
        Self {
            executable: std::env::var("CANT_AGY_BIN").unwrap_or_else(|_| "agy".into()),
            default_model: None,
        }
    }
}

impl AgyWorkerDriver {
    pub fn with_default_model(mut self, model: Option<String>) -> Self {
        self.default_model = model;
        self
    }
}

fn build_worker_prompt(spec: &WorkerSpec) -> String {
    let mut sections = Vec::new();
    sections.push(spec.task.clone());
    if let Some(ref input) = spec.input {
        if let Ok(pretty) = serde_json::to_string_pretty(input) {
            sections.push(format!("## Structured Input\n```json\n{pretty}\n```"));
        }
    }
    if let Some(ref caps) = spec.capabilities {
        if caps.is_empty() {
            sections.push(
                "## Tool Constraints\nThis task has no external tool capabilities granted. Answer directly from the provided task and structured input without reading files or running commands.".into(),
            );
        } else {
            sections.push(format!(
                "## Scoped Capabilities\nStay within these granted capabilities: {}",
                caps.join(", ")
            ));
        }
    }
    if let Some(ref schema) = spec.output_schema {
        if let Ok(s) = serde_json::to_string(schema) {
            sections.push(format!(
                "## Output Contract\nReturn ONLY valid JSON conforming to this schema:\n```json\n{s}\n```"
            ));
        }
    }
    sections.join("\n\n")
}

fn kill_pg(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = pid;
}

#[async_trait]
impl WorkerDriver for AgyWorkerDriver {
    fn id(&self) -> &'static str {
        "agy"
    }

    async fn run(
        &self,
        spec: WorkerSpec,
        on_progress: ProgressCallback,
        cancel: CancellationToken,
    ) -> Result<WorkerOutcome, String> {
        let prompt = build_worker_prompt(&spec);
        let mut cmd = Command::new(&self.executable);
        cmd.arg("--output-format")
            .arg("stream-json")
            .arg("--dangerously-skip-permissions")
            .arg("--disable-slash-commands")
            .current_dir(&spec.cwd)
            .env("CANT_WORKER", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        if let Some(ref sid) = spec.session_id {
            cmd.env("CANT_WORKER_SESSION_ID", sid);
        }
        if let Some(model) = spec.effective_model(self.default_model.as_deref()) {
            cmd.arg("--model").arg(model);
        }
        if let Some(secs) = spec.timeout_secs {
            if secs > 0 {
                cmd.arg("--print-timeout").arg(format!("{secs}s"));
            }
        }
        if let Some(ref schema) = spec.output_schema {
            if let Ok(encoded) = serde_json::to_string(schema) {
                cmd.arg("--json-schema").arg(encoded);
            }
        }
        cmd.arg(format!("--print={prompt}"));

        #[cfg(unix)]
        unsafe {
            cmd.pre_exec(|| {
                libc::setpgid(0, 0);
                Ok(())
            });
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Failed spawning agy worker ({}): {e}", self.executable))?;
        let pid = child.id();

        let stdout = child.stdout.take().ok_or("agy worker stdout unavailable")?;
        let stderr = child.stderr.take().ok_or("agy worker stderr unavailable")?;

        let stderr_task = tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut buf = String::new();
            let mut line = String::new();
            while let Ok(n) = reader.read_line(&mut line).await {
                if n == 0 {
                    break;
                }
                if buf.len() < 16 * 1024 {
                    buf.push_str(&line);
                }
                line.clear();
            }
            buf
        });

        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let mut streamed_text = String::new();
        let mut final_response: Option<String> = None;
        let mut final_status: Option<String> = None;
        let mut final_usage: Option<Value> = None;

        loop {
            line.clear();
            tokio::select! {
                _ = cancel.cancelled() => {
                    if let Some(p) = pid {
                        kill_pg(p);
                    }
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    return Err("Worker cancelled.".into());
                }
                read_res = reader.read_line(&mut line) => {
                    let n = read_res.map_err(|e| format!("Error reading agy stream: {e}"))?;
                    if n == 0 {
                        break;
                    }
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Ok(ev) = serde_json::from_str::<Value>(trimmed) {
                        match ev.get("event").and_then(Value::as_str) {
                            Some("step_update") => {
                                if let Some(su) = ev.get("step_update") {
                                    let step_type = su.get("step_type").and_then(Value::as_str).unwrap_or("");
                                    let state = su.get("state").and_then(Value::as_str).unwrap_or("");
                                    if let Some(delta) = su.get("text_delta").and_then(Value::as_str) {
                                        streamed_text.push_str(delta);
                                        on_progress(delta.to_string());
                                    } else if !step_type.is_empty() && step_type != "user_input" {
                                        on_progress(format!("[{step_type}: {state}]\n"));
                                    }
                                }
                            }
                            Some("result") => {
                                if let Some(res) = ev.get("result") {
                                    final_status = res.get("status").and_then(Value::as_str).map(ToString::to_string);
                                    final_response = res.get("response").and_then(Value::as_str).map(ToString::to_string);
                                    final_usage = res.get("usage").cloned();
                                }
                            }
                            _ => {}
                        }
                    } else {
                        on_progress(format!("{trimmed}\n"));
                    }
                }
            }
        }

        let status = child
            .wait()
            .await
            .map_err(|e| format!("Failed waiting on agy worker: {e}"))?;
        let stderr_text = stderr_task.await.unwrap_or_default();

        let text = final_response
            .unwrap_or(streamed_text)
            .trim()
            .to_string();

        let is_success = status.success()
            && final_status
                .as_deref()
                .map(|s| s.eq_ignore_ascii_case("SUCCESS"))
                .unwrap_or(true);
        if !is_success {
            let reason = if !stderr_text.trim().is_empty() {
                stderr_text.trim().to_string()
            } else if !text.is_empty() {
                text
            } else {
                format!(
                    "agy worker failed (exit={:?}, status={:?})",
                    status.code(),
                    final_status
                )
            };
            return Err(reason);
        }

        let data = if spec.output_schema.is_some() {
            extract_json(&text)
        } else {
            serde_json::from_str::<Value>(&text).ok()
        };

        Ok(WorkerOutcome {
            text,
            data,
            usage: final_usage,
        })
    }
}

fn extract_json(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        return Some(v);
    }
    // Strip optional markdown code fences ```json ... ```
    if let Some(rest) = trimmed.strip_prefix("```") {
        let after_lang = rest.split_once('\n').map(|(_, b)| b).unwrap_or(rest);
        if let Some(inner) = after_lang.strip_suffix("```") {
            if let Ok(v) = serde_json::from_str::<Value>(inner.trim()) {
                return Some(v);
            }
        }
    }
    None
}
