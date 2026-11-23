//! Projects Cant capability catalog entries into Steel native slash/kebab-case bindings.

use crate::catalog;
use cant_host::capability::{ArgumentMode, CapabilityMeta};
use serde::{de::IntoDeserializer, Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BindingMode {
    Single,
    Spread,
    Object,
    Options,
}

// A mode is a string on the wire, not an externally tagged JSON object.
fn deserialize_mode<'de, D: Deserializer<'de>>(de: D) -> Result<BindingMode, D::Error> {
    BindingMode::deserialize(String::deserialize(de)?.into_deserializer())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SteelKeyword {
    pub keyword: String,
    pub field: String,
    pub required: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SteelBinding {
    pub path: Vec<String>,
    pub name: String,
    #[serde(deserialize_with = "deserialize_mode")]
    pub mode: BindingMode,
    pub positional: Vec<String>,
    pub keywords: Vec<SteelKeyword>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature_args: Option<Vec<String>>,
}

struct ObjectShape {
    fields: Vec<String>,
    required: HashSet<String>,
}

fn reserved_names() -> &'static HashSet<String> {
    static RESERVED: OnceLock<HashSet<String>> = OnceLock::new();
    RESERVED.get_or_init(|| {
        let mut set = catalog::native_reservations();
        for constructor in catalog::constructors().expect("invalid bundled Steel constructors") {
            set.insert(constructor.name);
        }
        set
    })
}

pub fn kebab(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let mut out = String::with_capacity(value.len() + 4);
    for (i, &c) in chars.iter().enumerate() {
        if c == '_' {
            out.push('-');
            continue;
        }
        if c.is_ascii_uppercase() && i > 0 {
            let prev = chars[i - 1];
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_ascii_lowercase());
            if prev.is_ascii_lowercase()
                || prev.is_ascii_digit()
                || (prev.is_ascii_uppercase() && next_lower)
            {
                out.push('-');
            }
        }
        out.push(c.to_ascii_lowercase());
    }
    out
}

fn is_identifier(s: &str) -> bool {
    let mut bytes = s.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    first.is_ascii_lowercase()
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn object_shape(schema: &Value, depth: usize) -> Option<ObjectShape> {
    if depth > 16 {
        return None;
    }
    let obj = schema.as_object()?;
    if obj.contains_key("$ref") || obj.contains_key("patternProperties") {
        return None;
    }
    if let Some(ap) = obj.get("additionalProperties") {
        if ap != &Value::Bool(false) {
            return None;
        }
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(branches) = obj.get(key).and_then(Value::as_array) {
            if branches.is_empty() {
                return None;
            }
            let shapes: Option<Vec<ObjectShape>> = branches
                .iter()
                .map(|b| object_shape(b, depth + 1))
                .collect();
            let shapes = shapes?;
            let mut fields = Vec::new();
            let mut seen = HashSet::new();
            for s in &shapes {
                for f in &s.fields {
                    if seen.insert(f.clone()) {
                        fields.push(f.clone());
                    }
                }
            }
            let required = if key == "allOf" {
                shapes
                    .iter()
                    .flat_map(|s| s.required.iter().cloned())
                    .collect()
            } else {
                shapes[0]
                    .required
                    .iter()
                    .filter(|f| shapes.iter().all(|s| s.required.contains(*f)))
                    .cloned()
                    .collect()
            };
            return Some(ObjectShape { fields, required });
        }
    }

    if obj.get("type").and_then(Value::as_str) != Some("object") {
        return None;
    }
    let props = obj.get("properties").and_then(Value::as_object);
    let fields: Vec<String> = match props {
        Some(map) => map.keys().cloned().collect(),
        None if obj.get("additionalProperties") == Some(&Value::Bool(false)) => vec![],
        None => return None,
    };
    let required: HashSet<String> = obj
        .get("required")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(ToString::to_string))
                .collect()
        })
        .unwrap_or_default();
    Some(ObjectShape { fields, required })
}

