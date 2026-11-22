//! Codex CLI custom statusline renderer (`tui_extensions_statusline`).
//!
//! Renders Codex's thread snapshot alongside live Cant workspace and job state
//! into the declarative JSON `UiTree` expected by Codex's `resources/read`
//! statusline provider contract.

use cant_protocol::daemon::{JobSummary, WorkspaceSummary};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;

pub const STATUSLINE_RESOURCE_URI: &str = "ui://cant/statusline";
pub const STATUSLINE_META_KEY: &str = "com.openai/codex-statusline-state";

const MAX_JOB_NAME_CHARS: usize = 24;

#[derive(Debug, Default, Clone)]
pub struct CantStatusSnapshot {
    pub workspace: Option<WorkspaceSummary>,
    pub jobs: Vec<JobSummary>,
}

#[derive(Debug, Default, Deserialize)]
struct CodexStatuslineState {
    #[serde(default)]
    account: CodexAccount,
    #[serde(default)]
    thread: CodexThread,
}

#[derive(Debug, Default, Deserialize)]
struct CodexAccount {
    #[serde(default)]
    rate_limits: Vec<CodexRateLimitSnapshot>,
}

#[derive(Debug, Default, Deserialize)]
struct CodexRateLimitSnapshot {
    #[serde(default)]
    primary_limit: Option<CodexRateLimitWindow>,
}

