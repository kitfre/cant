//! Reverse-RPC AdapterHub: lets connected harness adapters (like Pi) dynamically register
//! session-scoped external capabilities (e.g. Pi native `mcp/*` tools) and external
//! `WorkerDriver`s (e.g. Pi SDK workers) over a persistent Unix-socket connection.

use crate::capability::{ArgumentMode, CallContext, Capability, CapabilityError, CapabilityMeta};
use crate::jobs::driver::{ProgressCallback, WorkerDriver, WorkerOutcome, WorkerSpec};
use async_trait::async_trait;
use cant_protocol::daemon::{DaemonResponse, ExternalCapabilitySpec, ExternalWorkerSpec};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub struct PendingWorker {
    pub tx: oneshot::Sender<Result<Value, String>>,
    pub on_progress: ProgressCallback,
}

type CapabilityReply = oneshot::Sender<Result<Value, (String, String)>>;

pub struct AdapterSession {
    pub adapter_id: String,
    pub session_id: String,
    pub driver_name: Option<String>,
    pub outbound: mpsc::Sender<DaemonResponse>,
    pub capabilities: RwLock<Vec<Arc<dyn Capability>>>,
    pub pending_caps: Mutex<HashMap<String, CapabilityReply>>,
    pub pending_workers: Mutex<HashMap<String, PendingWorker>>,
}

impl AdapterSession {
    pub fn set_capabilities(self: &Arc<Self>, specs: Vec<ExternalCapabilitySpec>) {
        let caps: Vec<Arc<dyn Capability>> = specs
            .into_iter()
            .map(|spec| {
                let mut hints = Map::new();
                if let Some(steel_pos) = spec.steel_positional {
                    hints.insert(
                        "steel".into(),
                        serde_json::json!({ "positional": steel_pos }),
                    );
                }
                let meta = CapabilityMeta {
                    tool_name: spec.tool_name,
                    api_path: spec.api_path,
                    description: spec.description,
                    params_schema: spec.params_schema,
                    result_schema: spec.result_schema,
                    argument_mode: ArgumentMode::Single,
                    language_hints: hints,
                };
                Arc::new(ExternalAdapterCapability {
                    meta,
                    adapter: Arc::clone(self),
                }) as Arc<dyn Capability>
            })
            .collect();
        *self.capabilities.write().unwrap() = caps;
    }

    pub fn resolve_result(
        &self,
        call_id: &str,
        ok: bool,
        value: Option<Value>,
        error: Option<String>,
    ) {
        if let Some(tx) = self.pending_caps.lock().unwrap().remove(call_id) {
            if ok {
                let _ = tx.send(Ok(value.unwrap_or(Value::Null)));
            } else {
                let _ = tx.send(Err((
                    "ExternalCapabilityError".into(),
                    error.unwrap_or_else(|| "External capability failed.".into()),
                )));
            }
            return;
        }
        if let Some(pw) = self.pending_workers.lock().unwrap().remove(call_id) {
            if ok {
                let _ = pw.tx.send(Ok(value.unwrap_or(Value::Null)));
            } else {
                let _ = pw.tx.send(Err(
                    error.unwrap_or_else(|| "External worker failed.".into())
                ));
            }
        }
    }

    pub fn forward_progress(&self, call_id: &str, chunk: String) {
        if let Some(pw) = self.pending_workers.lock().unwrap().get(call_id) {
            (pw.on_progress)(chunk);
        }
    }

    pub fn fail_all_pending(&self, reason: &str) {
        for (_, tx) in self.pending_caps.lock().unwrap().drain() {
            let _ = tx.send(Err(("AdapterDisconnectedError".into(), reason.to_string())));
        }
        for (_, pw) in self.pending_workers.lock().unwrap().drain() {
            let _ = pw.tx.send(Err(reason.to_string()));
        }
    }
}

struct ExternalAdapterCapability {
    meta: CapabilityMeta,
    adapter: Arc<AdapterSession>,
}

#[async_trait]
impl Capability for ExternalAdapterCapability {
    fn meta(&self) -> &CapabilityMeta {
        &self.meta
    }

    async fn call(&self, _ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let call_id = format!("extc_{}", Uuid::new_v4().simple());
        let (tx, rx) = oneshot::channel();
        self.adapter
            .pending_caps
            .lock()
            .unwrap()
            .insert(call_id.clone(), tx);

        let send_res = self
            .adapter
            .outbound
            .send(DaemonResponse::InvokeCapability {
                call_id: call_id.clone(),
                tool_name: self.meta.tool_name.clone(),
                args,
            })
            .await;
        if send_res.is_err() {
            self.adapter.pending_caps.lock().unwrap().remove(&call_id);
            return Err(CapabilityError::exec(
                "AdapterDisconnectedError",
                "Harness adapter disconnected before capability call could be sent.",
            ));
        }

        match rx.await {
            Ok(Ok(val)) => Ok(val),
            Ok(Err((name, msg))) => Err(CapabilityError::exec(name, msg)),
            Err(_) => Err(CapabilityError::exec(
                "AdapterDisconnectedError",
                "Harness adapter disconnected while executing capability.",
            )),
        }
    }
}

#[derive(Default)]
struct HubInner {
    version: u64,
    by_adapter_id: HashMap<String, Arc<AdapterSession>>,
    /// Maps session_id -> active adapter_id
    by_session: HashMap<String, String>,
    /// Maps driver_name -> latest adapter_id advertising that driver
    by_driver: HashMap<String, String>,
}

#[derive(Default)]
pub struct AdapterHub {
    inner: RwLock<HubInner>,
}

impl AdapterHub {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn version(&self) -> u64 {
        self.inner.read().unwrap().version
    }

