mod hook;

use cant_host::adapter::{AdapterHub, AdapterWorkerDriver};
use cant_host::capabilities::register_builtin_capabilities;
use cant_host::capability::CapabilityCatalog;
use cant_host::config::CantConfig;
use cant_host::daemon::{
    default_socket_path, restart_daemon, run_daemon, send_daemon_request, shutdown_daemon,
};
use cant_host::jobs::driver::{codex::CodexWorkerDriver, AgyWorkerDriver, WorkerDriverRegistry};
use cant_host::jobs::JobManager;
use cant_host::language::LanguageRegistry;
use cant_host::mcp::{run_mcp_stdio, McpOptions};
use cant_host::repl::ReplManager;
use cant_lang_steel::SteelLanguage;
use cant_protocol::daemon::{DaemonRequest, DaemonResponse, ReplRequest, WorkspaceKey, WorkspaceSummary};
use cant_protocol::runner::ReplOrigin;
use clap::{Parser, Subcommand};
use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "cant", version, about = "Local polyglot REPL host, jobs daemon & MCP server")]
struct Cli {
    /// Override daemon Unix socket path (defaults to $CANT_SOCKET or OS runtime dir)
    #[arg(long, global = true)]
    socket: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the local Cant daemon listening on a Unix socket
    Daemon,
    /// Run the stdio MCP server bridge (auto-starts daemon if needed)
    Mcp {
        /// Disable capability namespaces or paths (repeatable, e.g. --disable web)
        #[arg(long = "disable")]
        disable: Vec<String>,
        /// Worker driver for delegate/* and agents/* (default: agy)
        #[arg(long)]
        driver: Option<String>,
        /// Default guest language (default: steel)
        #[arg(long, default_value = "steel")]
        language: String,
    },
    /// Antigravity lifecycle hook helpers
    Hook {
        #[command(subcommand)]
        kind: HookCommand,
    },
    /// Send a REPL request to the daemon (or attach interactively if no code/op given)
    Repl {
        /// Session ID to attach to (default: "cli")
        #[arg(long, default_value = "cli")]
        session: String,
        /// Attach to the active Codex/agent workspace for the current directory
        #[arg(long)]
        attach: bool,
        /// Working directory for file/process capabilities (default: current dir)
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Guest language id
        #[arg(long, default_value = "steel")]
        language: String,
        /// Disable capability namespaces (repeatable, e.g. --disable web)
        #[arg(long = "disable")]
        disable: Vec<String>,
        /// Worker driver for delegate/* and agents/* (default: agy)
        #[arg(long)]
        driver: Option<String>,
        /// Operation: eval, inspect, search, describe, history, reset, close
        #[arg(long)]
        op: Option<String>,
        /// Optional binding name (for inspect/describe)
        #[arg(long)]
        name: Option<String>,
        /// Optional search query
        #[arg(long)]
        query: Option<String>,
        /// Optional origin: user or all
        #[arg(long)]
        origin: Option<String>,
        /// Code to evaluate (for op=eval)
        code: Option<String>,
    },
    /// Monitor and control background scripts, delegates, and agent flows
    Jobs {
        #[command(subcommand)]
        action: Option<JobsCommand>,
    },
    /// List active workspaces in the running daemon
    Workspaces,
    /// List active agent peers connected to the Cant daemon
    Peers {
        /// Include stale idle peers
        #[arg(long)]
        all: bool,
    },
    /// List or read agent communication channels
    Channels {
        /// Optional session ID (required to inspect direct threads)
        #[arg(long, global = true)]
        session: Option<String>,
        #[command(subcommand)]
        action: Option<ChannelsCommand>,
    },
    /// Send a message to a channel, #room, or peer handle
    Say {
        /// Target channel ID, #room name, or peer handle
        target: String,
        /// Message body
        message: String,
        /// Sender handle (default: "human")
        #[arg(long = "as", default_value = "human")]
        handle: String,
        /// Session ID to send as (default: "cli")
        #[arg(long, default_value = "cli")]
        session: String,
    },
    /// Ping the daemon (auto-starts if not running)
    Ping,
    /// Gracefully shut down the daemon if running
    Shutdown,
    /// Restart the daemon (or start it if idle) and wait until ready
    Restart,
}

