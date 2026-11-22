//! Delegated fresh-context workers (`delegate/*`) and structured agent DAGs (`agents/*`),
//! powered by the pluggable `WorkerDriver` (default: `agy --print` on your Google subscription).

use crate::capability::{
    ArgumentMode, CallContext, Capability, CapabilityCatalog, CapabilityError, CapabilityMeta,
};
use crate::jobs::driver::WorkerSpec;
use crate::jobs::{
    truncate_head_utf8, truncate_tail_utf8, JobManager, NotifyMode, MAX_OUTPUT_REPLY_BYTES,
};
use async_trait::async_trait;
use cant_protocol::daemon::{CompletionNotice, JobSummary};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const MAX_OUTSTANDING_DELEGATES: usize = 32;
const MAX_FOREGROUND_BATCH: usize = 4;
const MAX_FLOW_NODES: usize = 8;
const DEFAULT_FOREGROUND_TIMEOUT_SECS: u64 = 40;
const MAX_FOREGROUND_TIMEOUT_SECS: u64 = 45;
const MAX_MONITOR_BYTES: usize = 50 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl WorkerStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

pub struct DelegateJob {
    pub id: String,
    pub session_id: String,
    pub name: String,
    pub task: String,
    pub notify: NotifyMode,
    pub status: WorkerStatus,
    pub output: Option<String>,
    pub error: Option<String>,
    pub usage: Option<Value>,
    pub monitor: String,
    pub started_at: Instant,
    pub finished_ms: Option<u64>,
    pub cancel_token: CancellationToken,
    pub cancelled_explicitly: bool,
}

impl DelegateJob {
    pub fn status_str(&self) -> &'static str {
        self.status.as_str()
    }
    pub fn elapsed_ms(&self) -> u64 {
        self.finished_ms
            .unwrap_or_else(|| self.started_at.elapsed().as_millis() as u64)
    }
    pub fn push_monitor(&mut self, chunk: &str) {
        self.monitor.push_str(chunk);
        if self.monitor.len() > MAX_MONITOR_BYTES {
            self.monitor = truncate_tail_utf8(&self.monitor, MAX_MONITOR_BYTES);
        }
    }
    pub fn tail(&self, max_bytes: usize) -> String {
        if let Some(ref out) = self.output {
            truncate_tail_utf8(out, max_bytes)
        } else {
            truncate_tail_utf8(&self.monitor, max_bytes)
        }
    }
    pub fn summary(&self) -> JobSummary {
        JobSummary {
            id: self.id.clone(),
            kind: "delegate".into(),
            name: self.name.clone(),
            session_id: self.session_id.clone(),
            status: self.status_str().into(),
            elapsed_ms: self.elapsed_ms(),
            summary: self.task.chars().take(120).collect(),
        }
    }
}

pub fn cancel_delegate_job(job: &Arc<Mutex<DelegateJob>>) {
    let mut g = job.lock().unwrap();
    g.cancelled_explicitly = true;
    if !g.status.is_terminal() {
        g.status = WorkerStatus::Cancelled;
        g.cancel_token.cancel();
    }
}

#[derive(Debug, Clone)]
pub struct FlowNodeState {
    pub id: String,
    pub name: String,
    pub status: WorkerStatus,
    pub completion: Option<Value>,
    pub error: Option<String>,
    pub monitor: String,
}

pub struct AgentFlowJob {
    pub id: String,
    pub session_id: String,
    pub name: String,
    pub notify: NotifyMode,
    pub status: WorkerStatus,
    pub completion_token: String,
    pub nodes: HashMap<String, FlowNodeState>,
    pub node_order: Vec<String>,
    pub output_value: Option<Value>,
    pub error: Option<String>,
    pub started_at: Instant,
    pub finished_ms: Option<u64>,
    pub cancel_token: CancellationToken,
    pub cancelled_explicitly: bool,
}

impl AgentFlowJob {
    pub fn status_str(&self) -> &'static str {
        self.status.as_str()
    }
    pub fn elapsed_ms(&self) -> u64 {
        self.finished_ms
            .unwrap_or_else(|| self.started_at.elapsed().as_millis() as u64)
    }
    pub fn tail(&self, max_bytes: usize) -> String {
        let mut out = String::new();
        for id in &self.node_order {
            if let Some(node) = self.nodes.get(id) {
                out.push_str(&format!(
                    "--- node '{}' ({}) ---\n",
                    node.id,
                    node.status.as_str()
                ));
                if !node.monitor.is_empty() {
                    out.push_str(&node.monitor);
                    if !node.monitor.ends_with('\n') {
                        out.push('\n');
                    }
                }
                if let Some(ref err) = node.error {
                    out.push_str(&format!("ERROR: {err}\n"));
                }
            }
        }
        truncate_tail_utf8(&out, max_bytes)
    }
    pub fn summary(&self) -> JobSummary {
        let done = self
            .nodes
            .values()
            .filter(|n| n.status == WorkerStatus::Completed)
            .count();
        JobSummary {
            id: self.id.clone(),
            kind: "flow".into(),
            name: self.name.clone(),
            session_id: self.session_id.clone(),
            status: self.status_str().into(),
            elapsed_ms: self.elapsed_ms(),
            summary: format!("{done}/{} nodes completed", self.node_order.len()),
        }
    }
}

pub fn cancel_flow_job(job: &Arc<Mutex<AgentFlowJob>>) {
    let mut g = job.lock().unwrap();
    g.cancelled_explicitly = true;
    if !g.status.is_terminal() {
        g.status = WorkerStatus::Cancelled;
        g.cancel_token.cancel();
    }
}

// --- DAG reference helpers ($from / $artifactFrom) ---

fn collect_refs(val: &Value, deps: &mut HashSet<String>) {
    match val {
        Value::Object(map) => {
            if let Some(from) = map.get("$from").and_then(Value::as_str) {
                deps.insert(from.to_string());
            }
            if let Some(from) = map.get("$artifactFrom").and_then(Value::as_str) {
                deps.insert(from.to_string());
            }
            for v in map.values() {
                collect_refs(v, deps);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                collect_refs(v, deps);
            }
        }
        _ => {}
    }
}

