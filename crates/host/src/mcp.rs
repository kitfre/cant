//! Stdio MCP server bridge (`cant mcp`).
//!
//! Speaks MCP JSON-RPC 2.0 over stdin/stdout (newline-delimited) and forwards tool calls
//! to the auto-started local `cant` daemon over its Unix socket. When running with
//! `--driver codex`, also serves the Codex CLI custom statusline resource (`ui://cant/statusline`).

use crate::daemon::{send_daemon_request, send_daemon_request_if_running};
use crate::jobs::unified::{
    format_job_tail, format_status_markdown, jobs_tool_entry, JobsToolRequest, DEFAULT_TAIL_BYTES,
};
use crate::statusline::{
    codex_statusline_session, render_statusline, CantStatusSnapshot, STATUSLINE_META_KEY,
    STATUSLINE_RESOURCE_URI,
};
use cant_protocol::daemon::{DaemonRequest, DaemonResponse, ReplRequest, WorkspaceKey};
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const MCP_PROTOCOL_VERSION: &str = "2025-03-26";
const STATUSLINE_QUERY_TIMEOUT: Duration = Duration::from_millis(400);

#[derive(Debug, Clone)]
pub struct McpOptions {
    pub disabled_namespaces: Vec<String>,
    pub worker_driver: String,
    pub default_language: String,
}

impl Default for McpOptions {
    fn default() -> Self {
        Self {
            disabled_namespaces: Vec::new(),
            worker_driver: "agy".into(),
            default_language: "steel".into(),
        }
    }
}

pub async fn run_mcp_stdio(socket_path: &Path, options: McpOptions) -> Result<(), String> {
    run_mcp_io(
        tokio::io::stdin(),
        tokio::io::stdout(),
        socket_path,
        options,
    )
    .await
}

async fn run_mcp_io<R, W>(
    input: R,
    output: W,
    socket_path: &Path,
    options: McpOptions,
) -> Result<(), String>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let mut reader = BufReader::new(input);
    let mut line = String::new();
    let output = std::sync::Arc::new(tokio::sync::Mutex::new(output));
    let socket_path = socket_path.to_path_buf();
    let options = std::sync::Arc::new(options);
    let fallback_session = std::sync::Arc::new(format!("mcp-{}", std::process::id()));
    let fallback_cwd = std::sync::Arc::new(
        std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "/tmp".into()),
    );
    let mut in_flight: tokio::task::JoinSet<Result<(), String>> = tokio::task::JoinSet::new();

    loop {
        // Reap any completed tasks without blocking reads.
        while let Some(res) = in_flight.try_join_next() {
            if let Ok(Err(e)) = res {
                return Err(e);
            }
        }

        line.clear();
        let n = reader
            .read_line(&mut line)
            .await
            .map_err(|e| format!("MCP stdin error: {e}"))?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                let err = json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": { "code": -32700, "message": format!("Parse error: {e}") }
                });
                let mut out = output.lock().await;
                write_json_line(&mut *out, &err).await?;
                continue;
            }
        };

        let sock = socket_path.clone();
        let opts = std::sync::Arc::clone(&options);
        let fb_sid = std::sync::Arc::clone(&fallback_session);
        let fb_cwd = std::sync::Arc::clone(&fallback_cwd);
        let out_handle = std::sync::Arc::clone(&output);
        in_flight.spawn(async move {
            if let Some(response) = handle_mcp_request(&sock, &opts, &fb_sid, &fb_cwd, &msg).await {
                let mut out = out_handle.lock().await;
                write_json_line(&mut *out, &response).await?;
            }
            Ok(())
        });
    }

    while let Some(res) = in_flight.join_next().await {
        if let Ok(Err(e)) = res {
            return Err(e);
        }
    }
    Ok(())
}

