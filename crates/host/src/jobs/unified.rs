//! Unified job inspection/control across scripts, delegates, and agent flows:
//! - Steel capabilities: `jobs/list`, `jobs/tail`, `jobs/cancel`
//! - Shared MCP `jobs` tool schema, typed request parser, and markdown formatters.

use crate::capability::{
    is_path_disabled, ArgumentMode, CallContext, Capability, CapabilityCatalog, CapabilityError,
    CapabilityMeta,
};
use crate::jobs::MAX_OUTPUT_REPLY_BYTES;
use async_trait::async_trait;
use cant_protocol::daemon::{JobSummary, WorkspaceSummary};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

pub const DEFAULT_TAIL_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobKind {
    Script,
    Delegate,
    Flow,
}

impl JobKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Script => "script",
            Self::Delegate => "delegate",
            Self::Flow => "flow",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum JobsToolRequest {
    List {
        #[serde(default, rename = "activeOnly")]
        active_only: bool,
        #[serde(default)]
        kind: Option<JobKind>,
        #[serde(default, rename = "allSessions")]
        all_sessions: bool,
    },
    Tail {
        id: String,
        #[serde(default, rename = "maxBytes")]
        max_bytes: Option<usize>,
        #[serde(default, rename = "allSessions")]
        all_sessions: bool,
    },
    Cancel {
        id: String,
        #[serde(default, rename = "allSessions")]
        all_sessions: bool,
    },
}

impl JobsToolRequest {
    pub fn from_value(mut value: Value) -> Result<Self, serde_json::Error> {
        if let Some(obj) = value.as_object_mut() {
            if !obj.contains_key("op") {
                obj.insert("op".into(), Value::String("list".into()));
            }
        }
        serde_json::from_value(value)
    }
}

pub fn is_active_status(status: &str) -> bool {
    matches!(status, "running" | "queued")
}

pub fn sort_jobs_for_display(jobs: &mut [JobSummary]) {
    jobs.sort_by(|a, b| {
        let a_active = is_active_status(&a.status);
        let b_active = is_active_status(&b.status);
        b_active
            .cmp(&a_active)
            .then_with(|| a.elapsed_ms.cmp(&b.elapsed_ms))
            .then_with(|| a.id.cmp(&b.id))
    });
}

pub fn filter_jobs(
    mut jobs: Vec<JobSummary>,
    active_only: bool,
    kind: Option<JobKind>,
) -> Vec<JobSummary> {
    jobs.retain(|j| {
        (!active_only || is_active_status(&j.status)) && kind.is_none_or(|k| j.kind == k.as_str())
    });
    sort_jobs_for_display(&mut jobs);
    jobs
}