fn resolve_refs(val: &Value, completions: &HashMap<String, Value>) -> Result<Value, String> {
    match val {
        Value::Object(map) => {
            if let Some(node_id) = map.get("$from").and_then(Value::as_str) {
                let pointer = map.get("pointer").and_then(Value::as_str).unwrap_or("");
                let root = completions
                    .get(node_id)
                    .ok_or_else(|| format!("Referenced node '{node_id}' has no completion."))?;
                let resolved = if pointer.is_empty() || pointer == "/" {
                    root.clone()
                } else {
                    root.pointer(pointer).cloned().ok_or_else(|| {
                        format!("Pointer '{pointer}' not found on node '{node_id}'.")
                    })?
                };
                return Ok(resolved);
            }
            if let Some(node_id) = map.get("$artifactFrom").and_then(Value::as_str) {
                let name = map.get("name").and_then(Value::as_str).unwrap_or("");
                let root = completions.get(node_id).ok_or_else(|| {
                    format!("Referenced artifact node '{node_id}' has no completion.")
                })?;
                return Ok(json!({
                    "fromNode": node_id,
                    "artifact": name,
                    "completion": root,
                }));
            }
            let mut out = Map::new();
            for (k, v) in map {
                out.insert(k.clone(), resolve_refs(v, completions)?);
            }
            Ok(Value::Object(out))
        }
        Value::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for v in arr {
                out.push(resolve_refs(v, completions)?);
            }
            Ok(Value::Array(out))
        }
        other => Ok(other.clone()),
    }
}

#[derive(Debug, Clone)]
struct FlowNodeDef {
    id: String,
    name: String,
    task: String,
    input: Option<Value>,
    capabilities: Option<Vec<String>>,
    driver: String,
    model: Option<String>,
    timeout_secs: Option<u64>,
    output_schema: Option<Value>,
    deps: HashSet<String>,
}

fn parse_capabilities_opt(val: Option<&Value>) -> Option<Vec<String>> {
    val.and_then(Value::as_array).map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str().map(ToString::to_string))
            .collect()
    })
}

fn worker_driver<'a>(args: &'a Value, default: &'a str) -> Result<&'a str, CapabilityError> {
    match args.get("driver") {
        None => Ok(default),
        Some(value) => value
            .as_str()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                CapabilityError::invalid_args("driver", "Expected a nonempty worker driver ID.")
            }),
    }
}

fn parse_node_def(
    raw: &Value,
    default_id: Option<String>,
    default_driver: &str,
) -> Result<FlowNodeDef, CapabilityError> {
    let id = raw
        .get("id")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .or(default_id)
        .ok_or_else(|| CapabilityError::invalid_args("agents", "Node missing 'id'."))?;
    let task = raw
        .get("task")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            CapabilityError::invalid_args("agents", format!("Node '{id}' missing 'task'."))
        })?
        .to_string();
    let name = raw
        .get("name")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .unwrap_or_else(|| id.clone());
    let input = raw.get("input").cloned();
    let mut deps = HashSet::new();
    if let Some(ref inp) = input {
        collect_refs(inp, &mut deps);
    }
    if let Some(needs) = raw.get("needs").and_then(Value::as_array) {
        for n in needs {
            if let Some(s) = n.as_str() {
                deps.insert(s.to_string());
            }
        }
    }
    Ok(FlowNodeDef {
        id,
        name,
        task,
        input,
        driver: worker_driver(raw, default_driver)?.to_owned(),
        capabilities: parse_capabilities_opt(raw.get("capabilities")),
        model: raw
            .get("model")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        timeout_secs: raw.get("timeoutSec").and_then(Value::as_u64),
        output_schema: raw.get("outputSchema").cloned(),
        deps,
    })
}

fn validate_dag(nodes: &[FlowNodeDef]) -> Result<(), CapabilityError> {
    if nodes.is_empty() || nodes.len() > MAX_FLOW_NODES {
        return Err(CapabilityError::invalid_args(
            "agents.startFlow",
            format!("Flow must contain 1..={MAX_FLOW_NODES} nodes."),
        ));
    }
    let mut ids = HashSet::new();
    for n in nodes {
        if !ids.insert(n.id.clone()) {
            return Err(CapabilityError::invalid_args(
                "agents.startFlow",
                format!("Duplicate node id '{}'.", n.id),
            ));
        }
    }
    for n in nodes {
        for dep in &n.deps {
            if !ids.contains(dep) {
                return Err(CapabilityError::invalid_args(
                    "agents.startFlow",
                    format!("Node '{}' depends on unknown node '{dep}'.", n.id),
                ));
            }
        }
    }
    // Kahn cycle check
    let mut remaining: HashMap<&str, HashSet<&str>> = nodes
        .iter()
        .map(|n| (n.id.as_str(), n.deps.iter().map(String::as_str).collect()))
        .collect();
    while !remaining.is_empty() {
        let ready: Vec<&str> = remaining
            .iter()
            .filter(|(_, d)| d.is_empty())
            .map(|(k, _)| *k)
            .collect();
        if ready.is_empty() {
            return Err(CapabilityError::invalid_args(
                "agents.startFlow",
                "Flow contains a dependency cycle.",
            ));
        }
        for r in &ready {
            remaining.remove(r);
        }
        for d in remaining.values_mut() {
            for r in &ready {
                d.remove(r);
            }
        }
    }
    Ok(())
}

fn resolve_worker_cwd(base: &Path, supplied: Option<&str>) -> PathBuf {
    match supplied {
        Some(p) if Path::new(p).is_absolute() => PathBuf::from(p),
        Some(p) => base.join(p),
        None => base.to_path_buf(),
    }
}

async fn execute_worker_with_pool(
    jobs: &Arc<JobManager>,
    driver_id: &str,
    spec: WorkerSpec,
    on_progress: crate::jobs::driver::ProgressCallback,
    cancel: CancellationToken,
) -> Result<crate::jobs::driver::WorkerOutcome, String> {
    let driver = jobs
        .drivers
        .get(driver_id)
        .ok_or_else(|| format!("Unknown worker driver '{driver_id}'."))?;
    run_with_permit(jobs, driver, spec, on_progress, cancel).await
}

async fn run_with_permit(
    jobs: &JobManager,
    driver: Arc<dyn crate::jobs::driver::WorkerDriver>,
    spec: WorkerSpec,
    on_progress: crate::jobs::driver::ProgressCallback,
    cancel: CancellationToken,
) -> Result<crate::jobs::driver::WorkerOutcome, String> {
    let _permit = tokio::select! {
        res = jobs.worker_pool.clone().acquire_owned() => {
            res.map_err(|_| "Worker pool closed.".to_string())?
        }
        _ = cancel.cancelled() => {
            return Err("Worker cancelled while queued.".into());
        }
    };
    driver.run(spec, on_progress, cancel).await
}

// --- delegate/* capabilities ---