async fn handle_mcp_request(
    socket_path: &Path,
    options: &McpOptions,
    fallback_session: &str,
    fallback_cwd: &str,
    msg: &Value,
) -> Option<Value> {
    let id = msg.get("id")?.clone();
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(Value::Null);

    let response = match method {
        "initialize" => {
            let client_proto = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(MCP_PROTOCOL_VERSION);
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": client_proto,
                    "capabilities": {
                        "tools": { "listChanged": false },
                        "resources": {}
                    },
                    "serverInfo": {
                        "name": "cant",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }
            })
        }
        "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
        "tools/list" => {
            match send_daemon_request(
                socket_path,
                &DaemonRequest::ToolInfo {
                    language: options.default_language.clone(),
                    disabled_namespaces: options.disabled_namespaces.clone(),
                    session_id: None,
                },
            )
            .await
            {
                Ok(DaemonResponse::ToolInfo {
                    name,
                    description,
                    input_schema,
                }) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "tools": [
                            build_tool_entry(name, description, input_schema, &options.worker_driver),
                            jobs_tool_entry(&options.disabled_namespaces)
                        ]
                    }
                }),
                Ok(DaemonResponse::Error { message }) => jsonrpc_error(id, -32603, &message),
                Ok(_) => jsonrpc_error(id, -32603, "Unexpected daemon response for tools/list"),
                Err(e) => jsonrpc_error(id, -32603, &e),
            }
        }
        "resources/list" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "resources": [
                    {
                        "name": "footer",
                        "uri": STATUSLINE_RESOURCE_URI,
                        "mimeType": "application/json"
                    }
                ]
            }
        }),
        "resources/templates/list" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "resourceTemplates": [] }
        }),
        "resources/read" => {
            let uri = params.get("uri").and_then(Value::as_str).unwrap_or("");
            if uri != STATUSLINE_RESOURCE_URI {
                jsonrpc_error(id, -32602, &format!("Unknown resource URI: {uri}"))
            } else {
                let state = params
                    .get("_meta")
                    .and_then(|m| m.get(STATUSLINE_META_KEY))
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let snapshot = query_cant_status_snapshot(socket_path, &state).await;
                let tree = render_statusline(&state, &snapshot);
                let text = serde_json::to_string(&tree).unwrap_or_else(|_| "null".into());
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "contents": [
                            {
                                "uri": STATUSLINE_RESOURCE_URI,
                                "mimeType": "application/json",
                                "text": text
                            }
                        ]
                    }
                })
            }
        }
        "tools/call" => {
            let tool_name = params.get("name").and_then(Value::as_str).unwrap_or("");
            match tool_name {
                "repl" => {
                    let mut args = params
                        .get("arguments")
                        .cloned()
                        .unwrap_or_else(|| json!({}));
                    let session_id = take_string_field(&mut args, "sessionId")
                        .or_else(|| codex_session(&params))
                        .unwrap_or_else(|| fallback_session.to_string());
                    let cwd = take_string_field(&mut args, "cwd")
                        .unwrap_or_else(|| fallback_cwd.to_string());
                    let language = take_string_field(&mut args, "language")
                        .unwrap_or_else(|| options.default_language.clone());
                    strip_host_ui_fields(&mut args);

                    match serde_json::from_value::<ReplRequest>(args) {
                        Err(e) => mcp_tool_result(id, format!("Invalid repl arguments: {e}"), true),
                        Ok(request) => {
                            let req = DaemonRequest::Repl {
                                key: WorkspaceKey {
                                    session_id,
                                    cwd,
                                    language,
                                    disabled_namespaces: options.disabled_namespaces.clone(),
                                    worker_driver: options.worker_driver.clone(),
                                },
                                request,
                            };
                            match send_daemon_request(socket_path, &req).await {
                                Ok(DaemonResponse::Repl(reply)) => {
                                    mcp_tool_result(id, reply.text, reply.is_error)
                                }
                                Ok(DaemonResponse::Error { message }) => {
                                    mcp_tool_result(id, message, true)
                                }
                                Ok(_) => jsonrpc_error(id, -32603, "Unexpected daemon response"),
                                Err(e) => {
                                    mcp_tool_result(id, format!("Cant daemon error: {e}"), true)
                                }
                            }
                        }
                    }
                }
                "jobs" => {
                    let mut args = params
                        .get("arguments")
                        .cloned()
                        .unwrap_or_else(|| json!({}));
                    let session_id = take_string_field(&mut args, "sessionId")
                        .or_else(|| codex_session(&params))
                        .unwrap_or_else(|| fallback_session.to_string());
                    let _ = take_string_field(&mut args, "cwd");
                    strip_host_ui_fields(&mut args);
                    handle_jobs_tool_call(socket_path, id, session_id, args).await
                }
                _ => jsonrpc_error(id, -32602, &format!("Unknown tool: {tool_name}")),
            }
        }
        other => jsonrpc_error(id, -32601, &format!("Method not found: {other}")),
    };

    Some(response)
}

