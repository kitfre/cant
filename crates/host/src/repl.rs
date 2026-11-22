//! Persistent REPL workspace lifecycle, history, formatting, and multi-session manager.

use crate::adapter::AdapterHub;
use crate::capability::{is_path_disabled, ArgumentMode, CallContext, CapabilityCatalog, FileCoverageLedger};
use crate::executor::{ConsoleEntry, ExecutorError, RunnerExecutor};
use crate::comms::CommsHub;
use crate::jobs::JobManager;
use crate::language::{GuestLanguage, LanguageRegistry};
use cant_protocol::daemon::{ReplRequest, ReplToolResponse, WorkspaceKey, WorkspaceSummary};
use cant_protocol::runner::{
    Bindings, ConsoleLevel, Program, RunnerReplOp, RunnerExecution, MAX_INPUTS_BYTES,
    MAX_SOURCE_BYTES,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;
use uuid::Uuid;

const MAX_HISTORY_ENTRIES: usize = 100;
const MAX_HISTORY_BYTES: usize = 1024 * 1024;
const MAX_HISTORY_SOURCE_REPLY_BYTES: usize = 32 * 1024;
const MAX_REPLY_BYTES: usize = 50 * 1024;
const MAX_REPLY_LINES: usize = 2_000;

#[derive(Debug, Clone)]
struct Submission {
    id: u64,
    source: String,
    source_bytes: usize,
    status: &'static str,
    duration_ms: u64,
}

struct WorkspaceMeta {
    session_id: String,
    cwd: String,
    language: String,
    workspace_id: String,
    generation: AtomicU64,
    active: AtomicBool,
}

impl WorkspaceMeta {
    fn new(session_id: String, cwd: &Path, language: &str, workspace_id: String) -> Self {
        Self {
            session_id,
            cwd: cwd.to_string_lossy().into_owned(),
            language: language.to_string(),
            workspace_id,
            generation: AtomicU64::new(1),
            active: AtomicBool::new(false),
        }
    }

    fn set_state(&self, generation: u64, active: bool) {
        self.generation.store(generation, Ordering::Release);
        self.active.store(active, Ordering::Release);
    }

    fn snapshot(&self) -> WorkspaceSummary {
        WorkspaceSummary {
            session_id: self.session_id.clone(),
            cwd: self.cwd.clone(),
            language: self.language.clone(),
            workspace_id: self.workspace_id.clone(),
            generation: self.generation.load(Ordering::Acquire),
            active: self.active.load(Ordering::Acquire),
        }
    }
}

pub struct ReplWorkspace {
    workspace_id: String,
    session_id: String,
    cwd: PathBuf,
    disabled_namespaces: Vec<String>,
    worker_driver: String,
    language: Arc<dyn GuestLanguage>,
    catalog: Arc<CapabilityCatalog>,
    adapters: Arc<AdapterHub>,
    jobs: Arc<JobManager>,
    comms: Arc<CommsHub>,
    meta: Arc<WorkspaceMeta>,
    file_coverage: Arc<FileCoverageLedger>,
    executor: Option<RunnerExecutor>,
    catalog_version: Option<(u64, u64)>,
    generation: u64,
    next_submission: u64,
    history: VecDeque<Submission>,
    retained_bytes: usize,
}

impl ReplWorkspace {
    pub fn new(
        session_id: String,
        cwd: PathBuf,
        disabled_namespaces: Vec<String>,
        worker_driver: String,
        language: Arc<dyn GuestLanguage>,
        catalog: Arc<CapabilityCatalog>,
        adapters: Arc<AdapterHub>,
        jobs: Arc<JobManager>,
        comms: Arc<CommsHub>,
    ) -> Self {
        let workspace_id = Uuid::new_v4().to_string();
        let meta = Arc::new(WorkspaceMeta::new(
            session_id.clone(),
            &cwd,
            language.identity().id,
            workspace_id.clone(),
        ));
        Self {
            workspace_id,
            session_id,
            cwd,
            disabled_namespaces,
            worker_driver,
            language,
            catalog,
            adapters,
            jobs,
            comms,
            meta,
            file_coverage: Arc::new(FileCoverageLedger::default()),
            executor: None,
            catalog_version: None,
            generation: 1,
            next_submission: 0,
            history: VecDeque::new(),
            retained_bytes: 0,
        }
    }

    fn meta(&self) -> Arc<WorkspaceMeta> {
        Arc::clone(&self.meta)
    }

    fn sync_meta(&self) {
        let active = self.executor.as_ref().is_some_and(|e| e.is_alive());
        self.meta.set_state(self.generation, active);
    }

    fn remember(&mut self, sub: Submission) {
        self.retained_bytes += sub.source_bytes;
        self.history.push_back(sub);
        while self.history.len() > MAX_HISTORY_ENTRIES || self.retained_bytes > MAX_HISTORY_BYTES {
            if let Some(evicted) = self.history.pop_front() {
                self.retained_bytes = self.retained_bytes.saturating_sub(evicted.source_bytes);
            }
        }
    }

    async fn drop_executor(&mut self) -> (u64, u64) {
        let from = self.generation;
        if let Some(mut exec) = self.executor.take() {
            exec.dispose().await;
        }
        self.catalog_version = None;
        self.history.clear();
        self.retained_bytes = 0;
        self.generation += 1;
        self.sync_meta();
        (from, self.generation)
    }

    fn read_history(
        &self,
        id: Option<u64>,
        offset: Option<usize>,
        limit: Option<usize>,
    ) -> ReplToolResponse {
        if let Some(target_id) = id {
            if let Some(entry) = self.history.iter().find(|e| e.id == target_id) {
                let (source, truncated) =
                    truncate_utf8(&entry.source, MAX_HISTORY_SOURCE_REPLY_BYTES);
                let meta = json!({
                    "id": entry.id,
                    "sourceBytes": entry.source_bytes,
                    "status": entry.status,
                    "durationMs": entry.duration_ms,
                    "sourceTruncated": truncated,
                });
                let text = format!(
                    "repl · {} · g{} · ok\n\n{}\n\nSource:\n{}",
                    self.workspace_id,
                    self.generation,
                    self.language.format_metadata(&meta),
                    source
                );
                return ReplToolResponse {
                    text,
                    is_error: false,
                    workspace_id: self.workspace_id.clone(),
                    generation: self.generation,
                    submission_id: None,
                    status: "ok".into(),
                    raw_value: Some(meta),
                };
            } else {
                let text = format!(
                    "repl · {} · g{} · error\n\nReplHistoryError: Submission #{target_id} is not retained in this workspace generation.",
                    self.workspace_id, self.generation
                );
                return ReplToolResponse {
                    text,
                    is_error: true,
                    workspace_id: self.workspace_id.clone(),
                    generation: self.generation,
                    submission_id: None,
                    status: "error".into(),
                    raw_value: None,
                };
            }
        }

        let offset = offset.unwrap_or(0);
        let limit = limit.unwrap_or(20).clamp(1, 50);
        let total = self.history.len();
        let items: Vec<Value> = self
            .history
            .iter()
            .rev()
            .skip(offset)
            .take(limit)
            .map(|e| {
                json!({
                    "id": e.id,
                    "sourceBytes": e.source_bytes,
                    "status": e.status,
                    "durationMs": e.duration_ms,
                })
            })
            .collect();
        let has_more = offset + items.len() < total;
        let value = json!({
            "items": items,
            "total": total,
            "hasMore": has_more,
            "nextOffset": if has_more { json!(offset + items.len()) } else { Value::Null },
            "workspaceActive": self.executor.as_ref().is_some_and(|e| e.is_alive()),
        });
        let text = format!(
            "repl · {} · g{} · ok\n\n{}",
            self.workspace_id,
            self.generation,
            self.language.format_metadata(&value)
        );
        ReplToolResponse {
            text,
            is_error: false,
            workspace_id: self.workspace_id.clone(),
            generation: self.generation,
            submission_id: None,
            status: "ok".into(),
            raw_value: Some(value),
        }
    }

    pub async fn run(&mut self, request: ReplRequest) -> ReplToolResponse {
        match request {
            ReplRequest::Reset => {
                let (from, to) = self.drop_executor().await;
                let value = json!({ "workspace": "empty" });
                let text = format!(
                    "repl · {} · g{} · ok\n\nWorkspace restarted: g{} → g{} (reset). Retained bindings and history were discarded.\n\n{}",
                    self.workspace_id,
                    to,
                    from,
                    to,
                    self.language.format_metadata(&value)
                );
                return ReplToolResponse {
                    text,
                    is_error: false,
                    workspace_id: self.workspace_id.clone(),
                    generation: to,
                    submission_id: None,
                    status: "ok".into(),
                    raw_value: Some(value),
                };
            }
            ReplRequest::Close => {
                let (from, to) = self.drop_executor().await;
                let value = json!({ "workspace": "closed" });
                let text = format!(
                    "repl · {} · g{} · ok\n\nWorkspace closed: g{} → g{} (closed). Retained bindings and history were discarded.\n\n{}",
                    self.workspace_id,
                    to,
                    from,
                    to,
                    self.language.format_metadata(&value)
                );
                return ReplToolResponse {
                    text,
                    is_error: false,
                    workspace_id: self.workspace_id.clone(),
                    generation: to,
                    submission_id: None,
                    status: "ok".into(),
                    raw_value: Some(value),
                };
            }
            ReplRequest::History { id, offset, limit } => {
                return self.read_history(id, offset, limit);
            }
            _ => {}
        }

        let current_version = (self.catalog.version(), self.adapters.version());
        if let Some(prev) = self.catalog_version {
            if prev != current_version {
                let (from, to) = self.drop_executor().await;
                let text = format!(
                    "repl · {} · g{} · lost\n\nWorkspace restarted: g{} → g{} (catalog_changed). Retained bindings and history were discarded.\n\nThis request wasn't executed. Continue explicitly in the new workspace.",
                    self.workspace_id, to, from, to
                );
                return ReplToolResponse {
                    text,
                    is_error: true,
                    workspace_id: self.workspace_id.clone(),
                    generation: to,
                    submission_id: None,
                    status: "lost".into(),
                    raw_value: None,
                };
            }
        }

        let (source, inputs, runner_op, eval_source, describe_name) = match request {
            ReplRequest::Eval { code, inputs } => {
                if code.len() > MAX_SOURCE_BYTES {
                    return self.error_reply(
                        None,
                        "ReplPreparationError",
                        "REPL code exceeds the 256 KiB limit.",
                        false,
                    );
                }
                let inputs_bytes = serde_json::to_vec(&inputs)
                    .map(|b| b.len())
                    .unwrap_or(usize::MAX);
                if inputs_bytes > MAX_INPUTS_BYTES {
                    return self.error_reply(
                        None,
                        "ReplPreparationError",
                        "REPL inputs exceed the 256 KiB limit.",
                        false,
                    );
                }
                (code.clone(), inputs, RunnerReplOp::Eval, Some(code), None)
            }
            ReplRequest::Inspect {
                name,
                origin,
                offset,
                limit,
            } => (
                String::new(),
                BTreeMap::new(),
                RunnerReplOp::Inspect {
                    name,
                    origin,
                    offset,
                    limit: limit.map(|l| l.clamp(1, 50)),
                },
                None,
                None,
            ),
            ReplRequest::Search {
                query,
                origin,
                offset,
                limit,
            } => (
                String::new(),
                BTreeMap::new(),
                RunnerReplOp::Search {
                    query,
                    origin,
                    offset,
                    limit: limit.map(|l| l.clamp(1, 50)),
                },
                None,
                None,
            ),
            ReplRequest::Describe { name } => (
                String::new(),
                BTreeMap::new(),
                RunnerReplOp::Describe { name: name.clone() },
                None,
                Some(name),
            ),
            ReplRequest::History { .. } | ReplRequest::Reset | ReplRequest::Close => unreachable!(),
        };

        let submission_id = if eval_source.is_some() {
            self.next_submission += 1;
            Some(self.next_submission)
        } else {
            None
        };

        let extra_capabilities = self.adapters.capabilities_for_session(&self.session_id);
        let mut metas = self.catalog.metas_filtered(&self.disabled_namespaces);
        for cap in &extra_capabilities {
            let m = cap.meta();
            if !is_path_disabled(&m.api_path, &self.disabled_namespaces) {
                metas.push(m.clone());
            }
        }
        let api_paths: Vec<Vec<String>> = metas.iter().map(|m| m.api_path.clone()).collect();
        let spread_api_paths: Vec<Vec<String>> = metas
            .iter()
            .filter(|m| m.argument_mode == ArgumentMode::Spread)
            .map(|m| m.api_path.clone())
            .collect();
        let language_bindings = self.language.project_bindings(&metas);

        let execution = RunnerExecution {
            program: Program {
                format: self.language.identity().program_format.to_string(),
                source,
                repl: Some(runner_op),
            },
            inputs,
            bindings: Bindings {
                api_paths,
                spread_api_paths,
                language_bindings,
            },
        };

        if self.executor.is_none() {
            let command = match self.language.runner_command() {
                Ok(p) => p,
                Err(msg) => {
                    return self.error_reply(submission_id, "RunnerLaunchError", &msg, false)
                }
            };
            self.executor = Some(RunnerExecutor::new(command));
            self.catalog_version = Some(current_version);
            self.sync_meta();
        }

        let ctx = CallContext {
            cwd: self.cwd.clone(),
            session_id: self.session_id.clone(),
            language_id: self.language.identity().id.to_string(),
            worker_driver: self.worker_driver.clone(),
            disabled_namespaces: self.disabled_namespaces.clone(),
            file_coverage: Arc::clone(&self.file_coverage),
            catalog: Arc::clone(&self.catalog),
            extra_capabilities,
            language: Arc::clone(&self.language),
            jobs: Arc::clone(&self.jobs),
            comms: Arc::clone(&self.comms),
        };

        let started = Instant::now();
        let exec_res = self.executor.as_mut().unwrap().execute(execution, ctx).await;
        self.sync_meta();
        let duration_ms = started.elapsed().as_millis() as u64;

        match exec_res {
            Ok(output) => {
                if let Some(code) = eval_source {
                    let bytes = code.len();
                    self.remember(Submission {
                        id: submission_id.unwrap(),
                        source: code,
                        source_bytes: bytes,
                        status: "ok",
                        duration_ms,
                    });
                }
                let value = if let Some(name) = describe_name {
                    self.language.post_describe(&name, output.value, &metas)
                } else {
                    output.value
                };
                self.format_ok_reply(submission_id, value, &output.console)
            }
            Err(ExecutorError::Guest {
                name,
                message,
                console,
            }) => {
                let is_eval = eval_source.is_some();
                if let Some(code) = eval_source {
                    let bytes = code.len();
                    self.remember(Submission {
                        id: submission_id.unwrap(),
                        source: code,
                        source_bytes: bytes,
                        status: "error",
                        duration_ms,
                    });
                }
                let mut leading = vec![self.header(submission_id, "error")];
                if is_eval {
                    leading.push(
                        "Workspace retained; partial state and external effects may remain. Inspect before retrying.".into(),
                    );
                }
                leading.push(format!("{name}: {message}"));
                let text = assemble_reply(
                    &leading,
                    &format_console(&console, self.language.identity().console_prefix),
                );
                ReplToolResponse {
                    text,
                    is_error: true,
                    workspace_id: self.workspace_id.clone(),
                    generation: self.generation,
                    submission_id,
                    status: "error".into(),
                    raw_value: None,
                }
            }
            Err(ExecutorError::Runtime {
                name,
                message,
                console,
            }) => {
                let (from, to) = self.drop_executor().await;
                let leading = vec![
                    format!(
                        "repl · {} · g{}{} · lost",
                        self.workspace_id,
                        to,
                        submission_id.map(|id| format!("/#{id}")).unwrap_or_default()
                    ),
                    format!(
                        "Workspace restarted: g{from} → g{to} (runtime_failure). Retained bindings and history were discarded."
                    ),
                    format!("{name}: {message}"),
                ];
                let text = assemble_reply(
                    &leading,
                    &format_console(&console, self.language.identity().console_prefix),
                );
                ReplToolResponse {
                    text,
                    is_error: true,
                    workspace_id: self.workspace_id.clone(),
                    generation: to,
                    submission_id,
                    status: "lost".into(),
                    raw_value: None,
                }
            }
        }
    }

    fn header(&self, submission_id: Option<u64>, status: &str) -> String {
        let sub = submission_id.map(|id| format!("/#{id}")).unwrap_or_default();
        format!(
            "repl · {} · g{}{sub} · {status}",
            self.workspace_id, self.generation
        )
    }

    fn error_reply(
        &self,
        submission_id: Option<u64>,
        name: &str,
        message: &str,
        state_may_have_changed: bool,
    ) -> ReplToolResponse {
        let mut leading = vec![self.header(submission_id, "error")];
        if state_may_have_changed {
            leading.push(
                "Workspace retained; partial state and external effects may remain. Inspect before retrying.".into(),
            );
        }
        leading.push(format!("{name}: {message}"));
        ReplToolResponse {
            text: leading.join("\n\n"),
            is_error: true,
            workspace_id: self.workspace_id.clone(),
            generation: self.generation,
            submission_id,
            status: "error".into(),
            raw_value: None,
        }
    }

    fn format_ok_reply(
        &self,
        submission_id: Option<u64>,
        value: Value,
        console: &[ConsoleEntry],
    ) -> ReplToolResponse {
        let result_kind = self.language.identity().result_kind;
        let mut leading = vec![self.header(submission_id, "ok")];
        let mut parts: Vec<String> = Vec::new();

        if submission_id.is_some()
            && value.get("kind").and_then(Value::as_str) == Some(result_kind)
            && value.get("preview").and_then(Value::as_str).is_some()
        {
            if value.get("truncated").and_then(Value::as_bool) == Some(true) {
                leading.push(
                    "Value preview truncated; retain the value in a definition and inspect smaller parts.".into(),
                );
            }
            let preview = value.get("preview").and_then(Value::as_str).unwrap();
            parts.push(format!("⇒ {preview}"));
        } else if !value.is_null() {
            parts.push(self.language.format_metadata(&value));
        }

        parts.extend(format_console(
            console,
            self.language.identity().console_prefix,
        ));
        let text = assemble_reply(&leading, &parts);
        ReplToolResponse {
            text,
            is_error: false,
            workspace_id: self.workspace_id.clone(),
            generation: self.generation,
            submission_id,
            status: "ok".into(),
            raw_value: Some(value),
        }
    }
}

fn format_console(entries: &[ConsoleEntry], prefix: &str) -> Vec<String> {
    entries
        .iter()
        .map(|entry| {
            let label = match entry.level {
                ConsoleLevel::Log => prefix.to_string(),
                ConsoleLevel::Warn => format!("{prefix}.warn"),
                ConsoleLevel::Error => format!("{prefix}.error"),
            };
            let body = entry
                .values
                .iter()
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => serde_json::to_string(other).unwrap_or_default(),
                })
                .collect::<Vec<_>>()
                .join(" ");
            format!("{label}: {body}")
        })
        .collect()
}