pub fn resolve_steel_bindings(metas: &[CapabilityMeta]) -> Vec<SteelBinding> {
    let reserved = reserved_names();
    struct Candidate {
        path: Vec<String>,
        native_name: String,
        mode: BindingMode,
        positional: Vec<String>,
        keywords: Vec<SteelKeyword>,
        safe: bool,
    }

    let mut candidates = Vec::new();
    for meta in metas {
        let spread = meta.argument_mode == ArgumentMode::Spread;
        let shape = if spread {
            None
        } else {
            object_shape(&meta.params_schema, 0)
        };
        let steel_hints = meta.language_hints.get("steel").and_then(Value::as_object);
        let hint_positional: Vec<String> = steel_hints
            .and_then(|h| h.get("positional"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(ToString::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let hint_keywords: HashMap<String, String> = steel_hints
            .and_then(|h| h.get("keywords"))
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        let mode = if spread {
            BindingMode::Spread
        } else if shape.is_some() {
            BindingMode::Object
        } else {
            BindingMode::Single
        };

        let keywords: Vec<SteelKeyword> = match &shape {
            Some(s) => s
                .fields
                .iter()
                .map(|field| {
                    let default_kw = kebab(field);
                    let kw = hint_keywords
                        .get(field)
                        .cloned()
                        .unwrap_or_else(|| default_kw.clone());
                    let aliases = if kw != default_kw {
                        vec![default_kw]
                    } else {
                        vec![]
                    };
                    SteelKeyword {
                        keyword: kw,
                        field: field.clone(),
                        required: s.required.contains(field),
                        aliases,
                    }
                })
                .collect(),
            None => vec![],
        };

        let segments: Vec<String> = meta.api_path.iter().map(|s| kebab(s)).collect();
        let native_name = segments.join("/");
        let kw_valid = {
            let mut seen = HashSet::new();
            keywords.iter().all(|k| {
                is_identifier(&k.keyword)
                    && seen.insert(k.keyword.clone())
                    && k.aliases
                        .iter()
                        .all(|a| is_identifier(a) && seen.insert(a.clone()))
            })
        };
        let safe = meta.api_path.len() > 1
            && native_name.len() <= 1024
            && segments.iter().all(|s| is_identifier(s))
            && !reserved.contains(&native_name)
            && kw_valid
            && (spread || shape.is_some());

        candidates.push(Candidate {
            path: meta.api_path.clone(),
            native_name,
            mode,
            positional: hint_positional,
            keywords,
            safe,
        });
    }

    let mut counts: HashMap<String, usize> = HashMap::new();
    for c in &candidates {
        *counts.entry(c.native_name.clone()).or_insert(0) += 1;
    }

    candidates
        .into_iter()
        .map(|c| {
            let native = c.safe && counts.get(&c.native_name).copied().unwrap_or(0) == 1;
            SteelBinding {
                name: if native {
                    c.native_name
                } else {
                    format!("tools.{}", c.path.join("."))
                },
                mode: if native {
                    c.mode
                } else if c.mode == BindingMode::Spread {
                    BindingMode::Spread
                } else {
                    BindingMode::Single
                },
                positional: if native { c.positional } else { vec![] },
                keywords: if native { c.keywords } else { vec![] },
                path: c.path,
                signature_args: None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_modes_are_a_closed_wire_contract() {
        let mut wire = serde_json::json!({
            "name": "demo/read", "path": ["demo", "read"],
            "positional": [], "keywords": []
        });
        for mode in ["object", "single", "spread", "options"] {
            wire["mode"] = mode.into();
            let binding: SteelBinding = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(serde_json::to_value(binding).unwrap(), wire);
        }
        wire["mode"] = serde_json::json!({"single": null});
        assert!(serde_json::from_value::<SteelBinding>(wire.clone()).is_err());
        wire["mode"] = "other".into();
        assert!(serde_json::from_value::<SteelBinding>(wire).is_err());
    }

    #[test]
    fn kebab_converts_camel_and_snake() {
        assert_eq!(kebab("readMany"), "read-many");
        assert_eq!(kebab("expectedSha256"), "expected-sha256");
        assert_eq!(kebab("timeout_seconds"), "timeout-seconds");
    }
}