fn mcp_tool_result(id: Value, text: String, is_error: bool) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{ "type": "text", "text": text }],
            "isError": is_error
        }
    })
}

fn enrich_cross_session_hint(message: String, all_sessions: bool) -> String {
    if !all_sessions && message.contains("belongs to another session") {
        format!("{message} Pass allSessions: true to access jobs from other sessions.")
    } else {
        message
    }
}

async fn handle_jobs_tool_call(
    socket_path: &Path,
    id: Value,
    session_id: String,
    args: Value,
) -> Value {
    let req = match JobsToolRequest::from_value(args) {
        Ok(r) => r,
        Err(e) => return mcp_tool_result(id, format!("Invalid jobs arguments: {e}"), true),
    };

    match req {
        JobsToolRequest::List {
            active_only,
            kind,
            all_sessions,
        } => {
            let session_filter = (!all_sessions).then_some(session_id.clone());
            let jobs_req = DaemonRequest::ListJobs {
                session_id: session_filter.clone(),
            };
            let ws_req = DaemonRequest::ListWorkspaces;
            let (jobs_res, ws_res) = tokio::join!(
                send_daemon_request_if_running(socket_path, &jobs_req),
                send_daemon_request_if_running(socket_path, &ws_req),
            );
            let jobs = match jobs_res {
                Ok(None) => {
                    return mcp_tool_result(
                        id,
                        "Cant daemon is not running (no active workspaces or jobs).".into(),
                        false,
                    );
                }
                Ok(Some(DaemonResponse::Jobs { jobs })) => jobs,
                Ok(Some(DaemonResponse::Error { message })) => {
                    return mcp_tool_result(id, message, true);
                }
                Ok(Some(_)) => return jsonrpc_error(id, -32603, "Unexpected daemon response"),
                Err(e) => return mcp_tool_result(id, format!("Cant daemon error: {e}"), true),
            };
            let workspaces = match ws_res {
                Ok(Some(DaemonResponse::Workspaces { workspaces })) => workspaces,
                _ => Vec::new(),
            };
            let text = format_status_markdown(
                &workspaces,
                &jobs,
                session_filter.as_deref(),
                active_only,
                kind,
            );
            mcp_tool_result(id, text, false)
        }
        JobsToolRequest::Tail {
            id: job_id,
            max_bytes,
            all_sessions,
        } => {
            let session_filter = (!all_sessions).then_some(session_id);
            let max_bytes = max_bytes
                .map(|n| n.clamp(256, crate::jobs::MAX_OUTPUT_REPLY_BYTES))
                .unwrap_or(DEFAULT_TAIL_BYTES);
            let req = DaemonRequest::JobTail {
                id: job_id,
                max_bytes,
                session_id: session_filter,
            };
            match send_daemon_request_if_running(socket_path, &req).await {
                Ok(None) => mcp_tool_result(
                    id,
                    "Cant daemon is not running (no active jobs).".into(),
                    true,
                ),
                Ok(Some(DaemonResponse::JobTail {
                    id: jid,
                    kind,
                    status,
                    output,
                    summary,
                })) => mcp_tool_result(
                    id,
                    format_job_tail(summary.as_ref(), &jid, &kind, &status, &output),
                    false,
                ),
                Ok(Some(DaemonResponse::Error { message })) => {
                    mcp_tool_result(id, enrich_cross_session_hint(message, all_sessions), true)
                }
                Ok(Some(_)) => jsonrpc_error(id, -32603, "Unexpected daemon response"),
                Err(e) => mcp_tool_result(id, format!("Cant daemon error: {e}"), true),
            }
        }
        JobsToolRequest::Cancel {
            id: job_id,
            all_sessions,
        } => {
            let session_filter = (!all_sessions).then_some(session_id);
            let req = DaemonRequest::CancelJob {
                id: job_id.clone(),
                session_id: session_filter,
            };
            match send_daemon_request_if_running(socket_path, &req).await {
                Ok(None) => mcp_tool_result(
                    id,
                    "Cant daemon is not running (no active jobs to cancel).".into(),
                    true,
                ),
                Ok(Some(DaemonResponse::Ok)) => {
                    mcp_tool_result(id, format!("Cancelled job `{job_id}`."), false)
                }
                Ok(Some(DaemonResponse::Error { message })) => {
                    mcp_tool_result(id, enrich_cross_session_hint(message, all_sessions), true)
                }
                Ok(Some(_)) => jsonrpc_error(id, -32603, "Unexpected daemon response"),
                Err(e) => mcp_tool_result(id, format!("Cant daemon error: {e}"), true),
            }
        }
    }
}

