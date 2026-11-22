//! Live capability catalog discovery: `capabilities/list`, `capabilities/search`, `capabilities/describe`.

use crate::capability::{
    ArgumentMode, CallContext, Capability, CapabilityCatalog, CapabilityError, CapabilityMeta,
};
use async_trait::async_trait;
use serde_json::{json, Map, Value};
use std::sync::Arc;

const SEARCH_DEFAULT_LIMIT: usize = 20;
const SEARCH_MAX_LIMIT: usize = 50;
const SEARCH_MAX_BYTES: usize = 16 * 1024;
const SEARCH_DESC_BYTES: usize = 240;
const SEARCH_SIG_BYTES: usize = 2048;

fn compact_text(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_string(), false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

fn matches_namespace(api_path: &[String], namespace: Option<&str>) -> bool {
    let Some(ns) = namespace.filter(|s| !s.is_empty()) else {
        return true;
    };
    let prefix: Vec<&str> = ns.split(['.', '/']).filter(|s| !s.is_empty()).collect();
    prefix.len() <= api_path.len()
        && prefix
            .iter()
            .zip(api_path.iter())
            .all(|(p, a)| *p == a.as_str())
}

fn summary_json(ctx: &CallContext, metas: &[CapabilityMeta], meta: &CapabilityMeta) -> Value {
    let api_path = meta.public_name();
    let binding = ctx.language.binding_name(metas, meta);
    json!({
        "name": api_path,
        "apiPath": api_path,
        "binding": binding,
        "language": ctx.language.identity().language,
        "argumentMode": match meta.argument_mode {
            ArgumentMode::Single => "single",
            ArgumentMode::Spread => "spread",
        },
        "toolName": meta.tool_name,
        "description": meta.description,
    })
}

struct CapabilitiesList {
    meta: CapabilityMeta,
}

#[async_trait]
impl Capability for CapabilitiesList {
    fn meta(&self) -> &CapabilityMeta {
        &self.meta
    }

    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let namespace = args.get("namespace").and_then(Value::as_str);
        let metas = ctx.visible_metas();
        let mut items: Vec<(String, Value)> = metas
            .iter()
            .filter(|m| matches_namespace(&m.api_path, namespace))
            .map(|m| (m.public_name(), summary_json(ctx, &metas, m)))
            .collect();
        items.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(Value::Array(items.into_iter().map(|(_, v)| v).collect()))
    }
}

struct CapabilitiesSearch {
    meta: CapabilityMeta,
}

#[async_trait]
impl Capability for CapabilitiesSearch {
    fn meta(&self) -> &CapabilityMeta {
        &self.meta
    }

    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_lowercase();
        let namespace = args.get("namespace").and_then(Value::as_str);
        let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
        let limit = (args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(SEARCH_DEFAULT_LIMIT as u64) as usize)
            .clamp(1, SEARCH_MAX_LIMIT);

        let metas = ctx.visible_metas();
        let mut matches: Vec<(String, &CapabilityMeta, String)> = metas
            .iter()
            .filter(|m| matches_namespace(&m.api_path, namespace))
            .map(|m| {
                let api_path = m.public_name();
                let binding = ctx.language.binding_name(&metas, m);
                (api_path, m, binding)
            })
            .filter(|(api_path, m, binding)| {
                query.is_empty()
                    || api_path.to_lowercase().contains(&query)
                    || binding.to_lowercase().contains(&query)
                    || m.description.to_lowercase().contains(&query)
            })
            .collect();
        matches.sort_by(|a, b| a.0.cmp(&b.0));

        let total = matches.len();
        let mut items: Vec<Value> = Vec::new();
        for (api_path, m, binding) in matches.into_iter().skip(offset).take(limit) {
            let (desc, desc_trunc) = compact_text(&m.description, SEARCH_DESC_BYTES);
            let (sig, sig_trunc) =
                compact_text(&ctx.language.signature(&metas, m), SEARCH_SIG_BYTES);
            items.push(json!({
                "apiPath": api_path,
                "binding": binding,
                "signature": sig,
                "description": desc,
                "descriptionTruncated": desc_trunc,
                "signatureTruncated": sig_trunc,
            }));
            let page_est = serde_json::to_vec(&items).map(|b| b.len()).unwrap_or(0);
            if page_est > SEARCH_MAX_BYTES && items.len() > 1 {
                items.pop();
                break;
            }
        }
        let has_more = offset + items.len() < total;
        Ok(json!({
            "items": items,
            "total": total,
            "hasMore": has_more,
            "nextOffset": if has_more { json!(offset + items.len()) } else { Value::Null },
        }))
    }
}

