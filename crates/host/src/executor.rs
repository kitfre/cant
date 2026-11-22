//! Persistent runner process executor over NDJSON stdio.

use crate::capability::{CallContext, CapabilityCatalog, CapabilityError};
use crate::runner_process::{RunnerCommand, RunnerProcess};
use cant_protocol::runner::{
    ChildMessage, ConsoleLevel, HostCallRequest, ParentMessage, RunnerExecution, SerializedError,
    DEFAULT_MAX_PROTOCOL_BYTES, RUNNER_PROTOCOL_VERSION,
};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::mpsc;
use tokio::time::timeout;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_RUN_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_CONSOLE_BYTES: usize = 64 * 1024;
const MAX_CONSOLE_EVENTS: usize = 512;

#[derive(Debug, Clone)]
pub struct ConsoleEntry {
    pub level: ConsoleLevel,
    pub values: Vec<Value>,
}

#[derive(Debug, Clone)]
pub struct ExecutionOutput {
    pub value: Value,
    pub console: Vec<ConsoleEntry>,
}

#[derive(Debug)]
pub enum ExecutorError {
    /// Guest-level parse/compile/runtime error; persistent REPL workspace survives.
    Guest {
        name: String,
        message: String,
        console: Vec<ConsoleEntry>,
    },
    /// Runner/protocol/timeout failure; persistent REPL workspace is discarded.
    Runtime {
        name: String,
        message: String,
        console: Vec<ConsoleEntry>,
    },
}

struct RunningChild {
    process: RunnerProcess,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

pub struct RunnerExecutor {
    command: RunnerCommand,
    child: Option<RunningChild>,
    next_run_id: u64,
}

impl RunnerExecutor {
    pub fn new(command: RunnerCommand) -> Self {
        Self {
            command,
            child: None,
            next_run_id: 0,
        }
    }

    pub fn is_alive(&self) -> bool {
        self.child.is_some()
    }

    async fn ensure_started(&mut self) -> Result<&mut RunningChild, ExecutorError> {
        if self.child.is_none() {
            let mut process = self.command.spawn().map_err(|e| ExecutorError::Runtime {
                name: "RunnerLaunchError".into(),
                message: format!("Could not start runner {}: {e}", self.command.executable.display()),
                console: vec![],
            })?;

            let stdin = process
                .child
                .stdin
                .take()
                .ok_or_else(|| ExecutorError::Runtime {
                    name: "RunnerLaunchError".into(),
                    message: "Runner child stdin unavailable.".into(),
                    console: vec![],
                })?;
            let stdout = process
                .child
                .stdout
                .take()
                .ok_or_else(|| ExecutorError::Runtime {
                    name: "RunnerLaunchError".into(),
                    message: "Runner child stdout unavailable.".into(),
                    console: vec![],
                })?;
            // Drain stderr asynchronously so the child never blocks on stderr pipe buffer.
            if let Some(stderr) = process.child.stderr.take() {
                tokio::spawn(async move {
                    let mut reader = BufReader::new(stderr);
                    let mut buf = String::new();
                    while let Ok(n) = reader.read_line(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        if buf.len() > 64 * 1024 {
                            break;
                        }
                    }
                });
            }

            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            let read_res = timeout(STARTUP_TIMEOUT, reader.read_line(&mut line)).await;
            let bytes = match read_res {
                Ok(Ok(n)) if n > 0 => n,
                Ok(Ok(_)) => {
                    process.terminate().await;
                    return Err(ExecutorError::Runtime {
                        name: "RunnerStartupError".into(),
                        message: "Runner exited before sending ready handshake.".into(),
                        console: vec![],
                    });
                }
                Ok(Err(e)) => {
                    process.terminate().await;
                    return Err(ExecutorError::Runtime {
                        name: "RunnerStartupError".into(),
                        message: format!("Failed reading ready handshake: {e}"),
                        console: vec![],
                    });
                }
                Err(_) => {
                    process.terminate().await;
                    return Err(ExecutorError::Runtime {
                        name: "RunnerStartupError".into(),
                        message: "Timed out waiting for runner ready handshake.".into(),
                        console: vec![],
                    });
                }
            };
            if bytes > DEFAULT_MAX_PROTOCOL_BYTES {
                process.terminate().await;
                return Err(ExecutorError::Runtime {
                    name: "ProtocolError".into(),
                    message: "Runner ready frame exceeded size limit.".into(),
                    console: vec![],
                });
            }
            let ready: ChildMessage = match serde_json::from_str(line.trim_end()) {
                Ok(ready) => ready,
                Err(e) => {
                    process.terminate().await;
                    return Err(ExecutorError::Runtime {
                        name: "ProtocolError".into(),
                        message: format!("Malformed runner ready handshake: {e}"),
                        console: vec![],
                    });
                }
            };
            match ready {
                ChildMessage::Ready {
                    protocol_version, ..
                } if protocol_version == RUNNER_PROTOCOL_VERSION => {}
                ChildMessage::Fatal { error } => {
                    process.terminate().await;
                    return Err(ExecutorError::Runtime {
                        name: error.name,
                        message: error.message,
                        console: vec![],
                    });
                }
                _ => {
                    process.terminate().await;
                    return Err(ExecutorError::Runtime {
                        name: "ProtocolError".into(),
                        message: "Unexpected message before ready handshake.".into(),
                        console: vec![],
                    });
                }
            }

            self.child = Some(RunningChild {
                process,
                stdin,
                stdout: reader,
            });
        }
        Ok(self.child.as_mut().unwrap())
    }

