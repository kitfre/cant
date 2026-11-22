//! Built-in process & script capabilities:
//! `system/exec`, `system/pipeline`, `scripts/run`.
//!
//! Runs commands directly with literal argv (no shell interpretation), bounded inline output
//! with optional temp-file spill, and process-group termination on timeout.

use crate::capability::{
    ArgumentMode, CallContext, Capability, CapabilityCatalog, CapabilityError, CapabilityMeta,
};
use async_trait::async_trait;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use uuid::Uuid;

const DEFAULT_TIMEOUT_SECS: f64 = 120.0;
const MAX_TIMEOUT_SECS: f64 = 3600.0;
const DEFAULT_MAX_OUTPUT_BYTES: usize = 50 * 1024;
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_SPILL_BYTES: usize = 64 * 1024 * 1024;
const MAX_PIPELINE_STEPS: usize = 16;

struct StreamCapture {
    text: String,
    total_bytes: usize,
    truncated: bool,
    full_output_path: Option<String>,
}

async fn capture_stream<R: AsyncRead + Unpin>(
    mut reader: R,
    max_inline: usize,
) -> Result<StreamCapture, std::io::Error> {
    let mut ring = Vec::<u8>::new();
    let mut total_bytes = 0usize;
    let mut spill_file: Option<(PathBuf, fs::File)> = None;
    let mut spill_bytes = 0usize;
    let mut buf = [0u8; 8192];

    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        let chunk = &buf[..n];
        total_bytes += n;

        if total_bytes <= max_inline {
            ring.extend_from_slice(chunk);
        } else {
            if spill_file.is_none() {
                let spill_path = std::env::temp_dir().join(format!(
                    "cant-output-{}-{}.log",
                    std::process::id(),
                    Uuid::new_v4().simple()
                ));
                if let Ok(mut f) = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&spill_path)
                {
                    let _ = f.write_all(&ring);
                    spill_bytes = ring.len();
                    spill_file = Some((spill_path, f));
                }
            }
            if let Some((_, ref mut f)) = spill_file {
                let rem = MAX_SPILL_BYTES.saturating_sub(spill_bytes);
                if rem > 0 {
                    let take = chunk.len().min(rem);
                    let _ = f.write_all(&chunk[..take]);
                    spill_bytes += take;
                }
            }
            // Keep bounded tail inline
            if chunk.len() >= max_inline {
                ring.clear();
                ring.extend_from_slice(&chunk[chunk.len() - max_inline..]);
            } else {
                let overflow = (ring.len() + chunk.len()).saturating_sub(max_inline);
                if overflow > 0 {
                    ring.drain(..overflow);
                }
                ring.extend_from_slice(chunk);
            }
        }
    }

    let full_output_path = spill_file.map(|(p, _)| p.to_string_lossy().into_owned());
    Ok(StreamCapture {
        text: String::from_utf8_lossy(&ring).into_owned(),
        total_bytes,
        truncated: total_bytes > max_inline,
        full_output_path,
    })
}

fn kill_process_group(pid: u32) {
    #[cfg(unix)]
    unsafe {
        let pgid = -(pid as libc::pid_t);
        libc::kill(pgid, libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = pid;
}

fn configure_process_group(cmd: &mut Command) {
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            libc::setpgid(0, 0);
            Ok(())
        });
    }
}

fn parse_env(val: Option<&Value>) -> Result<BTreeMap<String, String>, CapabilityError> {
    let mut env = BTreeMap::new();
    let Some(obj) = val.and_then(Value::as_object) else {
        return Ok(env);
    };
    for (k, v) in obj {
        let s = v.as_str().ok_or_else(|| {
            CapabilityError::invalid_args("system", format!("env.{k} must be a string."))
        })?;
        env.insert(k.clone(), s.to_string());
    }
    Ok(env)
}

fn parse_string_array(val: Option<&Value>, label: &str) -> Result<Vec<String>, CapabilityError> {
    let Some(arr) = val.and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    arr.iter()
        .map(|v| {
            v.as_str()
                .map(ToString::to_string)
                .ok_or_else(|| CapabilityError::invalid_args(label, "Arguments must be strings."))
        })
        .collect()
}

fn resolve_cwd(base: &Path, supplied: Option<&str>) -> PathBuf {
    match supplied {
        Some(p) if Path::new(p).is_absolute() => PathBuf::from(p),
        Some(p) => base.join(p),
        None => base.to_path_buf(),
    }
}

struct PreparedScript {
    command: String,
    args: Vec<String>,
    temp_file: Option<PathBuf>,
}