fn truncate_utf8(text: &str, max_bytes: usize) -> (&str, bool) {
    if text.len() <= max_bytes {
        return (text, false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

fn assemble_reply(leading: &[String], parts: &[String]) -> String {
    let prefix = leading.join("\n\n");
    if parts.is_empty() {
        return prefix;
    }
    let body = parts.join("\n\n");
    let suffix = "\n[REPL output truncated; return smaller values or paginate inspection/history.]";
    let byte_budget = MAX_REPLY_BYTES
        .saturating_sub(prefix.len())
        .saturating_sub(suffix.len())
        .saturating_sub(2);
    let line_budget = MAX_REPLY_LINES.saturating_sub(prefix.lines().count() + 3);

    let mut out = String::new();
    let mut truncated = false;
    for (idx, line) in body.split('\n').enumerate() {
        if idx >= line_budget {
            truncated = true;
            break;
        }
        let extra = if idx == 0 { line.len() } else { line.len() + 1 };
        if out.len() + extra > byte_budget {
            let rem = byte_budget.saturating_sub(out.len() + usize::from(idx > 0));
            if rem > 0 {
                if idx > 0 {
                    out.push('\n');
                }
                let (slice, _) = truncate_utf8(line, rem);
                out.push_str(slice);
            }
            truncated = true;
            break;
        }
        if idx > 0 {
            out.push('\n');
        }
        out.push_str(line);
    }

    if truncated {
        format!("{prefix}\n\n{out}{suffix}")
    } else if out.is_empty() {
        prefix
    } else {
        format!("{prefix}\n\n{out}")
    }
}

struct WorkspaceSlot {
    meta: Arc<WorkspaceMeta>,
    workspace: Arc<Mutex<ReplWorkspace>>,
}

/// Manages isolated `ReplWorkspace` instances keyed by `(session_id, cwd, language)`.
pub struct ReplManager {
    languages: Arc<LanguageRegistry>,
    catalog: Arc<CapabilityCatalog>,
    adapters: Arc<AdapterHub>,
    jobs: Arc<JobManager>,
    comms: Arc<CommsHub>,
    workspaces: Mutex<HashMap<(String, String, String), WorkspaceSlot>>,
}

impl ReplManager {
    pub fn new(
        languages: Arc<LanguageRegistry>,
        catalog: Arc<CapabilityCatalog>,
        adapters: Arc<AdapterHub>,
        jobs: Arc<JobManager>,
    ) -> Self {
        Self {
            languages,
            catalog,
            adapters,
            jobs,
            comms: Arc::new(CommsHub::new()),
            workspaces: Mutex::new(HashMap::new()),
        }
    }

    pub fn adapters(&self) -> &Arc<AdapterHub> {
        &self.adapters
    }

    pub fn languages(&self) -> &LanguageRegistry {
        &self.languages
    }

    pub fn catalog(&self) -> &CapabilityCatalog {
        &self.catalog
    }

    pub fn jobs(&self) -> &Arc<JobManager> {
        &self.jobs
    }

    pub fn comms(&self) -> &Arc<CommsHub> {
        &self.comms
    }

    pub async fn run(
        &self,
        key: WorkspaceKey,
        request: ReplRequest,
    ) -> Result<ReplToolResponse, String> {
        let lang = self
            .languages
            .get(&key.language)
            .ok_or_else(|| format!("Unsupported language: {}", key.language))?;
        let sid = key.session_id.clone();
        let cwd_path = PathBuf::from(&key.cwd);
        let harness = crate::comms::infer_harness_with_driver(&sid, &key.worker_driver);
        let map_key = (key.session_id.clone(), key.cwd.clone(), key.language.clone());
        let workspace = {
            let mut guard = self.workspaces.lock().await;
            guard
                .entry(map_key)
                .or_insert_with(|| {
                    let ws = ReplWorkspace::new(
                        key.session_id,
                        PathBuf::from(key.cwd),
                        key.disabled_namespaces,
                        key.worker_driver,
                        lang,
                        Arc::clone(&self.catalog),
                        Arc::clone(&self.adapters),
                        Arc::clone(&self.jobs),
                        Arc::clone(&self.comms),
                    );
                    let meta = ws.meta();
                    WorkspaceSlot {
                        meta,
                        workspace: Arc::new(Mutex::new(ws)),
                    }
                })
                .workspace
                .clone()
        };
        let is_close = matches!(request, ReplRequest::Close);
        if !is_close {
            self.comms.touch_peer(&sid, &cwd_path, harness);
        }
        let _active_guard = (!is_close).then(|| self.comms.acquire_active(&sid));
        let mut ws = workspace.lock().await;
        let resp = ws.run(request).await;
        if is_close {
            self.comms.remove_peer(&sid);
        }
        Ok(resp)
    }

    #[cfg(test)]
    async fn workspace_mutex_for_test(&self, key: &WorkspaceKey) -> Option<Arc<Mutex<ReplWorkspace>>> {
        let map_key = (key.session_id.clone(), key.cwd.clone(), key.language.clone());
        self.workspaces
            .lock()
            .await
            .get(&map_key)
            .map(|slot| Arc::clone(&slot.workspace))
    }

    pub async fn list_workspaces(&self) -> Vec<WorkspaceSummary> {
        let guard = self.workspaces.lock().await;
        guard.values().map(|slot| slot.meta.snapshot()).collect()
    }

    pub async fn tool_info(
        &self,
        language_id: &str,
        disabled_namespaces: &[String],
        session_id: Option<&str>,
    ) -> Result<(String, Value), String> {
        let lang = self
            .languages
            .get(language_id)
            .ok_or_else(|| format!("Unsupported language: {language_id}"))?;
        let mut metas = self.catalog.metas_filtered(disabled_namespaces);
        if let Some(sid) = session_id {
            for cap in self.adapters.capabilities_for_session(sid) {
                let m = cap.meta();
                if !is_path_disabled(&m.api_path, disabled_namespaces) {
                    metas.push(m.clone());
                }
            }
        }
        let description = lang.tool_description(&metas);
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["op"],
            "properties": {
                "op": {
                    "type": "string",
                    "enum": ["eval", "inspect", "search", "describe", "history", "reset", "close"],
                    "description": "REPL operation to perform."
                },
                "code": {
                    "type": "string",
                    "maxLength": MAX_SOURCE_BYTES,
                    "description": lang.source_description()
                },
                "inputs": {
                    "type": "object",
                    "additionalProperties": { "type": "string" },
                    "description": lang.inputs_description()
                },
                "name": {
                    "type": "string",
                    "maxLength": 1024,
                    "description": "Binding name for inspect; required for describe."
                },
                "query": {
                    "type": "string",
                    "maxLength": 4096,
                    "description": "Search text."
                },
                "origin": {
                    "type": "string",
                    "enum": ["user", "all"],
                    "description": "Filter inspect/search bindings by origin."
                },
                "id": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "History submission id."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 50
                },
                "sessionId": {
                    "type": "string",
                    "description": "Optional workspace session ID (injected automatically by hook)."
                },
                "cwd": {
                    "type": "string",
                    "description": "Optional working directory (injected automatically by hook)."
                },
                "language": {
                    "type": "string",
                    "description": "Optional guest language id (default: steel)."
                }
            }
        });
        Ok((description, schema))
    }

    pub async fn shutdown(&self) {
        self.jobs.shutdown_all();
        let list: Vec<Arc<Mutex<ReplWorkspace>>> = {
            let mut guard = self.workspaces.lock().await;
            guard.drain().map(|(_, v)| v.workspace).collect()
        };
        for ws in list {
            let mut guard = ws.lock().await;
            let _ = guard.drop_executor().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::driver::WorkerDriverRegistry;
    use crate::language::LanguageIdentity;
    use crate::runner_process::RunnerCommand;
    use std::time::Duration;

    struct StubLang;
    impl GuestLanguage for StubLang {
        fn identity(&self) -> LanguageIdentity {
            LanguageIdentity {
                id: "steel",
                language: "Scheme",
                engine: "steel",
                program_format: "scheme",
                result_kind: "value",
                console_prefix: "",
            }
        }
        fn runner_command(&self) -> Result<RunnerCommand, String> {
            Err("stub".into())
        }
        fn project_bindings(&self, _: &[crate::capability::CapabilityMeta]) -> Value {
            json!([])
        }
        fn binding_name(
            &self,
            _: &[crate::capability::CapabilityMeta],
            m: &crate::capability::CapabilityMeta,
        ) -> String {
            m.api_path.join("/")
        }
        fn signature(
            &self,
            _: &[crate::capability::CapabilityMeta],
            m: &crate::capability::CapabilityMeta,
        ) -> String {
            format!("({})", m.api_path.join("/"))
        }
        fn tool_description(&self, _: &[crate::capability::CapabilityMeta]) -> String {
            "stub".into()
        }
        fn source_description(&self) -> &'static str {
            "code"
        }
        fn inputs_description(&self) -> &'static str {
            "inputs"
        }
        fn format_metadata(&self, v: &Value) -> String {
            v.to_string()
        }
        fn post_describe(
            &self,
            _: &str,
            v: Value,
            _: &[crate::capability::CapabilityMeta],
        ) -> Value {
            v
        }
    }

    fn make_manager() -> ReplManager {
        let mut langs = LanguageRegistry::new();
        langs.register(Arc::new(StubLang));
        let jobs = Arc::new(JobManager::new(Arc::new(WorkerDriverRegistry::new())));
        let catalog = Arc::new(CapabilityCatalog::new());
        let adapters = Arc::new(AdapterHub::new());
        ReplManager::new(Arc::new(langs), catalog, adapters, jobs)
    }

    #[tokio::test]
    async fn list_workspaces_does_not_block_while_workspace_is_running() {
        let mgr = make_manager();
        let key = WorkspaceKey {
            session_id: "sess-busy".into(),
            cwd: "/tmp/repo".into(),
            language: "steel".into(),
            disabled_namespaces: vec![],
            worker_driver: "agy".into(),
        };
        let _ = mgr
            .run(
                key.clone(),
                ReplRequest::History {
                    id: None,
                    offset: None,
                    limit: None,
                },
            )
            .await
            .unwrap();

        // Simulate an in-flight eval holding the workspace mutex.
        let ws_mutex = mgr.workspace_mutex_for_test(&key).await.unwrap();
        let _held = ws_mutex.lock().await;

        let summaries = tokio::time::timeout(Duration::from_millis(150), mgr.list_workspaces())
            .await
            .expect("list_workspaces deadlocked while workspace mutex was held");
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].session_id, "sess-busy");
        assert_eq!(summaries[0].cwd, "/tmp/repo");
        assert_eq!(summaries[0].generation, 1);

        drop(_held);
        let _ = mgr.run(key, ReplRequest::Reset).await.unwrap();
        let after_reset = mgr.list_workspaces().await;
        assert_eq!(after_reset[0].generation, 2);
        assert!(!after_reset[0].active);
    }
}