#[derive(Subcommand)]
enum HookCommand {
    /// PreToolUse hook injecting sessionId/cwd and optionally enforcing strict Cant mode
    PreTool {
        /// Block direct workspace file/shell tools and redirect to cant repl (overridable via CANT_STRICT=0 or config.strict=false)
        #[arg(long)]
        strict: bool,
        /// Harness hook protocol (auto-detected from stdin JSON if omitted)
        #[arg(long, value_enum, default_value_t = hook::HookHost::Auto)]
        host: hook::HookHost,
    },
    /// PreInvocation / UserPromptSubmit hook injecting completed background job notices
    PreInvocation {
        #[arg(long, value_enum, default_value_t = hook::HookHost::Auto)]
        host: hook::HookHost,
    },
    /// PostToolUse hook injecting completed background job notices mid-turn
    PostTool {
        #[arg(long, value_enum, default_value_t = hook::HookHost::Auto)]
        host: hook::HookHost,
    },
    /// Stop hook waking the agent when active background jobs completed
    Stop {
        #[arg(long, value_enum, default_value_t = hook::HookHost::Auto)]
        host: hook::HookHost,
    },
}

#[derive(Subcommand)]
enum JobsCommand {
    /// List all jobs (default)
    List {
        /// Optional filter by session ID
        #[arg(long)]
        session: Option<String>,
        /// Filter to the active Codex/agent workspace for the current directory
        #[arg(long)]
        attach: bool,
    },
    /// Show bounded monitor output for a job ID
    Tail {
        id: String,
        #[arg(long, default_value_t = 16384)]
        max_bytes: usize,
    },
    /// Cancel a running job by ID
    Cancel { id: String },
}