impl Drop for PreparedScript {
    fn drop(&mut self) {
        if let Some(ref p) = self.temp_file {
            let _ = fs::remove_file(p);
        }
    }
}

fn prepare_script(
    base_cwd: &Path,
    script: &Value,
    extra_args: Vec<String>,
) -> Result<PreparedScript, CapabilityError> {
    let kind = script
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| CapabilityError::invalid_args("scripts.run", "script.kind is required ('file' or 'inline')."))?;
    match kind {
        "file" => {
            let path = script
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| CapabilityError::invalid_args("scripts.run", "script.path is required."))?;
            let resolved = resolve_cwd(base_cwd, Some(path));
            Ok(PreparedScript {
                command: resolved.to_string_lossy().into_owned(),
                args: extra_args,
                temp_file: None,
            })
        }
        "inline" => {
            let interpreter = script
                .get("interpreter")
                .and_then(Value::as_str)
                .ok_or_else(|| CapabilityError::invalid_args("scripts.run", "script.interpreter is required."))?;
            let content = script
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| CapabilityError::invalid_args("scripts.run", "script.content is required."))?;
            let interp_args = parse_string_array(script.get("interpreterArgs"), "scripts.run")?;
            let ext = script
                .get("filenameExtension")
                .and_then(Value::as_str)
                .unwrap_or("");
            let tmp = std::env::temp_dir().join(format!(
                "cant-script-{}-{}{}",
                std::process::id(),
                Uuid::new_v4().simple(),
                ext
            ));
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
                .map_err(|e| CapabilityError::exec("ScriptError", e.to_string()))?;
            f.write_all(content.as_bytes())
                .map_err(|e| CapabilityError::exec("ScriptError", e.to_string()))?;
            drop(f);

            let mut args = interp_args;
            args.push(tmp.to_string_lossy().into_owned());
            args.extend(extra_args);
            Ok(PreparedScript {
                command: interpreter.to_string(),
                args,
                temp_file: Some(tmp),
            })
        }
        other => Err(CapabilityError::invalid_args(
            "scripts.run",
            format!("Unsupported script.kind: {other}"),
        )),
    }
}

async fn run_single_command(
    command: &str,
    args: &[String],
    cwd: &Path,
    env: &BTreeMap<String, String>,
    stdin_data: Option<&str>,
    timeout_secs: f64,
    max_output: usize,
) -> Result<Value, CapabilityError> {
    let mut cmd = Command::new(command);
    cmd.args(args)
        .current_dir(cwd)
        .envs(env)
        .stdin(if stdin_data.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    configure_process_group(&mut cmd);

    let started = Instant::now();
    let mut child = cmd
        .spawn()
        .map_err(|e| CapabilityError::exec("ProcessSpawnError", format!("{command}: {e}")))?;
    let pid = child.id();

    if let (Some(data), Some(mut stdin)) = (stdin_data, child.stdin.take()) {
        let bytes = data.as_bytes().to_vec();
        tokio::spawn(async move {
            let _ = stdin.write_all(&bytes).await;
            let _ = stdin.shutdown().await;
        });
    }

    let stdout_pipe = child.stdout.take().unwrap();
    let stderr_pipe = child.stderr.take().unwrap();
    let stdout_task = tokio::spawn(capture_stream(stdout_pipe, max_output));
    let stderr_task = tokio::spawn(capture_stream(stderr_pipe, max_output));

    let duration = Duration::from_secs_f64(timeout_secs.clamp(0.01, MAX_TIMEOUT_SECS));
    let (status_opt, timed_out) = match tokio::time::timeout(duration, child.wait()).await {
        Ok(Ok(status)) => (Some(status), false),
        Ok(Err(e)) => {
            return Err(CapabilityError::exec("ProcessError", e.to_string()));
        }
        Err(_) => {
            if let Some(p) = pid {
                kill_process_group(p);
            }
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            (None, true)
        }
    };

    let stdout_cap = stdout_task
        .await
        .map_err(|e| CapabilityError::exec("ProcessError", e.to_string()))?
        .map_err(|e| CapabilityError::exec("ProcessError", e.to_string()))?;
    let stderr_cap = stderr_task
        .await
        .map_err(|e| CapabilityError::exec("ProcessError", e.to_string()))?
        .map_err(|e| CapabilityError::exec("ProcessError", e.to_string()))?;

    let exit_code = status_opt.and_then(|s| s.code());
    #[cfg(unix)]
    let signal = {
        use std::os::unix::process::ExitStatusExt;
        status_opt.and_then(|s| s.signal())
    };
    #[cfg(not(unix))]
    let signal: Option<i32> = None;

    let ok = !timed_out && exit_code == Some(0);
    let mut out = json!({
        "ok": ok,
        "exitCode": exit_code,
        "signal": signal,
        "timedOut": timed_out,
        "durationMs": started.elapsed().as_millis() as u64,
        "stdout": stdout_cap.text,
        "stderr": stderr_cap.text,
        "stdoutBytes": stdout_cap.total_bytes,
        "stderrBytes": stderr_cap.total_bytes,
        "stdoutTruncated": stdout_cap.truncated,
        "stderrTruncated": stderr_cap.truncated,
    });
    if let Some(p) = stdout_cap.full_output_path {
        out.as_object_mut()
            .unwrap()
            .insert("stdoutFile".into(), Value::String(p));
    }
    if let Some(p) = stderr_cap.full_output_path {
        out.as_object_mut()
            .unwrap()
            .insert("stderrFile".into(), Value::String(p));
    }
    Ok(out)
}

// --- Capabilities ---

struct SystemExec(CapabilityMeta);
#[async_trait]
impl Capability for SystemExec {
    fn meta(&self) -> &CapabilityMeta {
        &self.0
    }
    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| CapabilityError::invalid_args("system.exec", "Missing 'command'."))?;
        let argv = parse_string_array(args.get("args"), "system.exec")?;
        let cwd = resolve_cwd(&ctx.cwd, args.get("cwd").and_then(Value::as_str));
        let env = parse_env(args.get("env"))?;
        let stdin = args.get("stdin").and_then(Value::as_str);
        let timeout = args
            .get("timeoutSeconds")
            .and_then(Value::as_f64)
            .unwrap_or(DEFAULT_TIMEOUT_SECS);
        let max_out = args
            .get("maxOutputBytes")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_MAX_OUTPUT_BYTES as u64)
            .clamp(1, MAX_OUTPUT_BYTES as u64) as usize;
        run_single_command(command, &argv, &cwd, &env, stdin, timeout, max_out).await
    }
}

