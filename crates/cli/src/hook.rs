//! Lifecycle hook handlers for Antigravity and Codex CLI:
//! - `cant hook pre-tool [--strict] [--host auto|agy|codex]`: injects `sessionId` and `cwd` into
//!   MCP `repl` tool arguments, and in strict mode denies direct workspace file/shell tools so the
//!   agent routes through `cant` `repl` instead.
//! - `cant hook pre-invocation [--host ...]`: drains active + passive completed job notices into
//!   the turn (`systemMessage` for Antigravity, `UserPromptSubmit` `additionalContext` for Codex).
//! - `cant hook post-tool [--host ...]`: drains completed job notices after a tool call in Codex
//!   (`PostToolUse` `additionalContext`).
//! - `cant hook stop [--host ...]`: if an active job finished (and continuation isn't already
//!   active), asks the host to continue (`decision: "continue"` for Antigravity, `decision: "block"`
//!   for Codex).

use cant_host::daemon::send_daemon_request_if_running;
use cant_protocol::daemon::{ChannelNotice, CompletionNotice, DaemonRequest, DaemonResponse, HarnessKind};
use clap::ValueEnum;
use serde_json::{json, Map, Value};
use std::io::Read;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum HookHost {
    Auto,
    Agy,
    Codex,
}

const AGY_STRICT_BLOCKED_TOOLS: &[&str] = &[
    "read_file",
    "view_file",
    "write_file",
    "write_to_file",
    "edit_file",
    "replace_file_content",
    "multi_replace_file_content",
    "run_command",
    "grep_search",
    "find_by_name",
    "list_dir",
];

const CODEX_STRICT_BLOCKED_TOOLS: &[&str] = &["Bash", "apply_patch"];

const MEDIA_EXTENSIONS: &[&str] = &[
    ".png", ".jpg", ".jpeg", ".gif", ".webp", ".svg", ".ico", ".bmp",
    ".pdf", ".mp3", ".wav", ".ogg", ".flac", ".m4a",
    ".mp4", ".webm", ".mov", ".avi", ".mkv",
];

fn parse_env_bool(val: &str) -> Option<bool> {
    match val.trim().to_ascii_lowercase().as_str() {
        "0" | "false" | "off" | "no" => Some(false),
        "1" | "true" | "on" | "yes" => Some(true),
        _ => None,
    }
}

pub fn resolve_strict(cli_flag: bool, config_strict: Option<bool>) -> bool {
    if let Some(parsed) = std::env::var("CANT_STRICT")
        .ok()
        .as_deref()
        .and_then(parse_env_bool)
    {
        return parsed;
    }
    config_strict.unwrap_or(cli_flag)
}

fn resolve_host(requested: HookHost, payload: &Value) -> HookHost {
    match requested {
        HookHost::Agy => HookHost::Agy,
        HookHost::Codex => HookHost::Codex,
        HookHost::Auto => {
            if payload.get("hook_event_name").is_some()
                || payload.get("session_id").is_some()
                || payload.get("tool_name").is_some()
            {
                HookHost::Codex
            } else {
                HookHost::Agy
            }
        }
    }
}

fn read_stdin_json() -> Result<Value, String> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|e| format!("Failed reading hook stdin: {e}"))?;
    Ok(serde_json::from_str(&input).unwrap_or(Value::Null))
}

fn extract_agy_context(payload: &Value) -> (Option<String>, Option<String>) {
    let worker_flag = std::env::var("CANT_WORKER").ok();
    let worker_sid = std::env::var("CANT_WORKER_SESSION_ID").ok();
    extract_agy_context_with_env(payload, worker_flag.as_deref(), worker_sid.as_deref())
}

fn extract_agy_context_with_env(
    payload: &Value,
    worker_flag: Option<&str>,
    worker_sid: Option<&str>,
) -> (Option<String>, Option<String>) {
    let override_sid = if worker_flag == Some("1") {
        worker_sid.map(str::trim).filter(|s| !s.is_empty()).map(ToString::to_string)
    } else {
        None
    };
    let conv_id = override_sid.or_else(|| {
        payload
            .get("conversationId")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(ToString::to_string)
    });

    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            payload
                .get("workspacePaths")
                .and_then(Value::as_array)
                .and_then(|arr| arr.first())
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
        })
        .map(ToString::to_string);

    (conv_id, cwd)
}