    pub async fn execute(
        &mut self,
        execution: RunnerExecution,
        ctx: CallContext,
    ) -> Result<ExecutionOutput, ExecutorError> {
        self.next_run_id += 1;
        let run_id = format!("r{}", self.next_run_id);

        let frame = cant_protocol::runner::encode_frame(
            &ParentMessage::Execute {
                run_id: run_id.clone(),
                program: execution.program,
                inputs: execution.inputs,
                bindings: execution.bindings,
            },
            DEFAULT_MAX_PROTOCOL_BYTES,
        )
        .map_err(|msg| ExecutorError::Runtime {
            name: "ProtocolError".into(),
            message: msg,
            console: vec![],
        })?;

        let result = self.run_frame(&run_id, &frame, ctx).await;
        if matches!(result, Err(ExecutorError::Runtime { .. })) {
            self.dispose().await;
        }
        result
    }

    async fn run_frame(
        &mut self,
        run_id: &str,
        frame: &str,
        ctx: CallContext,
    ) -> Result<ExecutionOutput, ExecutorError> {
        let child = self.ensure_started().await?;
        child
            .stdin
            .write_all(frame.as_bytes())
            .await
            .map_err(|e| ExecutorError::Runtime {
                name: "ProtocolError".into(),
                message: format!("Failed to write execute frame: {e}"),
                console: vec![],
            })?;
        child.stdin.flush().await.map_err(|e| ExecutorError::Runtime {
            name: "ProtocolError".into(),
            message: format!("Failed to flush execute frame: {e}"),
            console: vec![],
        })?;

        let mut console = Vec::new();
        let mut console_bytes = 0usize;
        let (tx, mut rx) = mpsc::channel::<ParentMessage>(64);
        let mut line = String::new();
        let deadline = tokio::time::Instant::now() + DEFAULT_RUN_TIMEOUT;

        loop {
            tokio::select! {
                Some(reply_msg) = rx.recv() => {
                    let encoded = cant_protocol::runner::encode_frame(&reply_msg, DEFAULT_MAX_PROTOCOL_BYTES)
                        .unwrap_or_else(|e| {
                            let fallback = match reply_msg {
                                ParentMessage::ToolResult { run_id, call_id, .. } => ParentMessage::ToolResult {
                                    run_id,
                                    call_id,
                                    ok: false,
                                    value: None,
                                    error: Some(SerializedError {
                                        name: "ProtocolError".into(),
                                        message: e,
                                        stack: None,
                                    }),
                                },
                                other => other,
                            };
                            cant_protocol::runner::encode_frame(&fallback, DEFAULT_MAX_PROTOCOL_BYTES).unwrap()
                        });
                    child.stdin.write_all(encoded.as_bytes()).await.map_err(|e| ExecutorError::Runtime {
                        name: "ProtocolError".into(),
                        message: format!("Failed writing tool_result: {e}"),
                        console: console.clone(),
                    })?;
                    child.stdin.flush().await.map_err(|e| ExecutorError::Runtime {
                        name: "ProtocolError".into(),
                        message: format!("Failed flushing tool_result: {e}"),
                        console: console.clone(),
                    })?;
                }
                read_res = tokio::time::timeout_at(deadline, child.stdout.read_line(&mut line)) => {
                    let n = match read_res {
                        Ok(Ok(n)) => n,
                        Ok(Err(e)) => {
                            return Err(ExecutorError::Runtime {
                                name: "ProtocolError".into(),
                                message: format!("Error reading runner stdout: {e}"),
                                console,
                            });
                        }
                        Err(_) => {
                            return Err(ExecutorError::Runtime {
                                name: "TimeoutError".into(),
                                message: "Runner execution timed out.".into(),
                                console,
                            });
                        }
                    };
                    if n == 0 {
                        return Err(ExecutorError::Runtime {
                            name: "RunnerExitError".into(),
                            message: "Runner exited unexpectedly.".into(),
                            console,
                        });
                    }
                    if line.len() > DEFAULT_MAX_PROTOCOL_BYTES {
                        return Err(ExecutorError::Runtime {
                            name: "ProtocolError".into(),
                            message: "Runner message exceeded protocol byte limit.".into(),
                            console,
                        });
                    }
                    let msg: ChildMessage = serde_json::from_str(line.trim_end()).map_err(|e| {
                        ExecutorError::Runtime {
                            name: "ProtocolError".into(),
                            message: format!("Malformed runner message: {e}"),
                            console: console.clone(),
                        }
                    })?;
                    line.clear();

                    match msg {
                        ChildMessage::Console { run_id: rid, level, values } if rid == run_id => {
                            if console.len() < MAX_CONSOLE_EVENTS {
                                let est = serde_json::to_vec(&values).map(|b| b.len()).unwrap_or(0);
                                if console_bytes + est <= MAX_CONSOLE_BYTES {
                                    console_bytes += est;
                                    console.push(ConsoleEntry { level, values });
                                }
                            }
                        }
                        ChildMessage::ToolCall { run_id: rid, call_id, request } if rid == run_id => {
                            let tx_clone = tx.clone();
                            let ctx_clone = ctx.clone();
                            let rid_clone = rid.clone();
                            tokio::spawn(async move {
                                let reply = dispatch_host_call(&ctx_clone, request).await;
                                let msg = match reply {
                                    Ok(value) => ParentMessage::ToolResult {
                                        run_id: rid_clone,
                                        call_id,
                                        ok: true,
                                        value: Some(value),
                                        error: None,
                                    },
                                    Err(err) => ParentMessage::ToolResult {
                                        run_id: rid_clone,
                                        call_id,
                                        ok: false,
                                        value: None,
                                        error: Some(SerializedError {
                                            name: err.name().to_string(),
                                            message: err.message(),
                                            stack: None,
                                        }),
                                    },
                                };
                                let _ = tx_clone.send(msg).await;
                            });
                        }
                        ChildMessage::Complete { run_id: rid, ok, value, error } if rid == run_id => {
                            if ok {
                                return Ok(ExecutionOutput { value, console });
                            } else {
                                let err = error.unwrap_or(SerializedError {
                                    name: "GuestError".into(),
                                    message: "Execution failed.".into(),
                                    stack: None,
                                });
                                return Err(ExecutorError::Guest {
                                    name: err.name,
                                    message: err.message,
                                    console,
                                });
                            }
                        }
                        ChildMessage::Fatal { error } => {
                            return Err(ExecutorError::Runtime {
                                name: error.name,
                                message: error.message,
                                console,
                            });
                        }
                        _ => {
                            return Err(ExecutorError::Runtime {
                                name: "ProtocolError".into(),
                                message: "Unexpected or mismatched runner frame.".into(),
                                console,
                            });
                        }
                    }
                }
            }
        }
    }

