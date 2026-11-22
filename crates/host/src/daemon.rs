//! Local Unix-socket daemon (`cant daemon`) with request/response RPC,
//! live event subscriptions (`Subscribe`), and bidirectional reverse-RPC (`AttachAdapter`).

use crate::repl::ReplManager;
use cant_protocol::daemon::{DaemonRequest, DaemonResponse};
use cant_protocol::runner::{encode_frame, DEFAULT_MAX_PROTOCOL_BYTES};
use std::io::ErrorKind;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub fn default_socket_path() -> PathBuf {
    if let Ok(explicit) = std::env::var("CANT_SOCKET") {
        if !explicit.trim().is_empty() {
            return PathBuf::from(explicit);
        }
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home)
            .join("Library")
            .join("Caches")
            .join("cant")
            .join("cant.sock")
    }
    #[cfg(not(target_os = "macos"))]
    {
        if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
            if !runtime.trim().is_empty() {
                return PathBuf::from(runtime).join("cant.sock");
            }
        }
        std::env::temp_dir().join(format!("cant-{}.sock", unsafe { libc::geteuid() }))
    }
}

/// Identify the socket, not just its pathname: another daemon may have rebound it.
fn socket_id(path: &Path) -> Result<Option<(u64, u64)>, String> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_socket() => Ok(Some((m.dev(), m.ino()))),
        Ok(_) => Err(format!("Not a Unix socket: {}", path.display())),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("Cannot inspect {}: {e}", path.display())),
    }
}

pub async fn run_daemon(socket_path: &Path, manager: Arc<ReplManager>) -> Result<(), String> {
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed creating socket directory {}: {e}", parent.display()))?;
    }
    if let Some(id) = socket_id(socket_path)? {
        match UnixStream::connect(socket_path).await {
            Ok(_) => {
                return Err(format!(
                    "Cant daemon is already listening on {}",
                    socket_path.display()
                ))
            }
            Err(e)
                if e.kind() == ErrorKind::ConnectionRefused
                    && socket_id(socket_path)? == Some(id) =>
            {
                std::fs::remove_file(socket_path)
                    .map_err(|e| format!("Removing stale socket: {e}"))?;
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(format!("Cannot probe {}: {e}", socket_path.display())),
        }
    }

    let listener = UnixListener::bind(socket_path)
        .map_err(|e| format!("Failed binding {}: {e}", socket_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600));
    }

    let bound_id = socket_id(socket_path)?;
    let shutdown = CancellationToken::new();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                break;
            }
            _ = tokio::signal::ctrl_c() => {
                break;
            }
            accept_res = listener.accept() => {
                match accept_res {
                    Ok((stream, _)) => {
                        let mgr = Arc::clone(&manager);
                        let shutdown = shutdown.clone();
                        tokio::spawn(async move {
                            handle_client(stream, mgr, shutdown).await;
                        });
                    }
                    Err(e) => {
                        eprintln!("[cantd] accept error: {e}");
                    }
                }
            }
        }
    }

    shutdown.cancel();
    manager.shutdown().await;
    if socket_id(socket_path)? == bound_id {
        std::fs::remove_file(socket_path).map_err(|e| format!("Removing daemon socket: {e}"))?;
    }
    Ok(())
}

