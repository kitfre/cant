//! Steel Scheme language adapter for Cant (`GuestLanguage` implementation).

pub mod bindings;
pub mod catalog;
pub mod presentation;

use cant_host::capability::CapabilityMeta;
use cant_host::language::{GuestLanguage, LanguageIdentity};
use cant_host::runner_process::RunnerCommand;
use serde_json::{json, Value};
use std::path::PathBuf;

#[derive(Debug, Default)]
pub struct SteelLanguage {
    explicit_runner_path: Option<PathBuf>,
}

impl SteelLanguage {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_runner_path(path: PathBuf) -> Self {
        Self {
            explicit_runner_path: Some(path),
        }
    }

    pub fn resolve_runner_path(&self) -> Result<PathBuf, String> {
        if let Some(ref p) = self.explicit_runner_path {
            return Ok(p.clone());
        }
        if let Some(path) = std::env::var_os("CANT_STEEL_RUNNER").filter(|s| !s.is_empty()) {
            return Ok(path.into());
        }
        let executable =
            std::env::current_exe().map_err(|e| format!("Cannot locate cant executable: {e}"))?;
        let sibling = executable.with_file_name("cant-steel-runner");
        if sibling.is_file() {
            return Ok(sibling);
        }
        Err(format!(
            "Steel runner not found at {}. Reinstall the cant package to install both binaries, build both with cargo build, or set CANT_STEEL_RUNNER.",
            sibling.display()
        ))
    }
}

impl GuestLanguage for SteelLanguage {
    fn identity(&self) -> LanguageIdentity {
        LanguageIdentity {
            id: "steel",
            language: "Scheme",
            engine: "Steel",
            program_format: "scheme",
            result_kind: "scheme",
            console_prefix: "print",
        }
    }

    fn runner_command(&self) -> Result<RunnerCommand, String> {
        Ok(RunnerCommand {
            executable: self.resolve_runner_path()?,
            args: vec![],
        })
    }

    fn project_bindings(&self, metas: &[CapabilityMeta]) -> Value {
        let projected = bindings::resolve_steel_bindings(metas);
        serde_json::to_value(projected).unwrap_or(Value::Array(vec![]))
    }

    fn binding_name(&self, metas: &[CapabilityMeta], meta: &CapabilityMeta) -> String {
        bindings::resolve_steel_bindings(metas)
            .into_iter()
            .find(|b| b.path == meta.api_path)
            .map(|b| b.name)
            .unwrap_or_else(|| format!("tools.{}", meta.api_path.join(".")))
    }

    fn signature(&self, metas: &[CapabilityMeta], meta: &CapabilityMeta) -> String {
        let binding = bindings::resolve_steel_bindings(metas)
            .into_iter()
            .find(|b| b.path == meta.api_path);
        presentation::signature(binding.as_ref(), meta)
    }

