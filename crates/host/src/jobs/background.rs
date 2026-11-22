//! Background script capabilities:
//! `background/start`, `background/output`, `background/status`, `background/list`, `background/cancel`.

use crate::capability::{
    ArgumentMode, CallContext, Capability, CapabilityCatalog, CapabilityError, CapabilityMeta,
};
use crate::jobs::{truncate_head_utf8, truncate_tail_utf8, NotifyMode, MAX_OUTPUT_REPLY_BYTES};
use async_trait::async_trait;
use cant_protocol::daemon::{CompletionNotice, JobSummary};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const MAX_ACTIVE_SCRIPTS: usize = 32;
const MAX_BUFFER_BYTES: usize = 10 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptStatus {
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

pub struct ScriptJob {
    pub id: String,
    pub session_id: String,
    pub script: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub notify: NotifyMode,
    pub status: ScriptStatus,
    pub settled: bool,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub pid: Option<u32>,
    pub started_at: Instant,
    pub finished_ms: Option<u64>,
    pub buffer: String,
    /// Total bytes ever appended (monotonic offset for cursor tracking).
    pub total_written: usize,
    /// Bytes dropped from front of `buffer` due to rolling cap.
    pub dropped_bytes: usize,
    /// Current read cursor (in monotonic total_written coordinates).
    pub cursor: usize,
    pub cancel_token: CancellationToken,
    pub cancelled_explicitly: bool,
}

impl ScriptJob {
    pub fn status_str(&self) -> &'static str {
        match self.status {
            ScriptStatus::Running => "running",
            ScriptStatus::Succeeded => "succeeded",
            ScriptStatus::Failed => "failed",
            ScriptStatus::Cancelled => "cancelled",
        }
    }

    pub fn elapsed_ms(&self) -> u64 {
        self.finished_ms
            .unwrap_or_else(|| self.started_at.elapsed().as_millis() as u64)
    }

    pub fn append(&mut self, chunk: &str) {
        self.total_written += chunk.len();
        self.buffer.push_str(chunk);
        if self.buffer.len() > MAX_BUFFER_BYTES {
            let overflow = self.buffer.len() - MAX_BUFFER_BYTES;
            let mut cut = overflow;
            while cut < self.buffer.len() && !self.buffer.is_char_boundary(cut) {
                cut += 1;
            }
            self.buffer.drain(..cut);
            self.dropped_bytes += cut;
        }
    }

    pub fn tail(&self, max_bytes: usize) -> String {
        truncate_tail_utf8(&self.buffer, max_bytes)
    }

    pub fn summary(&self) -> JobSummary {
        let cmd_desc = if self.args.is_empty() {
            self.script.clone()
        } else {
            format!("{} {}", self.script, self.args.join(" "))
        };
        JobSummary {
            id: self.id.clone(),
            kind: "script".into(),
            name: self.script.clone(),
            session_id: self.session_id.clone(),
            status: self.status_str().into(),
            elapsed_ms: self.elapsed_ms(),
            summary: cmd_desc.chars().take(120).collect(),
        }
    }
}

fn kill_pg(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = pid;
}

pub fn cancel_script_job(job: &Arc<Mutex<ScriptJob>>) {
    let mut g = job.lock().unwrap();
    g.cancelled_explicitly = true;
    if !g.settled {
        g.status = ScriptStatus::Cancelled;
        if let Some(pid) = g.pid {
            kill_pg(pid);
        }
        g.cancel_token.cancel();
    }
}

fn resolve_script_command(cwd: &Path, script: &str) -> String {
    let p = Path::new(script);
    if p.is_absolute() {
        return script.to_string();
    }
    let local = cwd.join(p);
    if local.is_file() {
        return local.to_string_lossy().into_owned();
    }
    script.to_string()
}

struct BackgroundStart(CapabilityMeta);

