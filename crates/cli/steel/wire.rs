use cant_lang_steel::bindings::SteelBinding;
use serde::Deserialize;
use serde_json::Value;
use std::io::{self, BufRead, Write};

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepts_repl_requests_and_rejects_invalid_options() {
        let frame = serde_json::json!({"type":"execute","runId":"r","program":{"format":"scheme","source":"(+ 1 2)","repl":{"op":"eval"}},"inputs":{},"bindings":{"apiPaths":[]}});
        assert!(read(&mut format!("{frame}\n").as_bytes()).is_ok());
        let mut missing = frame.clone();
        missing["program"].as_object_mut().unwrap().remove("repl");
        assert!(read(&mut format!("{missing}\n").as_bytes()).is_err());
        for request in [
            serde_json::json!({"op":"expand"}),
            serde_json::json!({"op":"inspect","offset":-1}),
            serde_json::json!({"op":"search","limit":1.5}),
            serde_json::json!({"op":"inspect","origin":"system"}),
        ] {
            let mut invalid = frame.clone();
            invalid["program"]["repl"] = request;
            assert!(read(&mut format!("{invalid}\n").as_bytes()).is_err());
        }
    }

    #[test]
    fn rejects_invalid_frames_and_missing_results() {
        for text in ["{}\n", "[]\n", "{\"type\":\"ready\"}\n", "{\"type\":\"tool_result\",\"runId\":\"r\",\"callId\":\"c\",\"ok\":true}\n", "{\"type\":\"tool_result\",\"runId\":\"r\",\"callId\":\"c\",\"ok\":false,\"error\":{\"name\":\"Error\",\"message\":\"no\",\"stack\":4}}\n"] {assert!(read(&mut text.as_bytes()).is_err(),"{text}");}
    }
    #[test]
    fn accepts_null_results_and_limits_unterminated_input() {
        assert!(matches!(read(&mut b"{\"type\":\"tool_result\",\"runId\":\"r\",\"callId\":\"c\",\"ok\":true,\"value\":null}\n".as_slice()).unwrap(),Message::ToolResult{value:Value::Null,..}));
        assert!(read(&mut vec![b' '; MAX_FRAME + 2].as_slice()).is_err());
    }
}

pub const MAX_FRAME: usize = 2 * 1024 * 1024;
pub const MAX_VALUE: usize = 256 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Program {
    pub format: String,
    pub source: String,
    pub repl: ReplRequest,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReplRequest {
    pub op: ReplOp,
    pub name: Option<String>,
    pub query: Option<String>,
    pub origin: Option<ReplOrigin>,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}
#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ReplOp {
    Eval,
    Inspect,
    Search,
    Describe,
}
#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ReplOrigin {
    User,
    All,
}
fn deserialize_steel_bindings<'de, D>(deserializer: D) -> Result<Vec<SteelBinding>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<Vec<SteelBinding>>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Bindings {
    #[serde(
        default,
        rename = "languageBindings",
        alias = "steelBindings",
        deserialize_with = "deserialize_steel_bindings"
    )]
    pub steel_bindings: Vec<SteelBinding>,
    pub api_paths: Vec<Vec<String>>,
    #[serde(default)]
    pub spread_api_paths: Vec<Vec<String>>,
    #[serde(default)]
    pub environment_scopes: bool,
}
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Message {
    Execute {
        #[serde(rename = "runId")]
        run_id: String,
        program: Program,
        inputs: std::collections::BTreeMap<String, String>,
        bindings: Bindings,
    },
    ToolResult {
        #[serde(rename = "runId")]
        run_id: String,
        #[serde(rename = "callId")]
        call_id: String,
        ok: bool,
        #[serde(default)]
        value: Value,
        error: Option<RemoteError>,
    },
    Cancel {
        #[serde(rename = "runId")]
        run_id: String,
    },
    Shutdown,
}
#[derive(Deserialize)]
pub struct RemoteError {
    pub name: String,
    pub message: String,
    #[serde(rename = "stack")]
    pub _stack: Option<String>,
}

pub fn read(reader: &mut impl BufRead) -> io::Result<Message> {
    let mut bytes = Vec::new();
    loop {
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Protocol input closed",
            ));
        }
        let count = chunk
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|index| index + 1)
            .unwrap_or(chunk.len());
        if bytes.len() + count > MAX_FRAME + 1 {
            return Err(io::Error::other("Protocol frame exceeds limit"));
        }
        bytes.extend_from_slice(&chunk[..count]);
        reader.consume(count);
        if bytes.last() == Some(&b'\n') {
            break;
        }
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if value.get("type").and_then(Value::as_str) == Some("tool_result")
        && value.get("ok").and_then(Value::as_bool) == Some(true)
        && value.get("value").is_none()
    {
        return Err(io::Error::other("Missing tool result value"));
    }
    serde_json::from_value(value).map_err(io::Error::other)
}

pub fn write(writer: &mut impl Write, value: &Value) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    if bytes.len() > MAX_FRAME {
        return Err(io::Error::other("Protocol frame exceeds limit"));
    }
    writer.write_all(&bytes)?;
    writer.write_all(b"\n")?;
    writer.flush()
}