fn extract_codex_context(payload: &Value) -> (Option<String>, Option<String>) {
    let session_id = payload
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| format!("codex-{s}"));

    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string);

    (session_id, cwd)
}

fn inject_fields(
    target: &mut Map<String, Value>,
    session_id: Option<String>,
    cwd: Option<String>,
) -> bool {
    let mut changed = false;
    if !target.contains_key("sessionId") {
        if let Some(id) = session_id {
            target.insert("sessionId".into(), Value::String(id));
            changed = true;
        }
    }
    if !target.contains_key("cwd") {
        if let Some(dir) = cwd {
            target.insert("cwd".into(), Value::String(dir));
            changed = true;
        }
    }
    changed
}

fn extract_target_path(args: Option<&Map<String, Value>>) -> Option<&str> {
    let args = args?;
    for key in ["Path", "path", "TargetFile", "AbsolutePath", "FilePath", "DirectoryPath"] {
        if let Some(s) = args.get(key).and_then(Value::as_str) {
            return Some(s);
        }
    }
    None
}

fn is_allowed_internal_or_media_path(tool_name: &str, path: &str) -> bool {
    // Allow Antigravity internal metadata (MCP schemas, artifacts, skills, transcripts) under /.gemini/
    if path.contains("/.gemini/") || path.starts_with(".gemini/") {
        return true;
    }
    // Allow reading binary/media files that Cant's UTF-8 text file APIs don't handle.
    if matches!(tool_name, "read_file" | "view_file") {
        let lower = path.to_ascii_lowercase();
        if MEDIA_EXTENSIONS.iter().any(|ext| lower.ends_with(ext)) {
            return true;
        }
    }
    false
}

fn strict_redirect_reason(tool_name: &str) -> &'static str {
    match tool_name {
        "read_file" | "view_file" => "Strict Cant mode: use cant repl with (files/read path) or (files/read-many paths) instead.",
        "write_file" | "write_to_file" => "Strict Cant mode: use cant repl with (files/write path content) or (files/replace path content #:expected-sha256 sha) instead.",
        "edit_file" | "replace_file_content" | "multi_replace_file_content" | "apply_patch" => "Strict Cant mode: use cant repl with (files/edit path (list (replace-text old new))) or (files/write ...) instead.",
        "grep_search" => "Strict Cant mode: use cant repl with (files/grep pattern ...) instead.",
        "find_by_name" => "Strict Cant mode: use cant repl with (files/find pattern ...) instead.",
        "list_dir" => "Strict Cant mode: use cant repl with (files/list path ...) instead.",
        "run_command" | "Bash" => "Strict Cant mode: use cant repl with (system/exec cmd args), (system/pipeline ...), (scripts/run ...), or (background/start ...) instead.",
        _ => "Strict Cant mode: use the cant repl tool instead.",
    }
}

pub fn run_pre_tool(strict: bool, host: HookHost) -> Result<(), String> {
    let payload = read_stdin_json()?;
    let response = process_pre_tool_for_host(&payload, strict, host);
    println!("{}", serde_json::to_string(&response).unwrap());
    Ok(())
}

pub fn process_pre_tool_for_host(payload: &Value, strict: bool, host: HookHost) -> Value {
    match resolve_host(host, payload) {
        HookHost::Codex => process_codex_pre_tool(payload, strict),
        HookHost::Agy | HookHost::Auto => process_agy_pre_tool(payload, strict),
    }
}

fn is_cant_tool_suffix(tool_name: &str, suffix: &str) -> bool {
    tool_name == suffix
        || (tool_name.contains("cant")
            && (tool_name.ends_with(&format!("__{suffix}"))
                || tool_name.ends_with(&format!("_{suffix}"))))
}

fn is_cant_mcp_tool_name(tool_name: &str) -> bool {
    is_cant_tool_suffix(tool_name, "repl") || is_cant_tool_suffix(tool_name, "jobs")
}