struct DelegateStart(CapabilityMeta);
#[async_trait]
impl Capability for DelegateStart {
    fn meta(&self) -> &CapabilityMeta {
        &self.0
    }
    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let task = args
            .get("task")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| CapabilityError::invalid_args("delegate.start", "Missing 'task'."))?
            .to_string();
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .unwrap_or_else(|| "delegate".into());
        let cwd = resolve_worker_cwd(&ctx.cwd, args.get("cwd").and_then(Value::as_str));
        let driver_id = worker_driver(&args, &ctx.worker_driver)?.to_owned();
        let model = args
            .get("model")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        let timeout_secs = args.get("timeoutSec").and_then(Value::as_u64);
        let capabilities = parse_capabilities_opt(args.get("capabilities"));
        let notify = NotifyMode::parse(args.get("notify").and_then(Value::as_str))
            .map_err(|e| CapabilityError::invalid_args("delegate.start", e))?;
        let channel_arg = crate::comms::ChannelArg::parse(args.get("channel"))
            .map_err(|e| CapabilityError::invalid_args("delegate.start", e))?;

        {
            let map = ctx.jobs.delegates.lock().unwrap();
            let outstanding = map
                .values()
                .filter(|j| !j.lock().unwrap().status.is_terminal())
                .count();
            if outstanding >= MAX_OUTSTANDING_DELEGATES {
                return Err(CapabilityError::exec(
                    "JobLimitError",
                    format!(
                        "At most {MAX_OUTSTANDING_DELEGATES} delegate jobs may be outstanding."
                    ),
                ));
            }
        }

        let id = format!("bgdel_{}", &Uuid::new_v4().simple().to_string()[..12]);
        let cancel_token = CancellationToken::new();
        let job = Arc::new(Mutex::new(DelegateJob {
            id: id.clone(),
            session_id: ctx.session_id.clone(),
            name: name.clone(),
            task: task.clone(),
            notify,
            status: WorkerStatus::Queued,
            output: None,
            error: None,
            usage: None,
            monitor: String::new(),
            started_at: Instant::now(),
            finished_ms: None,
            cancel_token: cancel_token.clone(),
            cancelled_explicitly: false,
        }));

        ctx.jobs
            .delegates
            .lock()
            .unwrap()
            .insert(id.clone(), Arc::clone(&job));

        let jobs = Arc::clone(&ctx.jobs);
        let session_id = ctx.session_id.clone();
        let (worker_guard, worker_sid, worker_channels, effective_task, channel_meta) =
            if let Some(ref ch_arg) = channel_arg {
                let wsid = format!("worker-{id}");
                let (guard, plan) = ctx
                    .comms
                    .attach_worker_to_channels(
                        &ctx.session_id,
                        &ctx.cwd,
                        &wsid,
                        &name,
                        &cwd,
                        ch_arg,
                        None,
                    )
                    .map_err(|e| CapabilityError::exec("CommsError", e))?;
                let full_task = format!("{task}\n\n{}", plan.prompt_block);
                (
                    Some(guard),
                    Some(wsid),
                    plan.channel_ids.clone(),
                    full_task,
                    Some((plan.primary_channel_id, plan.channel_ids, plan.peer_handle)),
                )
            } else {
                (None, None, Vec::new(), task, None)
            };
        let spec = WorkerSpec {
            name: name.clone(),
            task: effective_task,
            input: None,
            cwd,
            model,
            timeout_secs,
            output_schema: None,
            capabilities,
            session_id: worker_sid,
            channels: worker_channels,
        };

        ctx.jobs.emit_event(
            &ctx.session_id,
            cant_protocol::daemon::DaemonEvent::JobUpdated {
                job: job.lock().unwrap().summary(),
            },
        );
        let id_prog = id.clone();
        tokio::spawn(async move {
            let _worker_peer_guard = worker_guard;
            let job_prog = Arc::clone(&job);
            let jobs_prog = Arc::clone(&jobs);
            let sid_prog = session_id.clone();
            let on_progress: crate::jobs::driver::ProgressCallback = Arc::new(move |chunk| {
                let mut g = job_prog.lock().unwrap();
                if g.status == WorkerStatus::Queued {
                    g.status = WorkerStatus::Running;
                }
                g.push_monitor(&chunk);
                jobs_prog.emit_event(
                    &sid_prog,
                    cant_protocol::daemon::DaemonEvent::JobOutput {
                        id: id_prog.clone(),
                        chunk,
                    },
                );
            });

            {
                let mut g = job.lock().unwrap();
                if g.status == WorkerStatus::Queued {
                    g.status = WorkerStatus::Running;
                }
            }

            let res =
                execute_worker_with_pool(&jobs, &driver_id, spec, on_progress, cancel_token).await;

            let notice_opt = {
                let mut g = job.lock().unwrap();
                g.finished_ms = Some(g.started_at.elapsed().as_millis() as u64);
                match res {
                    Ok(outcome) if !g.cancelled_explicitly => {
                        g.status = WorkerStatus::Completed;
                        g.output = Some(outcome.text);
                        g.usage = outcome.usage;
                    }
                    Err(err) if !g.cancelled_explicitly => {
                        g.status = WorkerStatus::Failed;
                        g.error = Some(err);
                    }
                    _ => {
                        g.status = WorkerStatus::Cancelled;
                    }
                }
                if g.cancelled_explicitly || g.notify == NotifyMode::Off {
                    None
                } else {
                    let preview = g
                        .output
                        .as_deref()
                        .or(g.error.as_deref())
                        .unwrap_or("")
                        .chars()
                        .take(240)
                        .collect::<String>();
                    Some(CompletionNotice {
                        job_id: g.id.clone(),
                        kind: "delegate".into(),
                        name: g.name.clone(),
                        status: g.status_str().into(),
                        notify: g.notify.as_str().into(),
                        summary: format!(
                            "Delegate `{}` ({}) {} in {}ms — {}",
                            g.name,
                            g.id,
                            g.status_str(),
                            g.elapsed_ms(),
                            preview
                        ),
                        completion_token: None,
                    })
                }
            };

            let summary = job.lock().unwrap().summary();
            jobs.emit_event(
                &session_id,
                cant_protocol::daemon::DaemonEvent::JobUpdated { job: summary },
            );
            if let Some(n) = notice_opt {
                jobs.push_notice(session_id, n);
            }
        });

        let mut resp = json!({
            "id": id,
            "name": name,
            "status": "queued",
            "notify": notify.as_str(),
        });
        if let Some((primary, all_channels, peer_handle)) = channel_meta {
            if let Some(obj) = resp.as_object_mut() {
                obj.insert("channelId".into(), json!(primary));
                obj.insert("peer".into(), json!(peer_handle));
                if all_channels.len() > 1 {
                    obj.insert("channels".into(), json!(all_channels));
                }
            }
        }
        Ok(resp)
    }
}