#[derive(Subcommand)]
enum ChannelsCommand {
    /// List channels (default)
    List {
        /// Include closed channels
        #[arg(long)]
        all: bool,
    },
    /// Show recent messages in a channel or #room
    Read {
        /// Channel ID or #room name
        channel: String,
        /// Max number of recent messages to display
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
}

fn build_manager(cfg: &CantConfig) -> Arc<ReplManager> {
    let mut languages = LanguageRegistry::new();
    languages.register(Arc::new(SteelLanguage::new()));

    let adapters = Arc::new(AdapterHub::new());
    let mut drivers = WorkerDriverRegistry::new();
    drivers.register(Arc::new(
        AgyWorkerDriver::default().with_default_model(cfg.worker_model.clone()),
    ));
    drivers.register(Arc::new(CodexWorkerDriver::new(cfg.worker_model.clone())));
    drivers.register(Arc::new(AdapterWorkerDriver::new("pi", Arc::clone(&adapters))));
    let jobs = Arc::new(JobManager::new(Arc::new(drivers)));

    let catalog = Arc::new(CapabilityCatalog::new());
    register_builtin_capabilities(&catalog);

    Arc::new(ReplManager::new(
        Arc::new(languages),
        catalog,
        adapters,
        jobs,
    ))
}

fn merge_disabled(cfg: &CantConfig, cli_disable: Vec<String>) -> Vec<String> {
    let mut out = cfg.disabled_namespaces.clone();
    for item in cli_disable {
        if !out.contains(&item) {
            out.push(item);
        }
    }
    out
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let cfg = CantConfig::load();
    let socket = cli.socket.unwrap_or_else(default_socket_path);

    let res = match cli.command {
        Commands::Daemon => run_daemon(&socket, build_manager(&cfg)).await,
        Commands::Mcp {
            disable,
            driver,
            language,
        } => {
            let options = McpOptions {
                disabled_namespaces: merge_disabled(&cfg, disable),
                worker_driver: driver
                    .or_else(|| cfg.worker_driver.clone())
                    .unwrap_or_else(|| "agy".into()),
                default_language: language,
            };
            run_mcp_stdio(&socket, options).await
        }
        Commands::Hook { kind } => match kind {
            HookCommand::PreTool { strict, host } => {
                let effective_strict = hook::resolve_strict(strict, cfg.strict);
                hook::run_pre_tool(effective_strict, host)
            }
            HookCommand::PreInvocation { host } => hook::run_pre_invocation(&socket, host).await,
            HookCommand::PostTool { host } => hook::run_post_tool(&socket, host).await,
            HookCommand::Stop { host } => hook::run_stop(&socket, host).await,
        },
        Commands::Ping => match send_daemon_request(&socket, &DaemonRequest::Ping).await {
            Ok(resp) => {
                println!("{}", serde_json::to_string_pretty(&resp).unwrap());
                Ok(())
            }
            Err(e) => Err(e),
        },
        Commands::Peers { all } => {
            match send_daemon_request(&socket, &DaemonRequest::ListPeers { include_stale: all }).await {
                Ok(DaemonResponse::Peers { peers }) => {
                    if peers.is_empty() {
                        println!("No active peers.");
                    } else {
                        for p in peers {
                            let topic = p.topic.as_deref().map(|t| format!(" — {t}")).unwrap_or_default();
                            let stale = if p.stale { " [stale]" } else { "" };
                            println!(
                                "{:<18} {:<11} {:<10} {}{stale}{topic}",
                                p.handle,
                                format!("{:?}", p.harness).to_lowercase(),
                                format!("{:?}", p.reachability).to_lowercase(),
                                p.cwd
                            );
                        }
                    }
                    Ok(())
                }
                Ok(DaemonResponse::Error { message }) => Err(message),
                Ok(_) => Err("Unexpected daemon response.".into()),
                Err(e) => Err(e),
            }
        }
        Commands::Channels { session, action } => match action.unwrap_or(ChannelsCommand::List { all: false }) {
            ChannelsCommand::List { all } => {
                match send_daemon_request(
                    &socket,
                    &DaemonRequest::ListChannels {
                        session_id: session,
                        include_closed: all,
                    },
                )
                .await
                {
                    Ok(DaemonResponse::Channels { channels }) => {
                        if channels.is_empty() {
                            println!("No channels.");
                        } else {
                            for c in channels {
                                let state = if c.closed { "closed" } else { "open" };
                                let label = match c.name.as_deref() {
                                    Some(name) => format!("{} (#{name})", c.id),
                                    None => c.id.clone(),
                                };
                                let topic = c.topic.as_deref().map(|t| format!(" — {t}")).unwrap_or_default();
                                println!(
                                    "{:<24} {:<6} {:>3} msgs  [{}]{topic}",
                                    label,
                                    state,
                                    c.message_count,
                                    c.members.join(", ")
                                );
                            }
                        }
                        Ok(())
                    }
                    Ok(DaemonResponse::Error { message }) => Err(message),
                    Ok(_) => Err("Unexpected daemon response.".into()),
                    Err(e) => Err(e),
                }
            }
            ChannelsCommand::Read { channel, limit } => {
                match send_daemon_request(
                    &socket,
                    &DaemonRequest::ChannelRead {
                        channel,
                        session_id: session,
                        limit,
                    },
                )
                .await
                {
                    Ok(DaemonResponse::ChannelMessages { channel, messages }) => {
                        let label = match channel.name.as_deref() {
                            Some(name) => format!("{} (#{name})", channel.id),
                            None => channel.id.clone(),
                        };
                        println!(
                            "=== {} ({} msgs, members: {}) ===",
                            label,
                            channel.message_count,
                            channel.members.join(", ")
                        );
                        for m in messages {
                            println!("#{:<3} {}: {}", m.seq, m.from_handle, m.body);
                        }
                        Ok(())
                    }
                    Ok(DaemonResponse::Error { message }) => Err(message),
                    Ok(_) => Err("Unexpected daemon response.".into()),
                    Err(e) => Err(e),
                }
            }
        },
        Commands::Say {
            target,
            message,
            handle,
            session,
        } => {
            let cwd = std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().into_owned());
            match send_daemon_request(
                &socket,
                &DaemonRequest::ChannelSend {
                    target,
                    body: message,
                    session_id: Some(session),
                    handle: Some(handle),
                    cwd,
                },
            )
            .await
            {
                Ok(DaemonResponse::ChannelSent { channel_id, message }) => {
                    println!("Sent #{} to {} as {}.", message.seq, channel_id, message.from_handle);
                    Ok(())
                }
                Ok(DaemonResponse::Error { message }) => Err(message),
                Ok(_) => Err("Unexpected daemon response.".into()),
                Err(e) => Err(e),
            }
        }
        Commands::Workspaces => {
            match send_daemon_request(&socket, &DaemonRequest::ListWorkspaces).await {
                Ok(resp) => {
                    println!("{}", serde_json::to_string_pretty(&resp).unwrap());
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
        Commands::Jobs { action } => match action.unwrap_or(JobsCommand::List {
            session: None,
            attach: false,
        }) {
            JobsCommand::List { session, attach } => async {
                let session_id = if attach && session.is_none() {
                    let cwd = std::env::current_dir()
                        .unwrap_or_else(|_| PathBuf::from("."))
                        .to_string_lossy()
                        .into_owned();
                    lookup_attached_session(&socket, &cwd).await?
                } else {
                    session
                };
                match send_daemon_request(&socket, &DaemonRequest::ListJobs { session_id }).await {
                    Ok(DaemonResponse::Jobs { jobs }) => {
                        if jobs.is_empty() {
                            println!("No jobs.");
                        } else {
                            for j in jobs {
                                println!(
                                    "{:<18} {:<9} {:<10} {:>6}ms  {} — {}",
                                    j.id, j.kind, j.status, j.elapsed_ms, j.name, j.summary
                                );
                            }
                        }
                        Ok(())
                    }
                    Ok(DaemonResponse::Error { message }) => Err(message),
                    Ok(_) => Err("Unexpected daemon response.".into()),
                    Err(e) => Err(e),
                }
            }
            .await,
            JobsCommand::Tail { id, max_bytes } => {
                match send_daemon_request(
                    &socket,
                    &DaemonRequest::JobTail {
                        id,
                        max_bytes,
                        session_id: None,
                    },
                )
                .await
                {
                    Ok(DaemonResponse::JobTail {
                        id,
                        kind,
                        status,
                        output,
                        ..
                    }) => {
                        println!("=== {id} ({kind}, {status}) ===");
                        print!("{output}");
                        if !output.ends_with('\n') && !output.is_empty() {
                            println!();
                        }
                        Ok(())
                    }
                    Ok(DaemonResponse::Error { message }) => Err(message),
                    Ok(_) => Err("Unexpected daemon response.".into()),
                    Err(e) => Err(e),
                }
            }
            JobsCommand::Cancel { id } => {
                match send_daemon_request(
                    &socket,
                    &DaemonRequest::CancelJob {
                        id: id.clone(),
                        session_id: None,
                    },
                )
                    .await
                {
                    Ok(DaemonResponse::Ok) => {
                        println!("Cancelled job {id}.");
                        Ok(())
                    }
                    Ok(DaemonResponse::Error { message }) => Err(message),
                    Ok(_) => Err("Unexpected daemon response.".into()),
                    Err(e) => Err(e),
                }
            }
        },
        Commands::Shutdown => shutdown_daemon(&socket)
            .await
            .map(|()| println!("Cant daemon shut down.")),
        Commands::Restart => restart_daemon(&socket)
            .await
            .map(|()| println!("Cant daemon restarted.")),
        Commands::Repl {
            session,
            attach,
            cwd,
            language,
            disable,
            driver,
            op,
            name,
            query,
            origin,
            code,
        } => {
            let cwd_str = cwd
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_else(|| PathBuf::from("."))
                .to_string_lossy()
                .into_owned();
            let session_id = if attach {
                match lookup_attached_session(&socket, &cwd_str).await {
                    Ok(Some(id)) => id,
                    Ok(None) => {
                        eprintln!("cant error: No active workspace found for {cwd_str}. Run `cant workspaces` to inspect active sessions.");
                        std::process::exit(1);
                    }
                    Err(e) => {
                        eprintln!("cant error: {e}");
                        std::process::exit(1);
                    }
                }
            } else {
                session
            };
            let key = WorkspaceKey {
                session_id,
                cwd: cwd_str,
                language,
                disabled_namespaces: merge_disabled(&cfg, disable),
                worker_driver: driver
                    .or_else(|| cfg.worker_driver.clone())
                    .unwrap_or_else(|| "agy".into()),
            };
            if op.is_none() && code.is_none() {
                run_interactive_repl(&socket, key).await
            } else {
                let parsed_origin = match origin.as_deref() {
                    Some("user") => Some(ReplOrigin::User),
                    Some("all") => Some(ReplOrigin::All),
                    Some(other) => {
                        eprintln!("Invalid --origin '{other}' (expected 'user' or 'all')");
                        std::process::exit(2);
                    }
                    None => None,
                };
                let request = match op.as_deref().unwrap_or("eval") {
                    "eval" => ReplRequest::Eval {
                        code: code.unwrap_or_default(),
                        inputs: BTreeMap::new(),
                    },
                    "inspect" => ReplRequest::Inspect {
                        name,
                        origin: parsed_origin,
                        offset: None,
                        limit: None,
                    },
                    "search" => ReplRequest::Search {
                        query,
                        origin: parsed_origin,
                        offset: None,
                        limit: None,
                    },
                    "describe" => ReplRequest::Describe {
                        name: name.unwrap_or_default(),
                    },
                    "history" => ReplRequest::History {
                        id: None,
                        offset: None,
                        limit: None,
                    },
                    "reset" => ReplRequest::Reset,
                    "close" => ReplRequest::Close,
                    other => {
                        eprintln!("Unknown --op '{other}'");
                        std::process::exit(2);
                    }
                };
                match send_daemon_request(&socket, &DaemonRequest::Repl { key, request }).await {
                    Ok(DaemonResponse::Repl(reply)) => {
                        println!("{}", reply.text);
                        if reply.is_error {
                            std::process::exit(1);
                        }
                        Ok(())
                    }
                    Ok(DaemonResponse::Error { message }) => Err(message),
                    Ok(_) => Err("Unexpected daemon response.".into()),
                    Err(e) => Err(e),
                }
            }
        }
    };

    if let Err(e) = res {
        eprintln!("cant error: {e}");
        std::process::exit(1);
    }
}

fn resolve_attached_session(workspaces: &[WorkspaceSummary], cwd: &str) -> Option<String> {
    let target = PathBuf::from(cwd);
    let matches_cwd = |ws: &WorkspaceSummary| {
        ws.cwd == cwd || target.starts_with(PathBuf::from(&ws.cwd))
    };

    // 1. Active Codex workspace for this cwd (or parent repo root)
    if let Some(ws) = workspaces
        .iter()
        .find(|w| w.active && w.session_id.starts_with("codex-") && matches_cwd(w))
    {
        return Some(ws.session_id.clone());
    }
    // 2. Any active workspace matching this cwd
    if let Some(ws) = workspaces.iter().find(|w| w.active && matches_cwd(w)) {
        return Some(ws.session_id.clone());
    }
    // 3. Any Codex workspace matching this cwd
    if let Some(ws) = workspaces
        .iter()
        .find(|w| w.session_id.starts_with("codex-") && matches_cwd(w))
    {
        return Some(ws.session_id.clone());
    }
    // 4. Unique active Codex workspace anywhere
    let active_codex: Vec<&WorkspaceSummary> = workspaces
        .iter()
        .filter(|w| w.active && w.session_id.starts_with("codex-"))
        .collect();
    if active_codex.len() == 1 {
        return Some(active_codex[0].session_id.clone());
    }
    None
}

async fn lookup_attached_session(socket: &PathBuf, cwd: &str) -> Result<Option<String>, String> {
    match send_daemon_request(socket, &DaemonRequest::ListWorkspaces).await? {
        DaemonResponse::Workspaces { workspaces } => Ok(resolve_attached_session(&workspaces, cwd)),
        DaemonResponse::Error { message } => Err(message),
        _ => Err("Unexpected daemon response.".into()),
    }
}

async fn run_interactive_repl(socket: &PathBuf, key: WorkspaceKey) -> Result<(), String> {
    println!(
        "cant interactive REPL (session={}, lang={}, cwd={})",
        key.session_id, key.language, key.cwd
    );
    println!("Enter Scheme expressions; :vars [name], :history [id], :jobs, :reset, :help, :quit");
    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    loop {
        print!("cant> ");
        let _ = io::stdout().flush();
        let Some(Ok(line)) = lines.next() else {
            break;
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == ":quit" || trimmed == ":q" {
            break;
        }
        if trimmed == ":help" {
            println!(":vars [name]    Inspect user bindings or one binding");
            println!(":history [id]   List recent submissions or show source");
            println!(":jobs           List background/delegate jobs for this session");
            println!(":reset          Discard workspace bindings and history");
            println!(":quit           Exit REPL (workspace stays alive)\n");
            continue;
        }
        if trimmed == ":jobs" {
            match send_daemon_request(
                socket,
                &DaemonRequest::ListJobs {
                    session_id: Some(key.session_id.clone()),
                },
            )
            .await
            {
                Ok(DaemonResponse::Jobs { jobs }) if jobs.is_empty() => println!("No jobs.\n"),
                Ok(DaemonResponse::Jobs { jobs }) => {
                    for j in jobs {
                        println!(
                            "{:<18} {:<9} {:<10} {:>6}ms  {} — {}",
                            j.id, j.kind, j.status, j.elapsed_ms, j.name, j.summary
                        );
                    }
                    println!();
                }
                Ok(DaemonResponse::Error { message }) => eprintln!("Error: {message}\n"),
                Ok(_) => eprintln!("Unexpected response\n"),
                Err(e) => eprintln!("Daemon error: {e}\n"),
            }
            continue;
        }

        let req = if trimmed == ":reset" {
            ReplRequest::Reset
        } else if trimmed == ":inspect" || trimmed == ":vars" {
            ReplRequest::Inspect {
                name: None,
                origin: Some(ReplOrigin::User),
                offset: None,
                limit: None,
            }
        } else if let Some(rest) = trimmed.strip_prefix(":vars ").or_else(|| trimmed.strip_prefix(":inspect ")) {
            let name = rest.trim();
            ReplRequest::Inspect {
                name: (!name.is_empty()).then(|| name.to_string()),
                origin: name.is_empty().then_some(ReplOrigin::User),
                offset: None,
                limit: None,
            }
        } else if trimmed == ":history" {
            ReplRequest::History {
                id: None,
                offset: None,
                limit: None,
            }
        } else if let Some(rest) = trimmed.strip_prefix(":history ") {
            match rest.trim().parse::<u64>() {
                Ok(id) if id >= 1 => ReplRequest::History {
                    id: Some(id),
                    offset: None,
                    limit: None,
                },
                _ => {
                    eprintln!("Expected a positive submission ID.\n");
                    continue;
                }
            }
        } else {
            ReplRequest::Eval {
                code: trimmed.to_string(),
                inputs: BTreeMap::new(),
            }
        };

        match send_daemon_request(
            socket,
            &DaemonRequest::Repl {
                key: key.clone(),
                request: req,
            },
        )
        .await
        {
            Ok(DaemonResponse::Repl(reply)) => println!("{}\n", reply.text),
            Ok(DaemonResponse::Error { message }) => eprintln!("Error: {message}\n"),
            Ok(_) => eprintln!("Unexpected response\n"),
            Err(e) => eprintln!("Daemon error: {e}\n"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cant_protocol::daemon::WorkspaceSummary;

    fn ws(session_id: &str, cwd: &str, active: bool) -> WorkspaceSummary {
        WorkspaceSummary {
            session_id: session_id.into(),
            cwd: cwd.into(),
            language: "steel".into(),
            workspace_id: "ws-1".into(),
            generation: 1,
            active,
        }
    }

    #[test]
    fn resolve_attached_prefers_active_codex_workspace_for_cwd() {
        let workspaces = vec![
            ws("pi-abc", "/repo", true),
            ws("codex-old", "/repo", false),
            ws("codex-live", "/repo", true),
            ws("codex-other", "/other", true),
        ];
        assert_eq!(
            resolve_attached_session(&workspaces, "/repo").as_deref(),
            Some("codex-live")
        );
    }

    #[test]
    fn resolve_attached_falls_back_to_same_cwd_or_unique_active_codex() {
        let same_cwd = vec![ws("pi-abc", "/repo", true), ws("codex-other", "/other", true)];
        assert_eq!(
            resolve_attached_session(&same_cwd, "/repo").as_deref(),
            Some("pi-abc")
        );

        let subdir = vec![ws("codex-live", "/repo", true)];
        assert_eq!(
            resolve_attached_session(&subdir, "/repo/crates/cli").as_deref(),
            Some("codex-live")
        );

        let ambiguous = vec![
            ws("codex-a", "/other-a", true),
            ws("codex-b", "/other-b", true),
        ];
        assert_eq!(resolve_attached_session(&ambiguous, "/repo"), None);
    }
}