#[derive(Debug, Default, Deserialize)]
struct CodexRateLimitWindow {
    used_percent: f64,
    #[serde(default)]
    window_minutes: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct CodexThread {
    #[serde(default)]
    agent: CodexAgent,
    #[serde(default)]
    model: CodexModel,
    #[serde(default)]
    workspace: CodexWorkspace,
    #[serde(default)]
    usage: CodexUsage,
}

#[derive(Debug, Default, Deserialize)]
struct CodexAgent {
    #[serde(default)]
    state: String,
}

#[derive(Debug, Default, Deserialize)]
struct CodexModel {
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    reasoning_level: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct CodexWorkspace {
    #[serde(default)]
    cwd: String,
}

#[derive(Debug, Default, Deserialize)]
struct CodexUsage {
    #[serde(default)]
    context: CodexContext,
}

#[derive(Debug, Default, Deserialize)]
struct CodexContext {
    #[serde(default)]
    context_window_size: Option<u64>,
    #[serde(default)]
    used_tokens: Option<u64>,
    #[serde(default)]
    used_percentage: Option<f64>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum UiNode {
    Row {
        props: RowProps,
        children: Vec<UiNode>,
    },
    Text {
        props: TextProps,
        children: Vec<UiNode>,
    },
    Italic {
        children: Vec<UiNode>,
    },
    #[serde(untagged)]
    Leaf(String),
}

#[derive(Debug, Serialize)]
struct RowProps {
    gap: u64,
    wrap: &'static str,
}

#[derive(Debug, Default, Serialize)]
struct TextProps {
    #[serde(skip_serializing_if = "Option::is_none")]
    color: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    weight: Option<&'static str>,
}

fn text_node(color: &'static str, content: impl Into<String>) -> UiNode {
    UiNode::Text {
        props: TextProps {
            color: Some(color),
            weight: None,
        },
        children: vec![UiNode::Leaf(content.into())],
    }
}

fn bold_text_node(color: &'static str, content: impl Into<String>) -> UiNode {
    UiNode::Text {
        props: TextProps {
            color: Some(color),
            weight: Some("bold"),
        },
        children: vec![UiNode::Leaf(content.into())],
    }
}

pub fn codex_statusline_session(state: &Value) -> Option<String> {
    let thread_id = state.pointer("/thread/id")?.as_str()?.trim();
    (!thread_id.is_empty()).then(|| format!("codex-{thread_id}"))
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let prefix: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

fn format_window_label(minutes: Option<u64>) -> String {
    match minutes {
        Some(m) if m >= 1440 && m % 1440 == 0 => format!("{}d", m / 1440),
        Some(m) if m >= 60 && m % 60 == 0 => format!("{}h", m / 60),
        Some(m) if m > 0 => format!("{m}m"),
        _ => "limit".into(),
    }
}

fn context_used_pct(ctx: &CodexContext) -> Option<u64> {
    if let Some(pct) = ctx.used_percentage {
        if pct.is_finite() && pct >= 0.0 {
            return Some(pct.round() as u64);
        }
    }
    match (ctx.used_tokens, ctx.context_window_size) {
        (Some(used), Some(size)) if size > 0 => Some(((used as f64 / size as f64) * 100.0).round() as u64),
        _ => None,
    }
}

pub fn render_statusline(state_val: &Value, cant: &CantStatusSnapshot) -> Value {
    let state: CodexStatuslineState =
        serde_json::from_value(state_val.clone()).unwrap_or_default();
    let mut children: Vec<UiNode> = Vec::new();

    // 1. Model + reasoning level
    let model_name = state.thread.model.display_name.trim();
    if !model_name.is_empty() {
        let label = match state
            .thread
            .model
            .reasoning_level
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(level) => format!("{model_name} ({level})"),
            None => model_name.to_string(),
        };
        children.push(text_node("status-model", label));
    }

    // 2. Active run state (omit idle "ready")
    let agent_state = state.thread.agent.state.trim();
    if !agent_state.is_empty() && agent_state != "ready" {
        children.push(UiNode::Italic {
            children: vec![text_node("status-state", agent_state)],
        });
    }

    // 3. Context usage
    if let Some(pct) = context_used_pct(&state.thread.usage.context) {
        children.push(text_node("status-usage", format!("{pct}% ctx")));
    }

    // 4. Short workspace directory name
    let cwd = state.thread.workspace.cwd.trim();
    if !cwd.is_empty() {
        let dir_name = Path::new(cwd)
            .file_name()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(cwd);
        children.push(text_node("status-path", dir_name));
    }

    // 5. Cant REPL workspace badge
    if let Some(ws) = &cant.workspace {
        let badge = format!("repl g{}", ws.generation);
        children.push(text_node("status-thread", badge));
    } else {
        children.push(text_node("status-metadata", "repl"));
    }

    // 6. Cant jobs badge (Pi-style ▶ N jobs)
    let running: Vec<&JobSummary> = cant
        .jobs
        .iter()
        .filter(|j| matches!(j.status.as_str(), "running" | "queued"))
        .collect();
    if running.len() == 1 {
        let name = truncate_chars(running[0].name.trim(), MAX_JOB_NAME_CHARS);
        let badge = if name.is_empty() {
            "▶ 1 job".to_string()
        } else {
            format!("▶ 1 job ({name})")
        };
        children.push(bold_text_node("accent", badge));
    } else if running.len() > 1 {
        children.push(bold_text_node("accent", format!("▶ {} jobs", running.len())));
    } else {
        let failed = cant
            .jobs
            .iter()
            .filter(|j| matches!(j.status.as_str(), "failed" | "error"))
            .count();
        if failed > 0 {
            children.push(text_node("red", format!("{failed} failed")));
        }
    }

    // 7. Primary rate limit (compact)
    if let Some(limit) = state
        .account
        .rate_limits
        .iter()
        .find_map(|rl| rl.primary_limit.as_ref())
    {
        if limit.used_percent.is_finite() && limit.used_percent >= 50.0 {
            let win = format_window_label(limit.window_minutes);
            children.push(text_node(
                "status-limit",
                format!("{win} {}%", limit.used_percent.round() as u64),
            ));
        }
    }

    serde_json::to_value(UiNode::Row {
        props: RowProps {
            gap: 1,
            wrap: "wrap",
        },
        children,
    })
    .unwrap_or_else(|_| json!(null))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_state() -> Value {
        json!({
            "account": {
                "rate_limits": [
                    {
                        "limit_id": "codex",
                        "limit_name": null,
                        "primary_limit": { "used_percent": 64.2, "window_minutes": 300, "resets_at": null },
                        "secondary_limit": null
                    }
                ]
            },
            "app": { "cli_version": "0.171.0", "display_mode": "standard" },
            "config": { "statusline_items": [] },
            "thread": {
                "id": "019b-thread-1",
                "name": null,
                "agent": { "state": "working" },
                "model": {
                    "display_name": "GPT-6.2",
                    "provider": "openai",
                    "reasoning_level": "high"
                },
                "workspace": { "cwd": "/workspace/cant" },
                "usage": {
                    "context": {
                        "context_window_size": 200000,
                        "used_tokens": 50000,
                        "used_percentage": 25.0
                    }
                }
            }
        })
    }

    fn collect_text(node: &Value) -> String {
        match node {
            Value::String(s) => s.clone(),
            Value::Object(map) => map
                .get("children")
                .and_then(Value::as_array)
                .map(|arr| arr.iter().map(collect_text).collect::<Vec<_>>().join(" "))
                .unwrap_or_default(),
            _ => String::new(),
        }
    }

    fn count_nodes(node: &Value) -> usize {
        match node {
            Value::Null => 0,
            Value::String(_) => 1,
            Value::Object(map) => {
                1 + map
                    .get("children")
                    .and_then(Value::as_array)
                    .map(|arr| arr.iter().map(count_nodes).sum::<usize>())
                    .unwrap_or(0)
            }
            _ => 1,
        }
    }

    #[test]
    fn derives_codex_session_id_matching_mcp_convention() {
        assert_eq!(
            codex_statusline_session(&sample_state()).as_deref(),
            Some("codex-019b-thread-1")
        );
        assert_eq!(codex_statusline_session(&json!({"thread": {"id": "  "}})), None);
        assert_eq!(codex_statusline_session(&json!({})), None);
    }

    #[test]
    fn renders_model_context_cwd_workspace_and_running_jobs_in_valid_ui_tree() {
        let cant = CantStatusSnapshot {
            workspace: Some(WorkspaceSummary {
                session_id: "codex-019b-thread-1".into(),
                cwd: "/workspace/cant".into(),
                language: "steel".into(),
                workspace_id: "abcd1234-5678".into(),
                generation: 3,
                active: true,
            }),
            jobs: vec![
                JobSummary {
                    id: "job-1".into(),
                    kind: "background".into(),
                    name: "cargo test".into(),
                    session_id: "codex-019b-thread-1".into(),
                    status: "running".into(),
                    elapsed_ms: 4200,
                    summary: "running".into(),
                },
                JobSummary {
                    id: "job-2".into(),
                    kind: "delegate".into(),
                    name: "review".into(),
                    session_id: "codex-019b-thread-1".into(),
                    status: "queued".into(),
                    elapsed_ms: 100,
                    summary: "queued".into(),
                },
            ],
        };

        let tree = render_statusline(&sample_state(), &cant);
        assert_eq!(tree["type"], "row");
        assert!(tree["props"]["gap"].as_u64().is_some());
        assert!(count_nodes(&tree) <= 256);
        assert!(serde_json::to_string(&tree).unwrap().len() <= 64 * 1024);

        let text = collect_text(&tree);
        assert!(text.contains("GPT-6.2"), "missing model in: {text}");
        assert!(text.contains("high"), "missing reasoning in: {text}");
        assert!(text.contains("working"), "missing agent state in: {text}");
        assert!(text.contains("25%"), "missing context % in: {text}");
        assert!(text.contains("cant"), "missing cwd/cant in: {text}");
        assert!(text.contains("repl g3"), "missing workspace generation in: {text}");
        assert!(text.contains("▶ 2 jobs"), "missing running jobs badge in: {text}");
        assert!(text.contains("5h 64%"), "missing rate limit in: {text}");
    }

    #[test]
    fn renders_single_running_job_name_and_handles_minimal_state() {
        let cant = CantStatusSnapshot {
            workspace: None,
            jobs: vec![JobSummary {
                id: "job-1".into(),
                kind: "background".into(),
                name: "build-check".into(),
                session_id: "codex-t1".into(),
                status: "running".into(),
                elapsed_ms: 500,
                summary: "".into(),
            }],
        };

        let tree = render_statusline(&json!({}), &cant);
        assert_eq!(tree["type"], "row");
        let text = collect_text(&tree);
        assert!(text.contains("▶ 1 job (build-check)"), "unexpected text: {text}");
    }

    #[test]
    fn omits_ready_state_and_shows_failed_job_count_when_idle() {
        let mut state = sample_state();
        state["thread"]["agent"]["state"] = json!("ready");
        let cant = CantStatusSnapshot {
            workspace: None,
            jobs: vec![JobSummary {
                id: "job-9".into(),
                kind: "background".into(),
                name: "lint".into(),
                session_id: "codex-019b-thread-1".into(),
                status: "failed".into(),
                elapsed_ms: 1200,
                summary: "exit 1".into(),
            }],
        };

        let tree = render_statusline(&state, &cant);
        let text = collect_text(&tree);
        assert!(!text.contains("ready"), "should omit ready state: {text}");
        assert!(text.contains("1 failed"), "missing failed job badge: {text}");
    }
}