fn process_codex_pre_tool(payload: &Value, strict: bool) -> Value {
    let tool_name = payload
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let (session_id, cwd) = extract_codex_context(payload);

    if strict && CODEX_STRICT_BLOCKED_TOOLS.contains(&tool_name) {
        return json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": strict_redirect_reason(tool_name),
            }
        });
    }

    if is_cant_mcp_tool_name(tool_name) {
        let mut updated = payload
            .get("tool_input")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if inject_fields(&mut updated, session_id, cwd) {
            return json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "allow",
                    "updatedInput": updated,
                }
            });
        }
    }

    json!({})
}

fn process_agy_pre_tool(payload: &Value, strict: bool) -> Value {
    let tool_name = payload
        .get("toolCall")
        .and_then(|tc| tc.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let existing_args = payload
        .get("toolCall")
        .and_then(|tc| tc.get("args"))
        .and_then(Value::as_object);
    let (conv_id, cwd) = extract_agy_context(payload);

    if strict && AGY_STRICT_BLOCKED_TOOLS.contains(&tool_name) {
        if let Some(path) = extract_target_path(existing_args) {
            if is_allowed_internal_or_media_path(tool_name, path) {
                return json!({ "decision": "allow" });
            }
        }
        return json!({
            "decision": "deny",
            "reason": strict_redirect_reason(tool_name),
        });
    }

    if tool_name == "call_mcp_tool" {
        let Some(args) = existing_args else {
            return json!({ "decision": "allow" });
        };
        let is_cant_tool = matches!(
            args.get("ToolName").and_then(Value::as_str),
            Some("repl" | "jobs")
        ) && args
            .get("ServerName")
            .and_then(Value::as_str)
            .is_some_and(|s| s.contains("cant"));
        if !is_cant_tool {
            return json!({ "decision": "allow" });
        }
        let mut inner = args
            .get("Arguments")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if inject_fields(&mut inner, conv_id, cwd) {
            return json!({
                "decision": "allow",
                "overwrite": { "Arguments": inner }
            });
        }
        return json!({ "decision": "allow" });
    }

    if is_cant_mcp_tool_name(tool_name) || tool_name.contains("repl") {
        let mut overwrite = Map::new();
        let has_session = existing_args.and_then(|a| a.get("sessionId")).is_some();
        let has_cwd = existing_args.and_then(|a| a.get("cwd")).is_some();
        if !has_session {
            if let Some(id) = conv_id {
                overwrite.insert("sessionId".into(), Value::String(id));
            }
        }
        if !has_cwd {
            if let Some(dir) = cwd {
                overwrite.insert("cwd".into(), Value::String(dir));
            }
        }
        if !overwrite.is_empty() {
            return json!({
                "decision": "allow",
                "overwrite": overwrite
            });
        }
    }

    json!({ "decision": "allow" })
}

fn format_all_notices(notices: &[CompletionNotice], channels: &[ChannelNotice]) -> String {
    let mut sections = Vec::new();
    if !notices.is_empty() {
        let mut lines = vec!["Cant background job completions:".to_string()];
        for n in notices {
            lines.push(format!("- [{} {}] {}", n.kind, n.job_id, n.summary));
        }
        sections.push(lines.join("\n"));
    }
    if !channels.is_empty() {
        let mut lines = vec!["Cant channel messages:".to_string()];
        for m in channels {
            let label = m
                .channel_name
                .as_deref()
                .map(|n| format!("#{n}"))
                .unwrap_or_else(|| m.channel_id.clone());
            let trunc = if m.truncated { " (truncated)" } else { "" };
            lines.push(format!(
                "- [{label} from {}]{trunc} {}",
                m.from_handle, m.body
            ));
            lines.push(format!(
                "  Reply with: (channels/send \"{}\" \"...\")",
                m.channel_id
            ));
        }
        sections.push(lines.join("\n"));
    }
    sections.join("\n\n")
}

fn format_notice_response(
    host: HookHost,
    codex_event_name: &str,
    notices: &[CompletionNotice],
    channels: &[ChannelNotice],
) -> Value {
    if notices.is_empty() && channels.is_empty() {
        return json!({});
    }
    let msg = format_all_notices(notices, channels);
    match host {
        HookHost::Codex => json!({
            "hookSpecificOutput": {
                "hookEventName": codex_event_name,
                "additionalContext": msg,
            }
        }),
        HookHost::Agy | HookHost::Auto => json!({
            "injectSteps": [
                {
                    "systemMessage": {
                        "systemMessage": msg
                    }
                }
            ]
        }),
    }
}

async fn drain_session_notices(
    socket_path: &Path,
    session_id: String,
    cwd: Option<String>,
    harness: HarnessKind,
    include_passive: bool,
) -> (Vec<CompletionNotice>, Vec<ChannelNotice>) {
    let req = DaemonRequest::DrainNotices {
        session_id,
        include_passive,
        cwd,
        harness: Some(harness),
    };
    match send_daemon_request_if_running(socket_path, &req).await {
        Ok(Some(DaemonResponse::Notices {
            notices,
            channel_messages,
        })) => (notices, channel_messages),
        _ => (Vec::new(), Vec::new()),
    }
}

pub async fn run_pre_invocation(socket_path: &Path, host: HookHost) -> Result<(), String> {
    let payload = read_stdin_json()?;
    let effective = resolve_host(host, &payload);
    let (session_id, cwd, harness) = match effective {
        HookHost::Codex => {
            let (s, c) = extract_codex_context(&payload);
            (s, c, HarnessKind::Codex)
        }
        HookHost::Agy | HookHost::Auto => {
            let (s, c) = extract_agy_context(&payload);
            (s, c, HarnessKind::Antigravity)
        }
    };
    let Some(session_id) = session_id else {
        println!("{{}}");
        return Ok(());
    };

    let (notices, channels) = drain_session_notices(socket_path, session_id, cwd, harness, true).await;
    let out = format_notice_response(effective, "UserPromptSubmit", &notices, &channels);
    println!("{}", serde_json::to_string(&out).unwrap());
    Ok(())
}

pub async fn run_post_tool(socket_path: &Path, host: HookHost) -> Result<(), String> {
    let payload = read_stdin_json()?;
    let effective = resolve_host(host, &payload);
    let (session_id, cwd, harness) = match effective {
        HookHost::Codex => {
            let (s, c) = extract_codex_context(&payload);
            (s, c, HarnessKind::Codex)
        }
        HookHost::Agy | HookHost::Auto => {
            let (s, c) = extract_agy_context(&payload);
            (s, c, HarnessKind::Antigravity)
        }
    };
    let Some(session_id) = session_id else {
        println!("{{}}");
        return Ok(());
    };

    let (notices, channels) = drain_session_notices(socket_path, session_id, cwd, harness, true).await;
    let out = format_notice_response(effective, "PostToolUse", &notices, &channels);
    println!("{}", serde_json::to_string(&out).unwrap());
    Ok(())
}

fn format_codex_stop_response(
    payload: &Value,
    notices: &[CompletionNotice],
    channels: &[ChannelNotice],
) -> Value {
    let stop_hook_active = payload
        .get("stop_hook_active")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if stop_hook_active || (notices.is_empty() && channels.is_empty()) {
        return json!({});
    }
    json!({
        "decision": "block",
        "reason": format_all_notices(notices, channels),
    })
}

pub async fn run_stop(socket_path: &Path, host: HookHost) -> Result<(), String> {
    let payload = read_stdin_json()?;
    match resolve_host(host, &payload) {
        HookHost::Codex => {
            if payload
                .get("stop_hook_active")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                println!("{{}}");
                return Ok(());
            }
            let (Some(session_id), cwd) = extract_codex_context(&payload) else {
                println!("{{}}");
                return Ok(());
            };
            let (notices, channels) =
                drain_session_notices(socket_path, session_id, cwd, HarnessKind::Codex, false)
                    .await;
            let out = format_codex_stop_response(&payload, &notices, &channels);
            println!("{}", serde_json::to_string(&out).unwrap());
            Ok(())
        }
        HookHost::Agy | HookHost::Auto => {
            let continuation_active = payload
                .get("continuationActive")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let term_reason = payload
                .get("terminationReason")
                .and_then(Value::as_str)
                .unwrap_or("");

            if continuation_active || term_reason != "NO_TOOL_CALL" {
                println!("{}", json!({ "decision": "allow" }));
                return Ok(());
            }

            let (Some(conv_id), cwd) = extract_agy_context(&payload) else {
                println!("{}", json!({ "decision": "allow" }));
                return Ok(());
            };

            let (notices, channels) = drain_session_notices(
                socket_path,
                conv_id,
                cwd,
                HarnessKind::Antigravity,
                false,
            )
            .await;
            if notices.is_empty() && channels.is_empty() {
                println!("{}", json!({ "decision": "allow" }));
            } else {
                let reason = format_all_notices(&notices, &channels);
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "decision": "continue",
                        "reason": reason
                    }))
                    .unwrap()
                );
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injects_into_lazy_call_mcp_tool_arguments() {
        let payload = json!({
            "conversationId": "conv-1",
            "cwd": "/repo",
            "toolCall": {
                "name": "call_mcp_tool",
                "args": {
                    "ServerName": "cant_cant",
                    "ToolName": "repl",
                    "Arguments": { "op": "eval", "code": "(+ 1 2)" }
                }
            }
        });
        let out = process_pre_tool_for_host(&payload, true, HookHost::Auto);
        assert_eq!(out["decision"], "allow");
        assert_eq!(out["overwrite"]["Arguments"]["sessionId"], "conv-1");
        assert_eq!(out["overwrite"]["Arguments"]["cwd"], "/repo");
    }

    #[test]
    fn strict_denies_workspace_file_and_shell_tools() {
        let read_ws = json!({
            "toolCall": { "name": "read_file", "args": { "Path": "/repo/src/main.rs" } }
        });
        assert_eq!(process_pre_tool_for_host(&read_ws, true, HookHost::Auto)["decision"], "deny");
        assert_eq!(process_pre_tool_for_host(&read_ws, false, HookHost::Auto)["decision"], "allow");

        let run_cmd = json!({
            "toolCall": { "name": "run_command", "args": { "CommandLine": "ls" } }
        });
        assert_eq!(process_pre_tool_for_host(&run_cmd, true, HookHost::Auto)["decision"], "deny");
    }

    #[test]
    fn strict_allows_gemini_metadata_and_media_reads() {
        let schema_read = json!({
            "toolCall": { "name": "read_file", "args": { "Path": "/home/u/.gemini/antigravity-cli/mcp/cant_cant/repl.json" } }
        });
        assert_eq!(process_pre_tool_for_host(&schema_read, true, HookHost::Auto)["decision"], "allow");

        let img_read = json!({
            "toolCall": { "name": "read_file", "args": { "Path": "/repo/screenshot.png" } }
        });
        assert_eq!(process_pre_tool_for_host(&img_read, true, HookHost::Auto)["decision"], "allow");
    }

    #[test]
    fn codex_strict_pre_tool_denies_bash_and_apply_patch_with_hook_specific_output() {
        let bash = json!({
            "hook_event_name": "PreToolUse",
            "session_id": "019b-thread-1",
            "cwd": "/repo",
            "tool_name": "Bash",
            "tool_input": { "command": "ls -la" }
        });
        let denied = process_pre_tool_for_host(&bash, true, HookHost::Auto);
        assert_eq!(denied["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(denied["hookSpecificOutput"]["permissionDecision"], "deny");
        assert!(denied["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap()
            .contains("cant repl"));

        let allowed = process_pre_tool_for_host(&bash, false, HookHost::Codex);
        assert_eq!(allowed, json!({}));

        let patch = json!({
            "hook_event_name": "PreToolUse",
            "session_id": "019b-thread-1",
            "cwd": "/repo",
            "tool_name": "apply_patch",
            "tool_input": { "command": "*** Begin Patch" }
        });
        let patch_denied = process_pre_tool_for_host(&patch, true, HookHost::Codex);
        assert_eq!(patch_denied["hookSpecificOutput"]["permissionDecision"], "deny");
    }

    #[test]
    fn codex_pre_tool_injects_cwd_and_prefixed_session_id_into_cant_repl() {
        let repl_call = json!({
            "hook_event_name": "PreToolUse",
            "session_id": "019b-thread-2",
            "cwd": "/repo/worktree-a",
            "tool_name": "mcp__cant__repl",
            "tool_input": { "op": "eval", "code": "(+ 1 2)" }
        });
        let out = process_pre_tool_for_host(&repl_call, true, HookHost::Codex);
        assert_eq!(out["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(out["hookSpecificOutput"]["permissionDecision"], "allow");
        assert_eq!(
            out["hookSpecificOutput"]["updatedInput"]["sessionId"],
            "codex-019b-thread-2"
        );
        assert_eq!(
            out["hookSpecificOutput"]["updatedInput"]["cwd"],
            "/repo/worktree-a"
        );
        assert_eq!(
            out["hookSpecificOutput"]["updatedInput"]["code"],
            "(+ 1 2)"
        );

        let other_mcp = json!({
            "hook_event_name": "PreToolUse",
            "session_id": "019b-thread-2",
            "cwd": "/repo",
            "tool_name": "mcp__github__list_issues",
            "tool_input": { "repo": "openai/codex" }
        });
        assert_eq!(process_pre_tool_for_host(&other_mcp, true, HookHost::Codex), json!({}));
    }

    #[test]
    fn codex_notice_injection_and_stop_continuation_shapes() {
        let notices = vec![CompletionNotice {
            job_id: "bg-1".into(),
            kind: "background".into(),
            name: "cargo test".into(),
            status: "succeeded".into(),
            notify: "active".into(),
            summary: "ok (0)".into(),
            completion_token: None,
        }];

        let ups = format_notice_response(HookHost::Codex, "UserPromptSubmit", &notices, &[]);
        assert_eq!(ups["hookSpecificOutput"]["hookEventName"], "UserPromptSubmit");
        assert!(ups["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("bg-1"));

        let post = format_notice_response(HookHost::Codex, "PostToolUse", &notices, &[]);
        assert_eq!(post["hookSpecificOutput"]["hookEventName"], "PostToolUse");
        assert!(post["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("bg-1"));

        assert_eq!(format_notice_response(HookHost::Codex, "UserPromptSubmit", &[], &[]), json!({}));

        let stop_cont = format_codex_stop_response(&json!({"stop_hook_active": false}), &notices, &[]);
        assert_eq!(stop_cont["decision"], "block");
        assert!(stop_cont["reason"].as_str().unwrap().contains("bg-1"));

        let stop_guard = format_codex_stop_response(&json!({"stop_hook_active": true}), &notices, &[]);
        assert_eq!(stop_guard, json!({}));

        let stop_empty = format_codex_stop_response(&json!({"stop_hook_active": false}), &[], &[]);
        assert_eq!(stop_empty, json!({}));
    }

    #[test]
    fn injects_session_and_cwd_into_cant_jobs_mcp_tool_for_agy_and_codex() {
        let agy_lazy = json!({
            "conversationId": "conv-jobs-1",
            "cwd": "/repo",
            "toolCall": {
                "name": "call_mcp_tool",
                "args": {
                    "ServerName": "cant_cant",
                    "ToolName": "jobs",
                    "Arguments": { "op": "list" }
                }
            }
        });
        let out = process_pre_tool_for_host(&agy_lazy, true, HookHost::Auto);
        assert_eq!(out["decision"], "allow");
        assert_eq!(out["overwrite"]["Arguments"]["sessionId"], "conv-jobs-1");
        assert_eq!(out["overwrite"]["Arguments"]["cwd"], "/repo");

        let agy_direct = json!({
            "conversationId": "conv-jobs-1",
            "cwd": "/repo",
            "toolCall": {
                "name": "mcp_cant_jobs",
                "args": { "op": "list" }
            }
        });
        let direct_out = process_pre_tool_for_host(&agy_direct, true, HookHost::Auto);
        assert_eq!(direct_out["overwrite"]["sessionId"], "conv-jobs-1");
        assert_eq!(direct_out["overwrite"]["cwd"], "/repo");

        let unrelated = json!({
            "conversationId": "conv-jobs-1",
            "cwd": "/repo",
            "toolCall": {
                "name": "github_jobs",
                "args": {}
            }
        });
        assert!(process_pre_tool_for_host(&unrelated, true, HookHost::Auto).get("overwrite").is_none());

        let codex_jobs = json!({
            "hook_event_name": "PreToolUse",
            "session_id": "019b-thread-jobs",
            "cwd": "/repo/wt",
            "tool_name": "mcp__cant__jobs",
            "tool_input": { "op": "list" }
        });
        let codex_out = process_pre_tool_for_host(&codex_jobs, true, HookHost::Auto);
        assert_eq!(codex_out["hookSpecificOutput"]["permissionDecision"], "allow");
        assert_eq!(
            codex_out["hookSpecificOutput"]["updatedInput"]["sessionId"],
            "codex-019b-thread-jobs"
        );
        assert_eq!(codex_out["hookSpecificOutput"]["updatedInput"]["cwd"], "/repo/wt");

        let other_codex = json!({
            "hook_event_name": "PreToolUse",
            "session_id": "019b-thread-jobs",
            "cwd": "/repo/wt",
            "tool_name": "mcp__ci__jobs",
            "tool_input": {}
        });
        assert_eq!(process_pre_tool_for_host(&other_codex, true, HookHost::Auto), json!({}));
    }

    #[test]
    fn injects_into_eager_mcp_cant_cant_repl_directly() {
        let payload = json!({
            "conversationId": "conv-eager-1",
            "cwd": "/repo/cant",
            "toolCall": {
                "name": "mcp_cant_cant_repl",
                "args": { "op": "eval", "code": "(+ 1 2)", "toolSummary": "Add two numbers" }
            }
        });
        let out = process_pre_tool_for_host(&payload, true, HookHost::Auto);
        assert_eq!(out["decision"], "allow");
        assert_eq!(out["overwrite"]["sessionId"], "conv-eager-1");
        assert_eq!(out["overwrite"]["cwd"], "/repo/cant");
        assert!(out["overwrite"].get("Arguments").is_none());
    }

    #[test]
    fn formats_channel_messages_for_agy_and_codex_hooks() {
        use cant_protocol::daemon::{ChannelNotice, HarnessKind};
        let ch = vec![ChannelNotice {
            channel_id: "ch-1".into(),
            channel_name: None,
            topic: Some("login".into()),
            seq: 1,
            from: "pi-1".into(),
            from_handle: "frontend".into(),
            from_harness: HarnessKind::Pi,
            body: "Does /login expect access_token?".into(),
            truncated: false,
            timestamp_ms: 1,
        }];
        let msg = format_all_notices(&[], &ch);
        assert!(msg.contains("ch-1"));
        assert!(msg.contains("frontend"));
        assert!(msg.contains("Does /login expect access_token?"));
        assert!(msg.contains("channels/send"));

        let agy = format_notice_response(HookHost::Agy, "PreInvocation", &[], &ch);
        assert!(agy["injectSteps"][0]["systemMessage"]["systemMessage"]
            .as_str()
            .unwrap()
            .contains("ch-1"));

        let codex_stop = format_codex_stop_response(&json!({"stop_hook_active": false}), &[], &ch);
        assert_eq!(codex_stop["decision"], "block");
        assert!(codex_stop["reason"].as_str().unwrap().contains("ch-1"));
    }

    #[test]
    fn agy_worker_env_overrides_conversation_id() {
        let payload = json!({ "conversationId": "conv-ephemeral", "cwd": "/repo" });
        let (sid, cwd) = extract_agy_context_with_env(&payload, Some("1"), Some("worker-bgdel-1"));
        assert_eq!(sid.as_deref(), Some("worker-bgdel-1"));
        assert_eq!(cwd.as_deref(), Some("/repo"));

        // Ignored when CANT_WORKER is not "1"
        let (sid2, _) = extract_agy_context_with_env(&payload, None, Some("worker-bgdel-1"));
        assert_eq!(sid2.as_deref(), Some("conv-ephemeral"));
    }
}