async fn handle_client(stream: UnixStream, manager: Arc<ReplManager>, shutdown: CancellationToken) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();

    loop {
        line.clear();
        let n = match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        if n > DEFAULT_MAX_PROTOCOL_BYTES {
            let _ = write_response(
                &mut write_half,
                &DaemonResponse::Error {
                    message: "Request frame exceeds byte limit.".into(),
                },
            )
            .await;
            break;
        }
        let req: DaemonRequest = match serde_json::from_str(line.trim_end()) {
            Ok(r) => r,
            Err(e) => {
                let _ = write_response(
                    &mut write_half,
                    &DaemonResponse::Error {
                        message: format!("Invalid request: {e}"),
                    },
                )
                .await;
                continue;
            }
        };

        // Long-lived streaming modes take over the connection.
        match req {
            DaemonRequest::Subscribe { session_id } => {
                run_subscription(session_id, reader, write_half, manager, shutdown).await;
                return;
            }
            DaemonRequest::AttachAdapter {
                adapter_id,
                session_id,
                capabilities,
                worker_driver,
            } => {
                run_adapter_connection(
                    adapter_id,
                    session_id,
                    capabilities,
                    worker_driver,
                    reader,
                    write_half,
                    manager,
                    shutdown,
                )
                .await;
                return;
            }
            _ => {}
        }

        let mut stop_after = false;
        let resp = match req {
            DaemonRequest::Ping => DaemonResponse::Pong {
                version: env!("CARGO_PKG_VERSION").to_string(),
                pid: std::process::id(),
                languages: manager.languages().ids(),
                drivers: manager.jobs().drivers.ids(),
            },
            DaemonRequest::Repl { key, request } => match manager.run(key, request).await {
                Ok(reply) => DaemonResponse::Repl(reply),
                Err(message) => DaemonResponse::Error { message },
            },
            DaemonRequest::ListWorkspaces => DaemonResponse::Workspaces {
                workspaces: manager.list_workspaces().await,
            },
            DaemonRequest::ToolInfo {
                language,
                disabled_namespaces,
                session_id,
            } => match manager
                .tool_info(&language, &disabled_namespaces, session_id.as_deref())
                .await
            {
                Ok((description, input_schema)) => DaemonResponse::ToolInfo {
                    name: "repl".into(),
                    description,
                    input_schema,
                },
                Err(message) => DaemonResponse::Error { message },
            },
            DaemonRequest::ListJobs { session_id } => DaemonResponse::Jobs {
                jobs: manager.jobs().list_all_jobs(session_id.as_deref()),
            },
            DaemonRequest::JobTail {
                id,
                max_bytes,
                session_id,
            } => match manager
                .jobs()
                .tail_job(&id, max_bytes, session_id.as_deref())
            {
                Ok(tail) => DaemonResponse::JobTail {
                    id,
                    kind: tail.summary.kind.clone(),
                    status: tail.summary.status.clone(),
                    output: tail.output,
                    summary: Some(tail.summary),
                },
                Err(err) => DaemonResponse::Error {
                    message: err.to_string(),
                },
            },
            DaemonRequest::CancelJob { id, session_id } => {
                match manager.jobs().cancel_any_job(&id, session_id.as_deref()) {
                    Ok(()) => DaemonResponse::Ok,
                    Err(err) => DaemonResponse::Error {
                        message: err.to_string(),
                    },
                }
            }
            DaemonRequest::DrainNotices {
                session_id,
                include_passive,
                cwd,
                harness,
            } => {
                let cwd_path = Path::new(cwd.as_deref().unwrap_or(""));
                let kind = harness.unwrap_or_else(|| crate::comms::infer_harness(&session_id));
                manager.comms().touch_peer(&session_id, cwd_path, kind);
                DaemonResponse::Notices {
                    notices: manager.jobs().drain_notices(&session_id, include_passive),
                    channel_messages: manager.comms().drain_notices(&session_id),
                }
            }
            DaemonRequest::ListPeers { include_stale } => DaemonResponse::Peers {
                peers: manager.comms().list_peers(None, None, include_stale),
            },
            DaemonRequest::ListChannels {
                session_id,
                include_closed,
            } => DaemonResponse::Channels {
                channels: manager
                    .comms()
                    .list_channels(session_id.as_deref(), include_closed),
            },
            DaemonRequest::ChannelRead {
                channel,
                session_id,
                limit,
            } => match manager
                .comms()
                .peek_channel(session_id.as_deref(), &channel, limit)
            {
                Ok((summary, messages)) => DaemonResponse::ChannelMessages {
                    channel: summary,
                    messages,
                },
                Err(message) => DaemonResponse::Error { message },
            },
            DaemonRequest::ChannelSend {
                target,
                body,
                session_id,
                handle,
                cwd,
            } => {
                let sid = session_id.unwrap_or_else(|| "cli".into());
                let cwd_path = Path::new(cwd.as_deref().unwrap_or(""));
                match manager.comms().send_to_target(
                    &sid,
                    cwd_path,
                    handle.as_deref(),
                    &target,
                    body,
                ) {
                    Ok(res) => DaemonResponse::ChannelSent {
                        channel_id: res.channel_id,
                        message: res.message,
                    },
                    Err(message) => DaemonResponse::Error { message },
                }
            }
            DaemonRequest::Shutdown => {
                stop_after = true;
                DaemonResponse::Ok
            }
            DaemonRequest::UpdateCapabilities { .. }
            | DaemonRequest::AdapterResult { .. }
            | DaemonRequest::AdapterWorkerProgress { .. } => DaemonResponse::Error {
                message: "Adapter messages require an active AttachAdapter connection.".into(),
            },
            DaemonRequest::Subscribe { .. } | DaemonRequest::AttachAdapter { .. } => unreachable!(),
        };

        let result = write_response(&mut write_half, &resp).await;
        if stop_after {
            shutdown.cancel();
            break;
        }
        if result.is_err() {
            break;
        }
    }
}