    fn tool_description(&self, metas: &[CapabilityMeta]) -> String {
        let mut caps: Vec<String> = metas.iter().map(|m| self.signature(metas, m)).collect();
        caps.sort();

        let has_ns = |ns: &str| {
            metas
                .iter()
                .any(|m| m.api_path.first().map(String::as_str) == Some(ns))
        };
        let has_path = |path: &[&str]| {
            metas.iter().any(|m| {
                m.api_path.len() == path.len() && m.api_path.iter().zip(path).all(|(a, b)| a == b)
            })
        };

        let mut purposes = vec![
            "investigations",
            "multi-file analysis",
            "batch edits",
            "data transformations",
        ];
        if has_ns("background") {
            purposes.push("background jobs");
        }
        if has_ns("delegate") || has_ns("agents") {
            purposes.push("delegated agent workflows");
        }
        if has_ns("peers") || has_ns("channels") {
            purposes.push("peer discovery and agent channels");
        }

        let mut examples = Vec::new();
        if has_path(&["files", "read"]) {
            examples.push("`(files/read \"README.md\" #:limit 80)`");
        }
        if has_path(&["files", "grep"]) {
            examples.push("`(files/grep \"TODO\" #:glob \"*.rs\")`");
        }
        if has_path(&["system", "exec"]) {
            examples.push("`(system/exec \"git\" '(\"status\" \"--short\"))`");
        }
        if has_path(&["background", "start"]) {
            examples.push("`(background/start \"cargo\" '(\"test\"))`");
        }
        if has_path(&["delegate", "start"]) {
            examples.push("`(delegate/start (input 'task) #:name \"review\")`");
        }
        if has_path(&["peers", "list"]) {
            examples.push("`(peers/list)`");
        }
        if has_path(&["channels", "open"]) {
            examples.push("`(channels/open #:to \"frontend\" #:message \"...\")`");
        }
        if has_path(&["capabilities", "describe"]) {
            examples.push("`(capabilities/describe 'files/edit)`");
        }
        let examples_clause = if examples.is_empty() {
            String::new()
        } else {
            format!(" Examples: {}.", examples.join(", "))
        };

        let mut constructors = vec!["`(obj (k v) ...)`", "`(get rec 'k)`"];
        if has_path(&["files", "edit"]) {
            constructors.push("`(replace-text old new)`");
        }
        if has_ns("scripts") || has_ns("background") || has_path(&["system", "pipeline"]) {
            constructors.push("`(scripts/inline interp src)`");
            constructors.push("`(scripts/file path)`");
        }
        if has_path(&["system", "pipeline"]) {
            constructors.push("`(system/exec-step cmd args)`");
            constructors.push("`(system/script-step script args)`");
        }
        if has_ns("agents") {
            constructors.push("`(agents/task task ...)`");
            constructors.push("`(agents/node id task ...)`");
            constructors.push("`(agents/from node pointer)`");
            constructors.push("`(agents/artifact-from node name)`");
        }

        format!(
            "A persistent local Steel Scheme REPL workspace for {}. Retain parsed files, query results, narrowed candidates, definitions, and helper functions across evaluations.\n\n\
            Operations (`op`): `eval` (execute Scheme expressions in `code`, with optional literal string map `inputs` accessed via `(input 'key)`), `inspect`, `search`, `describe`, `history`, `reset`, `close`.\n\n\
            The last expression is your result. To show a command’s stdout, end with `(get result 'stdout)`. Use `print` only for intermediate output.\n\n\
            Prefer native slash/kebab-case procedures with positional primary arguments and `#:kebab-case` keyword options.{examples_clause} Use `(parallel-calls #:limit 4 ...)` for concurrent independent calls.\n\n\
            Pure constructors: {}. Use `(let-fields ([text sha256] file) ...)`, `json-parse`, `(split-many text sep)`, and `map`/`filter`/`->`/`->>`.\n\n\
            Available capabilities:\n{}",
            join_prose(&purposes),
            constructors.join(", "),
            caps.iter()
                .map(|s| format!("- {s}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    }

    fn source_description(&self) -> &'static str {
        "Required for eval: Steel Scheme expressions; the last value is returned."
    }

    fn inputs_description(&self) -> &'static str {
        "Eval-only literal string map; access inside Scheme with (input 'key)."
    }

    fn format_metadata(&self, value: &Value) -> String {
        presentation::format_steel_literal(value)
    }

    fn post_describe(&self, name: &str, mut value: Value, metas: &[CapabilityMeta]) -> Value {
        let is_builtin_binding = value.get("origin").and_then(Value::as_str) == Some("builtin")
            && value.get("kind").and_then(Value::as_str) == Some("binding")
            && value.get("initialized").and_then(Value::as_bool) == Some(true);
        if !is_builtin_binding {
            return value;
        }
        let bindings = bindings::resolve_steel_bindings(metas);
        let Some(binding) = bindings
            .iter()
            .find(|b| b.name == name || format!("tools.{}", b.path.join(".")) == name)
        else {
            return value;
        };
        let Some(meta) = metas.iter().find(|m| m.api_path == binding.path) else {
            return value;
        };
        let sig = presentation::signature(Some(binding), meta);
        if let Some(obj) = value.as_object_mut() {
            obj.insert(
                "capability".into(),
                json!({
                    "apiPath": meta.public_name(),
                    "binding": name,
                    "signature": sig,
                    "description": meta.description,
                    "provenance": "capability-catalog",
                }),
            );
        }
        value
    }
}

fn join_prose(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [a] => (*a).to_string(),
        [a, b] => format!("{a} and {b}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cant_host::capability::ArgumentMode;
    use serde_json::Map;

    fn meta(path: &[&str]) -> CapabilityMeta {
        CapabilityMeta {
            tool_name: format!("__cant_{}", path.join("_")),
            api_path: path.iter().map(|s| (*s).to_string()).collect(),
            description: "test".into(),
            params_schema: json!({ "type": "object", "properties": {} }),
            result_schema: json!({ "type": "object" }),
            argument_mode: ArgumentMode::Single,
            language_hints: Map::new(),
        }
    }

    #[test]
    fn tool_description_omits_disabled_delegate_and_agent_references() {
        let lang = SteelLanguage::new();
        let metas = vec![
            meta(&["files", "read"]),
            meta(&["files", "grep"]),
            meta(&["files", "edit"]),
            meta(&["system", "exec"]),
            meta(&["system", "pipeline"]),
            meta(&["background", "start"]),
            meta(&["capabilities", "describe"]),
        ];
        let desc = lang.tool_description(&metas);
        assert!(
            !desc.contains("delegate/start"),
            "unexpected delegate example: {desc}"
        );
        assert!(
            !desc.contains("agents/task"),
            "unexpected agents/task: {desc}"
        );
        assert!(
            !desc.contains("agents/node"),
            "unexpected agents/node: {desc}"
        );
        assert!(
            !desc.contains("agents/from"),
            "unexpected agents/from: {desc}"
        );
        assert!(
            !desc.contains("agents/artifact-from"),
            "unexpected agents/artifact-from: {desc}"
        );
        assert!(
            !desc.contains("delegated agent workflows"),
            "unexpected delegated intro: {desc}"
        );
        assert!(desc.contains("background jobs"));
        assert!(desc.contains("(background/start \"cargo\" '(\"test\"))"));
        assert!(desc.contains("(replace-text old new)"));
        assert!(desc.contains("(scripts/inline interp src)"));
        assert!(desc.contains("(system/exec-step cmd args)"));
    }

    #[test]
    fn tool_description_includes_delegate_and_agents_when_enabled() {
        let lang = SteelLanguage::new();
        let metas = vec![
            meta(&["files", "read"]),
            meta(&["background", "start"]),
            meta(&["delegate", "start"]),
            meta(&["agents", "run"]),
        ];
        let desc = lang.tool_description(&metas);
        assert!(desc.contains("background jobs, and delegated agent workflows"));
        assert!(desc.contains("(delegate/start (input 'task) #:name \"review\")"));
        assert!(desc.contains("(agents/task task ...)"));
        assert!(desc.contains("(agents/node id task ...)"));
        assert!(desc.contains("(agents/from node pointer)"));
        assert!(desc.contains("(agents/artifact-from node name)"));
    }

    #[test]
    fn tool_description_omits_background_and_edit_hints_when_disabled() {
        let lang = SteelLanguage::new();
        let metas = vec![
            meta(&["files", "read"]),
            meta(&["capabilities", "describe"]),
        ];
        let desc = lang.tool_description(&metas);
        assert!(!desc.contains("background jobs"));
        assert!(!desc.contains("background/start"));
        assert!(!desc.contains("delegated agent workflows"));
        assert!(!desc.contains("replace-text"));
        assert!(!desc.contains("system/exec-step"));
        assert!(desc.contains("(files/read \"README.md\" #:limit 80)"));
        assert!(desc.contains("(capabilities/describe 'files/edit)"));
        assert!(!desc.contains("peer discovery"));
        assert!(!desc.contains("(peers/list)"));
    }

    #[test]
    fn tool_description_includes_peers_and_channels_when_enabled() {
        let lang = SteelLanguage::new();
        let metas = vec![
            meta(&["files", "read"]),
            meta(&["peers", "list"]),
            meta(&["channels", "open"]),
            meta(&["channels", "send"]),
        ];
        let desc = lang.tool_description(&metas);
        assert!(desc.contains("peer discovery and agent channels"));
        assert!(desc.contains("(peers/list)"));
        assert!(desc.contains("(channels/open #:to \"frontend\" #:message \"...\")"));
    }
}