struct DelegateResult(CapabilityMeta);
#[async_trait]
impl Capability for DelegateResult {
    fn meta(&self) -> &CapabilityMeta {
        &self.0
    }
    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let id = args
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| CapabilityError::invalid_args("delegate.result", "Missing 'id'."))?;
        let job = ctx
            .jobs
            .delegates
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| {
                CapabilityError::exec("JobNotFoundError", format!("No delegate job '{id}'."))
            })?;
        let g = job.lock().unwrap();
        if g.session_id != ctx.session_id {
            return Err(CapabilityError::exec(
                "JobOwnershipError",
                "Delegate job belongs to another session.",
            ));
        }
        if !g.status.is_terminal() {
            return Ok(json!({
                "id": g.id,
                "name": g.name,
                "status": "pending",
                "state": g.status_str(),
                "durationMs": g.elapsed_ms(),
            }));
        }
        let (output, truncated) = match &g.output {
            Some(text) => {
                let (t, tr) = truncate_head_utf8(text, MAX_OUTPUT_REPLY_BYTES);
                (Some(t), tr)
            }
            None => (None, false),
        };
        Ok(json!({
            "id": g.id,
            "name": g.name,
            "status": g.status_str(),
            "output": output,
            "outputTruncated": truncated,
            "error": g.error,
            "usage": g.usage,
            "durationMs": g.elapsed_ms(),
        }))
    }
}

struct DelegateList(CapabilityMeta);
#[async_trait]
impl Capability for DelegateList {
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
        let mut items: Vec<Value> = ctx
            .jobs
            .delegates
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
                    "name": g.name,
                    "status": g.status_str(),
                    "durationMs": g.elapsed_ms(),
                }))
            })
            .collect();
        items.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        let total = items.len();
        let page: Vec<Value> = items.into_iter().skip(offset).take(limit).collect();
        let has_more = offset + page.len() < total;
        Ok(json!({
            "items": page,
            "total": total,
            "hasMore": has_more,
            "nextOffset": if has_more { json!(offset + page.len()) } else { Value::Null },
        }))
    }
}

struct DelegateCancel(CapabilityMeta);
#[async_trait]
impl Capability for DelegateCancel {
    fn meta(&self) -> &CapabilityMeta {
        &self.0
    }
    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let id = args
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| CapabilityError::invalid_args("delegate.cancel", "Missing 'id'."))?;
        let job = ctx
            .jobs
            .delegates
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| {
                CapabilityError::exec("JobNotFoundError", format!("No delegate job '{id}'."))
            })?;
        {
            let g = job.lock().unwrap();
            if g.session_id != ctx.session_id {
                return Err(CapabilityError::exec(
                    "JobOwnershipError",
                    "Delegate job belongs to another session.",
                ));
            }
        }
        cancel_delegate_job(&job);
        let g = job.lock().unwrap();
        Ok(json!({ "id": g.id, "status": g.status_str() }))
    }
}

// --- agents/* capabilities ---