    pub fn attach(
        &self,
        adapter_id: String,
        session_id: String,
        driver_name: Option<String>,
        capabilities: Vec<ExternalCapabilitySpec>,
        outbound: mpsc::Sender<DaemonResponse>,
    ) -> Arc<AdapterSession> {
        let session = Arc::new(AdapterSession {
            adapter_id: adapter_id.clone(),
            session_id: session_id.clone(),
            driver_name: driver_name.clone(),
            outbound,
            capabilities: RwLock::new(Vec::new()),
            pending_caps: Mutex::new(HashMap::new()),
            pending_workers: Mutex::new(HashMap::new()),
        });
        session.set_capabilities(capabilities);

        let mut g = self.inner.write().unwrap();
        g.by_adapter_id
            .insert(adapter_id.clone(), Arc::clone(&session));
        g.by_session.insert(session_id, adapter_id.clone());
        if let Some(driver) = driver_name {
            g.by_driver.insert(driver, adapter_id);
        }
        g.version += 1;
        session
    }

    pub fn update_capabilities(&self, adapter_id: &str, specs: Vec<ExternalCapabilitySpec>) {
        let adapter = self
            .inner
            .read()
            .unwrap()
            .by_adapter_id
            .get(adapter_id)
            .cloned();
        if let Some(a) = adapter {
            a.set_capabilities(specs);
            self.inner.write().unwrap().version += 1;
        }
    }

    pub fn detach(&self, adapter_id: &str) {
        let removed = {
            let mut g = self.inner.write().unwrap();
            let removed = g.by_adapter_id.remove(adapter_id);
            if let Some(ref a) = removed {
                if g.by_session.get(&a.session_id).map(String::as_str) == Some(adapter_id) {
                    g.by_session.remove(&a.session_id);
                }
                if let Some(ref drv) = a.driver_name {
                    if g.by_driver.get(drv).map(String::as_str) == Some(adapter_id) {
                        g.by_driver.remove(drv);
                    }
                }
                g.version += 1;
            }
            removed
        };
        if let Some(a) = removed {
            a.fail_all_pending("Harness adapter disconnected.");
        }
    }

    pub fn capabilities_for_session(&self, session_id: &str) -> Vec<Arc<dyn Capability>> {
        let adapter = {
            let g = self.inner.read().unwrap();
            g.by_session
                .get(session_id)
                .and_then(|id| g.by_adapter_id.get(id))
                .cloned()
        };
        match adapter {
            Some(a) => a.capabilities.read().unwrap().clone(),
            None => Vec::new(),
        }
    }

    pub fn driver_adapter(&self, driver_name: &str) -> Option<Arc<AdapterSession>> {
        let g = self.inner.read().unwrap();
        g.by_driver
            .get(driver_name)
            .and_then(|id| g.by_adapter_id.get(id))
            .cloned()
    }

    pub fn driver_names(&self) -> Vec<String> {
        let mut names: Vec<_> = self
            .inner
            .read()
            .unwrap()
            .by_driver
            .keys()
            .cloned()
            .collect();
        names.sort();
        names
    }
}

/// `WorkerDriver` backed by a connected harness adapter (e.g., Pi SDK workers over reverse-RPC).
pub struct AdapterWorkerDriver {
    driver_id: &'static str,
    hub: Arc<AdapterHub>,
}

impl AdapterWorkerDriver {
    pub fn new(driver_id: &'static str, hub: Arc<AdapterHub>) -> Self {
        Self { driver_id, hub }
    }
}

#[async_trait]
impl WorkerDriver for AdapterWorkerDriver {
    fn id(&self) -> &'static str {
        self.driver_id
    }

    async fn run(
        &self,
        spec: WorkerSpec,
        on_progress: ProgressCallback,
        cancel: CancellationToken,
    ) -> Result<WorkerOutcome, String> {
        let adapter = self.hub.driver_adapter(self.driver_id).ok_or_else(|| {
            format!(
                "No '{}' harness adapter is currently connected to cant daemon.",
                self.driver_id
            )
        })?;

        let call_id = format!("extw_{}", Uuid::new_v4().simple());
        let (tx, rx) = oneshot::channel();
        adapter
            .pending_workers
            .lock()
            .unwrap()
            .insert(call_id.clone(), PendingWorker { tx, on_progress });

        let wire_spec = ExternalWorkerSpec {
            name: spec.name,
            task: spec.task,
            input: spec.input,
            cwd: spec.cwd.to_string_lossy().into_owned(),
            model: spec.model,
            timeout_secs: spec.timeout_secs,
            output_schema: spec.output_schema,
            capabilities: spec.capabilities,
            session_id: spec.session_id,
            channels: spec.channels,
        };

        if adapter
            .outbound
            .send(DaemonResponse::RunWorker {
                call_id: call_id.clone(),
                spec: wire_spec,
            })
            .await
            .is_err()
        {
            adapter.pending_workers.lock().unwrap().remove(&call_id);
            return Err(format!(
                "Failed sending worker request to '{}' adapter.",
                self.driver_id
            ));
        }

        let res = tokio::select! {
            _ = cancel.cancelled() => {
                adapter.pending_workers.lock().unwrap().remove(&call_id);
                let _ = adapter.outbound.send(DaemonResponse::CancelWorker { call_id }).await;
                return Err("Worker cancelled.".into());
            }
            r = rx => r,
        };

        let val = match res {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err("Harness adapter disconnected during worker execution.".into()),
        };

        let text = val
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let data = val.get("data").cloned().filter(|v| !v.is_null());
        let usage = val.get("usage").cloned().filter(|v| !v.is_null());
        Ok(WorkerOutcome { text, data, usage })
    }
}
