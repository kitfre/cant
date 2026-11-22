//! Client <-> Daemon Unix socket protocol (NDJSON).
//!
//! Shared by the Antigravity MCP bridge, CLI clients, Pi adapter, and future harness adapters
//! (Claude Code, Codex, etc.).

use crate::runner::ReplOrigin;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum ReplRequest {
    Eval {
        code: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        inputs: BTreeMap<String, String>,
    },
    Inspect {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<ReplOrigin>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        offset: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
    },
    Search {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<ReplOrigin>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        offset: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
    },
    Describe {
        name: String,
    },
    History {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        offset: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
    },
    Reset,
    Close,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceKey {
    pub session_id: String,
    pub cwd: String,
    #[serde(default = "default_language")]
    pub language: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_namespaces: Vec<String>,
    #[serde(default = "default_worker_driver")]
    pub worker_driver: String,
}

fn default_language() -> String {
    "steel".to_string()
}

fn default_worker_driver() -> String {
    "agy".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalCapabilitySpec {
    pub tool_name: String,
    pub api_path: Vec<String>,
    pub description: String,
    #[serde(default)]
    pub params_schema: Value,
    #[serde(default)]
    pub result_schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steel_positional: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalWorkerSpec {
    pub name: String,
    pub task: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub channels: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HarnessKind {
    Pi,
    Antigravity,
    Codex,
    Cli,
    Worker,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PeerReachability {
    Push,
    Listening,
    Active,
    Idle,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerSummary {
    pub id: String,
    pub handle: String,
    pub cwd: String,
    pub harness: HarnessKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub reachability: PeerReachability,
    pub last_seen_ms: u64,
    pub stale: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChannelKind {
    Direct,
    Room,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelMessage {
    pub seq: u64,
    pub channel_id: String,
    pub from: String,
    pub from_handle: String,
    pub body: String,
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelNotice {
    pub channel_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub seq: u64,
    pub from: String,
    pub from_handle: String,
    pub from_harness: HarnessKind,
    pub body: String,
    pub truncated: bool,
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelSummary {
    pub id: String,
    pub kind: ChannelKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub closed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub members: Vec<String>,
    pub member_ids: Vec<String>,
    pub message_count: usize,
    pub unread: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonRequest {
    Ping,
    Repl {
        key: WorkspaceKey,
        request: ReplRequest,
    },
    ListWorkspaces,
    ToolInfo {
        #[serde(default = "default_language")]
        language: String,
        #[serde(default)]
        disabled_namespaces: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
    ListJobs {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
    JobTail {
        id: String,
        #[serde(default = "default_tail_bytes")]
        max_bytes: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
    CancelJob {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
    DrainNotices {
        session_id: String,
        #[serde(default)]
        include_passive: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        harness: Option<HarnessKind>,
    },
    ListPeers {
        #[serde(default)]
        include_stale: bool,
    },
    ListChannels {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(default)]
        include_closed: bool,
    },
    ChannelRead {
        channel: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(default = "default_channel_read_limit")]
        limit: usize,
    },
    ChannelSend {
        target: String,
        body: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        handle: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
    /// Subscribe to real-time job lifecycle & output events for a session (or all if omitted).
    Subscribe {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
    /// Attach a persistent bidirectional harness adapter (e.g. Pi) for reverse-RPC capabilities & workers.
    AttachAdapter {
        adapter_id: String,
        session_id: String,
        #[serde(default)]
        capabilities: Vec<ExternalCapabilitySpec>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        worker_driver: Option<String>,
    },
    /// Update the attached adapter's live external capability list (e.g. when MCP servers connect/disconnect).
    UpdateCapabilities {
        capabilities: Vec<ExternalCapabilitySpec>,
    },
    /// Reply from an attached adapter to an `InvokeCapability` or `RunWorker` reverse-RPC call.
    AdapterResult {
        call_id: String,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Streaming monitor output from an attached adapter's running worker.
    AdapterWorkerProgress {
        call_id: String,
        chunk: String,
    },
    Shutdown,
}

fn default_tail_bytes() -> usize {
    16 * 1024
}

fn default_channel_read_limit() -> usize {
    50
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSummary {
    pub session_id: String,
    pub cwd: String,
    pub language: String,
    pub workspace_id: String,
    pub generation: u64,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobSummary {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub session_id: String,
    pub status: String,
    pub elapsed_ms: u64,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionNotice {
    pub job_id: String,
    pub kind: String,
    pub name: String,
    pub status: String,
    pub notify: String,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum DaemonEvent {
    JobUpdated { job: JobSummary },
    JobOutput { id: String, chunk: String },
    JobCompleted { notice: CompletionNotice },
    ChannelMessage { notice: ChannelNotice },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplToolResponse {
    pub text: String,
    pub is_error: bool,
    pub workspace_id: String,
    pub generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submission_id: Option<u64>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_value: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonResponse {
    Pong {
        version: String,
        pid: u32,
        languages: Vec<String>,
        drivers: Vec<String>,
    },
    Repl(ReplToolResponse),
    Workspaces {
        workspaces: Vec<WorkspaceSummary>,
    },
    Jobs {
        jobs: Vec<JobSummary>,
    },
    JobTail {
        id: String,
        kind: String,
        status: String,
        output: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<JobSummary>,
    },
    Notices {
        notices: Vec<CompletionNotice>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        channel_messages: Vec<ChannelNotice>,
    },
    Peers {
        peers: Vec<PeerSummary>,
    },
    Channels {
        channels: Vec<ChannelSummary>,
    },
    ChannelMessages {
        channel: ChannelSummary,
        messages: Vec<ChannelMessage>,
    },
    ChannelSent {
        channel_id: String,
        message: ChannelMessage,
    },
    ToolInfo {
        name: String,
        description: String,
        input_schema: Value,
    },
    Event(DaemonEvent),
    AdapterAttached {
        adapter_id: String,
    },
    InvokeCapability {
        call_id: String,
        tool_name: String,
        args: Value,
    },
    RunWorker {
        call_id: String,
        spec: ExternalWorkerSpec,
    },
    CancelWorker {
        call_id: String,
    },
    Ok,
    Error {
        message: String,
    },
}