#[async_trait]
impl Capability for BackgroundStart {
    fn meta(&self) -> &CapabilityMeta {
        &self.0
    }

    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let script = args
            .get("script")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| CapabilityError::invalid_args("background.start", "Missing 'script'."))?
            .to_string();
        let argv: Vec<String> = args
            .get("args")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .map(|v| {
                        v.as_str()
                            .map(ToString::to_string)
                            .ok_or_else(|| CapabilityError::invalid_args("background.start", "args must be strings."))
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?
            .unwrap_or_default();
        let cwd = match args.get("cwd").and_then(Value::as_str) {
            Some(c) if Path::new(c).is_absolute() => PathBuf::from(c),
            Some(c) => ctx.cwd.join(c),
            None => ctx.cwd.clone(),
        };
        let mut env = BTreeMap::new();
        if let Some(obj) = args.get("env").and_then(Value::as_object) {
            for (k, v) in obj {
                let val = v.as_str().ok_or_else(|| {
                    CapabilityError::invalid_args("background.start", format!("env.{k} must be a string."))
                })?;
                env.insert(k.clone(), val.to_string());
            }
        }
        let notify = NotifyMode::parse(args.get("notify").and_then(Value::as_str))
            .map_err(|e| CapabilityError::invalid_args("background.start", e))?;

        {
            let map = ctx.jobs.scripts.lock().unwrap();
            let active = map
                .values()
                .filter(|j| !j.lock().unwrap().settled)
                .count();
            if active >= MAX_ACTIVE_SCRIPTS {
                return Err(CapabilityError::exec(
                    "JobLimitError",
                    format!("At most {MAX_ACTIVE_SCRIPTS} unsettled background scripts may run concurrently."),
                ));
            }
        }

        let resolved_cmd = resolve_script_command(&cwd, &script);
        let mut cmd = Command::new(&resolved_cmd);
        cmd.args(&argv)
            .current_dir(&cwd)
            .envs(&env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        unsafe {
            cmd.pre_exec(|| {
                libc::setpgid(0, 0);
                Ok(())
            });
        }

        let mut child = cmd.spawn().map_err(|e| {
            CapabilityError::exec("ProcessSpawnError", format!("{script}: {e}"))
        })?;
        let pid = child.id();
        let id = format!("bgsh_{}", &Uuid::new_v4().simple().to_string()[..12]);
        let cancel_token = CancellationToken::new();

        let job = Arc::new(Mutex::new(ScriptJob {
            id: id.clone(),
            session_id: ctx.session_id.clone(),
            script: script.clone(),
            args: argv.clone(),
            cwd: cwd.clone(),
            notify,
            status: ScriptStatus::Running,
            settled: false,
            exit_code: None,
            signal: None,
            pid,
            started_at: Instant::now(),
            finished_ms: None,
            buffer: String::new(),
            total_written: 0,
            dropped_bytes: 0,
            cursor: 0,
            cancel_token: cancel_token.clone(),
            cancelled_explicitly: false,
        }));

        ctx.jobs
            .scripts
            .lock()
            .unwrap()
            .insert(id.clone(), Arc::clone(&job));

        ctx.jobs.emit_event(&ctx.session_id, cant_protocol::daemon::DaemonEvent::JobUpdated { job: job.lock().unwrap().summary() });
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let job_out = Arc::clone(&job);
        let jobs_ev_out = Arc::clone(&ctx.jobs);
        let sid_out = ctx.session_id.clone();
        let id_out = id.clone();
        let t_out = tokio::spawn(async move {
            let mut r = stdout;
            let mut buf = [0u8; 4096];
            while let Ok(n) = r.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                job_out.lock().unwrap().append(&text);
                jobs_ev_out.emit_event(&sid_out, cant_protocol::daemon::DaemonEvent::JobOutput { id: id_out.clone(), chunk: text });
            }
        });
        let job_err = Arc::clone(&job);
        let jobs_ev_err = Arc::clone(&ctx.jobs);
        let sid_err = ctx.session_id.clone();
        let id_err = id.clone();
        let t_err = tokio::spawn(async move {
            let mut r = stderr;
            let mut buf = [0u8; 4096];
            while let Ok(n) = r.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                job_err.lock().unwrap().append(&text);
                jobs_ev_err.emit_event(&sid_err, cant_protocol::daemon::DaemonEvent::JobOutput { id: id_err.clone(), chunk: text });
            }
        });

        let jobs_mgr = Arc::clone(&ctx.jobs);
        let job_wait = Arc::clone(&job);
        let session_id = ctx.session_id.clone();
        tokio::spawn(async move {
            let status_res = tokio::select! {
                res = child.wait() => res.ok(),
                _ = cancel_token.cancelled() => {
                    if let Some(p) = pid {
                        kill_pg(p);
                    }
                    let _ = child.start_kill();
                    child.wait().await.ok()
                }
            };
            let _ = t_out.await;
            let _ = t_err.await;

            let (notice_opt, final_session) = {
                let mut g = job_wait.lock().unwrap();
                let code = status_res.and_then(|s| s.code());
                #[cfg(unix)]
                let sig = {
                    use std::os::unix::process::ExitStatusExt;
                    status_res.and_then(|s| s.signal())
                };
                #[cfg(not(unix))]
                let sig: Option<i32> = None;

                g.exit_code = code;
                g.signal = sig;
                g.finished_ms = Some(g.started_at.elapsed().as_millis() as u64);
                g.settled = true;
                if g.cancelled_explicitly {
                    g.status = ScriptStatus::Cancelled;
                } else if code == Some(0) {
                    g.status = ScriptStatus::Succeeded;
                } else {
                    g.status = ScriptStatus::Failed;
                }

                let notice = if g.cancelled_explicitly || g.notify == NotifyMode::Off {
                    None
                } else {
                    let tail_preview: String = g
                        .tail(400)
                        .lines()
                        .rev()
                        .take(3)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect::<Vec<_>>()
                        .join(" | ");
                    Some(CompletionNotice {
                        job_id: g.id.clone(),
                        kind: "script".into(),
                        name: g.script.clone(),
                        status: g.status_str().into(),
                        notify: g.notify.as_str().into(),
                        summary: format!(
                            "`{}` {} (exit={:?}, {}ms){}",
                            g.script,
                            g.status_str(),
                            g.exit_code,
                            g.elapsed_ms(),
                            if tail_preview.is_empty() {
                                String::new()
                            } else {
                                format!(" — {tail_preview}")
                            }
                        ),
                        completion_token: None,
                    })
                };
                (notice, session_id)
            };

            let final_summary = job_wait.lock().unwrap().summary();
            jobs_mgr.emit_event(&final_session, cant_protocol::daemon::DaemonEvent::JobUpdated { job: final_summary });
            if let Some(n) = notice_opt {
                jobs_mgr.push_notice(final_session, n);
            }
        });

        Ok(json!({
            "id": id,
            "status": "running",
            "pid": pid,
            "notify": notify.as_str(),
        }))
    }
}