fn build_tool_entry(
    name: String,
    description: String,
    mut input_schema: Value,
    worker_driver: &str,
) -> Value {
    if worker_driver == "codex" {
        json!({
            "name": name,
            "title": "Cant",
            "description": description,
            "inputSchema": input_schema,
            "_meta": {
                "openai/ui": {
                    "entrypoints": [
                        { "type": "statusline", "resourceUri": STATUSLINE_RESOURCE_URI }
                    ]
                }
            }
        })
    } else {
        if worker_driver == "agy" {
            if let Some(props) = input_schema
                .get_mut("properties")
                .and_then(Value::as_object_mut)
            {
                props.entry("toolSummary".to_string()).or_insert_with(|| {
                    json!({
                        "type": "string",
                        "description": "Brief 2-5 word noun phrase summarizing this REPL step (e.g. 'Read adapter configs', 'Run unit tests')."
                    })
                });
            }
        }
        json!({
            "name": name,
            "title": "Cant REPL",
            "description": description,
            "inputSchema": input_schema
        })
    }
}

async fn query_cant_status_snapshot(socket_path: &Path, state: &Value) -> CantStatusSnapshot {
    let Some(session_id) = codex_statusline_session(state) else {
        return CantStatusSnapshot::default();
    };
    let jobs_req = DaemonRequest::ListJobs {
        session_id: Some(session_id.clone()),
    };
    let ws_req = DaemonRequest::ListWorkspaces;
    let jobs_fut = send_daemon_request_if_running(socket_path, &jobs_req);
    let ws_fut = send_daemon_request_if_running(socket_path, &ws_req);

    let (jobs_res, ws_res) = match tokio::time::timeout(STATUSLINE_QUERY_TIMEOUT, async {
        tokio::join!(jobs_fut, ws_fut)
    })
    .await
    {
        Ok(pair) => pair,
        Err(_) => return CantStatusSnapshot::default(),
    };

    let jobs = match jobs_res {
        Ok(Some(DaemonResponse::Jobs { jobs })) => jobs,
        _ => Vec::new(),
    };
    let workspace = match ws_res {
        Ok(Some(DaemonResponse::Workspaces { workspaces })) => {
            workspaces.into_iter().find(|w| w.session_id == session_id)
        }
        _ => None,
    };

    CantStatusSnapshot { workspace, jobs }
}

// Codex sends this per tools/call; a single stdio transport is not necessarily
// one conversation. Other MCP clients keep the existing process-scoped fallback.
fn codex_session(params: &Value) -> Option<String> {
    let meta = params.pointer("/_meta/x-codex-turn-metadata")?;
    let decoded;
    let meta = if let Some(s) = meta.as_str() {
        decoded = serde_json::from_str::<Value>(s).ok()?;
        &decoded
    } else {
        meta
    };
    let thread = meta.get("thread_id")?.as_str()?.trim();
    (!thread.is_empty()).then(|| format!("codex-{thread}"))
}

const AGY_HOST_UI_FIELDS: &[&str] = &[
    "toolSummary",
    "toolAction",
    "explanation",
    "waitForPreviousTools",
];

/// Strip host-injected UI metadata keys before deserializing typed tool requests.
fn strip_host_ui_fields(value: &mut Value) {
    if let Some(map) = value.as_object_mut() {
        for key in AGY_HOST_UI_FIELDS {
            map.remove(*key);
        }
    }
}

fn take_string_field(value: &mut Value, key: &str) -> Option<String> {
    value
        .as_object_mut()
        .and_then(|map| map.remove(key))
        .and_then(|v| v.as_str().map(ToString::to_string))
        .filter(|s| !s.trim().is_empty())
}