struct ScriptsRun(CapabilityMeta);
#[async_trait]
impl Capability for ScriptsRun {
    fn meta(&self) -> &CapabilityMeta {
        &self.0
    }
    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let script = args
            .get("script")
            .ok_or_else(|| CapabilityError::invalid_args("scripts.run", "Missing 'script'."))?;
        let extra_args = parse_string_array(args.get("args"), "scripts.run")?;
        let cwd = resolve_cwd(&ctx.cwd, args.get("cwd").and_then(Value::as_str));
        let env = parse_env(args.get("env"))?;
        let stdin = args.get("stdin").and_then(Value::as_str);
        let timeout = args
            .get("timeoutSeconds")
            .and_then(Value::as_f64)
            .unwrap_or(DEFAULT_TIMEOUT_SECS);
        let max_out = args
            .get("maxOutputBytes")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_MAX_OUTPUT_BYTES as u64)
            .clamp(1, MAX_OUTPUT_BYTES as u64) as usize;

        let prepared = prepare_script(&cwd, script, extra_args)?;
        run_single_command(
            &prepared.command,
            &prepared.args,
            &cwd,
            &env,
            stdin,
            timeout,
            max_out,
        )
        .await
    }
}

struct SystemPipeline(CapabilityMeta);
#[async_trait]
impl Capability for SystemPipeline {
    fn meta(&self) -> &CapabilityMeta {
        &self.0
    }
    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let steps = args
            .get("steps")
            .and_then(Value::as_array)
            .ok_or_else(|| CapabilityError::invalid_args("system.pipeline", "Missing 'steps' array."))?;
        if steps.is_empty() || steps.len() > MAX_PIPELINE_STEPS {
            return Err(CapabilityError::invalid_args(
                "system.pipeline",
                format!("steps must contain 1..={MAX_PIPELINE_STEPS} steps."),
            ));
        }
        let default_cwd = resolve_cwd(&ctx.cwd, args.get("cwd").and_then(Value::as_str));
        let stdin_data = args.get("stdin").and_then(Value::as_str);
        let timeout_secs = args
            .get("timeoutSeconds")
            .and_then(Value::as_f64)
            .unwrap_or(DEFAULT_TIMEOUT_SECS);
        let max_out = args
            .get("maxOutputBytes")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_MAX_OUTPUT_BYTES as u64)
            .clamp(1, MAX_OUTPUT_BYTES as u64) as usize;

        let mut prepared_scripts = Vec::new();
        let mut step_specs = Vec::with_capacity(steps.len());
        for (idx, step) in steps.iter().enumerate() {
            let step_cwd = resolve_cwd(&default_cwd, step.get("cwd").and_then(Value::as_str));
            let step_env = parse_env(step.get("env"))?;
            let extra_args = parse_string_array(step.get("args"), "system.pipeline")?;
            let kind = step.get("kind").and_then(Value::as_str);
            let (command, argv) = if kind == Some("script") || step.get("script").is_some() {
                let script = step.get("script").ok_or_else(|| {
                    CapabilityError::invalid_args(
                        "system.pipeline",
                        format!("steps[{idx}] missing 'script'."),
                    )
                })?;
                let prep = prepare_script(&step_cwd, script, extra_args)?;
                let pair = (prep.command.clone(), prep.args.clone());
                prepared_scripts.push(prep);
                pair
            } else {
                let cmd = step
                    .get("command")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        CapabilityError::invalid_args(
                            "system.pipeline",
                            format!("steps[{idx}] missing 'command'."),
                        )
                    })?
                    .to_string();
                (cmd, extra_args)
            };
            step_specs.push((command, argv, step_cwd, step_env));
        }

        let started = Instant::now();
        let mut children = Vec::with_capacity(step_specs.len());
        let mut stderr_tasks = Vec::with_capacity(step_specs.len());
        let mut prev_stdout: Option<Stdio> = None;

        for (idx, (command, argv, step_cwd, step_env)) in step_specs.iter().enumerate() {
            let mut cmd = Command::new(command);
            cmd.args(argv)
                .current_dir(step_cwd)
                .envs(step_env)
                .stderr(Stdio::piped())
                .stdout(Stdio::piped())
                .kill_on_drop(true);
            if idx == 0 {
                if stdin_data.is_some() {
                    cmd.stdin(Stdio::piped());
                } else {
                    cmd.stdin(Stdio::null());
                }
            } else if let Some(prev) = prev_stdout.take() {
                cmd.stdin(prev);
            }
            configure_process_group(&mut cmd);

            let mut child = cmd.spawn().map_err(|e| {
                CapabilityError::exec("ProcessSpawnError", format!("{command}: {e}"))
            })?;
            if idx == 0 {
                if let (Some(data), Some(mut stdin)) = (stdin_data, child.stdin.take()) {
                    let bytes = data.as_bytes().to_vec();
                    tokio::spawn(async move {
                        let _ = stdin.write_all(&bytes).await;
                        let _ = stdin.shutdown().await;
                    });
                }
            }
            let stderr_pipe = child.stderr.take().unwrap();
            stderr_tasks.push(tokio::spawn(capture_stream(stderr_pipe, max_out)));

            if idx + 1 < step_specs.len() {
                let stdout_pipe = child.stdout.take().unwrap();
                let std_stdout = stdout_pipe
                    .into_owned_fd()
                    .map_err(|e| CapabilityError::exec("ProcessError", e.to_string()))?;
                prev_stdout = Some(Stdio::from(std_stdout));
            }
            children.push(child);
        }

        let last_stdout = children.last_mut().unwrap().stdout.take().unwrap();
        let stdout_task = tokio::spawn(capture_stream(last_stdout, max_out));

        let pids: Vec<Option<u32>> = children.iter().map(|c| c.id()).collect();
        let deadline = tokio::time::Instant::now()
            + Duration::from_secs_f64(timeout_secs.clamp(0.01, MAX_TIMEOUT_SECS));

        let mut timed_out = false;
        let mut statuses = Vec::with_capacity(children.len());
        for child in &mut children {
            if timed_out {
                let _ = child.start_kill();
                let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
                statuses.push(None);
                continue;
            }
            match tokio::time::timeout_at(deadline, child.wait()).await {
                Ok(Ok(st)) => statuses.push(Some(st)),
                Ok(Err(e)) => return Err(CapabilityError::exec("ProcessError", e.to_string())),
                Err(_) => {
                    timed_out = true;
                    for p in pids.iter().flatten() {
                        kill_process_group(*p);
                    }
                    let _ = child.start_kill();
                    let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
                    statuses.push(None);
                }
            }
        }

        let stdout_cap = stdout_task
            .await
            .map_err(|e| CapabilityError::exec("ProcessError", e.to_string()))?
            .map_err(|e| CapabilityError::exec("ProcessError", e.to_string()))?;

        let mut step_results = Vec::with_capacity(statuses.len());
        let mut all_ok = !timed_out;
        for (idx, (st, stderr_t)) in statuses.into_iter().zip(stderr_tasks.into_iter()).enumerate() {
            let err_cap = stderr_t
                .await
                .map_err(|e| CapabilityError::exec("ProcessError", e.to_string()))?
                .map_err(|e| CapabilityError::exec("ProcessError", e.to_string()))?;
            let code = st.and_then(|s| s.code());
            let step_ok = !timed_out && code == Some(0);
            if !step_ok {
                all_ok = false;
            }
            step_results.push(json!({
                "index": idx,
                "command": step_specs[idx].0,
                "ok": step_ok,
                "exitCode": code,
                "stderr": err_cap.text,
                "stderrBytes": err_cap.total_bytes,
                "stderrTruncated": err_cap.truncated,
            }));
        }

        drop(prepared_scripts);
        let mut out = json!({
            "ok": all_ok,
            "timedOut": timed_out,
            "durationMs": started.elapsed().as_millis() as u64,
            "stdout": stdout_cap.text,
            "stdoutBytes": stdout_cap.total_bytes,
            "stdoutTruncated": stdout_cap.truncated,
            "steps": step_results,
        });
        if let Some(p) = stdout_cap.full_output_path {
            out.as_object_mut()
                .unwrap()
                .insert("stdoutFile".into(), Value::String(p));
        }
        Ok(out)
    }
}