fn spawn_flow(
    ctx: &CallContext,
    name: String,
    nodes: Vec<FlowNodeDef>,
    output_ref: Option<Value>,
    notify: NotifyMode,
    channel_arg: Option<crate::comms::ChannelArg>,
) -> Result<Value, CapabilityError> {
    let id = format!("flow_{}", &Uuid::new_v4().simple().to_string()[..12]);
    let token = format!("ready_{}", &Uuid::new_v4().simple().to_string()[..16]);
    let cancel_token = CancellationToken::new();

    let node_order: Vec<String> = nodes.iter().map(|n| n.id.clone()).collect();
    let mut node_states = HashMap::new();
    for n in &nodes {
        node_states.insert(
            n.id.clone(),
            FlowNodeState {
                id: n.id.clone(),
                name: n.name.clone(),
                status: WorkerStatus::Queued,
                completion: None,
                error: None,
                monitor: String::new(),
            },
        );
    }

    let flow = Arc::new(Mutex::new(AgentFlowJob {
        id: id.clone(),
        session_id: ctx.session_id.clone(),
        name: name.clone(),
        notify,
        status: WorkerStatus::Running,
        completion_token: token.clone(),
        nodes: node_states,
        node_order: node_order.clone(),
        output_value: None,
        error: None,
        started_at: Instant::now(),
        finished_ms: None,
        cancel_token: cancel_token.clone(),
        cancelled_explicitly: false,
    }));

    ctx.jobs
        .flows
        .lock()
        .unwrap()
        .insert(id.clone(), Arc::clone(&flow));
    let summary = flow.lock().unwrap().summary();
    ctx.jobs.emit_event(
        &ctx.session_id,
        cant_protocol::daemon::DaemonEvent::JobUpdated { job: summary },
    );

    let shared_channels: Option<Vec<String>> = match channel_arg {
        Some(ref arg) => {
            let auto_room = format!("{name}-{}", &id[5..11]);
            let ids = ctx
                .comms
                .prepare_shared_channels(&ctx.session_id, &ctx.cwd, arg, &auto_room)
                .map_err(|e| CapabilityError::exec("CommsError", e))?;
            Some(ids)
        }
        None => None,
    };
    let primary_channel = shared_channels.as_ref().and_then(|v| v.first().cloned());
    let all_channels_ret = shared_channels.clone();
    let jobs = Arc::clone(&ctx.jobs);
    let comms = Arc::clone(&ctx.comms);
    let cwd = ctx.cwd.clone();
    let session_id = ctx.session_id.clone();
    let flow_id_for_workers = id.clone();

    tokio::spawn(async move {
        let mut pending: HashMap<String, FlowNodeDef> =
            nodes.into_iter().map(|n| (n.id.clone(), n)).collect();
        let mut completions: HashMap<String, Value> = HashMap::new();
        let mut in_flight: HashMap<String, tokio::task::JoinHandle<Result<Value, String>>> =
            HashMap::new();
        let mut flow_error: Option<String> = None;

        while (!pending.is_empty() || !in_flight.is_empty()) && flow_error.is_none() {
            if cancel_token.is_cancelled() {
                break;
            }
            // Launch all nodes whose dependencies are satisfied.
            let ready_ids: Vec<String> = pending
                .iter()
                .filter(|(_, def)| def.deps.iter().all(|d| completions.contains_key(d)))
                .map(|(k, _)| k.clone())
                .collect();

            for nid in ready_ids {
                let def = pending.remove(&nid).unwrap();
                let resolved_input = match &def.input {
                    Some(inp) => match resolve_refs(inp, &completions) {
                        Ok(v) => Some(v),
                        Err(e) => {
                            flow_error = Some(e);
                            break;
                        }
                    },
                    None => None,
                };
                if let Some(ns) = flow.lock().unwrap().nodes.get_mut(&nid) {
                    ns.status = WorkerStatus::Running;
                }
                let jobs_c = Arc::clone(&jobs);
                let comms_c = Arc::clone(&comms);
                let shared_ch_c = shared_channels.clone();
                let parent_sid_c = session_id.clone();
                let flow_id_c = flow_id_for_workers.clone();
                let driver_c = def.driver;
                let cwd_c = cwd.clone();
                let cancel_c = cancel_token.clone();
                let flow_c = Arc::clone(&flow);
                let nid_c = nid.clone();
                let nid_worker = nid.clone();

                let handle = tokio::spawn(async move {
                    let on_prog: crate::jobs::driver::ProgressCallback = Arc::new(move |chunk| {
                        if let Some(ns) = flow_c.lock().unwrap().nodes.get_mut(&nid_c) {
                            ns.monitor.push_str(&chunk);
                            if ns.monitor.len() > MAX_MONITOR_BYTES {
                                ns.monitor = truncate_tail_utf8(&ns.monitor, MAX_MONITOR_BYTES);
                            }
                        }
                    });
                    let (worker_guard, worker_sid, worker_channels, effective_task) =
                        if let Some(ref ch_ids) = shared_ch_c {
                            let wsid = format!("worker-{flow_id_c}-{nid_worker}");
                            let arg = crate::comms::ChannelArg::Refs(ch_ids.clone());
                            let (guard, plan) = comms_c.attach_worker_to_channels(
                                &parent_sid_c,
                                &cwd_c,
                                &wsid,
                                &def.name,
                                &cwd_c,
                                &arg,
                                Some(ch_ids),
                            )?;
                            let full_task = format!("{}\n\n{}", def.task, plan.prompt_block);
                            (Some(guard), Some(wsid), plan.channel_ids, full_task)
                        } else {
                            (None, None, Vec::new(), def.task)
                        };
                    let _worker_peer_guard = worker_guard;
                    let spec = WorkerSpec {
                        name: def.name,
                        task: effective_task,
                        input: resolved_input,
                        cwd: cwd_c,
                        model: def.model,
                        timeout_secs: def.timeout_secs,
                        output_schema: def.output_schema,
                        capabilities: def.capabilities,
                        session_id: worker_sid,
                        channels: worker_channels,
                    };
                    let outcome =
                        execute_worker_with_pool(&jobs_c, &driver_c, spec, on_prog, cancel_c)
                            .await?;
                    Ok(json!({
                        "completion": {
                            "text": outcome.text,
                            "data": outcome.data.unwrap_or(Value::Null),
                            "usage": outcome.usage.unwrap_or(Value::Null),
                        }
                    }))
                });
                in_flight.insert(nid, handle);
            }

            if flow_error.is_some() || in_flight.is_empty() {
                break;
            }

            // Wait for any in-flight node to complete.
            let mut finished_id = None;
            for (nid, handle) in &in_flight {
                if handle.is_finished() {
                    finished_id = Some(nid.clone());
                    break;
                }
            }
            if finished_id.is_none() {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                continue;
            }
            let nid = finished_id.unwrap();
            let handle = in_flight.remove(&nid).unwrap();
            match handle.await {
                Ok(Ok(comp)) => {
                    if let Some(ns) = flow.lock().unwrap().nodes.get_mut(&nid) {
                        ns.status = WorkerStatus::Completed;
                        ns.completion = Some(comp.clone());
                    }
                    completions.insert(nid, comp);
                }
                Ok(Err(e)) => {
                    if let Some(ns) = flow.lock().unwrap().nodes.get_mut(&nid) {
                        ns.status = WorkerStatus::Failed;
                        ns.error = Some(e.clone());
                    }
                    flow_error = Some(format!("Node '{nid}' failed: {e}"));
                    cancel_token.cancel();
                }
                Err(e) => {
                    flow_error = Some(format!("Node '{nid}' panicked: {e}"));
                    cancel_token.cancel();
                }
            }
        }

        // Abort any leftover in-flight tasks on failure/cancel
        for (_, handle) in in_flight {
            handle.abort();
        }

        let notice_opt = {
            let mut g = flow.lock().unwrap();
            g.finished_ms = Some(g.started_at.elapsed().as_millis() as u64);
            if g.cancelled_explicitly {
                g.status = WorkerStatus::Cancelled;
            } else if let Some(err) = flow_error {
                g.status = WorkerStatus::Failed;
                g.error = Some(err);
            } else {
                let output_val = match &output_ref {
                    Some(oref) => match resolve_refs(oref, &completions) {
                        Ok(v) => v,
                        Err(e) => {
                            g.status = WorkerStatus::Failed;
                            g.error = Some(e);
                            Value::Null
                        }
                    },
                    None => {
                        let mut m = Map::new();
                        for (k, v) in &completions {
                            m.insert(k.clone(), v.clone());
                        }
                        Value::Object(m)
                    }
                };
                if g.status != WorkerStatus::Failed {
                    g.status = WorkerStatus::Completed;
                    g.output_value = Some(output_val);
                }
            }

            if g.cancelled_explicitly || g.notify == NotifyMode::Off {
                None
            } else {
                Some(CompletionNotice {
                    job_id: g.id.clone(),
                    kind: "flow".into(),
                    name: g.name.clone(),
                    status: g.status_str().into(),
                    notify: g.notify.as_str().into(),
                    summary: format!(
                        "Flow `{}` ({}) {} in {}ms — collect with `(agents/collect \"{}\" #:completion-token \"{}\")`",
                        g.name,
                        g.id,
                        g.status_str(),
                        g.elapsed_ms(),
                        g.id,
                        g.completion_token
                    ),
                    completion_token: Some(g.completion_token.clone()),
                })
            }
        };

        let summary = flow.lock().unwrap().summary();
        jobs.emit_event(
            &session_id,
            cant_protocol::daemon::DaemonEvent::JobUpdated { job: summary },
        );
        if let Some(n) = notice_opt {
            jobs.push_notice(session_id, n);
        }
    });

    let mut resp = json!({
        "id": id,
        "status": "running",
        "nodes": node_order,
        "notify": notify.as_str(),
    });
    if let Some(primary) = primary_channel {
        if let Some(obj) = resp.as_object_mut() {
            obj.insert("channelId".into(), json!(primary));
            if let Some(all) = all_channels_ret.filter(|v| v.len() > 1) {
                obj.insert("channels".into(), json!(all));
            }
        }
    }
    Ok(resp)
}

