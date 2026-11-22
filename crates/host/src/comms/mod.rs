//! Local inter-agent peer discovery and channels (`peers/*` and `channels/*`).

pub mod capabilities;
#[cfg(test)]
mod tests;

use cant_protocol::daemon::{
    ChannelKind, ChannelMessage, ChannelNotice, ChannelSummary, DaemonEvent, HarnessKind,
    PeerReachability, PeerSummary,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;

pub const DEFAULT_STALE_PEER_AFTER: Duration = Duration::from_secs(30 * 60);
pub const MAX_NOTICE_BODY_BYTES: usize = 4096;
pub const DEFAULT_WAIT_TIMEOUT_SECS: u64 = 30;
pub const MAX_WAIT_TIMEOUT_SECS: u64 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeliveryStatus {
    Pushed,
    Woken,
    Queued,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryReport {
    pub peer: String,
    pub handle: String,
    pub status: DeliveryStatus,
}

#[derive(Debug, Clone, Default)]
pub struct OpenChannelOpts {
    pub to: Option<String>,
    pub name: Option<String>,
    pub members: Vec<String>,
    pub topic: Option<String>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenChannelResult {
    pub channel: ChannelSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<ChannelMessage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deliveries: Vec<DeliveryReport>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendResult {
    pub channel_id: String,
    pub message: ChannelMessage,
    pub deliveries: Vec<DeliveryReport>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WaitResult {
    pub timed_out: bool,
    pub messages: Vec<ChannelMessage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelArg {
    Auto,
    Refs(Vec<String>),
}

impl ChannelArg {
    pub fn parse(raw: Option<&serde_json::Value>) -> Result<Option<Self>, String> {
        let Some(val) = raw else {
            return Ok(None);
        };
        match val {
            serde_json::Value::Null | serde_json::Value::Bool(false) => Ok(None),
            serde_json::Value::Bool(true) => Ok(Some(Self::Auto)),
            serde_json::Value::String(s) => {
                let t = s.trim();
                if t.is_empty() {
                    Err("channel must not be an empty string.".into())
                } else {
                    Ok(Some(Self::Refs(vec![t.to_string()])))
                }
            }
            serde_json::Value::Array(arr) => {
                let mut refs = Vec::with_capacity(arr.len());
                for item in arr {
                    let s = item
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| "channel array entries must be non-empty strings.".to_string())?;
                    refs.push(s.to_string());
                }
                if refs.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(Self::Refs(refs)))
                }
            }
            _ => Err("channel must be true, a channel/room name string, or an array of channel/room strings.".into()),
        }
    }
}

pub struct WorkerPeerGuard {
    hub: Arc<CommsHub>,
    session_id: String,
}

impl Drop for WorkerPeerGuard {
    fn drop(&mut self) {
        self.hub.remove_peer(&self.session_id);
    }
}

#[derive(Debug, Clone)]
pub struct WorkerChannelPlan {
    pub channel_ids: Vec<String>,
    pub primary_channel_id: String,
    pub peer_handle: String,
    pub prompt_block: String,
}

struct PeerRecord {
    id: String,
    handle: String,
    explicit_handle: bool,
    cwd: PathBuf,
    harness: HarnessKind,
    topic: Option<String>,
    tags: Vec<String>,
    parent: Option<String>,
    last_seen_ms: u64,
    push_count: usize,
    listen_count: usize,
    active_count: usize,
}

impl PeerRecord {
    fn reachability(&self) -> PeerReachability {
        if self.push_count > 0 {
            PeerReachability::Push
        } else if self.listen_count > 0 {
            PeerReachability::Listening
        } else if self.active_count > 0 {
            PeerReachability::Active
        } else {
            PeerReachability::Idle
        }
    }

    fn is_stale(&self, now_ms: u64, stale_after_ms: u64) -> bool {
        if self.reachability() != PeerReachability::Idle {
            return false;
        }
        now_ms.saturating_sub(self.last_seen_ms) >= stale_after_ms
    }

    fn summary(&self, now_ms: u64, stale_after_ms: u64) -> PeerSummary {
        PeerSummary {
            id: self.id.clone(),
            handle: self.handle.clone(),
            cwd: self.cwd.to_string_lossy().into_owned(),
            harness: self.harness,
            topic: self.topic.clone(),
            tags: self.tags.clone(),
            parent: self.parent.clone(),
            reachability: self.reachability(),
            last_seen_ms: self.last_seen_ms,
            stale: self.is_stale(now_ms, stale_after_ms),
        }
    }
}

struct ChannelRecord {
    id: String,
    kind: ChannelKind,
    name: Option<String>,
    topic: Option<String>,
    closed: bool,
    summary: Option<String>,
    members: BTreeSet<String>,
    read_cursors: HashMap<String, u64>,
    notified_cursors: HashMap<String, u64>,
    messages: Vec<ChannelMessage>,
    next_seq: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadMode {
    Consume,
    Peek,
}

impl ChannelRecord {
    fn display_label(&self) -> String {
        match self.name.as_deref() {
            Some(name) => format!("{} (#{name})", self.id),
            None => self.id.clone(),
        }
    }

    fn can_access(&self, caller_id: Option<&str>) -> bool {
        match self.kind {
            ChannelKind::Room => true,
            ChannelKind::Direct => caller_id.is_some_and(|cid| self.members.contains(cid)),
        }
    }

    fn ensure_access(&self, caller_id: Option<&str>) -> Result<(), String> {
        if self.can_access(caller_id) {
            Ok(())
        } else {
            Err(format!(
                "Channel '{}' is a direct thread; only members can access it.",
                self.id
            ))
        }
    }

    fn summary(
        &self,
        viewer_id: Option<&str>,
        peers: &HashMap<String, PeerRecord>,
    ) -> ChannelSummary {
        let member_ids: Vec<String> = self.members.iter().cloned().collect();
        let members: Vec<String> = member_ids
            .iter()
            .map(|id| {
                peers
                    .get(id)
                    .map(|p| p.handle.clone())
                    .unwrap_or_else(|| id.clone())
            })
            .collect();
        let unread = viewer_id
            .map(|vid| {
                let cursor = self.read_cursors.get(vid).copied().unwrap_or(0);
                self.messages
                    .iter()
                    .filter(|m| m.seq > cursor && m.from != vid)
                    .count()
            })
            .unwrap_or(0);
        ChannelSummary {
            id: self.id.clone(),
            kind: self.kind,
            name: self.name.clone(),
            topic: self.topic.clone(),
            closed: self.closed,
            summary: self.summary.clone(),
            members,
            member_ids,
            message_count: self.messages.len(),
            unread,
        }
    }
}

#[derive(Default)]
struct CommsInner {
    peers: HashMap<String, PeerRecord>,
    channels: HashMap<String, ChannelRecord>,
    room_by_name: HashMap<String, String>,
    next_channel_num: u64,
}

pub struct PushGuard {
    hub: Arc<CommsHub>,
    session_id: String,
}

impl Drop for PushGuard {
    fn drop(&mut self) {
        let mut inner = self.hub.inner.lock().unwrap();
        if let Some(peer) = inner.peers.get_mut(&self.session_id) {
            peer.push_count = peer.push_count.saturating_sub(1);
            peer.last_seen_ms = now_ms();
        }
    }
}

pub struct ListenGuard {
    hub: Arc<CommsHub>,
    session_id: String,
}

impl Drop for ListenGuard {
    fn drop(&mut self) {
        let mut inner = self.hub.inner.lock().unwrap();
        if let Some(peer) = inner.peers.get_mut(&self.session_id) {
            peer.listen_count = peer.listen_count.saturating_sub(1);
            peer.last_seen_ms = now_ms();
        }
    }
}

pub struct ActiveGuard {
    hub: Arc<CommsHub>,
    session_id: String,
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        let mut inner = self.hub.inner.lock().unwrap();
        if let Some(peer) = inner.peers.get_mut(&self.session_id) {
            peer.active_count = peer.active_count.saturating_sub(1);
            peer.last_seen_ms = now_ms();
        }
    }
}

pub struct CommsHub {
    inner: Mutex<CommsInner>,
    stale_after_ms: u64,
    event_tx: broadcast::Sender<(String, DaemonEvent)>,
    wait_tx: broadcast::Sender<(String, String)>,
}

impl Default for CommsHub {
    fn default() -> Self {
        Self::new()
    }
}

impl CommsHub {
    pub fn new() -> Self {
        Self::with_stale_after(DEFAULT_STALE_PEER_AFTER)
    }

    pub fn with_stale_after(stale_after: Duration) -> Self {
        let (event_tx, _) = broadcast::channel(256);
        let (wait_tx, _) = broadcast::channel(256);
        Self {
            inner: Mutex::new(CommsInner::default()),
            stale_after_ms: stale_after.as_millis().min(u64::MAX as u128) as u64,
            event_tx,
            wait_tx,
        }
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<(String, DaemonEvent)> {
        self.event_tx.subscribe()
    }

    pub fn touch_peer(&self, session_id: &str, cwd: &Path, harness: HarnessKind) -> PeerSummary {
        let mut inner = self.inner.lock().unwrap();
        let now = now_ms();
        ensure_peer_locked(&mut inner, session_id, cwd, harness, now);
        inner.peers[session_id].summary(now, self.stale_after_ms)
    }

    pub fn announce(
        &self,
        session_id: &str,
        cwd: &Path,
        name: Option<&str>,
        topic: Option<&str>,
        tags: Option<Vec<String>>,
    ) -> Result<PeerSummary, String> {
        let mut inner = self.inner.lock().unwrap();
        let now = now_ms();
        ensure_peer_locked(&mut inner, session_id, cwd, infer_harness(session_id), now);

        if let Some(raw_name) = name {
            let slug = sanitize_slug(raw_name);
            if slug.is_empty() {
                return Err("Peer name must contain at least one letter or digit.".into());
            }
            let conflict = inner
                .peers
                .values()
                .any(|p| p.id != session_id && p.handle.eq_ignore_ascii_case(&slug));
            if conflict {
                return Err(format!(
                    "Peer handle '{slug}' is already in use by another active session."
                ));
            }
            let peer = inner.peers.get_mut(session_id).unwrap();
            peer.handle = slug;
            peer.explicit_handle = true;
        }
        let peer = inner.peers.get_mut(session_id).unwrap();
        if let Some(t) = topic {
            let trimmed = t.trim();
            peer.topic = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            };
        }
        if let Some(tag_list) = tags {
            peer.tags = tag_list
                .into_iter()
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect();
        }
        peer.last_seen_ms = now;
        Ok(peer.summary(now, self.stale_after_ms))
    }

    pub fn remove_peer(&self, session_id: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.peers.remove(session_id);
        for ch in inner.channels.values_mut() {
            ch.members.remove(session_id);
        }
    }

    pub fn peer_me(&self, session_id: &str, cwd: &Path) -> PeerSummary {
        self.touch_peer(session_id, cwd, infer_harness(session_id))
    }

    pub fn list_peers(
        &self,
        query: Option<&str>,
        tag: Option<&str>,
        include_stale: bool,
    ) -> Vec<PeerSummary> {
        let inner = self.inner.lock().unwrap();
        let now = now_ms();
        let q = query
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty());
        let tag_filter = tag
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty());

        let mut peers: Vec<PeerSummary> = inner
            .peers
            .values()
            .map(|p| p.summary(now, self.stale_after_ms))
            .filter(|p| include_stale || !p.stale)
            .filter(|p| {
                tag_filter
                    .as_ref()
                    .is_none_or(|tf| p.tags.iter().any(|t| t.to_lowercase() == *tf))
            })
            .filter(|p| {
                q.as_ref().is_none_or(|needle| {
                    p.handle.to_lowercase().contains(needle)
                        || p.id.to_lowercase().contains(needle)
                        || p.cwd.to_lowercase().contains(needle)
                        || p.topic
                            .as_deref()
                            .is_some_and(|t| t.to_lowercase().contains(needle))
                        || p.tags.iter().any(|t| t.to_lowercase().contains(needle))
                })
            })
            .collect();
        peers.sort_by(|a, b| a.handle.cmp(&b.handle).then_with(|| a.id.cmp(&b.id)));
        peers
    }

    pub fn acquire_push(self: &Arc<Self>, session_id: &str) -> PushGuard {
        let mut inner = self.inner.lock().unwrap();
        let now = now_ms();
        let peer = ensure_peer_locked(
            &mut inner,
            session_id,
            Path::new(""),
            infer_harness(session_id),
            now,
        );
        peer.push_count += 1;
        peer.last_seen_ms = now;
        PushGuard {
            hub: Arc::clone(self),
            session_id: session_id.to_string(),
        }
    }

    pub fn acquire_listen(self: &Arc<Self>, session_id: &str) -> ListenGuard {
        let mut inner = self.inner.lock().unwrap();
        let now = now_ms();
        let peer = ensure_peer_locked(
            &mut inner,
            session_id,
            Path::new(""),
            infer_harness(session_id),
            now,
        );
        peer.listen_count += 1;
        peer.last_seen_ms = now;
        ListenGuard {
            hub: Arc::clone(self),
            session_id: session_id.to_string(),
        }
    }

    pub fn acquire_active(self: &Arc<Self>, session_id: &str) -> ActiveGuard {
        let mut inner = self.inner.lock().unwrap();
        let now = now_ms();
        let peer = ensure_peer_locked(
            &mut inner,
            session_id,
            Path::new(""),
            infer_harness(session_id),
            now,
        );
        peer.active_count += 1;
        peer.last_seen_ms = now;
        ActiveGuard {
            hub: Arc::clone(self),
            session_id: session_id.to_string(),
        }
    }

    pub fn open_channel(
        &self,
        caller_id: &str,
        caller_cwd: &Path,
        opts: OpenChannelOpts,
    ) -> Result<OpenChannelResult, String> {
        let (result, events, wakeups) = {
            let mut inner = self.inner.lock().unwrap();
            let now = now_ms();
            ensure_peer_locked(
                &mut inner,
                caller_id,
                caller_cwd,
                infer_harness(caller_id),
                now,
            );

            let mut target_peer_ids = BTreeSet::new();
            target_peer_ids.insert(caller_id.to_string());
            if let Some(ref to) = opts.to {
                let resolved = resolve_peer_locked(&inner, to)?;
                target_peer_ids.insert(resolved);
            }
            for member in &opts.members {
                let resolved = resolve_peer_locked(&inner, member)?;
                target_peer_ids.insert(resolved);
            }

            let channel_id = if let Some(ref raw_name) = opts.name {
                let room_name = normalize_room_name(raw_name)?;
                if let Some(existing_id) = inner.room_by_name.get(&room_name).cloned() {
                    if let Some(ch) = inner.channels.get_mut(&existing_id) {
                        if !ch.closed {
                            for m in &target_peer_ids {
                                ch.members.insert(m.clone());
                            }
                            if let Some(ref topic) = opts.topic {
                                let trimmed = topic.trim();
                                if !trimmed.is_empty() && ch.topic.is_none() {
                                    ch.topic = Some(trimmed.to_string());
                                }
                            }
                            existing_id
                        } else {
                            create_channel_locked(
                                &mut inner,
                                ChannelKind::Room,
                                Some(room_name),
                                opts.topic.clone(),
                                target_peer_ids,
                            )
                        }
                    } else {
                        create_channel_locked(
                            &mut inner,
                            ChannelKind::Room,
                            Some(room_name),
                            opts.topic.clone(),
                            target_peer_ids,
                        )
                    }
                } else {
                    create_channel_locked(
                        &mut inner,
                        ChannelKind::Room,
                        Some(room_name),
                        opts.topic.clone(),
                        target_peer_ids,
                    )
                }
            } else {
                if opts.to.is_none() && opts.members.is_empty() {
                    return Err("channels/open requires #:name (for a room) or #:to / #:members (for a direct channel).".into());
                }
                create_channel_locked(
                    &mut inner,
                    ChannelKind::Direct,
                    None,
                    opts.topic.clone(),
                    target_peer_ids,
                )
            };

            let (message, deliveries, events, wakeups) = if let Some(body) = opts.message {
                let (msg, deliveries, events, wakeups) =
                    append_message_locked(&mut inner, caller_id, &channel_id, body, now)?;
                (Some(msg), deliveries, events, wakeups)
            } else {
                (None, Vec::new(), Vec::new(), Vec::new())
            };

            let summary = inner.channels[&channel_id].summary(Some(caller_id), &inner.peers);
            (
                OpenChannelResult {
                    channel: summary,
                    message,
                    deliveries,
                },
                events,
                wakeups,
            )
        };

        self.dispatch_outbound(events, wakeups);
        Ok(result)
    }

    pub fn list_channels(&self, caller_id: Option<&str>, include_all: bool) -> Vec<ChannelSummary> {
        let inner = self.inner.lock().unwrap();
        let mut out: Vec<ChannelSummary> = inner
            .channels
            .values()
            .filter(|ch| ch.can_access(caller_id) && (include_all || !ch.closed))
            .map(|ch| ch.summary(caller_id, &inner.peers))
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    pub fn join_channel(
        &self,
        caller_id: &str,
        caller_cwd: &Path,
        channel: &str,
    ) -> Result<ChannelSummary, String> {
        let mut inner = self.inner.lock().unwrap();
        let now = now_ms();
        ensure_peer_locked(
            &mut inner,
            caller_id,
            caller_cwd,
            infer_harness(caller_id),
            now,
        );
        let ch_id = resolve_channel_locked(&inner, channel)?;
        let ch = inner.channels.get_mut(&ch_id).unwrap();
        ch.ensure_access(Some(caller_id))?;
        if ch.closed {
            return Err(format!("Channel '{}' is closed.", ch.display_label()));
        }
        ch.members.insert(caller_id.to_string());
        Ok(inner.channels[&ch_id].summary(Some(caller_id), &inner.peers))
    }

    pub fn leave_channel(&self, caller_id: &str, channel: &str) -> Result<ChannelSummary, String> {
        let mut inner = self.inner.lock().unwrap();
        let ch_id = resolve_channel_locked(&inner, channel)?;
        let ch = inner.channels.get_mut(&ch_id).unwrap();
        ch.ensure_access(Some(caller_id))?;
        ch.members.remove(caller_id);
        Ok(inner.channels[&ch_id].summary(Some(caller_id), &inner.peers))
    }

    pub fn send_to_target(
        &self,
        caller_id: &str,
        caller_cwd: &Path,
        handle_override: Option<&str>,
        target: &str,
        body: String,
    ) -> Result<SendResult, String> {
        if let Some(h) = handle_override {
            let _ = self.announce(caller_id, caller_cwd, Some(h), None, None);
        }
        let trimmed = target.trim();
        let (result, events, wakeups) = {
            let mut inner = self.inner.lock().unwrap();
            let now = now_ms();
            ensure_peer_locked(
                &mut inner,
                caller_id,
                caller_cwd,
                infer_harness(caller_id),
                now,
            );
            // Prefer existing channel/room first; never mask channel errors by falling back to peers.
            let channel_id = if let Some(ch_id) = lookup_channel_locked(&inner, trimmed) {
                ch_id
            } else if trimmed.starts_with('#') {
                return Err(format!("Unknown channel '{trimmed}'."));
            } else {
                if body.trim().is_empty() {
                    return Err("Message body cannot be empty.".into());
                }
                let resolved_peer = resolve_peer_locked(&inner, trimmed)?;
                let mut members = BTreeSet::new();
                members.insert(caller_id.to_string());
                members.insert(resolved_peer);
                create_channel_locked(&mut inner, ChannelKind::Direct, None, None, members)
            };
            let (message, deliveries, events, wakeups) =
                append_message_locked(&mut inner, caller_id, &channel_id, body, now)?;
            (
                SendResult {
                    channel_id,
                    message,
                    deliveries,
                },
                events,
                wakeups,
            )
        };
        self.dispatch_outbound(events, wakeups);
        Ok(result)
    }

    pub fn send_message(
        &self,
        caller_id: &str,
        caller_cwd: &Path,
        channel: &str,
        body: String,
    ) -> Result<SendResult, String> {
        let (result, events, wakeups) = {
            let mut inner = self.inner.lock().unwrap();
            let now = now_ms();
            ensure_peer_locked(
                &mut inner,
                caller_id,
                caller_cwd,
                infer_harness(caller_id),
                now,
            );
            let ch_id = resolve_channel_locked(&inner, channel)?;
            let (message, deliveries, events, wakeups) =
                append_message_locked(&mut inner, caller_id, &ch_id, body, now)?;
            (
                SendResult {
                    channel_id: ch_id,
                    message,
                    deliveries,
                },
                events,
                wakeups,
            )
        };
        self.dispatch_outbound(events, wakeups);
        Ok(result)
    }

    pub fn read_channel(
        &self,
        caller_id: Option<&str>,
        channel: &str,
        unread_only: bool,
        limit: usize,
    ) -> Result<(ChannelSummary, Vec<ChannelMessage>), String> {
        self.read_channel_inner(caller_id, channel, unread_only, limit, ReadMode::Consume)
    }

    pub fn peek_channel(
        &self,
        caller_id: Option<&str>,
        channel: &str,
        limit: usize,
    ) -> Result<(ChannelSummary, Vec<ChannelMessage>), String> {
        self.read_channel_inner(caller_id, channel, false, limit, ReadMode::Peek)
    }

    fn read_channel_inner(
        &self,
        caller_id: Option<&str>,
        channel: &str,
        unread_only: bool,
        limit: usize,
        mode: ReadMode,
    ) -> Result<(ChannelSummary, Vec<ChannelMessage>), String> {
        let mut inner = self.inner.lock().unwrap();
        let ch_id = resolve_channel_locked(&inner, channel)?;
        let limit = limit.clamp(1, 500);

        let messages: Vec<ChannelMessage> = {
            let ch = inner.channels.get_mut(&ch_id).unwrap();
            ch.ensure_access(caller_id)?;
            let picked: Vec<ChannelMessage> = if unread_only {
                let cursor = caller_id
                    .and_then(|cid| ch.read_cursors.get(cid).copied())
                    .unwrap_or(0);
                ch.messages
                    .iter()
                    .filter(|m| m.seq > cursor && caller_id.is_none_or(|cid| m.from != cid))
                    .take(limit)
                    .cloned()
                    .collect()
            } else {
                let start = ch.messages.len().saturating_sub(limit);
                ch.messages[start..].to_vec()
            };
            if mode == ReadMode::Consume {
                if let (Some(cid), Some(last)) = (caller_id, picked.last()) {
                    let r = ch.read_cursors.entry(cid.to_string()).or_insert(0);
                    *r = (*r).max(last.seq);
                    let n = ch.notified_cursors.entry(cid.to_string()).or_insert(0);
                    *n = (*n).max(last.seq);
                }
            }
            picked
        };

        let summary = inner.channels[&ch_id].summary(caller_id, &inner.peers);
        Ok((summary, messages))
    }

    pub async fn wait_for_messages(
        self: &Arc<Self>,
        caller_id: &str,
        caller_cwd: &Path,
        channel: Option<&str>,
        timeout: Duration,
    ) -> Result<WaitResult, String> {
        let timeout = timeout.min(Duration::from_secs(MAX_WAIT_TIMEOUT_SECS));
        let _listen_guard = self.acquire_listen(caller_id);

        // Subscribe BEFORE checking unread messages to prevent missed-wakeup races.
        let mut rx = self.wait_tx.subscribe();

        let target_ch_id = {
            let mut inner = self.inner.lock().unwrap();
            let now = now_ms();
            ensure_peer_locked(
                &mut inner,
                caller_id,
                caller_cwd,
                infer_harness(caller_id),
                now,
            );
            match channel {
                Some(ch) => {
                    let ch_id = resolve_channel_locked(&inner, ch)?;
                    inner.channels[&ch_id].ensure_access(Some(caller_id))?;
                    Some(ch_id)
                }
                None => None,
            }
        };

        if let Some(msgs) = self.collect_unread_for_wait(caller_id, target_ch_id.as_deref()) {
            return Ok(WaitResult {
                timed_out: false,
                messages: msgs,
            });
        }

        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Ok((recipient_sid, ch_id))) => {
                    if recipient_sid != caller_id {
                        continue;
                    }
                    if target_ch_id
                        .as_deref()
                        .is_some_and(|want| want != ch_id.as_str())
                    {
                        continue;
                    }
                    if let Some(msgs) =
                        self.collect_unread_for_wait(caller_id, target_ch_id.as_deref())
                    {
                        return Ok(WaitResult {
                            timed_out: false,
                            messages: msgs,
                        });
                    }
                }
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => {
                    if let Some(msgs) =
                        self.collect_unread_for_wait(caller_id, target_ch_id.as_deref())
                    {
                        return Ok(WaitResult {
                            timed_out: false,
                            messages: msgs,
                        });
                    }
                }
                Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => {
                    return Ok(WaitResult {
                        timed_out: true,
                        messages: Vec::new(),
                    });
                }
            }
        }
    }

    pub fn close_channel(
        &self,
        caller_id: &str,
        channel: &str,
        summary: Option<String>,
    ) -> Result<ChannelSummary, String> {
        let mut inner = self.inner.lock().unwrap();
        let ch_id = resolve_channel_locked(&inner, channel)?;
        let ch = inner.channels.get_mut(&ch_id).unwrap();
        ch.ensure_access(Some(caller_id))?;
        ch.closed = true;
        if let Some(s) = summary {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                ch.summary = Some(trimmed.to_string());
            }
        }
        Ok(inner.channels[&ch_id].summary(Some(caller_id), &inner.peers))
    }

    pub fn prepare_shared_channels(
        &self,
        parent_id: &str,
        parent_cwd: &Path,
        arg: &ChannelArg,
        auto_room_name: &str,
    ) -> Result<Vec<String>, String> {
        let mut inner = self.inner.lock().unwrap();
        let now = now_ms();
        ensure_peer_locked(
            &mut inner,
            parent_id,
            parent_cwd,
            infer_harness(parent_id),
            now,
        );

        let specs: Vec<String> = match arg {
            ChannelArg::Auto => vec![auto_room_name.to_string()],
            ChannelArg::Refs(refs) => refs.clone(),
        };
        let mut ids = Vec::with_capacity(specs.len());
        for spec in specs {
            let ch_id = ensure_open_channel_locked(&mut inner, parent_id, &spec)?;
            if !ids.contains(&ch_id) {
                ids.push(ch_id);
            }
        }
        Ok(ids)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn attach_worker_to_channels(
        self: &Arc<Self>,
        parent_id: &str,
        parent_cwd: &Path,
        worker_id: &str,
        worker_handle_hint: &str,
        worker_cwd: &Path,
        arg: &ChannelArg,
        pre_opened_channels: Option<&[String]>,
    ) -> Result<(WorkerPeerGuard, WorkerChannelPlan), String> {
        let mut inner = self.inner.lock().unwrap();
        let now = now_ms();
        ensure_peer_locked(
            &mut inner,
            parent_id,
            parent_cwd,
            infer_harness(parent_id),
            now,
        );

        let base_handle = {
            let s = sanitize_slug(worker_handle_hint);
            if s.is_empty() {
                derive_base_handle(worker_id, worker_cwd)
            } else {
                s
            }
        };
        let peer_handle = disambiguate_handle(&inner.peers, &base_handle);

        // Resolve and authorize all channels before registering the worker peer so a failure
        // leaves no orphan worker peer behind.
        let channel_ids: Vec<String> = if let Some(ids) = pre_opened_channels {
            let mut out = Vec::with_capacity(ids.len());
            for id in ids {
                let ch_id = ensure_open_channel_locked(&mut inner, parent_id, id)?;
                if !out.contains(&ch_id) {
                    out.push(ch_id);
                }
            }
            out
        } else {
            match arg {
                ChannelArg::Auto => {
                    let mut members = BTreeSet::new();
                    members.insert(parent_id.to_string());
                    members.insert(worker_id.to_string());
                    let ch_id = create_channel_locked(
                        &mut inner,
                        ChannelKind::Direct,
                        None,
                        Some(format!("worker {peer_handle}")),
                        members,
                    );
                    vec![ch_id]
                }
                ChannelArg::Refs(refs) => {
                    let mut out = Vec::with_capacity(refs.len());
                    for r in refs {
                        let ch_id = ensure_open_channel_locked(&mut inner, parent_id, r)?;
                        if !out.contains(&ch_id) {
                            out.push(ch_id);
                        }
                    }
                    out
                }
            }
        };

        inner.peers.insert(
            worker_id.to_string(),
            PeerRecord {
                id: worker_id.to_string(),
                handle: peer_handle.clone(),
                explicit_handle: true,
                cwd: worker_cwd.to_path_buf(),
                harness: HarnessKind::Worker,
                topic: None,
                tags: Vec::new(),
                parent: Some(parent_id.to_string()),
                last_seen_ms: now,
                push_count: 0,
                listen_count: 0,
                active_count: 0,
            },
        );
        for ch_id in &channel_ids {
            if let Some(ch) = inner.channels.get_mut(ch_id) {
                ch.members.insert(worker_id.to_string());
            }
        }

        let mut lines = vec![
            "## Channels".to_string(),
            format!("You are registered as Cant peer `{peer_handle}` (`{worker_id}`)."),
            "Joined channels:".to_string(),
        ];
        for ch_id in &channel_ids {
            if let Some(ch) = inner.channels.get(ch_id) {
                let s = ch.summary(Some(worker_id), &inner.peers);
                let topic = s
                    .topic
                    .as_deref()
                    .map(|t| format!(" — topic: {t}"))
                    .unwrap_or_default();
                let name = s
                    .name
                    .as_deref()
                    .map(|n| format!(" (`#{n}`)"))
                    .unwrap_or_default();
                lines.push(format!(
                    "- `{}`{name} (members: {}){topic}",
                    s.id,
                    s.members.join(", ")
                ));
            }
        }
        let primary_channel_id = channel_ids.first().cloned().unwrap_or_default();
        lines.push(format!(
            "Use `(channels/send \"{primary_channel_id}\" \"...\")` to coordinate, `(channels/read \"{primary_channel_id}\")` for unread messages, and `(channels/wait \"{primary_channel_id}\" #:timeout 30)` to wait for a reply."
        ));

        let guard = WorkerPeerGuard {
            hub: Arc::clone(self),
            session_id: worker_id.to_string(),
        };
        Ok((
            guard,
            WorkerChannelPlan {
                channel_ids,
                primary_channel_id,
                peer_handle,
                prompt_block: lines.join("\n"),
            },
        ))
    }

    pub fn drain_notices(&self, session_id: &str) -> Vec<ChannelNotice> {
        let mut inner = self.inner.lock().unwrap();
        let mut notices = Vec::new();
        let peer_harnesses: HashMap<String, HarnessKind> = inner
            .peers
            .iter()
            .map(|(k, v)| (k.clone(), v.harness))
            .collect();

        for ch in inner.channels.values_mut() {
            if !ch.members.contains(session_id) {
                continue;
            }
            let cursor = ch.notified_cursors.get(session_id).copied().unwrap_or(0);
            let mut max_seq = cursor;
            for msg in ch.messages.iter().filter(|m| m.seq > cursor) {
                if msg.from == session_id {
                    max_seq = max_seq.max(msg.seq);
                    continue;
                }
                let harness = peer_harnesses
                    .get(&msg.from)
                    .copied()
                    .unwrap_or_else(|| infer_harness(&msg.from));
                notices.push(build_notice(ch, msg, harness));
                max_seq = max_seq.max(msg.seq);
            }
            if max_seq > cursor {
                ch.notified_cursors.insert(session_id.to_string(), max_seq);
            }
        }
        notices.sort_by_key(|n| (n.timestamp_ms, n.seq));
        notices
    }

    fn collect_unread_for_wait(
        &self,
        caller_id: &str,
        target_ch_id: Option<&str>,
    ) -> Option<Vec<ChannelMessage>> {
        let mut inner = self.inner.lock().unwrap();
        let mut collected = Vec::new();
        for ch in inner.channels.values_mut() {
            if let Some(want) = target_ch_id {
                if ch.id != want {
                    continue;
                }
            } else if !ch.members.contains(caller_id) {
                continue;
            }
            let cursor = ch.read_cursors.get(caller_id).copied().unwrap_or(0);
            let mut max_seq = cursor;
            for msg in ch.messages.iter().filter(|m| m.seq > cursor) {
                if msg.from != caller_id {
                    collected.push(msg.clone());
                }
                max_seq = max_seq.max(msg.seq);
            }
            if max_seq > cursor {
                ch.read_cursors.insert(caller_id.to_string(), max_seq);
                let n = ch
                    .notified_cursors
                    .entry(caller_id.to_string())
                    .or_insert(0);
                *n = (*n).max(max_seq);
            }
        }
        if collected.is_empty() {
            None
        } else {
            collected.sort_by_key(|m| (m.timestamp_ms, m.seq));
            Some(collected)
        }
    }

    fn dispatch_outbound(
        &self,
        events: Vec<(String, DaemonEvent)>,
        wakeups: Vec<(String, String)>,
    ) {
        for item in events {
            let _ = self.event_tx.send(item);
        }
        for item in wakeups {
            let _ = self.wait_tx.send(item);
        }
    }
}

