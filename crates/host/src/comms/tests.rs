use super::*;
use crate::capability::{CallContext, CapabilityCatalog, FileCoverageLedger};
use crate::jobs::driver::WorkerDriverRegistry;
use crate::jobs::JobManager;
use crate::language::{GuestLanguage, LanguageIdentity};
use crate::runner_process::RunnerCommand;
use cant_protocol::daemon::{ChannelKind, DaemonEvent, HarnessKind, PeerReachability};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

struct TestLang;
impl GuestLanguage for TestLang {
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
        Err("unused".into())
    }
    fn project_bindings(&self, _: &[crate::capability::CapabilityMeta]) -> Value {
        json!([])
    }
    fn binding_name(
        &self,
        _: &[crate::capability::CapabilityMeta],
        m: &crate::capability::CapabilityMeta,
    ) -> String {
        m.api_path.join("/")
    }
    fn signature(
        &self,
        _: &[crate::capability::CapabilityMeta],
        m: &crate::capability::CapabilityMeta,
    ) -> String {
        m.api_path.join("/")
    }
    fn tool_description(&self, _: &[crate::capability::CapabilityMeta]) -> String {
        String::new()
    }
    fn source_description(&self) -> &'static str { "" }
    fn inputs_description(&self) -> &'static str { "" }
    fn format_metadata(&self, value: &Value) -> String { value.to_string() }
    fn post_describe(&self, _: &str, value: Value, _: &[crate::capability::CapabilityMeta]) -> Value { value }
}

fn test_ctx(comms: Arc<CommsHub>, session_id: &str, cwd: &str) -> CallContext {
    let catalog = Arc::new(CapabilityCatalog::new());
    register_comms_capabilities(&catalog);
    CallContext {
        cwd: PathBuf::from(cwd),
        session_id: session_id.into(),
        language_id: "steel".into(),
        worker_driver: "agy".into(),
        disabled_namespaces: Vec::new(),
        file_coverage: Arc::new(FileCoverageLedger::default()),
        catalog,
        extra_capabilities: Vec::new(),
        language: Arc::new(TestLang),
        jobs: Arc::new(JobManager::new(Arc::new(WorkerDriverRegistry::new()))),
        comms,
    }
}

#[test]
fn auto_handles_disambiguate_same_repo_and_announce_updates_metadata() {
    let hub = Arc::new(CommsHub::new());
    let a = hub.touch_peer("sess-a", Path::new("/code/frontend"), HarnessKind::Antigravity);
    let b = hub.touch_peer("sess-b", Path::new("/code/frontend"), HarnessKind::Pi);
    assert_eq!(a.handle, "frontend");
    assert_eq!(b.handle, "frontend-2");

    let updated = hub
        .announce(
            "sess-b",
            Path::new("/code/frontend"),
            Some("ui-reviewer"),
            Some("reviewing login form"),
            Some(vec!["ui".into(), "auth".into()]),
        )
        .unwrap();
    assert_eq!(updated.handle, "ui-reviewer");
    assert_eq!(updated.topic.as_deref(), Some("reviewing login form"));
    assert_eq!(updated.tags, vec!["ui", "auth"]);

    // Claiming another live peer's handle fails cleanly
    let conflict = hub.announce("sess-a", Path::new("/code/frontend"), Some("ui-reviewer"), None, None);
    assert!(conflict.is_err());

    // Filtering by query and tag works
    let by_tag = hub.list_peers(None, Some("auth"), false);
    assert_eq!(by_tag.len(), 1);
    assert_eq!(by_tag[0].id, "sess-b");

    let by_query = hub.list_peers(Some("login"), None, false);
    assert_eq!(by_query.len(), 1);
    assert_eq!(by_query[0].id, "sess-b");
}