struct BackgroundOutput(CapabilityMeta);

#[async_trait]
impl Capability for BackgroundOutput {
    fn meta(&self) -> &CapabilityMeta {
        &self.0
    }

    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let id = args
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| CapabilityError::invalid_args("background.output", "Missing 'id'."))?;
        let filter = match args.get("filter").and_then(Value::as_str) {
            Some(pat) => Some(Regex::new(pat).map_err(|e| {
                CapabilityError::invalid_args("background.output", format!("Invalid filter regex: {e}"))
            })?),
            None => None,
        };
        let job = ctx
            .jobs
            .scripts
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| CapabilityError::exec("JobNotFoundError", format!("No background script '{id}'.")))?;

        let mut g = job.lock().unwrap();
        if g.session_id != ctx.session_id {
            return Err(CapabilityError::exec(
                "JobOwnershipError",
                "Background script belongs to another session.",
            ));
        }

        let dropped_chars = g.dropped_bytes.saturating_sub(g.cursor);
        let effective_cursor = g.cursor.max(g.dropped_bytes);
        let rel_offset = effective_cursor - g.dropped_bytes;
        let slice = if rel_offset <= g.buffer.len() && g.buffer.is_char_boundary(rel_offset) {
            &g.buffer[rel_offset..]
        } else {
            ""
        };
        let raw_chunk = match filter {
            Some(re) => slice
                .lines()
                .filter(|l| re.is_match(l))
                .collect::<Vec<_>>()
                .join("\n"),
            None => slice.to_string(),
        };
        g.cursor = g.total_written;

        let (text, truncated) = truncate_head_utf8(&raw_chunk, MAX_OUTPUT_REPLY_BYTES);
        Ok(json!({
            "id": g.id,
            "status": g.status_str(),
            "settled": g.settled,
            "exitCode": g.exit_code,
            "text": text,
            "truncated": truncated,
            "droppedChars": dropped_chars,
        }))
    }
}

struct BackgroundStatus(CapabilityMeta);

#[async_trait]
impl Capability for BackgroundStatus {
    fn meta(&self) -> &CapabilityMeta {
        &self.0
    }

    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let id = args
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| CapabilityError::invalid_args("background.status", "Missing 'id'."))?;
        let job = ctx
            .jobs
            .scripts
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| CapabilityError::exec("JobNotFoundError", format!("No background script '{id}'.")))?;
        let g = job.lock().unwrap();
        if g.session_id != ctx.session_id {
            return Err(CapabilityError::exec(
                "JobOwnershipError",
                "Background script belongs to another session.",
            ));
        }
        Ok(json!({
            "id": g.id,
            "script": g.script,
            "status": g.status_str(),
            "settled": g.settled,
            "exitCode": g.exit_code,
            "signal": g.signal,
            "durationMs": g.elapsed_ms(),
            "unreadBytes": g.total_written.saturating_sub(g.cursor),
        }))
    }
}

struct BackgroundList(CapabilityMeta);

#[async_trait]
impl Capability for BackgroundList {
    fn meta(&self) -> &CapabilityMeta {
        &self.0
    }

    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(20)
            .clamp(1, 50) as usize;