fn create_channel_locked(
    inner: &mut CommsInner,
    kind: ChannelKind,
    name: Option<String>,
    topic: Option<String>,
    members: BTreeSet<String>,
) -> String {
    inner.next_channel_num += 1;
    let id = format!("ch-{}", inner.next_channel_num);
    if let Some(ref room) = name {
        inner.room_by_name.insert(room.clone(), id.clone());
    }
    let clean_topic = topic
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    inner.channels.insert(
        id.clone(),
        ChannelRecord {
            id: id.clone(),
            kind,
            name,
            topic: clean_topic,
            closed: false,
            summary: None,
            members,
            read_cursors: HashMap::new(),
            notified_cursors: HashMap::new(),
            messages: Vec::new(),
            next_seq: 0,
        },
    );
    id
}

type AppendedMessage = (
    ChannelMessage,
    Vec<DeliveryReport>,
    Vec<(String, DaemonEvent)>,
    Vec<(String, String)>,
);

fn append_message_locked(
    inner: &mut CommsInner,
    caller_id: &str,
    channel_id: &str,
    body: String,
    now: u64,
) -> Result<AppendedMessage, String> {
    if body.trim().is_empty() {
        return Err("Message body cannot be empty.".into());
    }
    let (from_handle, from_harness) = inner
        .peers
        .get(caller_id)
        .map(|p| (p.handle.clone(), p.harness))
        .unwrap_or_else(|| (caller_id.to_string(), infer_harness(caller_id)));

    let peer_snapshot: HashMap<String, (String, PeerReachability)> = inner
        .peers
        .iter()
        .map(|(k, p)| (k.clone(), (p.handle.clone(), p.reachability())))
        .collect();

    let ch = inner
        .channels
        .get_mut(channel_id)
        .ok_or_else(|| format!("Unknown channel '{channel_id}'."))?;
    ch.ensure_access(Some(caller_id))?;
    if ch.closed {
        return Err(format!("Channel '{}' is closed.", ch.display_label()));
    }
    ch.members.insert(caller_id.to_string());
    ch.next_seq += 1;
    let seq = ch.next_seq;
    let message = ChannelMessage {
        seq,
        channel_id: ch.id.clone(),
        from: caller_id.to_string(),
        from_handle,
        body,
        timestamp_ms: now,
    };
    ch.messages.push(message.clone());

    let notice = build_notice(ch, &message, from_harness);
    let mut deliveries = Vec::new();
    let mut events = Vec::new();
    let mut wakeups = Vec::new();

    for member_id in &ch.members {
        if member_id == caller_id {
            continue;
        }
        let (handle, reachability) = peer_snapshot
            .get(member_id)
            .cloned()
            .unwrap_or_else(|| (member_id.clone(), PeerReachability::Idle));

        let status = match reachability {
            PeerReachability::Listening => {
                wakeups.push((member_id.clone(), ch.id.clone()));
                DeliveryStatus::Woken
            }
            PeerReachability::Push => {
                events.push((
                    member_id.clone(),
                    DaemonEvent::ChannelMessage {
                        notice: notice.clone(),
                    },
                ));
                wakeups.push((member_id.clone(), ch.id.clone()));
                let n = ch.notified_cursors.entry(member_id.clone()).or_insert(0);
                *n = (*n).max(seq);
                DeliveryStatus::Pushed
            }
            PeerReachability::Active | PeerReachability::Idle => {
                wakeups.push((member_id.clone(), ch.id.clone()));
                DeliveryStatus::Queued
            }
        };
        deliveries.push(DeliveryReport {
            peer: member_id.clone(),
            handle,
            status,
        });
    }

    Ok((message, deliveries, events, wakeups))
}