struct AgentsRun(CapabilityMeta);
#[async_trait]
impl Capability for AgentsRun {
    fn meta(&self) -> &CapabilityMeta {
        &self.0
    }
    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let task = args
            .get("task")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| CapabilityError::invalid_args("agents.run", "Missing 'task'."))?
            .to_string();
        let timeout_secs = args
            .get("timeoutSec")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_FOREGROUND_TIMEOUT_SECS)
            .clamp(1, MAX_FOREGROUND_TIMEOUT_SECS);
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("agent")
            .to_string();
        let channel_arg = crate::comms::ChannelArg::parse(args.get("channel"))
            .map_err(|e| CapabilityError::invalid_args("agents.run", e))?;
        let (_worker_guard, worker_sid, worker_channels, effective_task) =
            if let Some(ref ch_arg) = channel_arg {
                let wsid = format!("worker-run-{}", &Uuid::new_v4().simple().to_string()[..10]);
                let (guard, plan) = ctx
                    .comms
                    .attach_worker_to_channels(
                        &ctx.session_id,
                        &ctx.cwd,
                        &wsid,
                        &name,
                        &ctx.cwd,
                        ch_arg,
                        None,
                    )
                    .map_err(|e| CapabilityError::exec("CommsError", e))?;
                let full = format!("{task}\n\n{}", plan.prompt_block);
                (Some(guard), Some(wsid), plan.channel_ids, full)
            } else {
                (None, None, Vec::new(), task)
            };
        let spec = WorkerSpec {
            name,
            task: effective_task,
            input: args.get("input").cloned(),
            cwd: ctx.cwd.clone(),
            model: args
                .get("model")
                .and_then(Value::as_str)
                .map(ToString::to_string),
            timeout_secs: Some(timeout_secs),
            output_schema: args.get("outputSchema").cloned(),
            capabilities: parse_capabilities_opt(args.get("capabilities")),
            session_id: worker_sid,
            channels: worker_channels,
        };
        let cancel = CancellationToken::new();
        let on_prog: crate::jobs::driver::ProgressCallback = Arc::new(|_| {});
        let outcome = execute_worker_with_pool(
            &ctx.jobs,
            worker_driver(&args, &ctx.worker_driver)?,
            spec,
            on_prog,
            cancel,
        )
        .await
        .map_err(|e| CapabilityError::exec("AgentRunError", e))?;
        Ok(json!({
            "text": outcome.text,
            "data": outcome.data,
            "usage": outcome.usage,
        }))
    }
}

struct AgentsRunMany(CapabilityMeta);
#[async_trait]
impl Capability for AgentsRunMany {
    fn meta(&self) -> &CapabilityMeta {
        &self.0
    }
    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let tasks = args.get("tasks").and_then(Value::as_array).ok_or_else(|| {
            CapabilityError::invalid_args("agents.runMany", "Missing 'tasks' array.")
        })?;
        if tasks.is_empty() || tasks.len() > MAX_FOREGROUND_BATCH {
            return Err(CapabilityError::invalid_args(
                "agents.runMany",
                format!("tasks must contain 1..={MAX_FOREGROUND_BATCH} items."),
            ));
        }
        // A rejected batch must not leave detached workers running.
        let mut batch = Vec::with_capacity(tasks.len());
        for (i, item) in tasks.iter().enumerate() {
            let def = parse_node_def(item, Some(format!("task_{}", i + 1)), &ctx.worker_driver)?;
            let driver = ctx.jobs.drivers.get(&def.driver).ok_or_else(|| {
                CapabilityError::invalid_args(
                    "agents.runMany",
                    format!("Unknown worker driver '{}'.", def.driver),
                )
            })?;
            batch.push((def, driver));
        }
        let channel_arg = crate::comms::ChannelArg::parse(args.get("channel"))
            .map_err(|e| CapabilityError::invalid_args("agents.runMany", e))?;
        let shared_channels = match channel_arg {
            Some(ref arg) => {
                let auto_room = format!("batch-{}", &Uuid::new_v4().simple().to_string()[..8]);
                Some(
                    ctx.comms
                        .prepare_shared_channels(&ctx.session_id, &ctx.cwd, arg, &auto_room)
                        .map_err(|e| CapabilityError::exec("CommsError", e))?,
                )
            }
            None => None,
        };
        let mut handles = Vec::with_capacity(batch.len());
        for (idx, (def, driver)) in batch.into_iter().enumerate() {
            let timeout_secs = def
                .timeout_secs
                .unwrap_or(DEFAULT_FOREGROUND_TIMEOUT_SECS)
                .clamp(1, MAX_FOREGROUND_TIMEOUT_SECS);
            let (worker_guard, worker_sid, worker_channels, effective_task) =
                if let Some(ref ch_ids) = shared_channels {
                    let wsid = format!(
                        "worker-rm-{idx}-{}",
                        &Uuid::new_v4().simple().to_string()[..8]
                    );
                    let arg = crate::comms::ChannelArg::Refs(ch_ids.clone());
                    let (guard, plan) = ctx
                        .comms
                        .attach_worker_to_channels(
                            &ctx.session_id,
                            &ctx.cwd,
                            &wsid,
                            &def.name,
                            &ctx.cwd,
                            &arg,
                            Some(ch_ids),
                        )
                        .map_err(|e| CapabilityError::exec("CommsError", e))?;
                    let full = format!("{}\n\n{}", def.task, plan.prompt_block);
                    (Some(guard), Some(wsid), plan.channel_ids, full)
                } else {
                    (None, None, Vec::new(), def.task)
                };
            let spec = WorkerSpec {
                name: def.name,
                task: effective_task,
                input: def.input,
                cwd: ctx.cwd.clone(),
                model: def.model,
                timeout_secs: Some(timeout_secs),
                output_schema: def.output_schema,
                capabilities: def.capabilities,
                session_id: worker_sid,
                channels: worker_channels,
            };
            let jobs = Arc::clone(&ctx.jobs);
            handles.push(tokio::spawn(async move {
                let _worker_peer_guard = worker_guard;
                let on_prog: crate::jobs::driver::ProgressCallback = Arc::new(|_| {});
                run_with_permit(&jobs, driver, spec, on_prog, CancellationToken::new()).await
            }));
        }
        let mut results = Vec::with_capacity(handles.len());
        for h in handles {
            let outcome = h
                .await
                .map_err(|e| CapabilityError::exec("AgentRunError", e.to_string()))?
                .map_err(|e| CapabilityError::exec("AgentRunError", e))?;
            results.push(json!({
                "text": outcome.text,
                "data": outcome.data,
                "usage": outcome.usage,
            }));
        }
        Ok(Value::Array(results))
    }
}

struct AgentsStart(CapabilityMeta);
#[async_trait]
impl Capability for AgentsStart {
    fn meta(&self) -> &CapabilityMeta {
        &self.0
    }
    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let notify = NotifyMode::parse(args.get("notify").and_then(Value::as_str))
            .map_err(|e| CapabilityError::invalid_args("agents.start", e))?;
        let def = parse_node_def(&args, Some("main".into()), &ctx.worker_driver)?;
        let name = def.name.clone();
        validate_dag(std::slice::from_ref(&def))?;
        let channel_arg = crate::comms::ChannelArg::parse(args.get("channel"))
            .map_err(|e| CapabilityError::invalid_args("agents.start", e))?;
        spawn_flow(
            ctx,
            name,
            vec![def],
            Some(json!({ "$from": "main", "pointer": "/completion" })),
            notify,
            channel_arg,
        )
    }
}

