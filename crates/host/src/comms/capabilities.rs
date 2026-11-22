//! Steel capabilities for `peers/*` and `channels/*`.

use super::{OpenChannelOpts, DEFAULT_WAIT_TIMEOUT_SECS, MAX_WAIT_TIMEOUT_SECS};
use crate::capability::{
    ArgumentMode, CallContext, Capability, CapabilityCatalog, CapabilityError, CapabilityMeta,
};
use async_trait::async_trait;
use serde_json::{json, Map, Value};
use std::sync::Arc;
use std::time::Duration;

fn positional_hints(names: &[&str]) -> Map<String, Value> {
    let mut m = Map::new();
    if !names.is_empty() {
        m.insert("steel".into(), json!({ "positional": names }));
    }
    m
}

fn opt_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn req_str<'a>(args: &'a Value, key: &str, tool: &str) -> Result<&'a str, CapabilityError> {
    opt_str(args, key).ok_or_else(|| {
        CapabilityError::invalid_args(tool, format!("Missing required string argument '{key}'."))
    })
}

fn opt_str_list(
    args: &Value,
    key: &str,
    tool: &str,
) -> Result<Option<Vec<String>>, CapabilityError> {
    let Some(val) = args.get(key) else {
        return Ok(None);
    };
    if val.is_null() {
        return Ok(None);
    }
    let arr = val.as_array().ok_or_else(|| {
        CapabilityError::invalid_args(tool, format!("'{key}' must be an array of strings."))
    })?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let s = item.as_str().ok_or_else(|| {
            CapabilityError::invalid_args(tool, format!("'{key}' entries must be strings."))
        })?;
        out.push(s.to_string());
    }
    Ok(Some(out))
}

struct PeersList {
    meta: CapabilityMeta,
}
#[async_trait]
impl Capability for PeersList {
    fn meta(&self) -> &CapabilityMeta {
        &self.meta
    }
    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let query = opt_str(&args, "query");
        let tag = opt_str(&args, "tag");
        let include_stale = args.get("all").and_then(Value::as_bool).unwrap_or(false);
        let peers = ctx.comms.list_peers(query, tag, include_stale);
        serde_json::to_value(peers).map_err(|e| CapabilityError::exec("CommsError", e.to_string()))
    }
}

struct PeersMe {
    meta: CapabilityMeta,
}
#[async_trait]
impl Capability for PeersMe {
    fn meta(&self) -> &CapabilityMeta {
        &self.meta
    }
    async fn call(&self, ctx: &CallContext, _args: Value) -> Result<Value, CapabilityError> {
        let me = ctx.comms.peer_me(&ctx.session_id, &ctx.cwd);
        serde_json::to_value(me).map_err(|e| CapabilityError::exec("CommsError", e.to_string()))
    }
}

struct PeersAnnounce {
    meta: CapabilityMeta,
}
#[async_trait]
impl Capability for PeersAnnounce {
    fn meta(&self) -> &CapabilityMeta {
        &self.meta
    }
    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let name = opt_str(&args, "name");
        let topic = args.get("topic").and_then(Value::as_str);
        let tags = opt_str_list(&args, "tags", "peers/announce")?;
        let updated = ctx
            .comms
            .announce(&ctx.session_id, &ctx.cwd, name, topic, tags)
            .map_err(|e| CapabilityError::exec("CommsError", e))?;
        serde_json::to_value(updated)
            .map_err(|e| CapabilityError::exec("CommsError", e.to_string()))
    }
}

struct ChannelsOpen {
    meta: CapabilityMeta,
}
#[async_trait]
impl Capability for ChannelsOpen {
    fn meta(&self) -> &CapabilityMeta {
        &self.meta
    }
    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let opts = OpenChannelOpts {
            to: opt_str(&args, "to").map(ToString::to_string),
            name: opt_str(&args, "name").map(ToString::to_string),
            members: opt_str_list(&args, "members", "channels/open")?.unwrap_or_default(),
            topic: opt_str(&args, "topic").map(ToString::to_string),
            message: args
                .get("message")
                .and_then(Value::as_str)
                .map(ToString::to_string),
        };
        let res = ctx
            .comms
            .open_channel(&ctx.session_id, &ctx.cwd, opts)
            .map_err(|e| CapabilityError::exec("CommsError", e))?;
        serde_json::to_value(res).map_err(|e| CapabilityError::exec("CommsError", e.to_string()))
    }
}