fn ensure_peer_locked<'a>(
    inner: &'a mut CommsInner,
    session_id: &str,
    cwd: &Path,
    harness: HarnessKind,
    now: u64,
) -> &'a mut PeerRecord {
    let exists = inner.peers.contains_key(session_id);
    if !exists {
        let base = derive_base_handle(session_id, cwd);
        let handle = disambiguate_handle(&inner.peers, &base);
        inner.peers.insert(
            session_id.to_string(),
            PeerRecord {
                id: session_id.to_string(),
                handle,
                explicit_handle: false,
                cwd: cwd.to_path_buf(),
                harness,
                topic: None,
                tags: Vec::new(),
                parent: None,
                last_seen_ms: now,
                push_count: 0,
                listen_count: 0,
                active_count: 0,
            },
        );
    }
    let peer = inner.peers.get_mut(session_id).unwrap();
    peer.last_seen_ms = now;
    if !cwd.as_os_str().is_empty() && peer.cwd.as_os_str().is_empty() {
        peer.cwd = cwd.to_path_buf();
        if !peer.explicit_handle {
            let base = derive_base_handle(session_id, cwd);
            let taken_by_other = inner
                .peers
                .values()
                .any(|p| p.id != session_id && p.handle.eq_ignore_ascii_case(&base));
            if !taken_by_other {
                inner.peers.get_mut(session_id).unwrap().handle = base;
            }
        }
    }
    let peer = inner.peers.get_mut(session_id).unwrap();
    if peer.harness == HarnessKind::Unknown && harness != HarnessKind::Unknown {
        peer.harness = harness;
    }
    peer
}

