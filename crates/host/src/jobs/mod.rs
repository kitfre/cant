//! Daemon-owned JobManager: coordinates background scripts, fresh-context delegates,
//! structured agent DAG flows, shared 4-worker pool, per-session completion notices,
//! and real-time event broadcasting for connected UIs (Pi `/jobs`, footer, CLI watch).

pub mod agents;
pub mod background;
pub mod driver;
pub mod unified;

use crate::capability::CapabilityCatalog;
use cant_protocol::daemon::{CompletionNotice, DaemonEvent, JobSummary};
use driver::WorkerDriverRegistry;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, Semaphore};

pub const WORKER_POOL_SIZE: usize = 4;
pub const MAX_RETAINED_FINISHED_JOBS: usize = 40;
pub const MAX_OUTPUT_REPLY_BYTES: usize = 50 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyMode {
    Active,
    Passive,
    Off,
}

impl NotifyMode {
    pub fn parse(raw: Option<&str>) -> Result<Self, String> {
        match raw.unwrap_or("active") {
            "active" => Ok(Self::Active),
            "passive" => Ok(Self::Passive),
            "off" => Ok(Self::Off),
            other => Err(format!(
                "Invalid notify mode '{other}' (expected active|passive|off)."
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Passive => "passive",
            Self::Off => "off",
        }
    }
}

struct QueuedNotice {
    session_id: String,
    notice: CompletionNotice,
}

#[derive(Debug, Clone)]
pub struct JobTailResult {
    pub summary: JobSummary,
    pub output: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JobLookupError {
    #[error("No job found with id '{0}'.")]
    NotFound(String),
    #[error("Job '{id}' belongs to another session.")]
    WrongSession { id: String },
}

fn check_job_session(
    id: &str,
    owner_session: &str,
    session_filter: Option<&str>,
) -> Result<(), JobLookupError> {
    if session_filter.is_some_and(|s| s != owner_session) {
        return Err(JobLookupError::WrongSession { id: id.to_string() });
    }
    Ok(())
}

pub struct JobEventReceivers {
    pub lifecycle: broadcast::Receiver<(String, DaemonEvent)>,
    pub output: broadcast::Receiver<(String, DaemonEvent)>,
}

pub struct JobManager {
    pub drivers: Arc<WorkerDriverRegistry>,
    pub worker_pool: Arc<Semaphore>,
    pub scripts: Mutex<HashMap<String, Arc<Mutex<background::ScriptJob>>>>,
    pub delegates: Mutex<HashMap<String, Arc<Mutex<agents::DelegateJob>>>>,
    pub flows: Mutex<HashMap<String, Arc<Mutex<agents::AgentFlowJob>>>>,
    notices: Mutex<VecDeque<QueuedNotice>>,
    lifecycle_tx: broadcast::Sender<(String, DaemonEvent)>,
    output_tx: broadcast::Sender<(String, DaemonEvent)>,
}

impl JobManager {
    pub fn new(drivers: Arc<WorkerDriverRegistry>) -> Self {
        let (lifecycle_tx, _) = broadcast::channel(256);
        let (output_tx, _) = broadcast::channel(256);
        Self {
            drivers,
            worker_pool: Arc::new(Semaphore::new(WORKER_POOL_SIZE)),
            scripts: Mutex::new(HashMap::new()),
            delegates: Mutex::new(HashMap::new()),
            flows: Mutex::new(HashMap::new()),
            notices: Mutex::new(VecDeque::new()),
            lifecycle_tx,
            output_tx,
        }
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<(String, DaemonEvent)> {
        self.lifecycle_tx.subscribe()
    }

    pub fn subscribe_split(&self) -> JobEventReceivers {
        JobEventReceivers {
            lifecycle: self.lifecycle_tx.subscribe(),
            output: self.output_tx.subscribe(),
        }
    }

    pub fn emit_event(&self, session_id: &str, event: DaemonEvent) {
        match event {
            DaemonEvent::JobOutput { .. } => {
                let _ = self.output_tx.send((session_id.to_string(), event));
            }
            DaemonEvent::JobUpdated { .. }
            | DaemonEvent::JobCompleted { .. }
            | DaemonEvent::ChannelMessage { .. } => {
                let _ = self.lifecycle_tx.send((session_id.to_string(), event));
            }
        }
    }

    pub fn push_notice(&self, session_id: String, notice: CompletionNotice) {
        if notice.notify == "off" {
            return;
        }
        self.emit_event(
            &session_id,
            DaemonEvent::JobCompleted {
                notice: notice.clone(),
            },
        );
        let mut q = self.notices.lock().unwrap();
        q.push_back(QueuedNotice { session_id, notice });
        while q.len() > 256 {
            q.pop_front();
        }
    }

    pub fn drain_notices(&self, session_id: &str, include_passive: bool) -> Vec<CompletionNotice> {
        let mut q = self.notices.lock().unwrap();
        let mut drained = Vec::new();
        let mut kept = VecDeque::with_capacity(q.len());
        while let Some(item) = q.pop_front() {
            let matches_session = item.session_id == session_id;
            let matches_mode = item.notice.notify == "active"
                || (include_passive && item.notice.notify == "passive");
            if matches_session && matches_mode {
                drained.push(item.notice);
            } else {
                kept.push_back(item);
            }
        }
        *q = kept;
        drained
    }

    pub fn list_all_jobs(&self, session_filter: Option<&str>) -> Vec<JobSummary> {
        let mut out = Vec::new();
        for job in self.scripts.lock().unwrap().values() {
            let g = job.lock().unwrap();
            if session_filter.is_none_or(|s| s == g.session_id) {
                out.push(g.summary());
            }
        }
        for job in self.delegates.lock().unwrap().values() {
            let g = job.lock().unwrap();
            if session_filter.is_none_or(|s| s == g.session_id) {
                out.push(g.summary());
            }
        }
        for job in self.flows.lock().unwrap().values() {
            let g = job.lock().unwrap();
            if session_filter.is_none_or(|s| s == g.session_id) {
                out.push(g.summary());
            }
        }
        unified::sort_jobs_for_display(&mut out);
        out
    }

    pub fn tail_job(
        &self,
        id: &str,
        max_bytes: usize,
        session_filter: Option<&str>,
    ) -> Result<JobTailResult, JobLookupError> {
        if let Some(job) = self.scripts.lock().unwrap().get(id).cloned() {
            let g = job.lock().unwrap();
            check_job_session(id, &g.session_id, session_filter)?;
            return Ok(JobTailResult {
                summary: g.summary(),
                output: g.tail(max_bytes),
            });
        }
        if let Some(job) = self.delegates.lock().unwrap().get(id).cloned() {
            let g = job.lock().unwrap();
            check_job_session(id, &g.session_id, session_filter)?;
            return Ok(JobTailResult {
                summary: g.summary(),
                output: g.tail(max_bytes),
            });
        }
        if let Some(job) = self.flows.lock().unwrap().get(id).cloned() {
            let g = job.lock().unwrap();
            check_job_session(id, &g.session_id, session_filter)?;
            return Ok(JobTailResult {
                summary: g.summary(),
                output: g.tail(max_bytes),
            });
        }
        Err(JobLookupError::NotFound(id.to_string()))
    }

    pub fn cancel_any_job(
        &self,
        id: &str,
        session_filter: Option<&str>,
    ) -> Result<(), JobLookupError> {
        if let Some(job) = self.scripts.lock().unwrap().get(id).cloned() {
            {
                let g = job.lock().unwrap();
                check_job_session(id, &g.session_id, session_filter)?;
            }
            background::cancel_script_job(&job);
            let (sid, summary) = {
                let g = job.lock().unwrap();
                (g.session_id.clone(), g.summary())
            };
            self.emit_event(&sid, DaemonEvent::JobUpdated { job: summary });
            return Ok(());
        }
        if let Some(job) = self.delegates.lock().unwrap().get(id).cloned() {
            {
                let g = job.lock().unwrap();
                check_job_session(id, &g.session_id, session_filter)?;
            }
            agents::cancel_delegate_job(&job);
            let (sid, summary) = {
                let g = job.lock().unwrap();
                (g.session_id.clone(), g.summary())
            };
            self.emit_event(&sid, DaemonEvent::JobUpdated { job: summary });
            return Ok(());
        }
        if let Some(job) = self.flows.lock().unwrap().get(id).cloned() {
            {
                let g = job.lock().unwrap();
                check_job_session(id, &g.session_id, session_filter)?;
            }
            agents::cancel_flow_job(&job);
            let (sid, summary) = {
                let g = job.lock().unwrap();
                (g.session_id.clone(), g.summary())
            };
            self.emit_event(&sid, DaemonEvent::JobUpdated { job: summary });
            return Ok(());
        }
        Err(JobLookupError::NotFound(id.to_string()))
    }

    pub fn shutdown_all(&self) {
        for job in self.scripts.lock().unwrap().values() {
            background::cancel_script_job(job);
        }
        for job in self.delegates.lock().unwrap().values() {
            agents::cancel_delegate_job(job);
        }
        for job in self.flows.lock().unwrap().values() {
            agents::cancel_flow_job(job);
        }
    }
}

pub fn register_job_capabilities(catalog: &CapabilityCatalog) {
    background::register(catalog);
    agents::register(catalog);
    unified::register(catalog);
}

pub fn truncate_tail_utf8(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut start = text.len() - max_bytes;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_string()
}

pub fn truncate_head_utf8(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_string(), false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

#[cfg(test)]
mod unified_tests {
    use super::*;
    use crate::capability::{CallContext, CapabilityCatalog};
    use crate::jobs::agents::{AgentFlowJob, DelegateJob, FlowNodeState, WorkerStatus};
    use crate::jobs::background::{ScriptJob, ScriptStatus};
    use crate::language::{GuestLanguage, LanguageIdentity};
    use crate::runner_process::RunnerCommand;
    use serde_json::{json, Value};
    use std::path::PathBuf;
    use std::time::Instant;
    use tokio_util::sync::CancellationToken;

    struct DummyLanguage;
    impl GuestLanguage for DummyLanguage {
        fn identity(&self) -> LanguageIdentity {
            unreachable!()
        }
        fn runner_command(&self) -> Result<RunnerCommand, String> {
            unreachable!()
        }
        fn project_bindings(&self, _: &[crate::capability::CapabilityMeta]) -> Value {
            unreachable!()
        }
        fn binding_name(
            &self,
            _: &[crate::capability::CapabilityMeta],
            _: &crate::capability::CapabilityMeta,
        ) -> String {
            unreachable!()
        }
        fn signature(
            &self,
            _: &[crate::capability::CapabilityMeta],
            _: &crate::capability::CapabilityMeta,
        ) -> String {
            unreachable!()
        }
        fn tool_description(&self, _: &[crate::capability::CapabilityMeta]) -> String {
            unreachable!()
        }
        fn source_description(&self) -> &'static str {
            unreachable!()
        }
        fn inputs_description(&self) -> &'static str {
            unreachable!()
        }
        fn format_metadata(&self, _: &Value) -> String {
            unreachable!()
        }
        fn post_describe(
            &self,
            _: &str,
            _: Value,
            _: &[crate::capability::CapabilityMeta],
        ) -> Value {
            unreachable!()
        }
    }

    fn make_ctx(jobs: Arc<JobManager>, session_id: &str) -> CallContext {
        let catalog = Arc::new(CapabilityCatalog::new());
        register_job_capabilities(&catalog);
        CallContext {
            jobs,
            catalog,
            language: Arc::new(DummyLanguage),
            session_id: session_id.into(),
            worker_driver: "test".into(),
            language_id: "steel".into(),
            cwd: PathBuf::from("/repo"),
            disabled_namespaces: vec![],
            extra_capabilities: vec![],
            file_coverage: Default::default(),
            comms: Arc::new(crate::comms::CommsHub::new()),
        }
    }

    fn seed_jobs(jobs: &JobManager) {
        jobs.scripts.lock().unwrap().insert(
            "bg-1".into(),
            Arc::new(Mutex::new(ScriptJob {
                id: "bg-1".into(),
                session_id: "sess-a".into(),
                script: "cargo".into(),
                args: vec!["test".into()],
                cwd: PathBuf::from("/repo"),
                notify: NotifyMode::Active,
                status: ScriptStatus::Running,
                settled: false,
                exit_code: None,
                signal: None,
                pid: Some(4242),
                started_at: Instant::now(),
                finished_ms: None,
                buffer: "running 10 tests\n".into(),
                total_written: 17,
                dropped_bytes: 0,
                cursor: 0,
                cancel_token: CancellationToken::new(),
                cancelled_explicitly: false,
            })),
        );
        jobs.delegates.lock().unwrap().insert(
            "del-1".into(),
            Arc::new(Mutex::new(DelegateJob {
                id: "del-1".into(),
                session_id: "sess-a".into(),
                name: "review".into(),
                task: "Review PR changes".into(),
                notify: NotifyMode::Active,
                status: WorkerStatus::Running,
                output: None,
                error: None,
                usage: None,
                monitor: "checking crates/host/src/mcp.rs...\n".into(),
                started_at: Instant::now(),
                finished_ms: None,
                cancel_token: CancellationToken::new(),
                cancelled_explicitly: false,
            })),
        );
        let mut nodes = HashMap::new();
        nodes.insert(
            "plan".into(),
            FlowNodeState {
                id: "plan".into(),
                name: "plan".into(),
                status: WorkerStatus::Completed,
                completion: Some(json!({"ok": true})),
                error: None,
                monitor: "plan complete\n".into(),
            },
        );
        nodes.insert(
            "build".into(),
            FlowNodeState {
                id: "build".into(),
                name: "build".into(),
                status: WorkerStatus::Running,
                completion: None,
                error: None,
                monitor: "compiling...\n".into(),
            },
        );
        jobs.flows.lock().unwrap().insert(
            "flow-1".into(),
            Arc::new(Mutex::new(AgentFlowJob {
                id: "flow-1".into(),
                session_id: "sess-a".into(),
                name: "pipeline".into(),
                notify: NotifyMode::Active,
                status: WorkerStatus::Completed,
                completion_token: "tok-1".into(),
                nodes,
                node_order: vec!["plan".into(), "build".into()],
                output_value: Some(json!({"done": true})),
                error: None,
                started_at: Instant::now(),
                finished_ms: Some(1200),
                cancel_token: CancellationToken::new(),
                cancelled_explicitly: false,
            })),
        );
        jobs.delegates.lock().unwrap().insert(
            "del-other".into(),
            Arc::new(Mutex::new(DelegateJob {
                id: "del-other".into(),
                session_id: "sess-b".into(),
                name: "other-work".into(),
                task: "Other session task".into(),
                notify: NotifyMode::Active,
                status: WorkerStatus::Running,
                output: None,
                error: None,
                usage: None,
                monitor: "secret output\n".into(),
                started_at: Instant::now(),
                finished_ms: None,
                cancel_token: CancellationToken::new(),
                cancelled_explicitly: false,
            })),
        );
    }

    #[tokio::test]
    async fn unified_jobs_list_filters_by_session_kind_and_active_only() {
        let jobs = Arc::new(JobManager::new(Arc::new(WorkerDriverRegistry::new())));
        seed_jobs(&jobs);
        let ctx = make_ctx(Arc::clone(&jobs), "sess-a");
        let list_cap = ctx
            .catalog
            .get_by_tool_name("__cant_jobs_list")
            .expect("jobs/list capability should be registered");

        let all = list_cap.call(&ctx, json!({})).await.unwrap();
        assert_eq!(all["total"], 3);
        assert_eq!(all["activeCount"], 2);
        let ids: Vec<&str> = all["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|j| j["id"].as_str().unwrap())
            .collect();
        assert!(!ids.contains(&"del-other"));
        assert!(ids.contains(&"bg-1") && ids.contains(&"del-1") && ids.contains(&"flow-1"));
        // Active jobs come before terminal jobs.
        assert_eq!(ids[2], "flow-1");

        let only_active = list_cap.call(&ctx, json!({ "activeOnly": true })).await.unwrap();
        assert_eq!(only_active["total"], 2);
        assert_eq!(only_active["activeCount"], 2);

        let only_flows = list_cap.call(&ctx, json!({ "kind": "flow" })).await.unwrap();
        assert_eq!(only_flows["total"], 1);
        assert_eq!(only_flows["jobs"][0]["id"], "flow-1");
    }

    #[tokio::test]
    async fn unified_jobs_tail_is_non_destructive_and_session_scoped() {
        let jobs = Arc::new(JobManager::new(Arc::new(WorkerDriverRegistry::new())));
        seed_jobs(&jobs);
        let ctx = make_ctx(Arc::clone(&jobs), "sess-a");
        let tail_cap = ctx
            .catalog
            .get_by_tool_name("__cant_jobs_tail")
            .expect("jobs/tail capability should be registered");

        let script_tail = tail_cap.call(&ctx, json!({ "id": "bg-1" })).await.unwrap();
        assert_eq!(script_tail["kind"], "script");
        assert_eq!(script_tail["status"], "running");
        assert!(script_tail["output"].as_str().unwrap().contains("running 10 tests"));
        // Cursor must NOT advance on non-destructive tail.
        assert_eq!(jobs.scripts.lock().unwrap()["bg-1"].lock().unwrap().cursor, 0);

        let del_tail = tail_cap.call(&ctx, json!({ "id": "del-1" })).await.unwrap();
        assert_eq!(del_tail["kind"], "delegate");
        assert_eq!(del_tail["name"], "review");
        assert!(del_tail["output"].as_str().unwrap().contains("checking crates/host/src/mcp.rs"));

        let flow_tail = tail_cap.call(&ctx, json!({ "id": "flow-1" })).await.unwrap();
        assert_eq!(flow_tail["kind"], "flow");
        assert!(flow_tail["output"].as_str().unwrap().contains("node 'build' (running)"));

        let cross = tail_cap.call(&ctx, json!({ "id": "del-other" })).await;
        assert!(cross.is_err());
    }

    #[tokio::test]
    async fn unified_jobs_cancel_enforces_session_ownership() {
        let jobs = Arc::new(JobManager::new(Arc::new(WorkerDriverRegistry::new())));
        seed_jobs(&jobs);
        let ctx = make_ctx(Arc::clone(&jobs), "sess-a");
        let cancel_cap = ctx
            .catalog
            .get_by_tool_name("__cant_jobs_cancel")
            .expect("jobs/cancel capability should be registered");

        assert!(cancel_cap.call(&ctx, json!({ "id": "del-other" })).await.is_err());
        assert_eq!(
            jobs.delegates.lock().unwrap()["del-other"].lock().unwrap().status,
            WorkerStatus::Running
        );

        let cancelled = cancel_cap.call(&ctx, json!({ "id": "del-1" })).await.unwrap();
        assert_eq!(cancelled["cancelled"], true);
        assert_eq!(
            jobs.delegates.lock().unwrap()["del-1"].lock().unwrap().status,
            WorkerStatus::Cancelled
        );
    }
}