fn jsonrpc_error(id: Value, code: i32, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

async fn write_json_line<W: tokio::io::AsyncWrite + Unpin>(
    stdout: &mut W,
    value: &Value,
) -> Result<(), String> {
    let mut encoded = serde_json::to_string(value).map_err(|e| e.to_string())?;
    encoded.push('\n');
    stdout
        .write_all(encoded.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    stdout.flush().await.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn codex_tool_entry_advertises_statusline_entrypoint_only_for_codex_driver() {
        let codex_tool = build_tool_entry(
            "repl".into(),
            "desc".into(),
            json!({"type": "object"}),
            "codex",
        );
        assert_eq!(codex_tool["title"], "Cant");
        let entry = &codex_tool["_meta"]["openai/ui"]["entrypoints"][0];
        assert_eq!(entry["type"], "statusline");
        assert_eq!(
            entry["resourceUri"],
            crate::statusline::STATUSLINE_RESOURCE_URI
        );

        let agy_tool = build_tool_entry(
            "repl".into(),
            "desc".into(),
            json!({"type": "object"}),
            "agy",
        );
        assert!(agy_tool.get("_meta").is_none());
    }

    #[tokio::test]
    async fn initialize_and_resources_methods_serve_codex_statusline_contract() {
        let missing_sock = PathBuf::from("/tmp/cant-nonexistent-test-statusline.sock");
        let opts = McpOptions {
            worker_driver: "codex".into(),
            ..McpOptions::default()
        };

        let init = handle_mcp_request(
            &missing_sock,
            &opts,
            "fallback",
            "/tmp",
            &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}),
        )
        .await
        .unwrap();
        assert!(init["result"]["capabilities"]["resources"].is_object());

        let list = handle_mcp_request(
            &missing_sock,
            &opts,
            "fallback",
            "/tmp",
            &json!({"jsonrpc": "2.0", "id": 2, "method": "resources/list", "params": {}}),
        )
        .await
        .unwrap();
        assert_eq!(
            list["result"]["resources"][0]["uri"],
            crate::statusline::STATUSLINE_RESOURCE_URI
        );

        let templates = handle_mcp_request(
            &missing_sock,
            &opts,
            "fallback",
            "/tmp",
            &json!({"jsonrpc": "2.0", "id": 3, "method": "resources/templates/list", "params": {}}),
        )
        .await
        .unwrap();
        assert_eq!(templates["result"]["resourceTemplates"], json!([]));

        let read = handle_mcp_request(
            &missing_sock,
            &opts,
            "fallback",
            "/tmp",
            &json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "resources/read",
                "params": {
                    "uri": crate::statusline::STATUSLINE_RESOURCE_URI,
                    "_meta": {
                        "com.openai/codex-statusline-state": {
                            "thread": {
                                "id": "t-123",
                                "model": { "display_name": "GPT-6.2" },
                                "workspace": { "cwd": "/repo/cant" }
                            }
                        }
                    }
                }
            }),
        )
        .await
        .unwrap();
        let item = &read["result"]["contents"][0];
        assert_eq!(item["uri"], crate::statusline::STATUSLINE_RESOURCE_URI);
        assert_eq!(item["mimeType"], "application/json");
        let parsed: Value = serde_json::from_str(item["text"].as_str().unwrap()).unwrap();
        assert_eq!(parsed["type"], "row");

        let bad_read = handle_mcp_request(
            &missing_sock,
            &opts,
            "fallback",
            "/tmp",
            &json!({"jsonrpc": "2.0", "id": 5, "method": "resources/read", "params": {"uri": "ui://cant/unknown"}}),
        )
        .await
        .unwrap();
        assert_eq!(bad_read["error"]["code"], -32602);
    }

    #[test]
    fn codex_workspace_follows_the_thread_not_the_turn_or_mcp_process() {
        let session = |thread: &str, turn: &str| {
            codex_session(&json!({"_meta": {"x-codex-turn-metadata": {
                "thread_id": thread, "turn_id": turn
            }}}))
            .unwrap()
        };
        assert_eq!(session("a", "one"), session("a", "two"));
        assert_ne!(session("a", "one"), session("b", "one"));
        assert!(
            codex_session(&json!({"_meta":{"x-codex-turn-metadata":{"thread_id":""}}})).is_none()
        );
        assert!(codex_session(&json!({})).is_none());
    }

    #[tokio::test]
    async fn jobs_tool_does_not_autostart_daemon_when_not_running() {
        let dir = tempfile::tempdir().unwrap();
        let missing_sock = dir.path().join("missing.sock");
        let opts = McpOptions::default();

        let resp = handle_mcp_request(
            &missing_sock,
            &opts,
            "fallback",
            "/tmp",
            &json!({
                "jsonrpc": "2.0",
                "id": 10,
                "method": "tools/call",
                "params": {
                    "name": "jobs",
                    "arguments": {}
                }
            }),
        )
        .await
        .unwrap();

        assert_eq!(resp["result"]["isError"], false);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.to_ascii_lowercase().contains("not running")
                || text.to_ascii_lowercase().contains("no active"),
            "unexpected text: {text}"
        );
        assert!(
            !missing_sock.exists(),
            "jobs tool must not auto-start daemon"
        );
    }

    #[tokio::test]
    async fn mcp_exposes_jobs_tool_and_supports_list_tail_and_cancel() {
        use crate::adapter::AdapterHub;
        use crate::capability::CapabilityCatalog;
        use crate::daemon::run_daemon;
        use crate::jobs::agents::{AgentFlowJob, DelegateJob, FlowNodeState, WorkerStatus};
        use crate::jobs::driver::WorkerDriverRegistry;
        use crate::jobs::{JobManager, NotifyMode};
        use crate::language::{GuestLanguage, LanguageIdentity, LanguageRegistry};
        use crate::repl::ReplManager;
        use crate::runner_process::RunnerCommand;
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};
        use std::time::Instant;
        use tokio_util::sync::CancellationToken;

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
                "stub repl".into()
            }
            fn source_description(&self) -> &'static str {
                "code"
            }
            fn inputs_description(&self) -> &'static str {
                "inputs"
            }
            fn format_metadata(&self, _: &Value) -> String {
                String::new()
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

        let mut langs = LanguageRegistry::new();
        langs.register(Arc::new(StubLang));
        let jobs = Arc::new(JobManager::new(Arc::new(WorkerDriverRegistry::new())));
        let catalog = Arc::new(CapabilityCatalog::new());
        let adapters = Arc::new(AdapterHub::new());
        let manager = Arc::new(ReplManager::new(
            Arc::new(langs),
            catalog,
            adapters,
            Arc::clone(&jobs),
        ));

        // Seed jobs across two sessions.
        let mut nodes = HashMap::new();
        nodes.insert(
            "step1".into(),
            FlowNodeState {
                id: "step1".into(),
                name: "step1".into(),
                status: WorkerStatus::Running,
                completion: None,
                error: None,
                monitor: "analyzing module...\n".into(),
            },
        );
        jobs.flows.lock().unwrap().insert(
            "flow-a".into(),
            Arc::new(Mutex::new(AgentFlowJob {
                id: "flow-a".into(),
                session_id: "sess-a".into(),
                name: "audit".into(),
                notify: NotifyMode::Active,
                status: WorkerStatus::Running,
                completion_token: "tok-a".into(),
                nodes,
                node_order: vec!["step1".into()],
                output_value: None,
                error: None,
                started_at: Instant::now(),
                finished_ms: None,
                cancel_token: CancellationToken::new(),
                cancelled_explicitly: false,
            })),
        );
        jobs.delegates.lock().unwrap().insert(
            "del-b".into(),
            Arc::new(Mutex::new(DelegateJob {
                id: "del-b".into(),
                session_id: "sess-b".into(),
                name: "other-review".into(),
                task: "Review other repo".into(),
                notify: NotifyMode::Active,
                status: WorkerStatus::Running,
                output: None,
                error: None,
                usage: None,
                monitor: "other session log\n".into(),
                started_at: Instant::now(),
                finished_ms: None,
                cancel_token: CancellationToken::new(),
                cancelled_explicitly: false,
            })),
        );

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("cant-test.sock");
        let sock_for_daemon = sock.clone();
        let daemon_handle = tokio::spawn(async move {
            let _ = run_daemon(&sock_for_daemon, manager).await;
        });

        for _ in 0..50 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(sock.exists());

        let opts = McpOptions::default();

        // 1. tools/list advertises both repl and jobs.
        let listed = handle_mcp_request(
            &sock,
            &opts,
            "fallback",
            "/tmp",
            &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}}),
        )
        .await
        .unwrap();
        let tools = listed["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"repl"));
        assert!(names.contains(&"jobs"));

        // 2. jobs with omitted op defaults to list scoped to sessionId.
        let list_a = handle_mcp_request(
            &sock,
            &opts,
            "fallback",
            "/tmp",
            &json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": { "name": "jobs", "arguments": { "sessionId": "sess-a" } }
            }),
        )
        .await
        .unwrap();
        assert_eq!(list_a["result"]["isError"], false);
        let text_a = list_a["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text_a.contains("flow-a"), "expected flow-a in: {text_a}");
        assert!(
            !text_a.contains("del-b"),
            "did not expect del-b in: {text_a}"
        );

        // 3. list with allSessions: true includes both sessions.
        let list_all = handle_mcp_request(
            &sock,
            &opts,
            "fallback",
            "/tmp",
            &json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "jobs",
                    "arguments": { "op": "list", "allSessions": true, "sessionId": "sess-a" }
                }
            }),
        )
        .await
        .unwrap();
        let text_all = list_all["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text_all.contains("flow-a") && text_all.contains("del-b"));

        // 4. tail returns header + live node output for own session.
        let tail_a = handle_mcp_request(
            &sock,
            &opts,
            "fallback",
            "/tmp",
            &json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {
                    "name": "jobs",
                    "arguments": { "op": "tail", "id": "flow-a", "sessionId": "sess-a" }
                }
            }),
        )
        .await
        .unwrap();
        assert_eq!(tail_a["result"]["isError"], false);
        let tail_text = tail_a["result"]["content"][0]["text"].as_str().unwrap();
        assert!(tail_text.contains("flow-a"));
        assert!(tail_text.contains("audit"));
        assert!(tail_text.contains("analyzing module..."));

        // 5. cross-session tail & cancel blocked by default, allowed with allSessions: true.
        let cross_tail = handle_mcp_request(
            &sock,
            &opts,
            "fallback",
            "/tmp",
            &json!({
                "jsonrpc": "2.0",
                "id": 5,
                "method": "tools/call",
                "params": {
                    "name": "jobs",
                    "arguments": { "op": "tail", "id": "del-b", "sessionId": "sess-a" }
                }
            }),
        )
        .await
        .unwrap();
        assert_eq!(cross_tail["result"]["isError"], true);
        assert!(cross_tail["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("allSessions"));

        let cross_cancel = handle_mcp_request(
            &sock,
            &opts,
            "fallback",
            "/tmp",
            &json!({
                "jsonrpc": "2.0",
                "id": 6,
                "method": "tools/call",
                "params": {
                    "name": "jobs",
                    "arguments": { "op": "cancel", "id": "del-b", "sessionId": "sess-a" }
                }
            }),
        )
        .await
        .unwrap();
        assert_eq!(cross_cancel["result"]["isError"], true);
        assert_eq!(
            jobs.delegates.lock().unwrap()["del-b"]
                .lock()
                .unwrap()
                .status,
            WorkerStatus::Running
        );

        let override_cancel = handle_mcp_request(
            &sock,
            &opts,
            "fallback",
            "/tmp",
            &json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": "tools/call",
                "params": {
                    "name": "jobs",
                    "arguments": {
                        "op": "cancel",
                        "id": "del-b",
                        "allSessions": true,
                        "sessionId": "sess-a"
                    }
                }
            }),
        )
        .await
        .unwrap();
        assert_eq!(override_cancel["result"]["isError"], false);
        assert_eq!(
            jobs.delegates.lock().unwrap()["del-b"]
                .lock()
                .unwrap()
                .status,
            WorkerStatus::Cancelled
        );

        let _ = send_daemon_request(&sock, &DaemonRequest::Shutdown).await;
        let _ = daemon_handle.await;
    }

    #[test]
    fn agy_tool_entry_sets_title_and_optional_tool_summary_without_affecting_codex() {
        let base_schema = || {
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["op"],
                "properties": {
                    "op": { "type": "string" }
                }
            })
        };

        let agy_tool = build_tool_entry("repl".into(), "desc".into(), base_schema(), "agy");
        assert_eq!(agy_tool["title"], "Cant REPL");
        assert_eq!(
            agy_tool["inputSchema"]["properties"]["toolSummary"]["type"],
            "string"
        );
        assert!(agy_tool.get("_meta").is_none());

        let codex_tool = build_tool_entry("repl".into(), "desc".into(), base_schema(), "codex");
        assert_eq!(codex_tool["title"], "Cant");
        assert!(codex_tool["inputSchema"]["properties"]
            .get("toolSummary")
            .is_none());
    }

    #[tokio::test]
    async fn mcp_repl_strips_agy_ui_fields_before_deserializing_request() {
        use crate::adapter::AdapterHub;
        use crate::capability::CapabilityCatalog;
        use crate::daemon::run_daemon;
        use crate::jobs::driver::WorkerDriverRegistry;
        use crate::jobs::JobManager;
        use crate::language::{GuestLanguage, LanguageIdentity, LanguageRegistry};
        use crate::repl::ReplManager;
        use crate::runner_process::RunnerCommand;
        use std::sync::Arc;

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
                "stub repl".into()
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

        let mut langs = LanguageRegistry::new();
        langs.register(Arc::new(StubLang));
        let jobs = Arc::new(JobManager::new(Arc::new(WorkerDriverRegistry::new())));
        let catalog = Arc::new(CapabilityCatalog::new());
        let adapters = Arc::new(AdapterHub::new());
        let manager = Arc::new(ReplManager::new(Arc::new(langs), catalog, adapters, jobs));

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("cant-agy-ui.sock");
        let sock_for_daemon = sock.clone();
        let daemon_handle = tokio::spawn(async move {
            let _ = run_daemon(&sock_for_daemon, manager).await;
        });
        for _ in 0..50 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(sock.exists());

        let opts = McpOptions::default();
        let resp = handle_mcp_request(
            &sock,
            &opts,
            "fallback",
            "/tmp",
            &json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "repl",
                    "arguments": {
                        "op": "history",
                        "sessionId": "sess-ui",
                        "cwd": "/tmp",
                        "toolSummary": "Check recent history",
                        "toolAction": "Checking history",
                        "explanation": "Inspecting REPL history",
                        "waitForPreviousTools": false
                    }
                }
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            resp["result"]["isError"], false,
            "unexpected response: {resp}"
        );

        let _ = send_daemon_request(&sock, &DaemonRequest::Shutdown).await;
        let _ = daemon_handle.await;
    }

    #[tokio::test]
    async fn mcp_io_handles_concurrent_requests_without_head_of_line_blocking() {
        use std::sync::Arc;
        use tokio::net::UnixListener;
        use tokio::sync::Notify;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("cant-conc.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let gate = Arc::new(Notify::new());
        let gate_srv = Arc::clone(&gate);

        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                let g = Arc::clone(&gate_srv);
                tokio::spawn(async move {
                    let (r, mut w) = stream.into_split();
                    let mut br = BufReader::new(r);
                    let mut line = String::new();
                    let _ = br.read_line(&mut line).await;
                    if line.contains("\"s1\"") {
                        // s1 blocks until s2 arrives concurrently!
                        g.notified().await;
                    } else if line.contains("\"s2\"") {
                        g.notify_one();
                    }
                    let resp = DaemonResponse::Repl(cant_protocol::daemon::ReplToolResponse {
                        text: "ok".into(),
                        is_error: false,
                        workspace_id: "ws".into(),
                        generation: 1,
                        submission_id: None,
                        status: "ok".into(),
                        raw_value: None,
                    });
                    let frame = format!("{}\n", serde_json::to_string(&resp).unwrap());
                    let _ = w.write_all(frame.as_bytes()).await;
                    let _ = w.flush().await;
                });
            }
        });

        let (mut client_in, server_in) = tokio::io::duplex(4096);
        let (server_out, client_out) = tokio::io::duplex(4096);
        let sock_c = sock.clone();
        let mcp_task = tokio::spawn(async move {
            run_mcp_io(server_in, server_out, &sock_c, McpOptions::default()).await
        });

        let req1 = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"repl","arguments":{"op":"history","sessionId":"s1","cwd":"/tmp"}}});
        let req2 = json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"repl","arguments":{"op":"history","sessionId":"s2","cwd":"/tmp"}}});
        let payload = format!("{}\n{}\n", req1, req2);
        client_in.write_all(payload.as_bytes()).await.unwrap();
        client_in.flush().await.unwrap();
        drop(client_in);

        let mut out_reader = BufReader::new(client_out);
        let mut ids = Vec::new();
        for _ in 0..2 {
            let mut line = String::new();
            let n = tokio::time::timeout(
                Duration::from_millis(500),
                out_reader.read_line(&mut line),
            )
            .await
            .expect(
                "head-of-line blocked: second MCP request could not run while first was in flight",
            )
            .unwrap();
            assert!(n > 0);
            let v: Value = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(v["result"]["isError"], false, "unexpected MCP error: {v}");
            ids.push(v["id"].as_i64().unwrap());
        }
        ids.sort();
        assert_eq!(ids, vec![1, 2]);
        let _ = mcp_task.await;
        let _ = server.await;
    }
}