fn resolve_peer_locked(inner: &CommsInner, target: &str) -> Result<String, String> {
    let trimmed = target.trim();
    if inner.peers.contains_key(trimmed) {
        return Ok(trimmed.to_string());
    }
    let matches: Vec<&PeerRecord> = inner
        .peers
        .values()
        .filter(|p| p.handle.eq_ignore_ascii_case(trimmed))
        .collect();
    match matches.as_slice() {
        [only] => Ok(only.id.clone()),
        [] => {
            let mut handles: Vec<&str> = inner.peers.values().map(|p| p.handle.as_str()).collect();
            handles.sort();
            if handles.is_empty() {
                Err(format!("Unknown peer '{trimmed}' (no peers registered)."))
            } else {
                Err(format!(
                    "Unknown peer '{trimmed}'. Known peers: {}",
                    handles.join(", ")
                ))
            }
        }
        many => {
            let ids: Vec<&str> = many.iter().map(|p| p.id.as_str()).collect();
            Err(format!(
                "Ambiguous peer handle '{trimmed}' matches: {}",
                ids.join(", ")
            ))
        }
    }
}

fn ensure_open_channel_locked(
    inner: &mut CommsInner,
    member_id: &str,
    spec: &str,
) -> Result<String, String> {
    let trimmed = spec.trim();
    if let Some(existing_id) = lookup_channel_locked(inner, trimmed) {
        let ch = inner.channels.get_mut(&existing_id).unwrap();
        ch.ensure_access(Some(member_id))?;
        if !ch.closed {
            ch.members.insert(member_id.to_string());
            return Ok(existing_id);
        }
        if ch.kind == ChannelKind::Direct || trimmed == existing_id {
            return Err(format!("Channel '{}' is closed.", ch.display_label()));
        }
    }
    let room = normalize_room_name(trimmed)?;
    let mut members = BTreeSet::new();
    members.insert(member_id.to_string());
    Ok(create_channel_locked(
        inner,
        ChannelKind::Room,
        Some(room),
        None,
        members,
    ))
}

