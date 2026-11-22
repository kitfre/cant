//! Native call shapes are inert metadata. They only lower to the existing path request.
use crate::{json, wire::Bindings};
use cant_lang_steel::bindings::SteelBinding;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashSet};
use steel::{rvals::Result, SteelVal};

pub fn validate(bindings: &Bindings) -> Result<()> {
    if bindings.steel_bindings.len() > 4096 {
        return Err(json::error("Too many Steel bindings"));
    }
    let mut names = HashSet::new();
    let mut paths = HashSet::new();
    for binding in &bindings.steel_bindings {
        let legacy = format!("tools.{}", binding.path.join("."));
        if !bindings.api_paths.contains(&binding.path)
            || !paths.insert(&binding.path)
            || !names.insert(&binding.name)
            || (binding.name != legacy
                && (binding.name.len() > 1024
                    || !binding.name.contains('/')
                    || !binding
                        .name
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"/_-$".contains(&c))))
        {
            return Err(json::error("Invalid or duplicate Steel binding identity"));
        }
        let spread = bindings.spread_api_paths.contains(&binding.path);
        if !matches!(
            binding.mode.as_str(),
            "object" | "single" | "spread" | "options"
        ) || spread != matches!(binding.mode.as_str(), "spread" | "options")
            || (!matches!(binding.mode.as_str(), "object" | "options")
                && (!binding.positional.is_empty() || !binding.keywords.is_empty()))
            || binding.keywords.len() > 1024
            || binding
                .signature_args
                .as_ref()
                .is_some_and(|args| args.len() > 64 || args.iter().any(|arg| arg.len() > 512))
        {
            return Err(json::error("Invalid Steel binding call shape"));
        }
        let mut keywords = HashSet::new();
        let mut fields = HashSet::new();
        for field in &binding.keywords {
            if field.keyword.is_empty()
                || field.keyword.len() > 256
                || field.field.len() > 4096
                || !field
                    .keyword
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_-$".contains(&c))
                || !keywords.insert(field.keyword.as_str())
                || !fields.insert(&field.field)
            {
                return Err(json::error("Invalid or duplicate Steel keyword"));
            }
            for alias in &field.aliases {
                if alias.is_empty()
                    || alias.len() > 256
                    || !alias
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"_-$".contains(&c))
                    || !keywords.insert(alias.as_str())
                {
                    return Err(json::error("Invalid or duplicate Steel keyword alias"));
                }
            }
        }
        let mut positionals = HashSet::new();
        for field in &binding.positional {
            if (binding.mode != "options" && !fields.contains(field))
                || field.len() > 4096
                || !positionals.insert(field)
            {
                return Err(json::error("Invalid or duplicate Steel positional field"));
            }
        }
    }
    Ok(())
}

fn keyword(value: &SteelVal) -> Option<&str> {
    if let SteelVal::SymbolV(value) = value {
        value.as_str().strip_prefix("#:")
    } else {
        None
    }
}

fn identifier(value: &SteelVal, names: &BTreeMap<String, String>, location: &str) -> Result<Value> {
    match value {
        SteelVal::SymbolV(name) => names.get(name.as_str()).map(|v| Value::String(v.clone()))
            .ok_or_else(|| json::error(format!("Unknown capability symbol {name}; use a discovered binding or an exact canonical string"))),
        SteelVal::StringV(name) => Ok(Value::String(names.get(name.as_str()).cloned().unwrap_or_else(|| name.to_string()))),
        _ => json::to_json_at(value, location),
    }
}

