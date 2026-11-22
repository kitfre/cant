//! Language-neutral NDJSON wire protocol between the Cant host and language runners.
//!
//! Any language runner (Steel, Python, Lua, Janet, TypeScript, etc.) can plug into Cant by
//! speaking these newline-delimited JSON frames over private process pipes.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const RUNNER_PROTOCOL_VERSION: u32 = 1;
pub const DEFAULT_MAX_PROTOCOL_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_SOURCE_BYTES: usize = 256 * 1024;
pub const MAX_INPUTS_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunnerIdentity {
    pub id: String,
    pub program_format: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReplOrigin {
    User,
    All,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum RunnerReplOp {
    Eval,
    Describe {
        name: String,
    },
    Inspect {
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        origin: Option<ReplOrigin>,
        #[serde(skip_serializing_if = "Option::is_none")]
        offset: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
    },
    Search {
        #[serde(skip_serializing_if = "Option::is_none")]
        query: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        origin: Option<ReplOrigin>,
        #[serde(skip_serializing_if = "Option::is_none")]
        offset: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Program {
    pub format: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repl: Option<RunnerReplOp>,
}

/// Capability bindings snapshot sent to the runner.
///
/// `language_bindings` carries opaque, language-specific projection metadata (for example,
/// Steel positional/keyword descriptors) so core protocol types remain language-neutral.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bindings {
    pub api_paths: Vec<Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spread_api_paths: Vec<Vec<String>>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub language_bindings: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunnerExecution {
    pub program: Program,
    pub inputs: BTreeMap<String, String>,
    pub bindings: Bindings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SerializedError {
    pub name: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum HostCallRequest {
    Path { path: Vec<String>, args: Value },
    Tool { name: String, args: Value },
    Raw { name: String, args: Value },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ParentMessage {
    Execute {
        #[serde(rename = "runId")]
        run_id: String,
        program: Program,
        inputs: BTreeMap<String, String>,
        bindings: Bindings,
    },
    ToolResult {
        #[serde(rename = "runId")]
        run_id: String,
        #[serde(rename = "callId")]
        call_id: String,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        value: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<SerializedError>,
    },
    Cancel {
        #[serde(rename = "runId")]
        run_id: String,
    },
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConsoleLevel {
    Log,
    Warn,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChildMessage {
    Ready {
        #[serde(rename = "protocolVersion")]
        protocol_version: u32,
        runner: RunnerIdentity,
    },
    ToolCall {
        #[serde(rename = "runId")]
        run_id: String,
        #[serde(rename = "callId")]
        call_id: String,
        request: HostCallRequest,
    },
    Console {
        #[serde(rename = "runId")]
        run_id: String,
        level: ConsoleLevel,
        values: Vec<Value>,
    },
    Complete {
        #[serde(rename = "runId")]
        run_id: String,
        ok: bool,
        #[serde(default)]
        value: Value,
        #[serde(default)]
        error: Option<SerializedError>,
    },
    Fatal {
        error: SerializedError,
    },
}

pub fn encode_frame<T: Serialize>(msg: &T, max_bytes: usize) -> Result<String, String> {
    let mut encoded =
        serde_json::to_string(msg).map_err(|e| format!("Failed to serialize frame: {e}"))?;
    if encoded.len() > max_bytes {
        return Err(format!("Protocol frame exceeds {max_bytes}-byte limit."));
    }
    encoded.push('\n');
    Ok(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_execute_and_child_messages() {
        let exec = ParentMessage::Execute {
            run_id: "run-1".into(),
            program: Program {
                format: "scheme".into(),
                source: "(+ 1 2)".into(),
                repl: Some(RunnerReplOp::Eval),
            },
            inputs: BTreeMap::new(),
            bindings: Bindings {
                api_paths: vec![vec!["files".into(), "read".into()]],
                spread_api_paths: vec![],
                language_bindings: serde_json::json!([]),
            },
        };
        let line = encode_frame(&exec, DEFAULT_MAX_PROTOCOL_BYTES).unwrap();
        let parsed: ParentMessage = serde_json::from_str(line.trim_end()).unwrap();
        assert!(matches!(parsed, ParentMessage::Execute { run_id, .. } if run_id == "run-1"));

        let ready_json = r#"{"type":"ready","protocolVersion":1,"runner":{"id":"steel-repl","programFormat":"scheme"}}"#;
        let ready: ChildMessage = serde_json::from_str(ready_json).unwrap();
        assert!(matches!(
            ready,
            ChildMessage::Ready {
                protocol_version: 1,
                ..
            }
        ));
    }
}