pub fn format_elapsed(elapsed_ms: u64) -> String {
    if elapsed_ms < 1_000 {
        format!("{elapsed_ms}ms")
    } else if elapsed_ms < 60_000 {
        format!("{:.1}s", elapsed_ms as f64 / 1_000.0)
    } else {
        let secs = elapsed_ms / 1_000;
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

pub fn format_status_markdown(
    workspaces: &[WorkspaceSummary],
    jobs: &[JobSummary],
    session_filter: Option<&str>,
    active_only: bool,
    kind: Option<JobKind>,
) -> String {
    let mut lines = Vec::new();

    if let Some(session_id) = session_filter {
        if let Some(ws) = workspaces.iter().find(|w| w.session_id == session_id) {
            let state = if ws.active { "active" } else { "idle" };
            lines.push(format!(
                "Workspace: `{}` · g{} ({}, {}, `{}`)",
                ws.workspace_id, ws.generation, ws.language, state, ws.cwd
            ));
        }
    } else if !workspaces.is_empty() {
        lines.push(format!("Workspaces ({}):", workspaces.len()));
        for ws in workspaces {
            let state = if ws.active { "active" } else { "idle" };
            lines.push(format!(
                "- `{}` (`{}`) · g{} · {} · `{}`",
                ws.session_id, ws.workspace_id, ws.generation, state, ws.cwd
            ));
        }
    }

    let filtered = filter_jobs(jobs.to_vec(), active_only, kind);
    let active_count = filtered
        .iter()
        .filter(|j| is_active_status(&j.status))
        .count();

    if filtered.is_empty() {
        let qualifier = match (active_only, kind) {
            (true, Some(k)) => format!("active {} ", k.as_str()),
            (true, None) => "active ".into(),
            (false, Some(k)) => format!("{} ", k.as_str()),
            (false, None) => String::new(),
        };
        let scope = if session_filter.is_some() {
            " in this session"
        } else {
            ""
        };
        lines.push(format!("No {qualifier}jobs{scope}."));
        return lines.join("\n");
    }

    lines.push(format!(
        "Jobs ({} total, {} active):",
        filtered.len(),
        active_count
    ));

    if session_filter.is_some() {
        for j in &filtered {
            lines.push(format_job_row(j));
        }
    } else {
        let mut by_session: BTreeMap<&str, Vec<&JobSummary>> = BTreeMap::new();
        for j in &filtered {
            by_session.entry(&j.session_id).or_default().push(j);
        }
        for (sid, group) in by_session {
            lines.push(format!("### Session `{sid}`"));
            for j in group {
                lines.push(format_job_row(j));
            }
        }
    }

    lines.join("\n")
}

fn format_job_row(j: &JobSummary) -> String {
    let icon = match j.status.as_str() {
        "running" => "▶",
        "queued" => "⏳",
        "completed" | "succeeded" => "✓",
        "failed" => "✗",
        "cancelled" => "⊘",
        _ => "•",
    };
    let elapsed = format_elapsed(j.elapsed_ms);
    if j.summary.is_empty() || j.summary == j.name {
        format!(
            "- {icon} `{}` [{} · {} · {}] **{}**",
            j.id, j.kind, j.status, elapsed, j.name
        )
    } else {
        format!(
            "- {icon} `{}` [{} · {} · {}] **{}** — {}",
            j.id, j.kind, j.status, elapsed, j.name, j.summary
        )
    }
}

pub fn format_job_tail(
    summary: Option<&JobSummary>,
    id: &str,
    kind: &str,
    status: &str,
    output: &str,
) -> String {
    let mut lines = Vec::new();
    if let Some(s) = summary {
        lines.push(format!(
            "Job `{}` — **{}** [{} · {} · {}]",
            s.id,
            s.name,
            s.kind,
            s.status,
            format_elapsed(s.elapsed_ms)
        ));
        if !s.summary.is_empty() && s.summary != s.name {
            lines.push(format!("Summary: {}", s.summary));
        }
    } else {
        lines.push(format!("Job `{id}` [{kind} · {status}]"));
    }

    if output.trim().is_empty() {
        lines.push("(no output yet)".into());
    } else {
        lines.push(String::new());
        lines.push(output.trim_end_matches('\n').to_string());
    }
    lines.join("\n")
}

fn join_job_labels(items: &[&str]) -> String {
    match items {
        [] => "background jobs".into(),
        [a] => (*a).to_string(),
        [a, b] => format!("{a} and {b}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

pub fn jobs_tool_entry(disabled_namespaces: &[String]) -> Value {
    let mut kinds = Vec::new();
    let mut labels = Vec::new();
    if !is_path_disabled(&["background".into(), "start".into()], disabled_namespaces) {
        kinds.push("script");
        labels.push("background scripts");
    }
    if !is_path_disabled(&["delegate".into(), "start".into()], disabled_namespaces) {
        kinds.push("delegate");
        labels.push("delegated workers");
    }
    if !is_path_disabled(&["agents".into(), "startFlow".into()], disabled_namespaces) {
        kinds.push("flow");
        labels.push("agent flows");
    }
    if kinds.is_empty() {
        kinds.extend(["script", "delegate", "flow"]);
    }
    let description = format!(
        "Inspect or control Cant {} without running Scheme code.\n\nOperations (`op`, defaults to `list`): `list` (workspace + job status summary), `tail` (non-destructive status + bounded live monitor/output for `id`), `cancel` (stop a running job by `id`). Scoped to the current session by default; pass `allSessions: true` to inspect or cancel across sessions.",
        join_job_labels(&labels)
    );

    json!({
        "name": "jobs",
        "title": "Cant Jobs",
        "description": description,
        "inputSchema": {
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "op": {
                    "type": "string",
                    "enum": ["list", "tail", "cancel"],
                    "description": "Operation to perform (default: list)."
                },
                "id": {
                    "type": "string",
                    "description": "Job ID (required for tail and cancel)."
                },
                "kind": {
                    "type": "string",
                    "enum": kinds,
                    "description": "Optional kind filter for list."
                },
                "activeOnly": {
                    "type": "boolean",
                    "description": "When true, list only queued/running jobs."
                },
                "allSessions": {
                    "type": "boolean",
                    "description": "When true, include or allow accessing jobs from other sessions."
                },
                "maxBytes": {
                    "type": "integer",
                    "minimum": 256,
                    "maximum": MAX_OUTPUT_REPLY_BYTES,
                    "description": "Maximum tail bytes to return (default: 8192)."
                },
                "sessionId": {
                    "type": "string",
                    "description": "Optional workspace session ID (injected automatically by hook)."
                },
                "cwd": {
                    "type": "string",
                    "description": "Optional working directory (injected automatically by hook)."
                }
            }
        }
    })
}

fn steel_hints(positional: &[&str], keywords: &[(&str, &str)]) -> Map<String, Value> {
    let mut inner = Map::new();
    if !positional.is_empty() {
        inner.insert("positional".into(), json!(positional));
    }
    if !keywords.is_empty() {
        let mut kw = Map::new();
        for (json_key, scheme_kw) in keywords {
            kw.insert((*json_key).into(), Value::String((*scheme_kw).into()));
        }
        inner.insert("keywords".into(), Value::Object(kw));
    }
    let mut m = Map::new();
    m.insert("steel".into(), Value::Object(inner));
    m
}

struct JobsList(CapabilityMeta);
#[async_trait]
impl Capability for JobsList {
    fn meta(&self) -> &CapabilityMeta {
        &self.0
    }
    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let active_only = args
            .get("activeOnly")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let kind = match args.get("kind") {
            None | Some(Value::Null) => None,
            Some(v) => Some(serde_json::from_value::<JobKind>(v.clone()).map_err(|_| {
                CapabilityError::invalid_args("jobs.list", "kind must be script, delegate, or flow")
            })?),
        };
        let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(20)
            .clamp(1, 50) as usize;

        let all = filter_jobs(
            ctx.jobs.list_all_jobs(Some(&ctx.session_id)),
            active_only,
            kind,
        );
        let total = all.len();
        let active_count = all.iter().filter(|j| is_active_status(&j.status)).count();
        let page: Vec<Value> = all
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|j| {
                json!({
                    "id": j.id,
                    "kind": j.kind,
                    "name": j.name,
                    "status": j.status,
                    "elapsedMs": j.elapsed_ms,
                    "summary": j.summary,
                })
            })
            .collect();
        let returned = page.len();
        Ok(json!({
            "jobs": page,
            "total": total,
            "activeCount": active_count,
            "offset": offset,
            "limit": limit,
            "returned": returned,
            "hasMore": offset + returned < total,
        }))
    }
}

struct JobsTail(CapabilityMeta);
#[async_trait]
impl Capability for JobsTail {
    fn meta(&self) -> &CapabilityMeta {
        &self.0
    }
    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let id = args
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| CapabilityError::invalid_args("jobs.tail", "missing non-empty 'id'"))?;
        let max_bytes = args
            .get("maxBytes")
            .and_then(Value::as_u64)
            .map(|n| (n as usize).clamp(256, MAX_OUTPUT_REPLY_BYTES))
            .unwrap_or(DEFAULT_TAIL_BYTES);

        let tail = ctx
            .jobs
            .tail_job(id, max_bytes, Some(&ctx.session_id))
            .map_err(|e| CapabilityError::exec("JobError", e.to_string()))?;

        Ok(json!({
            "id": tail.summary.id,
            "kind": tail.summary.kind,
            "name": tail.summary.name,
            "status": tail.summary.status,
            "elapsedMs": tail.summary.elapsed_ms,
            "summary": tail.summary.summary,
            "output": tail.output,
        }))
    }
}