struct AgentsStartMany(CapabilityMeta);
#[async_trait]
impl Capability for AgentsStartMany {
    fn meta(&self) -> &CapabilityMeta {
        &self.0
    }
    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let notify = NotifyMode::parse(args.get("notify").and_then(Value::as_str))
            .map_err(|e| CapabilityError::invalid_args("agents.startMany", e))?;
        let tasks = args
            .get("tasks")
            .and_then(Value::as_array)
            .ok_or_else(|| CapabilityError::invalid_args("agents.startMany", "Missing 'tasks'."))?;
        let mut nodes = Vec::with_capacity(tasks.len());
        for (i, item) in tasks.iter().enumerate() {
            nodes.push(parse_node_def(
                item,
                Some(format!("task_{}", i + 1)),
                &ctx.worker_driver,
            )?);
        }
        validate_dag(&nodes)?;
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("batch")
            .to_string();
        let channel_arg = crate::comms::ChannelArg::parse(args.get("channel"))
            .map_err(|e| CapabilityError::invalid_args("agents.startMany", e))?;
        spawn_flow(ctx, name, nodes, None, notify, channel_arg)
    }
}

struct AgentsStartFlow(CapabilityMeta);
#[async_trait]
impl Capability for AgentsStartFlow {
    fn meta(&self) -> &CapabilityMeta {
        &self.0
    }
    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let notify = NotifyMode::parse(args.get("notify").and_then(Value::as_str))
            .map_err(|e| CapabilityError::invalid_args("agents.startFlow", e))?;
        let raw_nodes = args
            .get("nodes")
            .and_then(Value::as_array)
            .ok_or_else(|| CapabilityError::invalid_args("agents.startFlow", "Missing 'nodes'."))?;
        let mut nodes = Vec::with_capacity(raw_nodes.len());
        for item in raw_nodes {
            nodes.push(parse_node_def(item, None, &ctx.worker_driver)?);
        }
        validate_dag(&nodes)?;
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("flow")
            .to_string();
        let output_ref = args.get("output").cloned();
        let channel_arg = crate::comms::ChannelArg::parse(args.get("channel"))
            .map_err(|e| CapabilityError::invalid_args("agents.startFlow", e))?;
        spawn_flow(ctx, name, nodes, output_ref, notify, channel_arg)
    }
}

struct AgentsCollect(CapabilityMeta);
#[async_trait]
impl Capability for AgentsCollect {
    fn meta(&self) -> &CapabilityMeta {
        &self.0
    }
    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let id = args
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| CapabilityError::invalid_args("agents.collect", "Missing 'id'."))?;
        let token = args
            .get("completionToken")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                CapabilityError::invalid_args("agents.collect", "Missing 'completionToken'.")
            })?;
        let flow = ctx
            .jobs
            .flows
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| {
                CapabilityError::exec("JobNotFoundError", format!("No agent flow '{id}'."))
            })?;
        let g = flow.lock().unwrap();
        if g.session_id != ctx.session_id {
            return Err(CapabilityError::exec(
                "JobOwnershipError",
                "Agent flow belongs to another session.",
            ));
        }
        if !g.status.is_terminal() {
            return Err(CapabilityError::exec(
                "JobPendingError",
                "Agent flow has not completed yet; wait for its completion notification.",
            ));
        }
        if g.completion_token != token {
            return Err(CapabilityError::exec(
                "InvalidTokenError",
                "Invalid completionToken for this agent flow.",
            ));
        }
        Ok(json!({
            "id": g.id,
            "name": g.name,
            "status": g.status_str(),
            "output": g.output_value,
            "error": g.error,
            "durationMs": g.elapsed_ms(),
        }))
    }
}

struct AgentsCancel(CapabilityMeta);
#[async_trait]
impl Capability for AgentsCancel {
    fn meta(&self) -> &CapabilityMeta {
        &self.0
    }
    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let id = args
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| CapabilityError::invalid_args("agents.cancel", "Missing 'id'."))?;
        let flow = ctx
            .jobs
            .flows
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| {
                CapabilityError::exec("JobNotFoundError", format!("No agent flow '{id}'."))
            })?;
        {
            let g = flow.lock().unwrap();
            if g.session_id != ctx.session_id {
                return Err(CapabilityError::exec(
                    "JobOwnershipError",
                    "Agent flow belongs to another session.",
                ));
            }
        }
        cancel_flow_job(&flow);
        let g = flow.lock().unwrap();
        Ok(json!({ "id": g.id, "status": g.status_str() }))
    }
}

fn steel_hints(positional: &[&str], keywords: &[(&str, &str)]) -> Map<String, Value> {
    let mut inner = Map::new();
    if !positional.is_empty() {
        inner.insert("positional".into(), json!(positional));
    }
    if !keywords.is_empty() {
        let mut kw = Map::new();
        for (field, k) in keywords {
            kw.insert((*field).into(), Value::String((*k).into()));
        }
        inner.insert("keywords".into(), Value::Object(kw));
    }
    let mut m = Map::new();
    m.insert("steel".into(), Value::Object(inner));
    m
}