fn steel_pos(fields: &[&str]) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("steel".into(), json!({ "positional": fields }));
    m
}

pub fn register(catalog: &CapabilityCatalog) {
    catalog.register(Arc::new(SystemExec(CapabilityMeta {
        tool_name: "__cant_system_exec".into(),
        api_path: vec!["system".into(), "exec".into()],
        description: "Execute a command directly with literal argv (no shell). Returns structured stdout/stderr, exitCode, timedOut, and optional spill file paths.".into(),
        params_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["command"],
            "properties": {
                "command": { "type": "string" },
                "args": { "type": "array", "items": { "type": "string" } },
                "cwd": { "type": "string" },
                "env": { "type": "object", "additionalProperties": { "type": "string" } },
                "stdin": { "type": "string" },
                "timeoutSeconds": { "type": "number", "minimum": 0.01, "maximum": 3600 },
                "maxOutputBytes": { "type": "integer", "minimum": 1, "maximum": 262144 }
            }
        }),
        result_schema: json!({ "type": "object" }),
        argument_mode: ArgumentMode::Single,
        language_hints: steel_pos(&["command", "args"]),
    })));

    catalog.register(Arc::new(SystemPipeline(CapabilityMeta {
        tool_name: "__cant_system_pipeline".into(),
        api_path: vec!["system".into(), "pipeline".into()],
        description: "Run a multi-stage OS pipeline of exec or script steps (use system/exec-step and system/script-step in Steel). Intermediate stdout stays out of model context.".into(),
        params_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["steps"],
            "properties": {
                "steps": { "type": "array", "minItems": 1, "maxItems": 16 },
                "cwd": { "type": "string" },
                "stdin": { "type": "string" },
                "timeoutSeconds": { "type": "number", "minimum": 0.01, "maximum": 3600 },
                "maxOutputBytes": { "type": "integer", "minimum": 1, "maximum": 262144 }
            }
        }),
        result_schema: json!({ "type": "object" }),
        argument_mode: ArgumentMode::Single,
        language_hints: steel_pos(&["steps"]),
    })));

    catalog.register(Arc::new(ScriptsRun(CapabilityMeta {
        tool_name: "__cant_scripts_run".into(),
        api_path: vec!["scripts".into(), "run".into()],
        description: "Run an executable script file or inline script via an interpreter (use scripts/inline or scripts/file in Steel).".into(),
        params_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["script"],
            "properties": {
                "script": { "type": "object" },
                "args": { "type": "array", "items": { "type": "string" } },
                "cwd": { "type": "string" },
                "env": { "type": "object", "additionalProperties": { "type": "string" } },
                "stdin": { "type": "string" },
                "timeoutSeconds": { "type": "number", "minimum": 0.01, "maximum": 3600 },
                "maxOutputBytes": { "type": "integer", "minimum": 1, "maximum": 262144 }
            }
        }),
        result_schema: json!({ "type": "object" }),
        argument_mode: ArgumentMode::Single,
        language_hints: steel_pos(&["script", "args"]),
    })));
}