struct CapabilitiesDescribe {
    meta: CapabilityMeta,
}

#[async_trait]
impl Capability for CapabilitiesDescribe {
    fn meta(&self) -> &CapabilityMeta {
        &self.meta
    }

    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let metas = ctx.visible_metas();
        let describe_one = |name: &str| -> Value {
            let trimmed = name.strip_prefix("tools.").unwrap_or(name);
            let path: Vec<String> = trimmed
                .split(['.', '/'])
                .filter(|s| !s.is_empty())
                .map(ToString::to_string)
                .collect();
            let found = metas.iter().find(|m| {
                m.api_path == path
                    || m.tool_name == name
                    || ctx.language.binding_name(&metas, m) == name
            });
            match found {
                Some(m) => {
                    let mut obj = summary_json(ctx, &metas, m);
                    if let Some(map) = obj.as_object_mut() {
                        map.insert("parameters".into(), m.params_schema.clone());
                        map.insert("resultSchema".into(), m.result_schema.clone());
                        map.insert(
                            "signature".into(),
                            Value::String(ctx.language.signature(&metas, m)),
                        );
                    }
                    obj
                }
                None => Value::Null,
            }
        };

        if let Some(names) = args.get("names").and_then(Value::as_array) {
            let out: Vec<Value> = names
                .iter()
                .map(|v| v.as_str().map(describe_one).unwrap_or(Value::Null))
                .collect();
            Ok(Value::Array(out))
        } else if let Some(name) = args.get("name").and_then(Value::as_str) {
            Ok(describe_one(name))
        } else {
            Err(CapabilityError::invalid_args(
                "capabilities.describe",
                "Expected 'name' string or 'names' array.",
            ))
        }
    }
}

pub fn register(catalog: &CapabilityCatalog) {
    catalog.register(Arc::new(CapabilitiesList {
        meta: CapabilityMeta {
            tool_name: "__cant_capabilities_list".into(),
            api_path: vec!["capabilities".into(), "list".into()],
            description: "List the live callable capability catalog, optionally filtered by namespace prefix.".into(),
            params_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "namespace": { "type": "string", "description": "Optional API namespace prefix, e.g. files or system." }
                }
            }),
            result_schema: json!({ "type": "array" }),
            argument_mode: ArgumentMode::Single,
            language_hints: Map::new(),
        },
    }));

    let mut search_hints = Map::new();
    search_hints.insert("steel".into(), json!({ "positional": ["query"] }));
    catalog.register(Arc::new(CapabilitiesSearch {
        meta: CapabilityMeta {
            tool_name: "__cant_capabilities_search".into(),
            api_path: vec!["capabilities".into(), "search".into()],
            description: "Search callable capabilities by case-insensitive substring in API paths, bindings, or descriptions. Returns compact paginated signatures.".into(),
            params_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "query": { "type": "string" },
                    "namespace": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 50 },
                    "offset": { "type": "integer", "minimum": 0 }
                }
            }),
            result_schema: json!({ "type": "object" }),
            argument_mode: ArgumentMode::Single,
            language_hints: search_hints,
        },
    }));

    let mut desc_hints = Map::new();
    desc_hints.insert("steel".into(), json!({ "positional": ["name"] }));
    catalog.register(Arc::new(CapabilitiesDescribe {
        meta: CapabilityMeta {
            tool_name: "__cant_capabilities_describe".into(),
            api_path: vec!["capabilities".into(), "describe".into()],
            description: "Describe one or more callable capabilities with full descriptions and JSON schemas.".into(),
            params_schema: json!({
                "anyOf": [
                    {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["name"],
                        "properties": { "name": { "type": "string" } }
                    },
                    {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["names"],
                        "properties": { "names": { "type": "array", "items": { "type": "string" } } }
                    }
                ]
            }),
            result_schema: json!({}),
            argument_mode: ArgumentMode::Single,
            language_hints: desc_hints,
        },
    }));
}