fn lookup_channel_locked(inner: &CommsInner, channel: &str) -> Option<String> {
    let trimmed = channel.trim();
    if inner.channels.contains_key(trimmed) {
        return Some(trimmed.to_string());
    }
    if let Ok(room) = normalize_room_name(trimmed) {
        if let Some(id) = inner.room_by_name.get(&room) {
            if inner.channels.contains_key(id) {
                return Some(id.clone());
            }
        }
    }
    None
}

fn resolve_channel_locked(inner: &CommsInner, channel: &str) -> Result<String, String> {
    let trimmed = channel.trim();
    lookup_channel_locked(inner, trimmed).ok_or_else(|| format!("Unknown channel '{trimmed}'."))
}

fn normalize_room_name(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim().trim_start_matches('#');
    let slug = sanitize_slug(trimmed);
    if slug.is_empty() {
        return Err("Channel room name must contain at least one letter or digit.".into());
    }
    Ok(slug)
}

fn derive_base_handle(session_id: &str, cwd: &Path) -> String {
    if let Some(name) = cwd.file_name().and_then(|s| s.to_str()) {
        let slug = sanitize_slug(name);
        if !slug.is_empty() {
            return slug;
        }
    }
    let fallback = sanitize_slug(session_id);
    if fallback.is_empty() {
        "peer".to_string()
    } else {
        fallback.chars().take(16).collect()
    }
}

