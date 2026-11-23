//! Steel literal metadata formatting and signature presentation.

use crate::bindings::{BindingMode, SteelBinding};
use cant_host::capability::{ArgumentMode, CapabilityMeta};
use serde_json::Value;
use std::collections::HashMap;

pub fn format_steel_literal(value: &Value) -> String {
    literal(value, 0, true)
}

fn compound(open: &str, items: &[String], depth: usize, pretty: bool) -> String {
    let flat = format!(
        "{open}{})",
        items.iter().map(|i| format!(" {i}")).collect::<String>()
    );
    if !pretty || (flat.len() <= 100 && !flat.contains('\n')) {
        return flat;
    }
    let inner_indent = "  ".repeat(depth + 1);
    let close_indent = "  ".repeat(depth);
    let body = items
        .iter()
        .map(|i| format!("{inner_indent}{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("{open}\n{body}\n{close_indent})")
}

fn literal(value: &Value, depth: usize, pretty: bool) -> String {
    if depth > 64 {
        return "\"<max-depth>\"".into();
    }
    match value {
        Value::Null => "json-null".into(),
        Value::Bool(true) => "#t".into(),
        Value::Bool(false) => "#f".into(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => {
            let mut out = String::with_capacity(s.len() + 2);
            out.push('"');
            for ch in s.chars() {
                match ch {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    c if (c as u32) < 32 || c == '\x7f' => {
                        out.push_str(&format!("\\u{{{:x}}}", c as u32));
                    }
                    c => out.push(c),
                }
            }
            out.push('"');
            out
        }
        Value::Array(arr) => {
            let items: Vec<String> = arr.iter().map(|v| literal(v, depth + 1, pretty)).collect();
            compound("(list", &items, depth, pretty)
        }
        Value::Object(map) => {
            let items: Vec<String> = map
                .iter()
                .map(|(k, v)| {
                    format!(
                        "{} {}",
                        literal(&Value::String(k.clone()), depth + 1, false),
                        literal(v, depth + 1, pretty)
                    )
                })
                .collect();
            compound("(hash", &items, depth, pretty)
        }
    }
}

pub fn signature(binding: Option<&SteelBinding>, meta: &CapabilityMeta) -> String {
    let name = binding
        .map(|b| b.name.as_str())
        .unwrap_or_else(|| meta.tool_name.as_str());
    let Some(b) = binding else {
        let arg = if meta.argument_mode == ArgumentMode::Spread {
            "arg ..."
        } else {
            "argument"
        };
        return format!("(tools.{} {arg})", meta.api_path.join("."));
    };
    if !matches!(b.mode, BindingMode::Object | BindingMode::Options) {
        let arg = if meta.argument_mode == ArgumentMode::Spread {
            "arg ..."
        } else {
            "argument"
        };
        return format!("({name} {arg})");
    }

    let kw_by_field: HashMap<&str, (&str, bool)> = b
        .keywords
        .iter()
        .map(|k| (k.field.as_str(), (k.keyword.as_str(), k.required)))
        .collect();
    let mut parts = vec![name.to_string()];
    for pos in &b.positional {
        let req = b.mode == BindingMode::Options
            || kw_by_field.get(pos.as_str()).is_some_and(|(_, r)| *r);
        parts.push(if req { pos.clone() } else { format!("[{pos}]") });
    }
    for kw in &b.keywords {
        if b.mode == BindingMode::Object && b.positional.contains(&kw.field) {
            continue;
        }
        let arg = format!("#:{} {}", kw.keyword, kw.field);
        parts.push(if kw.required { arg } else { format!("[{arg}]") });
    }
    format!("({})", parts.join(" "))
}