#[test]
fn reachability_raii_guards_enforce_precedence_and_cleanup() {
    let hub = Arc::new(CommsHub::new());
    let p = hub.touch_peer("sess-1", Path::new("/repo/cant"), HarnessKind::Antigravity);
    assert_eq!(p.reachability, PeerReachability::Idle);

    {
        let _active = hub.acquire_active("sess-1");
        assert_eq!(
            hub.peer_me("sess-1", Path::new("/repo/cant")).reachability,
            PeerReachability::Active
        );
        {
            let _listen = hub.acquire_listen("sess-1");
            assert_eq!(
                hub.peer_me("sess-1", Path::new("/repo/cant")).reachability,
                PeerReachability::Listening
            );
            {
                let _push = hub.acquire_push("sess-1");
                assert_eq!(
                    hub.peer_me("sess-1", Path::new("/repo/cant")).reachability,
                    PeerReachability::Push
                );
            }
            assert_eq!(
                hub.peer_me("sess-1", Path::new("/repo/cant")).reachability,
                PeerReachability::Listening
            );
        }
        assert_eq!(
            hub.peer_me("sess-1", Path::new("/repo/cant")).reachability,
            PeerReachability::Active
        );
    }
    assert_eq!(
        hub.peer_me("sess-1", Path::new("/repo/cant")).reachability,
        PeerReachability::Idle
    );
}

#[test]
fn stale_peers_are_hidden_by_default_unless_push_or_include_stale() {
    let hub = Arc::new(CommsHub::with_stale_after(Duration::from_millis(10)));
    hub.touch_peer("idle-agy", Path::new("/repo/a"), HarnessKind::Antigravity);
    hub.touch_peer("live-pi", Path::new("/repo/b"), HarnessKind::Pi);
    let _push = hub.acquire_push("live-pi");

    std::thread::sleep(Duration::from_millis(25));

    let visible = hub.list_peers(None, None, false);
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].id, "live-pi");
    assert!(!visible[0].stale);

    let all = hub.list_peers(None, None, true);
    assert_eq!(all.len(), 2);
    let agy = all.iter().find(|p| p.id == "idle-agy").unwrap();
    assert!(agy.stale);
}