struct ChannelsList {
    meta: CapabilityMeta,
}
#[async_trait]
impl Capability for ChannelsList {
    fn meta(&self) -> &CapabilityMeta {
        &self.meta
    }
    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let all = args.get("all").and_then(Value::as_bool).unwrap_or(false);
        let channels = ctx.comms.list_channels(Some(&ctx.session_id), all);
        serde_json::to_value(channels)
            .map_err(|e| CapabilityError::exec("CommsError", e.to_string()))
    }
}

struct ChannelsJoin {
    meta: CapabilityMeta,
}
#[async_trait]
impl Capability for ChannelsJoin {
    fn meta(&self) -> &CapabilityMeta {
        &self.meta
    }
    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let ch = req_str(&args, "channel", "channels/join")?;
        let summary = ctx
            .comms
            .join_channel(&ctx.session_id, &ctx.cwd, ch)
            .map_err(|e| CapabilityError::exec("CommsError", e))?;
        serde_json::to_value(summary)
            .map_err(|e| CapabilityError::exec("CommsError", e.to_string()))
    }
}

struct ChannelsLeave {
    meta: CapabilityMeta,
}
#[async_trait]
impl Capability for ChannelsLeave {
    fn meta(&self) -> &CapabilityMeta {
        &self.meta
    }
    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let ch = req_str(&args, "channel", "channels/leave")?;
        let summary = ctx
            .comms
            .leave_channel(&ctx.session_id, ch)
            .map_err(|e| CapabilityError::exec("CommsError", e))?;
        serde_json::to_value(summary)
            .map_err(|e| CapabilityError::exec("CommsError", e.to_string()))
    }
}

struct ChannelsSend {
    meta: CapabilityMeta,
}
#[async_trait]
impl Capability for ChannelsSend {
    fn meta(&self) -> &CapabilityMeta {
        &self.meta
    }
    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let ch = req_str(&args, "channel", "channels/send")?;
        let body = req_str(&args, "body", "channels/send")?;
        let res = ctx
            .comms
            .send_message(&ctx.session_id, &ctx.cwd, ch, body.to_string())
            .map_err(|e| CapabilityError::exec("CommsError", e))?;
        serde_json::to_value(res).map_err(|e| CapabilityError::exec("CommsError", e.to_string()))
    }
}

struct ChannelsRead {
    meta: CapabilityMeta,
}
#[async_trait]
impl Capability for ChannelsRead {
    fn meta(&self) -> &CapabilityMeta {
        &self.meta
    }
    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let ch = req_str(&args, "channel", "channels/read")?;
        let unread = args.get("unread").and_then(Value::as_bool).unwrap_or(true);
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(50) as usize;
        let (channel, messages) = ctx
            .comms
            .read_channel(Some(&ctx.session_id), ch, unread, limit)
            .map_err(|e| CapabilityError::exec("CommsError", e))?;
        Ok(json!({ "channel": channel, "messages": messages }))
    }
}

struct ChannelsWait {
    meta: CapabilityMeta,
}
#[async_trait]
impl Capability for ChannelsWait {
    fn meta(&self) -> &CapabilityMeta {
        &self.meta
    }
    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let ch = opt_str(&args, "channel");
        let secs = args
            .get("timeout")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_WAIT_TIMEOUT_SECS)
            .clamp(1, MAX_WAIT_TIMEOUT_SECS);
        let res = ctx
            .comms
            .wait_for_messages(&ctx.session_id, &ctx.cwd, ch, Duration::from_secs(secs))
            .await
            .map_err(|e| CapabilityError::exec("CommsError", e))?;
        serde_json::to_value(res).map_err(|e| CapabilityError::exec("CommsError", e.to_string()))
    }
}