fn disambiguate_handle(peers: &HashMap<String, PeerRecord>, base: &str) -> String {
    if !peers.values().any(|p| p.handle.eq_ignore_ascii_case(base)) {
        return base.to_string();
    }
    for n in 2..=999 {
        let candidate = format!("{base}-{n}");
        if !peers
            .values()
            .any(|p| p.handle.eq_ignore_ascii_case(&candidate))
        {
            return candidate;
        }
    }
    format!("{base}-{}", now_ms() % 10_000)
}

fn sanitize_slug(raw: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in raw.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if matches!(ch, '-' | '_' | ' ' | '.' | '/') && !out.is_empty() && !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

pub fn infer_harness_with_driver(session_id: &str, worker_driver: &str) -> HarnessKind {
    let by_id = infer_harness(session_id);
    if by_id != HarnessKind::Unknown {
        return by_id;
    }
    match worker_driver.trim() {
        "agy" => HarnessKind::Antigravity,
        "codex" => HarnessKind::Codex,
        "pi" => HarnessKind::Pi,
        _ => HarnessKind::Unknown,
    }
}

pub fn infer_harness(session_id: &str) -> HarnessKind {
    if session_id.starts_with("pi-worker-") || session_id.starts_with("worker-") {
        HarnessKind::Worker
    } else if session_id.starts_with("pi-") {
        HarnessKind::Pi
    } else if session_id == "cli" || session_id.starts_with("cli-") {
        HarnessKind::Cli
    } else if session_id.starts_with("codex-") {
        HarnessKind::Codex
    } else {
        HarnessKind::Unknown
    }
}

fn build_notice(
    ch: &ChannelRecord,
    msg: &ChannelMessage,
    from_harness: HarnessKind,
) -> ChannelNotice {
    let (body, truncated) = truncate_utf8(&msg.body, MAX_NOTICE_BODY_BYTES);
    ChannelNotice {
        channel_id: ch.id.clone(),
        channel_name: ch.name.clone(),
        topic: ch.topic.clone(),
        seq: msg.seq,
        from: msg.from.clone(),
        from_handle: msg.from_handle.clone(),
        from_harness,
        body,
        truncated,
        timestamp_ms: msg.timestamp_ms,
    }
}

fn truncate_utf8(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_string(), false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (format!("{}…", &text[..end]), true)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn register_comms_capabilities(catalog: &crate::capability::CapabilityCatalog) {
    capabilities::register(catalog);
}
