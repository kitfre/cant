//! Launch policy for Codex 0.171's stdio app-server (not `codex exec`).
use crate::jobs::driver::WorkerSpec;
use serde_json::{json, Value};
use tokio::process::Command;

pub(super) fn configure(cmd: &mut Command, spec: &WorkerSpec) -> Result<(), String> {
    // Codex native tools are not Cant capability leases. Even [] means *no*
    // tools: read-only is a filesystem policy, not a way to enforce that lease.
    if spec.capabilities.is_some() {
        return Err(
            "Codex workers do not support explicit Cant capabilities (including an empty lease)"
                .into(),
        );
    }

    // Keep CODEX_HOME, the keyring, provider configuration and credentials in
    // place. Do not copy auth.json or synthesize a different Codex identity.
    cmd.env("CANT_WORKER", "1").env_remove("CANT_SOCKET");
    cmd.arg("--strict-config"); // no falling back to unrestricted defaults on bad overrides
    for feature in [
        "plugins",
        "remote_plugin",
        "apps",
        "hooks",
        "skill_mcp_dependency_install",
        "multi_agent_v2",
        "agent_message_board_mcp",
        "shell_tool",
        "code_mode",
        "computer_use",
        "browser_use",
    ] {
        cmd.args(["--disable", feature]);
    }
    for setting in [
        "agents.enabled=false",
        "web_search=\"disabled\"",
        "approval_policy=\"never\"",
        "sandbox_mode=\"read-only\"",
        "notify=[]",
        // Empty tables merge with inherited config in 0.171. Disable the
        // standalone registration by name as well as disabling plugins above.
        // Codex requires a transport even for an absent/disabled server.
        "mcp_servers.cant={enabled=false,command=\"false\"}",
    ] {
        cmd.args(["-c", setting]);
    }
    Ok(())
}

pub(super) fn thread_params(spec: &WorkerSpec, model: Option<&str>) -> Value {
    let mut params = json!({
        "cwd": spec.cwd,
        "ephemeral": true,
        "approvalPolicy": "never",
        "sandbox": "read-only",
    });
    if let Some(model) = model.map(str::trim).filter(|m| !m.is_empty()) {
        params["model"] = json!(model);
    }
    params
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> WorkerSpec {
        WorkerSpec {
            name: "policy-test".into(),
            task: "answer".into(),
            cwd: "/workspace".into(),
            input: None,
            output_schema: None,
            timeout_secs: None,
            model: None,
            capabilities: None,
            session_id: None,
            channels: Vec::new(),
        }
    }

    #[test]
    fn explicit_leases_are_not_silently_reinterpreted_as_native_tools() {
        for capabilities in [vec![], vec!["files.read".to_owned()]] {
            let mut spec = spec();
            spec.capabilities = Some(capabilities);
            let mut cmd = Command::new("codex");
            assert!(configure(&mut cmd, &spec)
                .unwrap_err()
                .contains("capabilities"));
            assert_eq!(cmd.as_std().get_args().count(), 0);
        }
    }

    #[test]
    fn worker_is_read_only_noninteractive_and_keeps_configured_model_by_default() {
        let params = thread_params(&spec(), None);
        assert_eq!(params["cwd"], "/workspace");
        assert_eq!(params["sandbox"], "read-only");
        assert_eq!(params["approvalPolicy"], "never");
        assert_eq!(params["ephemeral"], true);
        assert!(params.get("model").is_none());
        assert_eq!(
            thread_params(&spec(), Some("custom-model"))["model"],
            "custom-model"
        );
    }

    #[test]
    fn launch_overrides_do_not_replace_auth_or_provider_configuration() {
        let mut cmd = Command::new("codex");
        cmd.env("CODEX_HOME", "/private/config")
            .env("OPENAI_API_KEY", "placeholder");
        configure(&mut cmd, &spec()).unwrap();
        let cmd = cmd.as_std();
        let env: std::collections::HashMap<_, _> = cmd.get_envs().collect();
        for (k, v) in [
            ("CODEX_HOME", "/private/config"),
            ("OPENAI_API_KEY", "placeholder"),
        ] {
            assert_eq!(env[std::ffi::OsStr::new(k)], Some(std::ffi::OsStr::new(v)));
        }
        let args: Vec<_> = cmd.get_args().map(|a| a.to_str().unwrap()).collect();
        for flag in ["plugins", "multi_agent_v2", "shell_tool", "hooks"] {
            assert!(args.windows(2).any(|pair| pair == ["--disable", flag]));
        }
        assert!(args.contains(&"agents.enabled=false"));
        assert!(!args.contains(&"multi_agent"));
    }
}