async fn run_subscription(
    session_filter: Option<String>,
    mut reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    mut write_half: tokio::net::unix::OwnedWriteHalf,
    manager: Arc<ReplManager>,
    shutdown: CancellationToken,
) {
    // Subscribe before acknowledging so events emitted right after Ok cannot be missed.
    let mut rx = manager.jobs().subscribe_split();
    let mut comms_rx = manager.comms().subscribe_events();
    let _push_guard = session_filter
        .as_deref()
        .map(|sid| manager.comms().acquire_push(sid));
    if write_response(&mut write_half, &DaemonResponse::Ok)
        .await
        .is_err()
    {
        return;
    }
    if let Some(ref sid) = session_filter {
        for notice in manager.comms().drain_notices(sid) {
            let ev = cant_protocol::daemon::DaemonEvent::ChannelMessage { notice };
            if write_response(&mut write_half, &DaemonResponse::Event(ev))
                .await
                .is_err()
            {
                return;
            }
        }
    }
    let mut dummy = String::new();
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            read_res = reader.read_line(&mut dummy) => {
                if matches!(read_res, Ok(0) | Err(_)) {
                    break;
                }
                dummy.clear();
            }
            ev = rx.lifecycle.recv() => {
                match ev {
                    Ok((sid, event)) => {
                        if session_filter.as_deref().is_none_or(|f| f == sid)
                            && write_response(&mut write_half, &DaemonResponse::Event(event))
                                .await
                                .is_err()
                        {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        let jobs = manager.jobs().list_all_jobs(session_filter.as_deref());
                        for job in jobs {
                            let ev = cant_protocol::daemon::DaemonEvent::JobUpdated { job };
                            if write_response(&mut write_half, &DaemonResponse::Event(ev))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            ev = comms_rx.recv() => {
                match ev {
                    Ok((sid, event)) => {
                        if session_filter.as_deref() == Some(sid.as_str())
                            && write_response(&mut write_half, &DaemonResponse::Event(event))
                                .await
                                .is_err()
                        {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        if let Some(ref sid) = session_filter {
                            for notice in manager.comms().drain_notices(sid) {
                                let ev = cant_protocol::daemon::DaemonEvent::ChannelMessage { notice };
                                if write_response(&mut write_half, &DaemonResponse::Event(ev))
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            ev = rx.output.recv() => {
                match ev {
                    Ok((sid, event)) => {
                        if session_filter.as_deref().is_none_or(|f| f == sid)
                            && write_response(&mut write_half, &DaemonResponse::Event(event))
                                .await
                                .is_err()
                        {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_adapter_connection(
    adapter_id: String,
    session_id: String,
    capabilities: Vec<cant_protocol::daemon::ExternalCapabilitySpec>,
    worker_driver: Option<String>,
    mut reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    mut write_half: tokio::net::unix::OwnedWriteHalf,
    manager: Arc<ReplManager>,
    shutdown: CancellationToken,
) {
    let (out_tx, mut out_rx) = mpsc::channel::<DaemonResponse>(64);
    let session = manager.adapters().attach(
        adapter_id.clone(),
        session_id,
        worker_driver,
        capabilities,
        out_tx,
    );

    if write_response(
        &mut write_half,
        &DaemonResponse::AdapterAttached {
            adapter_id: adapter_id.clone(),
        },
    )
    .await
    .is_err()
    {
        manager.adapters().detach(&adapter_id);
        return;
    }

    let mut line = String::new();
    loop {
        line.clear();
        tokio::select! {
            _ = shutdown.cancelled() => break,
            Some(msg) = out_rx.recv() => {
                if write_response(&mut write_half, &msg).await.is_err() {
                    break;
                }
            }
            read_res = reader.read_line(&mut line) => {
                match read_res {
                    Ok(0) | Err(_) => break,
                    Ok(n) if n > DEFAULT_MAX_PROTOCOL_BYTES => break,
                    Ok(_) => {
                        let Ok(req) = serde_json::from_str::<DaemonRequest>(line.trim_end()) else {
                            continue;
                        };
                        match req {
                            DaemonRequest::UpdateCapabilities { capabilities } => {
                                manager.adapters().update_capabilities(&adapter_id, capabilities);
                            }
                            DaemonRequest::AdapterResult { call_id, ok, value, error } => {
                                session.resolve_result(&call_id, ok, value, error);
                            }
                            DaemonRequest::AdapterWorkerProgress { call_id, chunk } => {
                                session.forward_progress(&call_id, chunk);
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    manager.adapters().detach(&adapter_id);
}

async fn write_response(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    resp: &DaemonResponse,
) -> Result<(), String> {
    let frame = encode_frame(resp, DEFAULT_MAX_PROTOCOL_BYTES)?;
    writer
        .write_all(frame.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    writer.flush().await.map_err(|e| e.to_string())
}

async fn exchange_on_stream(
    stream: UnixStream,
    request: &DaemonRequest,
) -> Result<DaemonResponse, String> {
    let (read_half, mut write_half) = stream.into_split();
    let frame = encode_frame(request, DEFAULT_MAX_PROTOCOL_BYTES)?;
    write_half
        .write_all(frame.as_bytes())
        .await
        .map_err(|e| format!("Failed writing to cant daemon: {e}"))?;
    write_half
        .flush()
        .await
        .map_err(|e| format!("Failed flushing to cant daemon: {e}"))?;

    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    let n = reader
        .read_line(&mut line)
        .await
        .map_err(|e| format!("Failed reading from cant daemon: {e}"))?;
    if n == 0 {
        return Err("Cant daemon closed connection before replying.".into());
    }
    serde_json::from_str(line.trim_end()).map_err(|e| format!("Malformed daemon response: {e}"))
}

/// Sends a request to an already-running daemon without spawning one (ideal for fast hooks).
pub async fn send_daemon_request_if_running(
    socket_path: &Path,
    request: &DaemonRequest,
) -> Result<Option<DaemonResponse>, String> {
    let Ok(stream) = UnixStream::connect(socket_path).await else {
        return Ok(None);
    };
    exchange_on_stream(stream, request).await.map(Some)
}

/// Stop only the listener that currently owns this path. Older daemons may
/// acknowledge shutdown but wake a subscriber instead of the accept loop, so
/// retry the RPC while the same endpoint remains. Never signal a reported PID.
pub async fn shutdown_daemon(path: &Path) -> Result<(), String> {
    let Some(id) = socket_id(path)? else {
        return Ok(());
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        while socket_id(path)? == Some(id) {
            let stream = match UnixStream::connect(path).await {
                Ok(stream) => stream,
                Err(e)
                    if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) =>
                {
                    break
                }
                Err(e) => return Err(format!("Cannot connect to {}: {e}", path.display())),
            };
            if socket_id(path)? != Some(id) {
                break;
            }
            // During teardown the listener can still exist but no longer accept.
            match tokio::time::timeout(
                Duration::from_millis(500),
                exchange_on_stream(stream, &DaemonRequest::Shutdown),
            )
            .await
            {
                Ok(Ok(DaemonResponse::Ok)) | Err(_) => {}
                Ok(Ok(DaemonResponse::Error { message })) => return Err(message),
                Ok(Ok(_)) => return Err("Unexpected response to Shutdown".into()),
                Ok(Err(e)) if socket_id(path)? == Some(id) => return Err(e),
                Ok(Err(_)) => break,
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(())
    })
    .await
    .map_err(|_| {
        format!(
            "Timed out waiting for daemon on {} to shut down",
            path.display()
        )
    })?
}

/// Stop the current listener, then require a working RPC on the new listener.
pub async fn restart_daemon(path: &Path) -> Result<(), String> {
    shutdown_daemon(path).await?;
    match tokio::time::timeout(
        Duration::from_secs(5),
        send_daemon_request(path, &DaemonRequest::Ping),
    )
    .await
    .map_err(|_| format!("Timed out pinging daemon on {}", path.display()))??
    {
        DaemonResponse::Pong { .. } => Ok(()),
        DaemonResponse::Error { message } => Err(message),
        _ => Err("Unexpected response to Ping".into()),
    }
}

/// Sends a single request to the local daemon, auto-starting the daemon via `current_exe daemon`
/// if it is not yet running.
pub async fn send_daemon_request(
    socket_path: &Path,
    request: &DaemonRequest,
) -> Result<DaemonResponse, String> {
    let stream = connect_or_spawn(socket_path).await?;
    exchange_on_stream(stream, request).await
}

pub async fn connect_or_spawn(socket_path: &Path) -> Result<UnixStream, String> {
    if let Ok(stream) = UnixStream::connect(socket_path).await {
        return Ok(stream);
    }
    let exe = std::env::current_exe().map_err(|e| format!("Failed resolving current exe: {e}"))?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.args(["daemon", "--socket"])
        .arg(socket_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    cmd.spawn()
        .map_err(|e| format!("Failed auto-starting cant daemon ({}): {e}", exe.display()))?;

    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if let Ok(stream) = UnixStream::connect(socket_path).await {
            return Ok(stream);
        }
    }
    Err(format!(
        "Timed out waiting for cant daemon on {}",
        socket_path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::AdapterHub;
    use crate::capability::CapabilityCatalog;
    use crate::jobs::driver::WorkerDriverRegistry;
    use crate::jobs::JobManager;
    use crate::language::LanguageRegistry;
    use cant_protocol::daemon::{CompletionNotice, DaemonEvent, JobSummary};
    use tokio::time::timeout;

    fn test_manager() -> Arc<ReplManager> {
        let jobs = Arc::new(JobManager::new(Arc::new(WorkerDriverRegistry::new())));
        Arc::new(ReplManager::new(
            Arc::new(LanguageRegistry::new()),
            Arc::new(CapabilityCatalog::new()),
            Arc::new(AdapterHub::new()),
            jobs,
        ))
    }

    fn summary(session_id: &str, id: &str, status: &str) -> JobSummary {
        JobSummary {
            id: id.into(),
            kind: "script".into(),
            name: "build".into(),
            session_id: session_id.into(),
            status: status.into(),
            elapsed_ms: 10,
            summary: String::new(),
        }
    }

    #[tokio::test]
    async fn noisy_output_never_drops_lifecycle_events() {
        let manager = test_manager();
        let (server, client) = UnixStream::pair().unwrap();
        let (srv_r, srv_w) = server.into_split();
        let shutdown = CancellationToken::new();

        let sub_task = tokio::spawn(run_subscription(
            Some("s1".into()),
            BufReader::new(srv_r),
            srv_w,
            Arc::clone(&manager),
            shutdown.clone(),
        ));

        let mut client_reader = BufReader::new(client);
        let mut line = String::new();
        client_reader.read_line(&mut line).await.unwrap();
        assert!(matches!(
            serde_json::from_str::<DaemonResponse>(line.trim_end()).unwrap(),
            DaemonResponse::Ok
        ));

        // Emit job start, then flood >256 output chunks before finishing.
        manager.jobs().emit_event(
            "s1",
            DaemonEvent::JobUpdated {
                job: summary("s1", "job-1", "running"),
            },
        );
        for i in 0..400 {
            manager.jobs().emit_event(
                "s1",
                DaemonEvent::JobOutput {
                    id: "job-1".into(),
                    chunk: format!("line {i}\n"),
                },
            );
        }
        manager.jobs().emit_event(
            "s1",
            DaemonEvent::JobUpdated {
                job: summary("s1", "job-1", "succeeded"),
            },
        );
        manager.jobs().emit_event(
            "s1",
            DaemonEvent::JobCompleted {
                notice: CompletionNotice {
                    job_id: "job-1".into(),
                    kind: "script".into(),
                    name: "build".into(),
                    status: "succeeded".into(),
                    notify: "active".into(),
                    summary: "done".into(),
                    completion_token: None,
                },
            },
        );

        let mut statuses = Vec::new();
        let mut saw_completed = false;
        while !saw_completed {
            line.clear();
            let n = timeout(Duration::from_secs(2), client_reader.read_line(&mut line))
                .await
                .expect("timed out waiting for lifecycle event")
                .unwrap();
            assert!(n > 0, "subscription closed prematurely");
            if let DaemonResponse::Event(ev) = serde_json::from_str(line.trim_end()).unwrap() {
                match ev {
                    DaemonEvent::JobUpdated { job } => statuses.push(job.status),
                    DaemonEvent::JobCompleted { .. } => saw_completed = true,
                    DaemonEvent::JobOutput { .. } | DaemonEvent::ChannelMessage { .. } => {}
                }
            }
        }

        assert_eq!(
            statuses,
            vec!["running".to_string(), "succeeded".to_string()],
            "noisy JobOutput must never evict JobUpdated lifecycle events"
        );

        shutdown.cancel();
        let _ = sub_task.await;
    }

    #[tokio::test]
    async fn lifecycle_lag_resyncs_current_jobs() {
        let manager = test_manager();
        let (server, client) = UnixStream::pair().unwrap();
        let (srv_r, srv_w) = server.into_split();
        let shutdown = CancellationToken::new();

        let sub_task = tokio::spawn(run_subscription(
            Some("s1".into()),
            BufReader::new(srv_r),
            srv_w,
            Arc::clone(&manager),
            shutdown.clone(),
        ));

        let mut client_reader = BufReader::new(client);
        let mut line = String::new();
        client_reader.read_line(&mut line).await.unwrap();
        assert!(matches!(
            serde_json::from_str::<DaemonResponse>(line.trim_end()).unwrap(),
            DaemonResponse::Ok
        ));

        // Insert a real tracked job for s1 and emit its start, then overflow the lifecycle buffer with other sessions.
        manager.jobs().flows.lock().unwrap().insert(
            "flow-1".into(),
            Arc::new(std::sync::Mutex::new(crate::jobs::agents::AgentFlowJob {
                id: "flow-1".into(),
                session_id: "s1".into(),
                name: "tracked".into(),
                notify: crate::jobs::NotifyMode::Active,
                status: crate::jobs::agents::WorkerStatus::Running,
                completion_token: "tok".into(),
                nodes: Default::default(),
                node_order: vec![],
                output_value: None,
                error: None,
                started_at: std::time::Instant::now(),
                finished_ms: None,
                cancel_token: CancellationToken::new(),
                cancelled_explicitly: false,
            })),
        );
        manager.jobs().emit_event(
            "s1",
            DaemonEvent::JobUpdated {
                job: summary("s1", "flow-1", "running"),
            },
        );
        for i in 0..300 {
            manager.jobs().emit_event(
                "other-session",
                DaemonEvent::JobUpdated {
                    job: summary("other-session", &format!("other-{i}"), "running"),
                },
            );
        }

        line.clear();
        let n = timeout(Duration::from_secs(1), client_reader.read_line(&mut line))
            .await
            .expect("expected lagged subscriber to resync current jobs")
            .unwrap();
        assert!(n > 0);
        match serde_json::from_str::<DaemonResponse>(line.trim_end()).unwrap() {
            DaemonResponse::Event(DaemonEvent::JobUpdated { job }) => {
                assert_eq!(job.id, "flow-1");
                assert_eq!(job.status, "running");
            }
            other => panic!("expected resynced JobUpdated for flow-1, got {other:?}"),
        }

        shutdown.cancel();
        let _ = sub_task.await;
    }

    #[tokio::test]
    async fn subscription_pushes_channel_messages_and_tracks_push_reachability() {
        use crate::comms::OpenChannelOpts;
        use cant_protocol::daemon::{HarnessKind, PeerReachability};

        let manager = test_manager();
        manager
            .comms()
            .touch_peer("pi-1", Path::new("/repo/pi"), HarnessKind::Pi);
        manager
            .comms()
            .touch_peer("agy-1", Path::new("/repo/agy"), HarnessKind::Antigravity);

        let (server, client) = UnixStream::pair().unwrap();
        let (srv_r, srv_w) = server.into_split();
        let shutdown = CancellationToken::new();

        let sub_task = tokio::spawn(run_subscription(
            Some("pi-1".into()),
            BufReader::new(srv_r),
            srv_w,
            Arc::clone(&manager),
            shutdown.clone(),
        ));

        let mut client_reader = BufReader::new(client);
        let mut line = String::new();
        client_reader.read_line(&mut line).await.unwrap();
        assert!(matches!(
            serde_json::from_str::<DaemonResponse>(line.trim_end()).unwrap(),
            DaemonResponse::Ok
        ));

        assert_eq!(
            manager
                .comms()
                .peer_me("pi-1", Path::new("/repo/pi"))
                .reachability,
            PeerReachability::Push
        );

        manager
            .comms()
            .open_channel(
                "agy-1",
                Path::new("/repo/agy"),
                OpenChannelOpts {
                    to: Some("pi".into()),
                    message: Some("hello from agy".into()),
                    ..Default::default()
                },
            )
            .unwrap();

        line.clear();
        let n = timeout(Duration::from_secs(1), client_reader.read_line(&mut line))
            .await
            .expect("expected pushed ChannelMessage event")
            .unwrap();
        assert!(n > 0);
        match serde_json::from_str::<DaemonResponse>(line.trim_end()).unwrap() {
            DaemonResponse::Event(DaemonEvent::ChannelMessage { notice }) => {
                assert_eq!(notice.body, "hello from agy");
                assert_eq!(notice.from_handle, "agy");
            }
            other => panic!("expected ChannelMessage, got {other:?}"),
        }

        shutdown.cancel();
        let _ = sub_task.await;
        assert_eq!(
            manager
                .comms()
                .peer_me("pi-1", Path::new("/repo/pi"))
                .reachability,
            PeerReachability::Idle
        );
    }

    #[tokio::test]
    async fn subscription_flushes_queued_channel_messages_on_connect() {
        use crate::comms::OpenChannelOpts;
        use cant_protocol::daemon::HarnessKind;

        let manager = test_manager();
        manager
            .comms()
            .touch_peer("pi-2", Path::new("/repo/pi"), HarnessKind::Pi);
        manager
            .comms()
            .touch_peer("agy-2", Path::new("/repo/agy"), HarnessKind::Antigravity);

        // Message arrives before pi-2 subscribes
        manager
            .comms()
            .open_channel(
                "agy-2",
                Path::new("/repo/agy"),
                OpenChannelOpts {
                    to: Some("pi".into()),
                    message: Some("queued before connect".into()),
                    ..Default::default()
                },
            )
            .unwrap();

        let (server, client) = UnixStream::pair().unwrap();
        let (srv_r, srv_w) = server.into_split();
        let shutdown = CancellationToken::new();
        let sub_task = tokio::spawn(run_subscription(
            Some("pi-2".into()),
            BufReader::new(srv_r),
            srv_w,
            Arc::clone(&manager),
            shutdown.clone(),
        ));

        let mut client_reader = BufReader::new(client);
        let mut line = String::new();
        client_reader.read_line(&mut line).await.unwrap();
        assert!(matches!(
            serde_json::from_str::<DaemonResponse>(line.trim_end()).unwrap(),
            DaemonResponse::Ok
        ));

        line.clear();
        timeout(Duration::from_secs(1), client_reader.read_line(&mut line))
            .await
            .expect("expected flushed ChannelMessage")
            .unwrap();
        match serde_json::from_str::<DaemonResponse>(line.trim_end()).unwrap() {
            DaemonResponse::Event(DaemonEvent::ChannelMessage { notice }) => {
                assert_eq!(notice.body, "queued before connect");
            }
            other => panic!("expected flushed ChannelMessage, got {other:?}"),
        }

        shutdown.cancel();
        let _ = sub_task.await;
    }

    #[tokio::test]
    async fn sessionless_subscription_does_not_receive_channel_messages() {
        use crate::comms::OpenChannelOpts;
        use cant_protocol::daemon::HarnessKind;

        let manager = test_manager();
        manager
            .comms()
            .touch_peer("pi-1", Path::new("/repo/pi"), HarnessKind::Pi);
        manager
            .comms()
            .touch_peer("agy-1", Path::new("/repo/agy"), HarnessKind::Antigravity);

        // Session-scoped subscriber (pi-1) and sessionless subscriber (None)
        let (srv_scoped, cli_scoped) = UnixStream::pair().unwrap();
        let (srv_global, cli_global) = UnixStream::pair().unwrap();
        let shutdown = CancellationToken::new();

        let (scoped_r, scoped_w) = srv_scoped.into_split();
        let scoped_task = tokio::spawn(run_subscription(
            Some("pi-1".into()),
            BufReader::new(scoped_r),
            scoped_w,
            Arc::clone(&manager),
            shutdown.clone(),
        ));
        let (global_r, global_w) = srv_global.into_split();
        let global_task = tokio::spawn(run_subscription(
            None,
            BufReader::new(global_r),
            global_w,
            Arc::clone(&manager),
            shutdown.clone(),
        ));

        let mut scoped_reader = BufReader::new(cli_scoped);
        let mut global_reader = BufReader::new(cli_global);
        let mut line = String::new();
        scoped_reader.read_line(&mut line).await.unwrap();
        line.clear();
        global_reader.read_line(&mut line).await.unwrap();

        manager
            .comms()
            .open_channel(
                "agy-1",
                Path::new("/repo/agy"),
                OpenChannelOpts {
                    to: Some("pi".into()),
                    message: Some("private dm".into()),
                    ..Default::default()
                },
            )
            .unwrap();

        // Session-scoped subscriber receives the DM event.
        line.clear();
        timeout(Duration::from_secs(1), scoped_reader.read_line(&mut line))
            .await
            .expect("scoped subscriber should receive ChannelMessage")
            .unwrap();
        assert!(matches!(
            serde_json::from_str::<DaemonResponse>(line.trim_end()).unwrap(),
            DaemonResponse::Event(DaemonEvent::ChannelMessage { .. })
        ));

        // Sessionless subscriber must NOT receive channel messages.
        let mut global_line = String::new();
        let got_global = timeout(
            Duration::from_millis(100),
            global_reader.read_line(&mut global_line),
        )
        .await;
        assert!(
            got_global.is_err(),
            "sessionless subscriber unexpectedly received: {global_line}"
        );

        shutdown.cancel();
        let _ = scoped_task.await;
        let _ = global_task.await;
    }

    #[tokio::test]
    async fn cli_channel_read_requires_member_session_for_dm_and_peeks_without_consuming() {
        use crate::comms::OpenChannelOpts;
        use cant_protocol::daemon::HarnessKind;

        let manager = test_manager();
        manager
            .comms()
            .touch_peer("alice", Path::new("/repo/alice"), HarnessKind::Antigravity);
        manager
            .comms()
            .touch_peer("bob", Path::new("/repo/bob"), HarnessKind::Antigravity);
        manager.comms().touch_peer(
            "charlie",
            Path::new("/repo/charlie"),
            HarnessKind::Antigravity,
        );

        let dm = manager
            .comms()
            .open_channel(
                "alice",
                Path::new("/repo/alice"),
                OpenChannelOpts {
                    to: Some("bob".into()),
                    message: Some("hello bob".into()),
                    ..Default::default()
                },
            )
            .unwrap()
            .channel
            .id;

        async fn send_req(manager: &Arc<ReplManager>, req: &DaemonRequest) -> DaemonResponse {
            let (server, client) = UnixStream::pair().unwrap();
            let shutdown = CancellationToken::new();
            let task = tokio::spawn(handle_client(server, Arc::clone(manager), shutdown));
            let (r, mut w) = client.into_split();
            let payload = format!("{}\n", serde_json::to_string(req).unwrap());
            w.write_all(payload.as_bytes()).await.unwrap();
            w.shutdown().await.unwrap();
            let mut reader = BufReader::new(r);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let _ = task.await;
            serde_json::from_str(line.trim_end()).unwrap()
        }

        // Sessionless and nonmember reads are rejected for a DM.
        let resp_none = send_req(
            &manager,
            &DaemonRequest::ChannelRead {
                channel: dm.clone(),
                session_id: None,
                limit: 50,
            },
        )
        .await;
        assert!(
            matches!(resp_none, DaemonResponse::Error { .. }),
            "got {resp_none:?}"
        );

        let resp_charlie = send_req(
            &manager,
            &DaemonRequest::ChannelRead {
                channel: dm.clone(),
                session_id: Some("charlie".into()),
                limit: 50,
            },
        )
        .await;
        assert!(
            matches!(resp_charlie, DaemonResponse::Error { .. }),
            "got {resp_charlie:?}"
        );

        // Member read succeeds and peeks without advancing Bob's unread or notice cursors.
        let resp_bob = send_req(
            &manager,
            &DaemonRequest::ChannelRead {
                channel: dm.clone(),
                session_id: Some("bob".into()),
                limit: 50,
            },
        )
        .await;
        match resp_bob {
            DaemonResponse::ChannelMessages { channel, messages } => {
                assert_eq!(channel.id, dm);
                assert_eq!(channel.unread, 1);
                assert_eq!(messages.len(), 1);
                assert_eq!(messages[0].body, "hello bob");
            }
            other => panic!("expected ChannelMessages, got {other:?}"),
        }

        // Bob's queued notice and unread message must still be available.
        let notices = manager.comms().drain_notices("bob");
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].body, "hello bob");
        let (_, unread) = manager
            .comms()
            .read_channel(Some("bob"), &dm, true, 50)
            .unwrap();
        assert_eq!(unread.len(), 1);
    }
}
