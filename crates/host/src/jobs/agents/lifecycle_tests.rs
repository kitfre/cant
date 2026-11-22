use super::*;
use crate::jobs::driver::{ProgressCallback, WorkerDriver, WorkerDriverRegistry, WorkerOutcome};
use crate::language::{GuestLanguage, LanguageIdentity};
use crate::runner_process::RunnerCommand;
use std::sync::atomic::{AtomicUsize, Ordering};

// These capabilities do not interact with the guest language.
struct NoGuest;
impl GuestLanguage for NoGuest {
    fn identity(&self) -> LanguageIdentity {
        unreachable!()
    }
    fn runner_command(&self) -> Result<RunnerCommand, String> {
        unreachable!()
    }
    fn project_bindings(&self, _: &[CapabilityMeta]) -> Value {
        unreachable!()
    }
    fn binding_name(&self, _: &[CapabilityMeta], _: &CapabilityMeta) -> String {
        unreachable!()
    }
    fn signature(&self, _: &[CapabilityMeta], _: &CapabilityMeta) -> String {
        unreachable!()
    }
    fn tool_description(&self, _: &[CapabilityMeta]) -> String {
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
    fn post_describe(&self, _: &str, _: Value, _: &[CapabilityMeta]) -> Value {
        unreachable!()
    }
}

#[derive(Default)]
struct Calls {
    count: AtomicUsize,
    specs: Mutex<Vec<WorkerSpec>>,
}
#[async_trait]
impl WorkerDriver for Calls {
    fn id(&self) -> &'static str {
        "count"
    }
    async fn run(
        &self,
        spec: WorkerSpec,
        _: ProgressCallback,
        _: CancellationToken,
    ) -> Result<WorkerOutcome, String> {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.specs.lock().unwrap().push(spec);
        Ok(WorkerOutcome {
            text: "ok".into(),
            data: None,
            usage: None,
        })
    }
}

fn context() -> (CallContext, Arc<Calls>) {
    let calls = Arc::new(Calls::default());
    let mut drivers = WorkerDriverRegistry::new();
    drivers.register(calls.clone());
    let catalog = Arc::new(CapabilityCatalog::new());
    register(&catalog);
    (
        CallContext {
            jobs: Arc::new(JobManager::new(Arc::new(drivers))),
            catalog,
            language: Arc::new(NoGuest),
            session_id: "session".into(),
            worker_driver: "count".into(),
            language_id: "none".into(),
            cwd: Default::default(),
            disabled_namespaces: vec![],
            extra_capabilities: vec![],
            file_coverage: Default::default(),
            comms: Arc::new(crate::comms::CommsHub::new()),
        },
        calls,
    )
}

#[tokio::test]
async fn invalid_batch_never_starts_earlier_workers() {
    let (ctx, calls) = context();
    let cap = ctx
        .catalog
        .get_by_tool_name("__cant_agents_run_many")
        .unwrap();
    for invalid in [
        json!({"task":"work","driver":"missing"}),
        json!({"task":" "}),
    ] {
        let args = json!({"tasks":[{"task":"first"}, invalid]});
        assert!(cap.call(&ctx, args).await.is_err());
        tokio::task::yield_now().await;
        assert_eq!(calls.count.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn flow_registration_publishes_running_summary_before_worker_runs() {
    let (ctx, _) = context();
    let mut events = ctx.jobs.subscribe_events();
    let cap = ctx.catalog.get_by_tool_name("__cant_agents_start").unwrap();
    let result = cap
        .call(&ctx, json!({"task":"test", "notify":"off"}))
        .await
        .unwrap();
    let (sid, event) = events.try_recv().unwrap();
    let cant_protocol::daemon::DaemonEvent::JobUpdated { job } = event else {
        panic!("expected JobUpdated")
    };
    assert_eq!(sid, ctx.session_id);
    assert_eq!(job.id, result["id"].as_str().unwrap());
    assert!(ctx.jobs.flows.lock().unwrap().contains_key(&job.id));
    assert_eq!(job.status, "running");
}

#[tokio::test]
async fn delegate_start_with_channel_auto_creates_channel_and_cleans_up_peer() {
    let (ctx, calls) = context();
    let cap = ctx
        .catalog
        .get_by_tool_name("__cant_delegate_start")
        .unwrap();
    let result = cap
        .call(
            &ctx,
            json!({"task": "refactor parser", "name": "parser", "channel": true, "notify": "off"}),
        )
        .await
        .unwrap();

    let ch_id = result["channelId"]
        .as_str()
        .expect("expected channelId on delegate/start response")
        .to_string();
    assert_eq!(result["peer"].as_str(), Some("parser"));

    // Wait briefly for the spawned mock worker to finish
    for _ in 0..50 {
        if calls.count.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    // Let cleanup run after driver.run returns
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;

    let specs = calls.specs.lock().unwrap();
    assert_eq!(specs.len(), 1);
    assert!(specs[0].session_id.is_some());
    assert_eq!(specs[0].channels, vec![ch_id.clone()]);
    assert!(specs[0].task.contains("## Channels"));
    assert!(specs[0].task.contains(&ch_id));

    // Worker peer is cleaned up after exit, while the channel remains readable
    let worker_sid = specs[0].session_id.as_deref().unwrap();
    assert!(ctx
        .comms
        .list_peers(None, None, true)
        .iter()
        .all(|p| p.id != worker_sid));
    assert!(ctx
        .comms
        .list_channels(Some(&ctx.session_id), true)
        .iter()
        .any(|c| c.id == ch_id));
}

#[tokio::test]
async fn agents_start_many_with_room_joins_parent_and_all_workers() {
    let (ctx, calls) = context();
    let cap = ctx
        .catalog
        .get_by_tool_name("__cant_agents_start_many")
        .unwrap();
    let result = cap
        .call(
            &ctx,
            json!({
                "tasks": [
                    {"name": "server", "task": "audit server"},
                    {"name": "client", "task": "audit client"}
                ],
                "channel": "auth-audit",
                "notify": "off"
            }),
        )
        .await
        .unwrap();

    let ch_id = result["channelId"]
        .as_str()
        .expect("expected channelId on agents/start-many response")
        .to_string();
    assert!(
        ch_id.starts_with("ch-"),
        "expected stable ch-N id, got {ch_id}"
    );
    let (room_summary, _) = ctx
        .comms
        .read_channel(Some(&ctx.session_id), "#auth-audit", false, 10)
        .unwrap();
    assert_eq!(room_summary.id, ch_id);
    assert_eq!(room_summary.name.as_deref(), Some("auth-audit"));

    for _ in 0..50 {
        if calls.count.load(Ordering::SeqCst) == 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let specs = calls.specs.lock().unwrap();
    assert_eq!(specs.len(), 2);
    for s in specs.iter() {
        assert!(s.session_id.is_some());
        assert_eq!(s.channels, vec![ch_id.clone()]);
        assert!(s.task.contains(&ch_id));
        assert!(s.task.contains("#auth-audit"));
        assert!(s.task.contains("## Channels"));
    }
}
