//! Approval-free capability registry and execution context.

use crate::comms::CommsHub;
use crate::jobs::JobManager;
use async_trait::async_trait;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CapabilityError {
    #[error("No capability is registered for {0}.")]
    NotFound(String),
    #[error("Capability {0} is disabled in this workspace profile.")]
    Disabled(String),
    #[error("Invalid arguments for {tool}: {message}")]
    InvalidArgs { tool: String, message: String },
    #[error("{name}: {message}")]
    Execution { name: String, message: String },
}

impl CapabilityError {
    pub fn name(&self) -> &str {
        match self {
            CapabilityError::NotFound(_) => "CapabilityNotFoundError",
            CapabilityError::Disabled(_) => "CapabilityDisabledError",
            CapabilityError::InvalidArgs { .. } => "ToolArgumentError",
            CapabilityError::Execution { name, .. } => name.as_str(),
        }
    }

    pub fn message(&self) -> String {
        match self {
            CapabilityError::Execution { message, .. } => message.clone(),
            other => other.to_string(),
        }
    }

    pub fn exec(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Execution {
            name: name.into(),
            message: message.into(),
        }
    }

    pub fn invalid_args(tool: impl Into<String>, message: impl Into<String>) -> Self {
        Self::InvalidArgs {
            tool: tool.into(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgumentMode {
    Single,
    Spread,
}

#[derive(Debug, Clone)]
pub struct CapabilityMeta {
    pub tool_name: String,
    pub api_path: Vec<String>,
    pub description: String,
    pub params_schema: Value,
    pub result_schema: Value,
    pub argument_mode: ArgumentMode,
    /// Per-language binding hints, e.g. `{"steel": {"positional": ["path"]}}`.
    pub language_hints: Map<String, Value>,
}

impl CapabilityMeta {
    pub fn public_name(&self) -> String {
        self.api_path.join(".")
    }
}

pub fn is_path_disabled(api_path: &[String], disabled: &[String]) -> bool {
    if disabled.is_empty() {
        return false;
    }
    let dotted = api_path.join(".");
    disabled.iter().any(|entry| {
        let norm = entry.trim().replace('/', ".");
        if norm.is_empty() {
            return false;
        }
        dotted == norm
            || dotted.starts_with(&format!("{norm}."))
            || api_path.first().is_some_and(|first| first == &norm)
    })
}

/// Tracks line ranges read per canonical file path + SHA-256 digest within a workspace session,
/// enforcing safe `files/replace` only after full-file read coverage.
#[derive(Debug, Default)]
pub struct FileCoverageLedger {
    entries: Mutex<HashMap<PathBuf, CoverageEntry>>,
}

#[derive(Debug, Clone)]
struct CoverageEntry {
    sha256: String,
    total_lines: usize,
    ranges: Vec<(usize, usize)>,
}

impl FileCoverageLedger {
    pub fn record_complete(&self, canonical_path: PathBuf, sha256: String, total_lines: usize) {
        let mut guard = self.entries.lock().unwrap();
        guard.insert(
            canonical_path,
            CoverageEntry {
                sha256,
                total_lines,
                ranges: vec![(1, total_lines.max(1))],
            },
        );
    }

    pub fn record_chunk(
        &self,
        canonical_path: PathBuf,
        sha256: String,
        total_lines: usize,
        start_line: usize,
        end_line: usize,
    ) -> bool {
        let mut guard = self.entries.lock().unwrap();
        let entry = guard
            .entry(canonical_path)
            .or_insert_with(|| CoverageEntry {
                sha256: sha256.clone(),
                total_lines,
                ranges: Vec::new(),
            });
        if entry.sha256 != sha256 || entry.total_lines != total_lines {
            entry.sha256 = sha256;
            entry.total_lines = total_lines;
            entry.ranges.clear();
        }
        entry.ranges.push((start_line, end_line));
        entry.ranges.sort_by_key(|r| r.0);
        let mut merged: Vec<(usize, usize)> = Vec::new();
        for (s, e) in entry.ranges.drain(..) {
            if let Some(last) = merged.last_mut() {
                if s <= last.1.saturating_add(1) {
                    last.1 = last.1.max(e);
                    continue;
                }
            }
            merged.push((s, e));
        }
        entry.ranges = merged;
        Self::is_complete(entry)
    }

    pub fn has_complete(&self, canonical_path: &PathBuf, expected_sha256: &str) -> bool {
        let guard = self.entries.lock().unwrap();
        guard
            .get(canonical_path)
            .is_some_and(|e| e.sha256.eq_ignore_ascii_case(expected_sha256) && Self::is_complete(e))
    }

    fn is_complete(entry: &CoverageEntry) -> bool {
        entry.ranges.len() == 1 && entry.ranges[0].0 == 1 && entry.ranges[0].1 >= entry.total_lines
    }
}

#[derive(Clone)]
pub struct CallContext {
    pub cwd: PathBuf,
    pub session_id: String,
    pub language_id: String,
    pub worker_driver: String,
    pub disabled_namespaces: Vec<String>,
    pub file_coverage: Arc<FileCoverageLedger>,
    pub catalog: Arc<CapabilityCatalog>,
    pub extra_capabilities: Vec<Arc<dyn Capability>>,
    pub language: Arc<dyn crate::language::GuestLanguage>,
    pub jobs: Arc<JobManager>,
    pub comms: Arc<CommsHub>,
}

impl CallContext {
    pub fn visible_metas(&self) -> Vec<CapabilityMeta> {
        let mut metas = self.catalog.metas_filtered(&self.disabled_namespaces);
        for cap in &self.extra_capabilities {
            let m = cap.meta();
            if !is_path_disabled(&m.api_path, &self.disabled_namespaces) {
                metas.push(m.clone());
            }
        }
        metas
    }
}

#[async_trait]
pub trait Capability: Send + Sync {
    fn meta(&self) -> &CapabilityMeta;
    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError>;
}

#[derive(Default)]
struct CatalogInner {
    version: u64,
    order: Vec<String>,
    by_tool_name: HashMap<String, Arc<dyn Capability>>,
    by_api_path: HashMap<Vec<String>, Arc<dyn Capability>>,
}

#[derive(Default)]
pub struct CapabilityCatalog {
    inner: RwLock<CatalogInner>,
}

impl CapabilityCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, capability: Arc<dyn Capability>) {
        let meta = capability.meta();
        let mut guard = self.inner.write().unwrap();
        if !guard.by_tool_name.contains_key(&meta.tool_name) {
            guard.order.push(meta.tool_name.clone());
        }
        guard
            .by_api_path
            .insert(meta.api_path.clone(), Arc::clone(&capability));
        guard
            .by_tool_name
            .insert(meta.tool_name.clone(), capability);
        guard.version += 1;
    }

    pub fn version(&self) -> u64 {
        self.inner.read().unwrap().version
    }

    pub fn list(&self) -> Vec<Arc<dyn Capability>> {
        let guard = self.inner.read().unwrap();
        guard
            .order
            .iter()
            .filter_map(|name| guard.by_tool_name.get(name).cloned())
            .collect()
    }

    pub fn metas(&self) -> Vec<CapabilityMeta> {
        self.list().into_iter().map(|c| c.meta().clone()).collect()
    }

    pub fn metas_filtered(&self, disabled: &[String]) -> Vec<CapabilityMeta> {
        self.metas()
            .into_iter()
            .filter(|m| !is_path_disabled(&m.api_path, disabled))
            .collect()
    }

    pub fn get_by_api_path(&self, path: &[String]) -> Option<Arc<dyn Capability>> {
        self.inner.read().unwrap().by_api_path.get(path).cloned()
    }

    pub fn get_by_tool_name(&self, name: &str) -> Option<Arc<dyn Capability>> {
        self.inner.read().unwrap().by_tool_name.get(name).cloned()
    }
}