struct ChannelsClose {
    meta: CapabilityMeta,
}
#[async_trait]
impl Capability for ChannelsClose {
    fn meta(&self) -> &CapabilityMeta {
        &self.meta
    }
    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let ch = req_str(&args, "channel", "channels/close")?;
        let summary = opt_str(&args, "summary").map(ToString::to_string);
        let closed = ctx
            .comms
            .close_channel(&ctx.session_id, ch, summary)
            .map_err(|e| CapabilityError::exec("CommsError", e))?;
        serde_json::to_value(closed).map_err(|e| CapabilityError::exec("CommsError", e.to_string()))
    }
}

pub fn register(catalog: &CapabilityCatalog) {
    catalog.register(Arc::new(PeersList {
        meta: CapabilityMeta {
            tool_name: "__cant_peers_list".into(),
            api_path: vec!["peers".into(), "list".into()],
            description: "List active local agent peers connected to the Cant daemon (handles, cwds, topics, harnesses, reachability).".into(),
            params_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "query": { "type": "string", "description": "Optional case-insensitive filter across handle, cwd, topic, and tags." },
                    "tag": { "type": "string", "description": "Optional exact tag filter." },
                    "all": { "type": "boolean", "description": "Include stale idle peers (default: false)." }
                }
            }),
            result_schema: json!({ "type": "array" }),
            argument_mode: ArgumentMode::Single,
            language_hints: positional_hints(&[]),
        },
    }));

    catalog.register(Arc::new(PeersMe {
        meta: CapabilityMeta {
            tool_name: "__cant_peers_me".into(),
            api_path: vec!["peers".into(), "me".into()],
            description: "Return this session's own peer identity, handle, and reachability."
                .into(),
            params_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {}
            }),
            result_schema: json!({ "type": "object" }),
            argument_mode: ArgumentMode::Single,
            language_hints: positional_hints(&[]),
        },
    }));

    catalog.register(Arc::new(PeersAnnounce {
        meta: CapabilityMeta {
            tool_name: "__cant_peers_announce".into(),
            api_path: vec!["peers".into(), "announce".into()],
            description: "Advertise or update this session's peer handle, current topic, and tags for other agents to discover.".into(),
            params_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "name": { "type": "string", "description": "Short unique handle (e.g. 'auth-backend')." },
                    "topic": { "type": "string", "description": "Short summary of what this agent is working on." },
                    "tags": { "type": "array", "items": { "type": "string" } }
                }
            }),
            result_schema: json!({ "type": "object" }),
            argument_mode: ArgumentMode::Single,
            language_hints: positional_hints(&[]),
        },
    }));

    catalog.register(Arc::new(ChannelsOpen {
        meta: CapabilityMeta {
            tool_name: "__cant_channels_open".into(),
            api_path: vec!["channels".into(), "open".into()],
            description: "Open a direct channel (#:to peer) or named room (#:name room) with other local agents, optionally sending an initial #:message.".into(),
            params_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "to": { "type": "string", "description": "Target peer handle or session ID for a direct channel." },
                    "name": { "type": "string", "description": "Named room slug (e.g. 'api-contract' or '#api-contract'). Idempotent if already open." },
                    "members": { "type": "array", "items": { "type": "string" }, "description": "Additional peer handles or session IDs to invite." },
                    "topic": { "type": "string", "description": "Channel topic/purpose." },
                    "message": { "type": "string", "description": "Optional initial message to send immediately." }
                }
            }),
            result_schema: json!({ "type": "object" }),
            argument_mode: ArgumentMode::Single,
            language_hints: positional_hints(&[]),
        },
    }));

    catalog.register(Arc::new(ChannelsList {
        meta: CapabilityMeta {
            tool_name: "__cant_channels_list".into(),
            api_path: vec!["channels".into(), "list".into()],
            description: "List open rooms and channels you belong to (set #:all #t to include closed channels).".into(),
            params_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "all": { "type": "boolean", "description": "Include closed channels and all direct channels (default: false)." }
                }
            }),
            result_schema: json!({ "type": "array" }),
            argument_mode: ArgumentMode::Single,
            language_hints: positional_hints(&[]),
        },
    }));

    catalog.register(Arc::new(ChannelsJoin {
        meta: CapabilityMeta {
            tool_name: "__cant_channels_join".into(),
            api_path: vec!["channels".into(), "join".into()],
            description: "Join an existing open channel by ID or #room name.".into(),
            params_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["channel"],
                "properties": {
                    "channel": { "type": "string" }
                }
            }),
            result_schema: json!({ "type": "object" }),
            argument_mode: ArgumentMode::Single,
            language_hints: positional_hints(&["channel"]),
        },
    }));

    catalog.register(Arc::new(ChannelsLeave {
        meta: CapabilityMeta {
            tool_name: "__cant_channels_leave".into(),
            api_path: vec!["channels".into(), "leave".into()],
            description: "Leave a channel by ID or #room name.".into(),
            params_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["channel"],
                "properties": {
                    "channel": { "type": "string" }
                }
            }),
            result_schema: json!({ "type": "object" }),
            argument_mode: ArgumentMode::Single,
            language_hints: positional_hints(&["channel"]),
        },
    }));

    catalog.register(Arc::new(ChannelsSend {
        meta: CapabilityMeta {
            tool_name: "__cant_channels_send".into(),
            api_path: vec!["channels".into(), "send".into()],
            description: "Send a message to a channel by ID or #room name; returns per-recipient delivery status (pushed, woken, queued).".into(),
            params_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["channel", "body"],
                "properties": {
                    "channel": { "type": "string" },
                    "body": { "type": "string" }
                }
            }),
            result_schema: json!({ "type": "object" }),
            argument_mode: ArgumentMode::Single,
            language_hints: positional_hints(&["channel", "body"]),
        },
    }));

    catalog.register(Arc::new(ChannelsRead {
        meta: CapabilityMeta {
            tool_name: "__cant_channels_read".into(),
            api_path: vec!["channels".into(), "read".into()],
            description: "Read unread messages (default #:unread #t) or recent history from a channel, advancing your read cursor.".into(),
            params_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["channel"],
                "properties": {
                    "channel": { "type": "string" },
                    "unread": { "type": "boolean", "description": "Only return unread messages since your cursor (default: true)." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 500 }
                }
            }),
            result_schema: json!({ "type": "object" }),
            argument_mode: ArgumentMode::Single,
            language_hints: positional_hints(&["channel"]),
        },
    }));

    catalog.register(Arc::new(ChannelsWait {
        meta: CapabilityMeta {
            tool_name: "__cant_channels_wait".into(),
            api_path: vec!["channels".into(), "wait".into()],
            description: "Wait up to #:timeout seconds (default 30, max 100) for new messages on a specific channel or any joined channel.".into(),
            params_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "channel": { "type": "string", "description": "Optional channel ID or #room name; omit to wait on any joined channel." },
                    "timeout": { "type": "integer", "minimum": 1, "maximum": 100 }
                }
            }),
            result_schema: json!({ "type": "object" }),
            argument_mode: ArgumentMode::Single,
            language_hints: positional_hints(&["channel"]),
        },
    }));

    catalog.register(Arc::new(ChannelsClose {
        meta: CapabilityMeta {
            tool_name: "__cant_channels_close".into(),
            api_path: vec!["channels".into(), "close".into()],
            description:
                "Close a channel with an optional #:summary once coordination is complete.".into(),
            params_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["channel"],
                "properties": {
                    "channel": { "type": "string" },
                    "summary": { "type": "string" }
                }
            }),
            result_schema: json!({ "type": "object" }),
            argument_mode: ArgumentMode::Single,
            language_hints: positional_hints(&["channel"]),
        },
    }));
}