pub fn register(catalog: &CapabilityCatalog) {
    catalog.register(Arc::new(DelegateStart(CapabilityMeta {
        tool_name: "__cant_delegate_start".into(),
        api_path: vec!["delegate".into(), "start".into()],
        description: "Launch a fresh-context background delegate worker (powered by the configured worker driver, default agy). Returns a receipt immediately; wait for completion notification.".into(),
        params_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["task"],
            "properties": {
                "task": { "type": "string" },
                "name": { "type": "string" },
                "capabilities": { "type": "array", "items": { "type": "string" } },
                "cwd": { "type": "string" },
                "driver": { "type": "string", "description": "Worker driver ID; defaults to the workspace driver." },
                "model": { "type": "string" },
                "timeoutSec": { "type": "integer", "minimum": 0 },
                "notify": { "type": "string", "enum": ["active", "passive", "off"] },
                "channel": { "description": "Attach worker to a channel: true (auto-open), channel/#room name, or array of channel names." }
            }
        }),
        result_schema: json!({ "type": "object" }),
        argument_mode: ArgumentMode::Single,
        language_hints: steel_hints(&["task"], &[("timeoutSec", "timeout-seconds")]),
    })));

    catalog.register(Arc::new(DelegateResult(CapabilityMeta {
        tool_name: "__cant_delegate_result".into(),
        api_path: vec!["delegate".into(), "result".into()],
        description: "Retrieve non-blocking status or bounded final output of a delegate job."
            .into(),
        params_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["id"],
            "properties": { "id": { "type": "string" } }
        }),
        result_schema: json!({ "type": "object" }),
        argument_mode: ArgumentMode::Single,
        language_hints: steel_hints(&["id"], &[]),
    })));

    catalog.register(Arc::new(DelegateList(CapabilityMeta {
        tool_name: "__cant_delegate_list".into(),
        api_path: vec!["delegate".into(), "list".into()],
        description: "List delegate jobs owned by this session.".into(),
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

    catalog.register(Arc::new(DelegateCancel(CapabilityMeta {
        tool_name: "__cant_delegate_cancel".into(),
        api_path: vec!["delegate".into(), "cancel".into()],
        description: "Cancel a delegate job and suppress its completion notice.".into(),
        params_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["id"],
            "properties": { "id": { "type": "string" } }
        }),
        result_schema: json!({ "type": "object" }),
        argument_mode: ArgumentMode::Single,
        language_hints: steel_hints(&["id"], &[]),
    })));

    catalog.register(Arc::new(AgentsRun(CapabilityMeta {
        tool_name: "__cant_agents_run".into(),
        api_path: vec!["agents".into(), "run".into()],
        description: "Run one short foreground structured worker (40s default, 45s max) with optional input and outputSchema.".into(),
        params_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["task"],
            "properties": {
                "task": { "type": "string" },
                "name": { "type": "string" },
                "input": {},
                "capabilities": { "type": "array", "items": { "type": "string" } },
                "driver": { "type": "string", "description": "Worker driver ID; defaults to the workspace driver." },
                "model": { "type": "string" },
                "timeoutSec": { "type": "integer", "minimum": 1, "maximum": 45 },
                "outputSchema": { "type": "object" },
                "channel": { "description": "Attach worker to a channel: true, channel/#room name, or array of channel names." }
            }
        }),
        result_schema: json!({ "type": "object" }),
        argument_mode: ArgumentMode::Single,
        language_hints: steel_hints(&["task"], &[("timeoutSec", "timeout-seconds")]),
    })));

    catalog.register(Arc::new(AgentsRunMany(CapabilityMeta {
        tool_name: "__cant_agents_run_many".into(),
        api_path: vec!["agents".into(), "runMany".into()],
        description: "Run up to 4 short foreground structured tasks concurrently (use agents/task in Steel).".into(),
        params_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["tasks"],
            "properties": {
                "tasks": { "type": "array", "minItems": 1, "maxItems": 4 },
                "channel": { "description": "Attach all tasks to a shared channel/room: true, room name, or array of channel names." }
            }
        }),
        result_schema: json!({ "type": "array" }),
        argument_mode: ArgumentMode::Single,
        language_hints: steel_hints(&["tasks"], &[]),
    })));

    catalog.register(Arc::new(AgentsStart(CapabilityMeta {
        tool_name: "__cant_agents_start".into(),
        api_path: vec!["agents".into(), "start".into()],
        description: "Start a durable single-node structured agent job. Collect with agents/collect using the completion token from the notice.".into(),
        params_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["task"],
            "properties": {
                "task": { "type": "string" },
                "name": { "type": "string" },
                "input": {},
                "capabilities": { "type": "array", "items": { "type": "string" } },
                "driver": { "type": "string", "description": "Worker driver ID; defaults to the workspace driver." },
                "model": { "type": "string" },
                "timeoutSec": { "type": "integer", "minimum": 0 },
                "outputSchema": { "type": "object" },
                "notify": { "type": "string", "enum": ["active", "passive", "off"] },
                "channel": { "description": "Attach worker to a channel: true, channel/#room name, or array of channel names." }
            }
        }),
        result_schema: json!({ "type": "object" }),
        argument_mode: ArgumentMode::Single,
        language_hints: steel_hints(&["task"], &[("timeoutSec", "timeout-seconds")]),
    })));

    catalog.register(Arc::new(AgentsStartMany(CapabilityMeta {
        tool_name: "__cant_agents_start_many".into(),
        api_path: vec!["agents".into(), "startMany".into()],
        description: "Start up to 8 independent durable agent tasks concurrently (use agents/task in Steel).".into(),
        params_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["tasks"],
            "properties": {
                "tasks": { "type": "array", "minItems": 1, "maxItems": 8 },
                "name": { "type": "string" },
                "notify": { "type": "string", "enum": ["active", "passive", "off"] },
                "channel": { "description": "Attach all tasks to a shared channel/room: true, room name, or array of channel names." }
            }
        }),
        result_schema: json!({ "type": "object" }),
        argument_mode: ArgumentMode::Single,
        language_hints: steel_hints(&["tasks"], &[]),
    })));

    catalog.register(Arc::new(AgentsStartFlow(CapabilityMeta {
        tool_name: "__cant_agents_start_flow".into(),
        api_path: vec!["agents".into(), "startFlow".into()],
        description: "Start a static DAG of up to 8 agent nodes with $from / $artifactFrom piping and needs ordering (use agents/node and agents/from in Steel).".into(),
        params_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["nodes"],
            "properties": {
                "nodes": { "type": "array", "minItems": 1, "maxItems": 8 },
                "name": { "type": "string" },
                "output": {},
                "notify": { "type": "string", "enum": ["active", "passive", "off"] },
                "channel": { "description": "Attach all flow nodes to a shared channel/room: true, room name, or array of channel names." }
            }
        }),
        result_schema: json!({ "type": "object" }),
        argument_mode: ArgumentMode::Single,
        language_hints: steel_hints(&["nodes"], &[]),
    })));

    catalog.register(Arc::new(AgentsCollect(CapabilityMeta {
        tool_name: "__cant_agents_collect".into(),
        api_path: vec!["agents".into(), "collect".into()],
        description:
            "Collect the final output of a completed durable agent flow using its completionToken."
                .into(),
        params_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["id", "completionToken"],
            "properties": {
                "id": { "type": "string" },
                "completionToken": { "type": "string" }
            }
        }),
        result_schema: json!({ "type": "object" }),
        argument_mode: ArgumentMode::Single,
        language_hints: steel_hints(&["id"], &[]),
    })));

    catalog.register(Arc::new(AgentsCancel(CapabilityMeta {
        tool_name: "__cant_agents_cancel".into(),
        api_path: vec!["agents".into(), "cancel".into()],
        description: "Cancel a running durable agent flow and suppress its completion notice."
            .into(),
        params_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["id"],
            "properties": { "id": { "type": "string" } }
        }),
        result_schema: json!({ "type": "object" }),
        argument_mode: ArgumentMode::Single,
        language_hints: steel_hints(&["id"], &[]),
    })));
}

#[cfg(test)]
mod lifecycle_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_driver_overrides_session_without_changing_model() {
        let node = |extra: Value| {
            let mut args = json!({"id":"n", "task":"review", "model":"requested"});
            args.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            parse_node_def(&args, None, "pi")
        };
        assert_eq!(node(json!({})).unwrap().driver, "pi");
        let explicit = node(json!({"driver":"codex"})).unwrap();
        assert_eq!(explicit.driver, "codex");
        assert_eq!(explicit.model.as_deref(), Some("requested"));
        assert!(node(json!({"driver":false})).is_err());
    }
}