    pub async fn dispose(&mut self) {
        if let Some(mut running) = self.child.take() {
            if let Ok(shutdown) =
                cant_protocol::runner::encode_frame(&ParentMessage::Shutdown, DEFAULT_MAX_PROTOCOL_BYTES)
            {
                let _ = running.stdin.write_all(shutdown.as_bytes()).await;
                let _ = running.stdin.flush().await;
            }
            drop(running.stdin);
            let _ = timeout(Duration::from_millis(250), running.process.child.wait()).await;
            running.process.terminate().await;
        }
    }
}

async fn dispatch_host_call(
    ctx: &CallContext,
    request: HostCallRequest,
) -> Result<Value, CapabilityError> {
    let (capability, args, raw) = match request {
        HostCallRequest::Path { path, args } => {
            if crate::capability::is_path_disabled(&path, &ctx.disabled_namespaces) {
                return Err(CapabilityError::Disabled(path.join(".")));
            }
            let cap = ctx
                .catalog
                .get_by_api_path(&path)
                .or_else(|| ctx.extra_capabilities.iter().find(|c| c.meta().api_path == path).cloned())
                .ok_or_else(|| CapabilityError::NotFound(path.join(".")))?;
            (cap, args, false)
        }
        HostCallRequest::Tool { name, args } => {
            let cap = resolve_by_name(&ctx.catalog, &ctx.extra_capabilities, &name)?;
            if crate::capability::is_path_disabled(&cap.meta().api_path, &ctx.disabled_namespaces) {
                return Err(CapabilityError::Disabled(cap.meta().public_name()));
            }
            (cap, args, false)
        }
        HostCallRequest::Raw { name, args } => {
            let cap = resolve_by_name(&ctx.catalog, &ctx.extra_capabilities, &name)?;
            if crate::capability::is_path_disabled(&cap.meta().api_path, &ctx.disabled_namespaces) {
                return Err(CapabilityError::Disabled(cap.meta().public_name()));
            }
            (cap, args, true)
        }
    };
    let result = capability.call(ctx, args).await?;
    if raw {
        Ok(serde_json::json!({
            "text": serde_json::to_string(&result).unwrap_or_default(),
            "details": result,
            "isError": false,
        }))
    } else {
        Ok(result)
    }
}

fn resolve_by_name(
    catalog: &CapabilityCatalog,
    extras: &[Arc<dyn crate::capability::Capability>],
    name: &str,
) -> Result<Arc<dyn crate::capability::Capability>, CapabilityError> {
    if let Some(cap) = catalog.get_by_tool_name(name) {
        return Ok(cap);
    }
    if let Some(cap) = extras.iter().find(|c| c.meta().tool_name == name) {
        return Ok(Arc::clone(cap));
    }
    let trimmed = name.strip_prefix("tools.").unwrap_or(name);
    let path: Vec<String> = trimmed
        .split(['.', '/'])
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect();
    if let Some(cap) = catalog.get_by_api_path(&path) {
        return Ok(cap);
    }
    if let Some(cap) = extras.iter().find(|c| c.meta().api_path == path) {
        return Ok(Arc::clone(cap));
    }
    Err(CapabilityError::NotFound(name.to_string()))
}