        let mut owned: Vec<Value> = ctx
            .jobs
            .scripts
            .lock()
            .unwrap()
            .values()
            .filter_map(|j| {
                let g = j.lock().unwrap();
                if g.session_id != ctx.session_id {
                    return None;
                }
                Some(json!({
                    "id": g.id,
                    "script": g.script,
                    "status": g.status_str(),
                    "settled": g.settled,
                    "exitCode": g.exit_code,
                    "durationMs": g.elapsed_ms(),
                }))
            })
            .collect();
        owned.sort_by(|a, b| {
            a.get("id")
                .and_then(Value::as_str)
                .cmp(&b.get("id").and_then(Value::as_str))
        });
        let total = owned.len();
        let items: Vec<Value> = owned.into_iter().skip(offset).take(limit).collect();
        let has_more = offset + items.len() < total;
        Ok(json!({
            "items": items,
            "total": total,
            "hasMore": has_more,
            "nextOffset": if has_more { json!(offset + items.len()) } else { Value::Null },
        }))
    }
}

struct BackgroundCancel(CapabilityMeta);

#[async_trait]
impl Capability for BackgroundCancel {
    fn meta(&self) -> &CapabilityMeta {
        &self.0
    }

    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let id = args
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| CapabilityError::invalid_args("background.cancel", "Missing 'id'."))?;
        let job = ctx
            .jobs
            .scripts
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| CapabilityError::exec("JobNotFoundError", format!("No background script '{id}'.")))?;
        {
            let g = job.lock().unwrap();
            if g.session_id != ctx.session_id {
                return Err(CapabilityError::exec(
                    "JobOwnershipError",
                    "Background script belongs to another session.",
                ));
            }
        }
        cancel_script_job(&job);
        let g = job.lock().unwrap();
        Ok(json!({
            "id": g.id,
            "status": g.status_str(),
            "settled": g.settled,
        }))
    }
}

fn steel_pos(fields: &[&str]) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("steel".into(), json!({ "positional": fields }));
    m
}

pub fn register(catalog: &CapabilityCatalog) {
    catalog.register(Arc::new(BackgroundStart(CapabilityMeta {
        tool_name: "__cant_background_start".into(),
        api_path: vec!["background".into(), "start".into()],
        description: "Start a background executable/PATH command with literal argv. Returns a job receipt immediately; completion is delivered via notification.".into(),
        params_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["script"],
            "properties": {
                "script": { "type": "string" },
                "args": { "type": "array", "items": { "type": "string" } },
                "cwd": { "type": "string" },
                "env": { "type": "object", "additionalProperties": { "type": "string" } },
                "notify": { "type": "string", "enum": ["active", "passive", "off"] }
            }
        }),
        result_schema: json!({ "type": "object" }),
        argument_mode: ArgumentMode::Single,
        language_hints: steel_pos(&["script", "args"]),
    })));

    catalog.register(Arc::new(BackgroundOutput(CapabilityMeta {
        tool_name: "__cant_background_output".into(),
        api_path: vec!["background".into(), "output".into()],
        description: "Read newly produced merged stdout/stderr from a background script, advancing its read cursor (optional regex filter).".into(),
        params_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["id"],
            "properties": {
                "id": { "type": "string" },
                "filter": { "type": "string" }
            }
        }),
        result_schema: json!({ "type": "object" }),
        argument_mode: ArgumentMode::Single,
        language_hints: steel_pos(&["id"]),
    })));

    catalog.register(Arc::new(BackgroundStatus(CapabilityMeta {
        tool_name: "__cant_background_status".into(),
        api_path: vec!["background".into(), "status".into()],
        description: "Check status of a background script without advancing its output cursor.".into(),
        params_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["id"],
            "properties": { "id": { "type": "string" } }
        }),
        result_schema: json!({ "type": "object" }),
        argument_mode: ArgumentMode::Single,
        language_hints: steel_pos(&["id"]),
    })));

    catalog.register(Arc::new(BackgroundList(CapabilityMeta {
        tool_name: "__cant_background_list".into(),
        api_path: vec!["background".into(), "list".into()],
        description: "List background scripts owned by this session.".into(),
        params_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "offset": { "type": "integer", "minimum": 0 },
                "limit": { "type": "integer", "minimum": 1, "maximum": 50 }
            }
        }),
        result_schema: json!({ "type": "object" }),
        argument_mode: ArgumentMode::Single,
        language_hints: Map::new(),
    })));

    catalog.register(Arc::new(BackgroundCancel(CapabilityMeta {
        tool_name: "__cant_background_cancel".into(),
        api_path: vec!["background".into(), "cancel".into()],
        description: "Cancel a running background script and suppress its completion notification.".into(),
        params_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["id"],
            "properties": { "id": { "type": "string" } }
        }),
        result_schema: json!({ "type": "object" }),
        argument_mode: ArgumentMode::Single,
        language_hints: steel_pos(&["id"]),
    })));
}