struct JobsCancel(CapabilityMeta);
#[async_trait]
impl Capability for JobsCancel {
    fn meta(&self) -> &CapabilityMeta {
        &self.0
    }
    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let id = args
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                CapabilityError::invalid_args("jobs.cancel", "missing non-empty 'id'")
            })?;

        ctx.jobs
            .cancel_any_job(id, Some(&ctx.session_id))
            .map_err(|e| CapabilityError::exec("JobError", e.to_string()))?;

        Ok(json!({
            "id": id,
            "cancelled": true,
        }))
    }
}

pub fn register(catalog: &CapabilityCatalog) {
    catalog.register(Arc::new(JobsList(CapabilityMeta {
        tool_name: "__cant_jobs_list".into(),
        api_path: vec!["jobs".into(), "list".into()],
        description: "List all jobs (scripts, delegates, and agent flows) owned by this session, active first.".into(),
        params_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "activeOnly": { "type": "boolean" },
                "kind": { "type": "string", "enum": ["script", "delegate", "flow"] },
                "offset": { "type": "integer", "minimum": 0 },
                "limit": { "type": "integer", "minimum": 1, "maximum": 50 }
            }
        }),
        result_schema: json!({ "type": "object" }),
        argument_mode: ArgumentMode::Single,
        language_hints: steel_hints(&[], &[("activeOnly", "active-only")]),
    })));

    catalog.register(Arc::new(JobsTail(CapabilityMeta {
        tool_name: "__cant_jobs_tail".into(),
        api_path: vec!["jobs".into(), "tail".into()],
        description: "Inspect status and non-destructive live monitor/output tail for any job (script, delegate, or agent flow).".into(),
        params_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["id"],
            "properties": {
                "id": { "type": "string" },
                "maxBytes": { "type": "integer", "minimum": 256, "maximum": MAX_OUTPUT_REPLY_BYTES }
            }
        }),
        result_schema: json!({ "type": "object" }),
        argument_mode: ArgumentMode::Single,
        language_hints: steel_hints(&["id"], &[("maxBytes", "max-bytes")]),
    })));

    catalog.register(Arc::new(JobsCancel(CapabilityMeta {
        tool_name: "__cant_jobs_cancel".into(),
        api_path: vec!["jobs".into(), "cancel".into()],
        description: "Cancel any running job (script, delegate, or agent flow) owned by this session.".into(),
        params_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["id"],
            "properties": {
                "id": { "type": "string" }
            }
        }),
        result_schema: json!({ "type": "object" }),
        argument_mode: ArgumentMode::Single,
        language_hints: steel_hints(&["id"], &[]),
    })));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jobs_tool_entry_filters_description_and_kinds_for_disabled_namespaces() {
        let all = jobs_tool_entry(&[]);
        let all_desc = all["description"].as_str().unwrap();
        assert!(all_desc.contains("background scripts, delegated workers, and agent flows"));
        assert_eq!(
            all["inputSchema"]["properties"]["kind"]["enum"],
            json!(["script", "delegate", "flow"])
        );

        let scripts_only = jobs_tool_entry(&["delegate".into(), "agents".into()]);
        let scripts_desc = scripts_only["description"].as_str().unwrap();
        assert!(scripts_desc.contains("background scripts"));
        assert!(!scripts_desc.contains("delegated workers"));
        assert!(!scripts_desc.contains("agent flows"));
        assert_eq!(
            scripts_only["inputSchema"]["properties"]["kind"]["enum"],
            json!(["script"])
        );

        let no_delegate = jobs_tool_entry(&["delegate".into()]);
        let no_delegate_desc = no_delegate["description"].as_str().unwrap();
        assert!(no_delegate_desc.contains("background scripts and agent flows"));
        assert!(!no_delegate_desc.contains("delegated workers"));
        assert_eq!(
            no_delegate["inputSchema"]["properties"]["kind"]["enum"],
            json!(["script", "flow"])
        );
    }
}