#[test]
fn direct_channel_and_named_room_with_dual_watermarks() {
    let hub = Arc::new(CommsHub::new());
    hub.touch_peer("backend", Path::new("/code/backend"), HarnessKind::Antigravity);
    hub.touch_peer("frontend", Path::new("/code/frontend"), HarnessKind::Antigravity);

    // Open direct channel with initial message
    let opened = hub
        .open_channel(
            "backend",
            Path::new("/code/backend"),
            OpenChannelOpts {
                to: Some("frontend".into()),
                topic: Some("login payload".into()),
                message: Some("Does /login expect access_token?".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(opened.channel.kind, ChannelKind::Direct);
    assert_eq!(opened.channel.member_ids.len(), 2);
    assert_eq!(opened.deliveries.len(), 1);
    assert_eq!(opened.deliveries[0].status, DeliveryStatus::Queued);

    // Sender never gets notified of their own message
    assert!(hub.drain_notices("backend").is_empty());

    // Recipient drains notice once, not twice
    let notices = hub.drain_notices("frontend");
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].body, "Does /login expect access_token?");
    assert!(hub.drain_notices("frontend").is_empty());

    // Even after notice drain, explicit unread read still sees the message until read
    let (_, unread) = hub
        .read_channel(Some("frontend"), &opened.channel.id, true, 50)
        .unwrap();
    assert_eq!(unread.len(), 1);
    let (_, unread_after) = hub
        .read_channel(Some("frontend"), &opened.channel.id, true, 50)
        .unwrap();
    assert!(unread_after.is_empty());

    // Reading before draining also advances notified_seq so hooks don't double-deliver!
    hub.send_message(
        "frontend",
        Path::new("/code/frontend"),
        &opened.channel.id,
        "Yes, access_token.".into(),
    )
    .unwrap();
    let (_, got) = hub
        .read_channel(Some("backend"), &opened.channel.id, true, 50)
        .unwrap();
    assert_eq!(got.len(), 1);
    assert!(hub.drain_notices("backend").is_empty());

    // Named room is idempotent on open and discoverable by name with or without '#'
    let r1 = hub
        .open_channel(
            "backend",
            Path::new("/code/backend"),
            OpenChannelOpts {
                name: Some("#api-v2".into()),
                topic: Some("v2 migration".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let r2 = hub
        .open_channel(
            "frontend",
            Path::new("/code/frontend"),
            OpenChannelOpts {
                name: Some("api-v2".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(r1.channel.id, r2.channel.id);
    assert_eq!(r2.channel.member_ids.len(), 2);

    // Closing prevents further sends
    let closed = hub
        .close_channel("backend", "#api-v2", Some("Done".into()))
        .unwrap();
    assert!(closed.closed);
    assert_eq!(closed.summary.as_deref(), Some("Done"));
    assert!(hub
        .send_message("frontend", Path::new("/code/frontend"), "#api-v2", "late".into())
        .is_err());
}

#[tokio::test]
async fn push_subscriber_receives_channel_event_and_waiter_wakes_immediately() {
    let hub = Arc::new(CommsHub::new());
    hub.touch_peer("pi-peer", Path::new("/code/pi"), HarnessKind::Pi);
    hub.touch_peer("agy-peer", Path::new("/code/agy"), HarnessKind::Antigravity);

    let _push_guard = hub.acquire_push("pi-peer");
    let mut events = hub.subscribe_events();

    let opened = hub
        .open_channel(
            "agy-peer",
            Path::new("/code/agy"),
            OpenChannelOpts {
                to: Some("pi".into()),
                message: Some("ping pi".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(opened.deliveries[0].status, DeliveryStatus::Pushed);

    let (target_sid, ev) = events.recv().await.unwrap();
    assert_eq!(target_sid, "pi-peer");
    match ev {
        DaemonEvent::ChannelMessage { notice } => {
            assert_eq!(notice.body, "ping pi");
            assert_eq!(notice.channel_id, opened.channel.id);
        }
        other => panic!("expected ChannelMessage, got {other:?}"),
    }

    // Now agy-peer waits for reply while pi-peer sends it asynchronously
    let hub_clone = Arc::clone(&hub);
    let ch_id = opened.channel.id.clone();
    let waiter = tokio::spawn(async move {
        hub_clone
            .wait_for_messages(
                "agy-peer",
                Path::new("/code/agy"),
                Some(&ch_id),
                Duration::from_secs(5),
            )
            .await
            .unwrap()
    });

    // Give waiter a moment to enter listening state
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert_eq!(
        hub.peer_me("agy-peer", Path::new("/code/agy")).reachability,
        PeerReachability::Listening
    );

    let send_res = hub
        .send_message("pi-peer", Path::new("/code/pi"), &opened.channel.id, "pong agy".into())
        .unwrap();
    assert_eq!(send_res.deliveries[0].status, DeliveryStatus::Woken);

    let waited = waiter.await.unwrap();
    assert!(!waited.timed_out);
    assert_eq!(waited.messages.len(), 1);
    assert_eq!(waited.messages[0].body, "pong agy");
    // Wait advanced notified_seq so Stop hook won't duplicate it
    assert!(hub.drain_notices("agy-peer").is_empty());
}

#[tokio::test]
async fn capabilities_round_trip_peers_and_channels() {
    let hub = Arc::new(CommsHub::new());
    let ctx_a = test_ctx(Arc::clone(&hub), "sess-a", "/code/alpha");
    let ctx_b = test_ctx(Arc::clone(&hub), "sess-b", "/code/beta");

    let announce = ctx_a.catalog.get_by_api_path(&["peers".into(), "announce".into()]).expect("peers/announce registered");
    let me_a = announce
        .call(&ctx_a, json!({ "name": "alpha", "topic": "api" }))
        .await
        .unwrap();
    assert_eq!(me_a["handle"], "alpha");

    let peers_me = ctx_b.catalog.get_by_api_path(&["peers".into(), "me".into()]).expect("peers/me registered");
    let me_b = peers_me.call(&ctx_b, json!({})).await.unwrap();
    assert_eq!(me_b["handle"], "beta");

    let peers_list = ctx_a.catalog.get_by_api_path(&["peers".into(), "list".into()]).expect("peers/list registered");
    let listed = peers_list.call(&ctx_a, json!({})).await.unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 2);

    let open = ctx_a.catalog.get_by_api_path(&["channels".into(), "open".into()]).expect("channels/open registered");
    let opened = open
        .call(&ctx_a, json!({ "to": "beta", "message": "hello beta" }))
        .await
        .unwrap();
    let ch_id = opened["channel"]["id"].as_str().unwrap().to_string();

    let read = ctx_b.catalog.get_by_api_path(&["channels".into(), "read".into()]).expect("channels/read registered");
    let got = read.call(&ctx_b, json!({ "channel": ch_id })).await.unwrap();
    assert_eq!(got["messages"].as_array().unwrap().len(), 1);
    assert_eq!(got["messages"][0]["body"], "hello beta");
}

#[test]
fn sending_before_reading_preserves_unread_messages_and_queued_notices() {
    let hub = Arc::new(CommsHub::new());
    hub.touch_peer("alice", Path::new("/code/alice"), HarnessKind::Antigravity);
    hub.touch_peer("bob", Path::new("/code/bob"), HarnessKind::Antigravity);

    let opened = hub
        .open_channel(
            "alice",
            Path::new("/code/alice"),
            OpenChannelOpts {
                to: Some("bob".into()),
                message: Some("first from alice".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let ch_id = opened.channel.id;

    // Bob replies before reading or draining notices, then Alice sends another message.
    hub.send_message("bob", Path::new("/code/bob"), &ch_id, "bob reply".into())
        .unwrap();
    hub.send_message(
        "alice",
        Path::new("/code/alice"),
        &ch_id,
        "second from alice".into(),
    )
    .unwrap();

    // Unread count reflects both of Alice's messages and excludes Bob's own reply.
    let bob_summary = hub
        .list_channels(Some("bob"), false)
        .into_iter()
        .find(|c| c.id == ch_id)
        .unwrap();
    assert_eq!(bob_summary.unread, 2);

    // Queued notices still include the message that arrived before Bob's reply.
    let notices = hub.drain_notices("bob");
    let notice_bodies: Vec<&str> = notices.iter().map(|n| n.body.as_str()).collect();
    assert_eq!(notice_bodies, vec!["first from alice", "second from alice"]);
    assert!(hub.drain_notices("bob").is_empty());

    // Unread paging skips Bob's own message before applying limit=1.
    let (s1, page1) = hub.read_channel(Some("bob"), &ch_id, true, 1).unwrap();
    assert_eq!(
        page1.iter().map(|m| m.body.as_str()).collect::<Vec<_>>(),
        vec!["first from alice"]
    );
    assert_eq!(s1.unread, 1);

    let (s2, page2) = hub.read_channel(Some("bob"), &ch_id, true, 1).unwrap();
    assert_eq!(
        page2.iter().map(|m| m.body.as_str()).collect::<Vec<_>>(),
        vec!["second from alice"]
    );
    assert_eq!(s2.unread, 0);

    let (_, empty) = hub.read_channel(Some("bob"), &ch_id, true, 50).unwrap();
    assert!(empty.is_empty());

    // Full history still contains both sides in order.
    let (_, history) = hub.read_channel(Some("bob"), &ch_id, false, 50).unwrap();
    assert_eq!(
        history.iter().map(|m| m.body.as_str()).collect::<Vec<_>>(),
        vec!["first from alice", "bob reply", "second from alice"]
    );

    // Reading an incoming message after replying still suppresses its queued notice.
    hub.send_message(
        "alice",
        Path::new("/code/alice"),
        &ch_id,
        "third from alice".into(),
    )
    .unwrap();
    hub.send_message(
        "bob",
        Path::new("/code/bob"),
        &ch_id,
        "bob second reply".into(),
    )
    .unwrap();
    let (s3, page3) = hub.read_channel(Some("bob"), &ch_id, true, 50).unwrap();
    assert_eq!(
        page3.iter().map(|m| m.body.as_str()).collect::<Vec<_>>(),
        vec!["third from alice"]
    );
    assert_eq!(s3.unread, 0);
    assert!(hub.drain_notices("bob").is_empty());
}

#[test]
fn reopened_room_moves_alias_while_preserving_closed_history_by_id() {
    let hub = Arc::new(CommsHub::new());
    hub.touch_peer("alice", Path::new("/code/alice"), HarnessKind::Antigravity);
    hub.touch_peer("bob", Path::new("/code/bob"), HarnessKind::Antigravity);

    let r1 = hub
        .open_channel(
            "alice",
            Path::new("/code/alice"),
            OpenChannelOpts {
                name: Some("#api-v2".into()),
                message: Some("v1 message".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(r1.channel.id.starts_with("ch-"));
    assert_eq!(r1.channel.name.as_deref(), Some("api-v2"));

    // Idempotent open while still active reuses the same ID.
    let r1_again = hub
        .open_channel(
            "bob",
            Path::new("/code/bob"),
            OpenChannelOpts {
                name: Some("api-v2".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(r1_again.channel.id, r1.channel.id);

    hub.close_channel("alice", "#api-v2", Some("v1 done".into()))
        .unwrap();

    // Reopening creates a fresh channel ID and repoints the #api-v2 alias.
    let r2 = hub
        .open_channel(
            "alice",
            Path::new("/code/alice"),
            OpenChannelOpts {
                name: Some("api-v2".into()),
                message: Some("v2 message".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(r2.channel.id.starts_with("ch-"));
    assert_ne!(r2.channel.id, r1.channel.id);

    let (alias_summary, alias_msgs) = hub.read_channel(Some("bob"), "#api-v2", false, 50).unwrap();
    assert_eq!(alias_summary.id, r2.channel.id);
    assert!(!alias_summary.closed);
    assert_eq!(
        alias_msgs.iter().map(|m| m.body.as_str()).collect::<Vec<_>>(),
        vec!["v2 message"]
    );

    // Closed room history remains addressable by its original stable ID.
    let (old_summary, old_msgs) = hub
        .read_channel(Some("bob"), &r1.channel.id, false, 50)
        .unwrap();
    assert_eq!(old_summary.id, r1.channel.id);
    assert!(old_summary.closed);
    assert_eq!(
        old_msgs.iter().map(|m| m.body.as_str()).collect::<Vec<_>>(),
        vec!["v1 message"]
    );

    let err = hub
        .send_message("bob", Path::new("/code/bob"), &r1.channel.id, "late".into())
        .unwrap_err();
    assert!(err.contains("is closed"), "unexpected error: {err}");
}

#[test]
fn send_to_target_reports_closed_room_and_unknown_hash_room_without_peer_fallback() {
    let hub = Arc::new(CommsHub::new());
    hub.touch_peer("alice", Path::new("/code/alice"), HarnessKind::Antigravity);

    let opened = hub
        .open_channel(
            "alice",
            Path::new("/code/alice"),
            OpenChannelOpts {
                name: Some("ops".into()),
                ..Default::default()
            },
        )
        .unwrap();
    hub.close_channel("alice", "#ops", None).unwrap();

    // Closed room by #alias must report closed, not fall back to peer lookup.
    let err_alias = hub
        .send_to_target(
            "cli",
            Path::new("/code/cli"),
            Some("human"),
            "#ops",
            "late".into(),
        )
        .unwrap_err();
    assert!(err_alias.contains("is closed"), "got: {err_alias}");

    // Closed room by ID must also report closed.
    let err_id = hub
        .send_to_target(
            "cli",
            Path::new("/code/cli"),
            Some("human"),
            &opened.channel.id,
            "late".into(),
        )
        .unwrap_err();
    assert!(err_id.contains("is closed"), "got: {err_id}");

    // Explicit #room that does not exist must report Unknown channel, not Unknown peer.
    let err_hash = hub
        .send_to_target(
            "cli",
            Path::new("/code/cli"),
            Some("human"),
            "#does-not-exist",
            "hello".into(),
        )
        .unwrap_err();
    assert!(err_hash.contains("Unknown channel"), "got: {err_hash}");

    // Plain target matching a peer still opens a direct channel.
    let sent = hub
        .send_to_target(
            "cli",
            Path::new("/code/cli"),
            Some("human"),
            "alice",
            "hi alice".into(),
        )
        .unwrap();
    assert!(sent.channel_id.starts_with("ch-"));
    assert_eq!(sent.message.body, "hi alice");

    // Plain target matching neither channel nor peer reports Unknown peer.
    let err_peer = hub
        .send_to_target(
            "cli",
            Path::new("/code/cli"),
            Some("human"),
            "ghost-peer",
            "hello".into(),
        )
        .unwrap_err();
    assert!(err_peer.contains("Unknown peer"), "got: {err_peer}");
}

#[tokio::test]
async fn direct_threads_enforce_membership_across_all_operations() {
    let hub = Arc::new(CommsHub::new());
    hub.touch_peer("alice", Path::new("/code/alice"), HarnessKind::Antigravity);
    hub.touch_peer("bob", Path::new("/code/bob"), HarnessKind::Antigravity);
    hub.touch_peer("charlie", Path::new("/code/charlie"), HarnessKind::Antigravity);

    let dm = hub
        .open_channel(
            "alice",
            Path::new("/code/alice"),
            OpenChannelOpts {
                to: Some("bob".into()),
                message: Some("secret dm".into()),
                ..Default::default()
            },
        )
        .unwrap()
        .channel
        .id;
    let room = hub
        .open_channel(
            "alice",
            Path::new("/code/alice"),
            OpenChannelOpts {
                name: Some("public".into()),
                ..Default::default()
            },
        )
        .unwrap()
        .channel
        .id;

    // Nonmember and sessionless callers only see named rooms, even with include_all=true.
    for viewer in [Some("charlie"), None] {
        for include_all in [false, true] {
            let ids: Vec<String> = hub
                .list_channels(viewer, include_all)
                .into_iter()
                .map(|c| c.id)
                .collect();
            assert!(ids.contains(&room), "viewer={viewer:?} should see room");
            assert!(
                !ids.contains(&dm),
                "viewer={viewer:?} include_all={include_all} must not see DM"
            );
        }
    }

    // Nonmember and sessionless callers cannot read, join, leave, send, wait, close, or attach workers to a DM.
    assert!(hub.read_channel(Some("charlie"), &dm, false, 50).is_err());
    assert!(hub.read_channel(None, &dm, false, 50).is_err());
    assert!(hub.join_channel("charlie", Path::new("/code/charlie"), &dm).is_err());
    assert!(hub.leave_channel("charlie", &dm).is_err());
    assert!(hub
        .send_message("charlie", Path::new("/code/charlie"), &dm, "intrude".into())
        .is_err());
    let say_err = hub
        .send_to_target(
            "charlie",
            Path::new("/code/charlie"),
            None,
            &dm,
            "intrude".into(),
        )
        .unwrap_err();
    assert!(
        !say_err.contains("Unknown peer"),
        "direct thread denial must not fall back to peer lookup: {say_err}"
    );
    assert!(hub
        .wait_for_messages(
            "charlie",
            Path::new("/code/charlie"),
            Some(&dm),
            Duration::from_millis(20),
        )
        .await
        .is_err());
    assert!(hub.close_channel("charlie", &dm, None).is_err());

    let refs = ChannelArg::Refs(vec![dm.clone()]);
    assert!(hub
        .prepare_shared_channels("charlie", Path::new("/code/charlie"), &refs, "auto")
        .is_err());
    assert!(hub
        .attach_worker_to_channels(
            "charlie",
            Path::new("/code/charlie"),
            "worker-snoop",
            "snoop",
            Path::new("/code/charlie"),
            &refs,
            None,
        )
        .is_err());
    // Failed worker attachment must not leak a zombie peer record.
    assert!(hub
        .list_peers(None, None, true)
        .iter()
        .all(|p| p.id != "worker-snoop"));

    // Original participants can still list, read, reply, and close the DM.
    assert!(hub.list_channels(Some("alice"), false).iter().any(|c| c.id == dm));
    assert!(hub.list_channels(Some("bob"), false).iter().any(|c| c.id == dm));
    let (_, msgs) = hub.read_channel(Some("bob"), &dm, true, 50).unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].body, "secret dm");

    hub.send_message("bob", Path::new("/code/bob"), &dm, "reply".into())
        .unwrap();
    let (_, alice_msgs) = hub.read_channel(Some("alice"), &dm, true, 50).unwrap();
    assert_eq!(alice_msgs.len(), 1);
    assert_eq!(alice_msgs[0].body, "reply");

    hub.close_channel("alice", &dm, Some("done".into())).unwrap();
    assert!(hub.list_channels(Some("alice"), true).iter().any(|c| c.id == dm));
    assert!(!hub.list_channels(Some("charlie"), true).iter().any(|c| c.id == dm));
    // Re-attaching a closed DM by ID must fail instead of creating a room named #ch-N.
    assert!(hub
        .prepare_shared_channels("alice", Path::new("/code/alice"), &refs, "auto")
        .is_err());
}