pub fn arguments(
    binding: &SteelBinding,
    args: &[SteelVal],
    names: &BTreeMap<String, String>,
) -> Result<Value> {
    let argument = |value: &SteelVal, i: usize| {
        json::to_json_at(value, &format!("{} argument {}", binding.name, i + 1))
    };
    match binding.mode.as_str() {
        "options" => {
            let prefix = binding.positional.len();
            let first_keyword = args.iter().position(|value| keyword(value).is_some());
            if let Some(index) = first_keyword {
                if index != prefix {
                    return Err(json::argument_error(format!("{}: expected {prefix} positional arguments before options; missing prefix or mixed options record/keywords", binding.name)));
                }
                let mut values = args[..prefix]
                    .iter()
                    .enumerate()
                    .map(|(i, v)| argument(v, i))
                    .collect::<Result<Vec<_>>>()?;
                let mut options = binding.clone();
                options.mode = "object".into();
                options.positional.clear();
                values.push(arguments(&options, &args[prefix..], names)?);
                return Ok(Value::Array(values));
            }
            if args.len() < prefix || args.len() > prefix + 1 {
                return Err(json::argument_error(format!(
                    "{}: expected {prefix} positional arguments and an optional options record",
                    binding.name
                )));
            }
            return args
                .iter()
                .enumerate()
                .map(|(i, v)| argument(v, i))
                .collect::<Result<Vec<_>>>()
                .map(Value::Array);
        }
        "spread" => {
            if args.iter().any(|value| keyword(value).is_some()) {
                return Err(json::argument_error(format!(
                    "{}: keywords are not supported; use positional arguments",
                    binding.name
                )));
            }
            return args
                .iter()
                .enumerate()
                .map(|(i, v)| argument(v, i))
                .collect::<Result<Vec<_>>>()
                .map(Value::Array);
        }
        "single" => {
            return if args.len() == 1 {
                argument(&args[0], 0)
            } else {
                Err(json::argument_error(format!(
                    "{} expects exactly one argument",
                    binding.name
                )))
            }
        }
        _ => {}
    }
    let fail = |message: String| json::argument_error(format!("{}: {message}", binding.name));
    let mut object = Map::new();
    let mut positional = 0;
    let mut saw_keyword = false;
    let mut index = 0;
    while index < args.len() {
        let (field, value) = if let Some(key) = keyword(&args[index]) {
            saw_keyword = true;
            let Some(field) = binding.keywords.iter().find(|field| {
                field.keyword == key || field.aliases.iter().any(|alias| alias == key)
            }) else {
                let available = binding
                    .keywords
                    .iter()
                    .take(12)
                    .map(|field| format!("#:{}", field.keyword))
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(fail(format!(
                    "unknown keyword #:{key}; expected {available}"
                )));
            };
            index += 1;
            if index >= args.len() || keyword(&args[index]).is_some() {
                return Err(fail(format!("missing value for #:{key}")));
            }
            (&field.field, &args[index])
        } else {
            if saw_keyword {
                return Err(fail("positional arguments must precede keywords".into()));
            }
            let Some(field) = binding.positional.get(positional) else {
                return Err(fail(format!(
                    "expected at most {} positional arguments; use keywords for options",
                    binding.positional.len()
                )));
            };
            positional += 1;
            (field, &args[index])
        };
        if object.contains_key(field) {
            return Err(fail(format!("duplicate argument {field}")));
        }
        // Symbolic capability identities are an explicit discovery convenience, not generic JSON coercion.
        let keyword = binding
            .keywords
            .iter()
            .find(|entry| &entry.field == field)
            .unwrap();
        let location = format!("{} argument #:{}", binding.name, keyword.keyword);
        let value = if binding.path == ["capabilities", "describe"] && field == "name" {
            identifier(value, names, &location)?
        } else if binding.path == ["capabilities", "describe"] && field == "names" {
            match value {
                SteelVal::ListV(values) => Value::Array(
                    values
                        .iter()
                        .enumerate()
                        .map(|(i, v)| identifier(v, names, &format!("{location}[{i}]")))
                        .collect::<Result<Vec<_>>>()?,
                ),
                _ => json::to_json_at(value, &location)?,
            }
        } else {
            json::to_json_at(value, &location)?
        };
        object.insert(field.clone(), value);
        index += 1;
    }
    for field in &binding.keywords {
        if field.required && !object.contains_key(&field.field) {
            return Err(fail(format!(
                "missing required argument #:{}",
                field.keyword
            )));
        }
    }
    Ok(Value::Object(object))
}

#[test]
fn pinned_reservations_cover_all_shadowable_runtime_globals() {
    let reserved = cant_lang_steel::catalog::native_reservations();
    let engine = steel::steel_vm::engine::Engine::new_sandboxed();
    for name in engine
        .globals()
        .iter()
        .map(|v| v.resolve().to_string())
        .filter(|name| {
            name.contains('/')
                && name.split('/').all(|segment| {
                    !segment.is_empty()
                        && segment.as_bytes()[0].is_ascii_lowercase()
                        && segment
                            .bytes()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
                })
        })
    {
        assert!(reserved.contains(&name), "Reserve Steel primitive {name} in native-reservations.json before upgrading the engine");
    }
}
