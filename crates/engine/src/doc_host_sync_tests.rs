//! Exercise the dispatcher against slow but live loopback transports.
use super::*;
use futures::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zeron_sync::chat_frames::{decode, encode, frame_type};

// These scenarios exercise the production process-wide budget. Keep the
// complete scenario, including transport shutdown, inside the same guard.
static SYNC_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Clone, Copy)]
enum Hold {
    Checkpoint,
    Backfill,
}

struct Relay {
    url: String,
    release: watch::Sender<bool>,
    joins: Arc<AtomicUsize>,
    joins_by_chat: Arc<Mutex<HashMap<String, usize>>>,
    rows_requests: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn relay(hold: Hold) -> Relay {
    let remote = loro::LoroDoc::new();
    remote
        .get_text("slow-checkpoint")
        .insert(0, "received")
        .unwrap();
    remote.commit();
    let checkpoint = remote.export(loro::ExportMode::Snapshot).unwrap();
    let frontier = remote.oplog_vv().encode();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (release, gate) = watch::channel(false);
    let joins = Arc::new(AtomicUsize::new(0));
    let rows_requests = Arc::new(AtomicUsize::new(0));
    let joined = joins.clone();
    let joins_by_chat = Arc::new(Mutex::new(HashMap::new()));
    let joined_by_chat = joins_by_chat.clone();
    let requested = rows_requests.clone();
    let task = tokio::spawn(async move {
        let mut peers = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (mut stream, _) = accepted.unwrap();
                    let checkpoint = checkpoint.clone();
                    let frontier = frontier.clone();
                    let joined = joined.clone();
                    let joined_by_chat = joined_by_chat.clone();
                    let requested = requested.clone();
                    let mut gate = gate.clone();
                    peers.spawn(async move {
                        // Inspect without consuming the websocket upgrade.
                        let mut buffer = [0; 8192];
                        let (headers, length) = loop {
                            let n = stream.peek(&mut buffer).await.unwrap_or(0);
                            if n == 0 { return }
                            if let Some(end) = buffer[..n].windows(4).position(|v| v == b"\r\n\r\n") {
                                break (String::from_utf8_lossy(&buffer[..end]).to_ascii_lowercase(), end + 4);
                            }
                            assert!(n < buffer.len(), "oversized test request");
                            tokio::time::sleep(Duration::from_millis(1)).await;
                        };
                        if !headers.contains("upgrade: websocket") {
                            stream.read_exact(&mut buffer[..length]).await.unwrap();
                            if headers.lines().next().unwrap().contains("/checkpoint") {
                                if matches!(hold, Hold::Checkpoint) {
                                    let _ = gate.wait_for(|released| *released).await;
                                }
                                let header = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", checkpoint.len());
                                if stream.write_all(header.as_bytes()).await.is_ok() {
                                    let _ = stream.write_all(&checkpoint).await;
                                }
                            } else {
                                // Prevent HTTP fallback from masking a stalled WS backfill.
                                let _ = stream.write_all(b"HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                            }
                            return;
                        }
                        let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else { return };
                        joined.fetch_add(1, Ordering::SeqCst);
                        let chat = headers.lines().next().unwrap().split('/').nth(2).unwrap().to_owned();
                        *lock(&joined_by_chat).entry(chat).or_default() += 1;
                        let mut sequence = 0u64;
                        while let Some(Ok(message)) = ws.next().await {
                            if let tokio_tungstenite::tungstenite::Message::Text(text) = &message {
                                if text == "ping" { let _ = ws.send("pong".into()).await; }
                                continue;
                            }
                            let Some(frame) = decode(&message.into_data()) else { continue };
                            let reply = match frame.kind {
                                frame_type::HELLO => encode(frame_type::STATE, &serde_json::json!({
                                    "headSeq": 0, "seqFloor": 0, "checkpointSeq": 0,
                                    "checkpointSize": if matches!(hold, Hold::Checkpoint) {checkpoint.len()} else {0},
                                    "rowCount": 0, "rowBytes": 0,
                                }), &frontier),
                                frame_type::ROWS_REQ => {
                                    requested.fetch_add(1, Ordering::SeqCst);
                                    if matches!(hold, Hold::Backfill) {
                                        let _ = gate.wait_for(|released| *released).await;
                                    }
                                    encode(frame_type::ROWS_DONE, &serde_json::json!({"headSeq": sequence}), &[])
                                }
                                frame_type::PUSH => {
                                    sequence += 1;
                                    encode(frame_type::ACK, &serde_json::json!({"batchId": frame.header["batchId"], "seq": sequence, "dup": false}), &[])
                                }
                                frame_type::PROBE => encode(frame_type::PROBE_OK, &serde_json::json!({"headSeq": sequence}), &[]),
                                _ => continue,
                            };
                            if ws.send(reply.into()).await.is_err() { break }
                        }
                    });
                }
                _ = peers.join_next(), if !peers.is_empty() => {}
            }
        }
    });
    Relay {
        url,
        release,
        joins,
        joins_by_chat,
        rows_requests,
        task,
    }
}

async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("dispatcher failed to make progress");
}

fn host(store: Arc<DocsStore>, relay: &Relay) -> DocHost {
    DocHost::new(
        store,
        DocHostConfig {
            device_id: "host".into(),
            default_harness: HarnessId::Mock,
            edge: Some(EdgeConfig::with_static_token(&relay.url, "test")),
        },
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_checkpoints_survive_idle_grace_with_all_slots_occupied() {
    let _budget_guard = SYNC_TEST_LOCK.lock().await;
    let relay = relay(Hold::Checkpoint).await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(DocsStore::open(dir.path()).unwrap());
    for i in 0..ACTIVE_SYNC_CAP {
        let id = format!("slow-{i:02}");
        // Model already-published chats: no outgoing operations pin the client.
        store.initialize_chat_outbox(&id, &[]).unwrap();
        store.schedule_sync_job(&id, "wake").unwrap();
    }
    let host = host(store.clone(), &relay);
    until(|| relay.rows_requests.load(Ordering::SeqCst) == ACTIVE_SYNC_CAP).await;
    assert_eq!(
        store.sync_work_counts().unwrap(),
        (0, ACTIVE_SYNC_CAP as u64)
    );
    // Real time: every checkpoint remains incomplete beyond the 10 s grace.
    tokio::time::sleep(Duration::from_millis(SYNC_IDLE_MS as u64 + 500)).await;
    assert_eq!(
        relay.joins.load(Ordering::SeqCst),
        ACTIVE_SYNC_CAP,
        "idle retirement restarted incomplete checkpoint downloads"
    );
    assert_eq!(
        store.sync_work_counts().unwrap(),
        (0, ACTIVE_SYNC_CAP as u64)
    );
    relay.release.send_replace(true);
    until(|| store.sync_work_counts().unwrap() == (0, 0)).await;
    for i in 0..ACTIVE_SYNC_CAP {
        let bytes = store
            .load_snapshot(&format!("slow-{i:02}"))
            .unwrap()
            .unwrap();
        let doc = loro::LoroDoc::new();
        doc.import(&bytes).unwrap();
        assert_eq!(doc.get_text("slow-checkpoint").to_string(), "received");
    }
    host.shutdown_workers().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_wake_during_stalled_backfill_does_not_block_other_admissions() {
    let _budget_guard = SYNC_TEST_LOCK.lock().await;
    let relay = relay(Hold::Backfill).await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(DocsStore::open(dir.path()).unwrap());
    store.initialize_chat_outbox("stalled", &[]).unwrap();
    let host = host(store, &relay);
    let stalled = host.open("stalled").unwrap();
    until(|| relay.rows_requests.load(Ordering::SeqCst) == 1).await;
    host.enqueue_wakeup("stalled").unwrap();
    // A wake stays on the original client while its first read is stalled.
    tokio::time::sleep(Duration::from_millis(250)).await;
    let other = host.open("unrelated").unwrap();
    until(|| lock(&other.chat2).is_some()).await;
    assert!(other.sync_started.load(Ordering::Acquire));
    assert!(!lock(&stalled.chat2).as_ref().is_some_and(|c| c.caught_up()));
    assert_eq!(lock(&relay.joins_by_chat).get("stalled"), Some(&1));
    assert!(
        host.inner
            .store
            .sync_job_version("stalled", "wake")
            .unwrap()
            .is_some()
    );
    tokio::time::timeout(Duration::from_secs(2), host.shutdown_workers())
        .await
        .expect("host shutdown waited on a peer holding ROWS_DONE");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_connection_closes_while_caller_keeps_document() {
    let _budget_guard = SYNC_TEST_LOCK.lock().await;
    let relay = relay(Hold::Checkpoint).await;
    relay.release.send_replace(true);
    let dir = tempfile::tempdir().unwrap();
    let host = host(Arc::new(DocsStore::open(dir.path()).unwrap()), &relay);
    let handle = host.open("idle").unwrap();
    until(|| lock(&handle.chat2).as_ref().is_some_and(|c| c.caught_up())).await;
    until(|| !host.inner.store.has_pending_chat_updates("idle").unwrap()).await;
    handle
        .last_access
        .store(now_ms() - SYNC_IDLE_MS - 1, Ordering::Release);
    until(|| !handle.sync_started.load(Ordering::Acquire)).await;
    assert!(lock(&handle.chat2).is_none());
    assert!(lock(&host.inner.handles).contains_key("idle"));
    let writer = handle.writer();
    assert_eq!(handle.writers.load(Ordering::Acquire), 1);
    drop(writer);
    assert_eq!(handle.writers.load(Ordering::Acquire), 0);
    host.shutdown_workers().await;
}

fn workspace(store: Arc<DocsStore>) -> WorkspaceHost {
    WorkspaceHost::open(
        store,
        crate::workspace_host::WorkspaceHostConfig {
            device_id: "host".into(),
            device_name: "Test host".into(),
            platform: "linux".into(),
            org_id: "test-org".into(),
            user_id: "test-user".into(),
            edge: None,
        },
    )
    .unwrap()
}

fn legacy_chat(workspace: &WorkspaceHost, id: &str, device: &str, generation: Option<u32>) {
    workspace
        .create_chat(id, None, Some(device), None, None)
        .unwrap();
    let mut row = workspace.chat(id).unwrap().unwrap();
    row.room_gen = generation;
    workspace.import_chat_row(&row).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn twelve_foreign_legacy_wakes_do_not_block_interactive_sync() {
    let _budget_guard = SYNC_TEST_LOCK.lock().await;
    let relay = relay(Hold::Checkpoint).await;
    relay.release.send_replace(true);
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(DocsStore::open(dir.path()).unwrap());
    let workspace = workspace(store.clone());
    for i in 0..ACTIVE_SYNC_CAP {
        let id = format!("legacy-{i:02}");
        legacy_chat(
            &workspace,
            &id,
            "another-host",
            if i % 2 == 0 { None } else { Some(1) },
        );
        store.schedule_sync_job(&id, "wake").unwrap();
    }
    let host = host(store.clone(), &relay);
    host.set_workspace(workspace);
    // Reach the old failure state (12 phantom clients) or successful retirement.
    until(|| {
        store.sync_work_counts().unwrap().1 == 0
            || lock(&host.inner.handles)
                .values()
                .filter(|h| h.sync_started.load(Ordering::Acquire))
                .count()
                == ACTIVE_SYNC_CAP
    })
    .await;
    let interactive = host.open("interactive").unwrap();
    let _view = interactive.watch_messages();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !lock(&interactive.chat2)
            .as_ref()
            .is_some_and(|c| c.caught_up())
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("foreign legacy wakes occupied every admission slot");
    until(|| store.sync_work_counts().unwrap().1 == 0).await;
    assert!(
        lock(&host.inner.handles)
            .values()
            .filter(|h| h.chat_id.starts_with("legacy-"))
            .all(|h| !h.sync_started.load(Ordering::Acquire)
                && !h.sync_requested.load(Ordering::Acquire))
    );
    assert!(lock(&host.inner.seed_waiting).is_empty());
    for i in 0..ACTIVE_SYNC_CAP {
        host.enqueue_wakeup(&format!("legacy-{i:02}")).unwrap();
    }
    until(|| store.sync_work_counts().unwrap().1 == 0).await;
    assert_eq!(
        relay.joins.load(Ordering::SeqCst),
        1,
        "rejected wakes started network work"
    );
    host.shutdown_workers().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admissible_wakes_preserve_missing_rows_modern_readers_and_local_epoch() {
    let _budget_guard = SYNC_TEST_LOCK.lock().await;
    let relay = relay(Hold::Checkpoint).await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(DocsStore::open(dir.path()).unwrap());
    let workspace = workspace(store.clone());
    workspace
        .create_chat("modern-foreign", None, Some("another-host"), None, None)
        .unwrap();
    legacy_chat(&workspace, "epoch-override", "another-host", Some(1));
    let thin = SessionDoc::init("epoch-override").unwrap();
    store
        .save_snapshot_with_cursor(
            "epoch-override",
            &thin.export_snapshot().unwrap(),
            0,
            crate::chat2_host::CHAT2_DOC_EPOCH,
        )
        .unwrap();
    let ids = ["missing-row", "modern-foreign", "epoch-override"];
    for id in ids {
        store.initialize_chat_outbox(id, &[]).unwrap();
        store.schedule_sync_job(id, "wake").unwrap();
    }
    let host = host(store.clone(), &relay);
    host.set_workspace(workspace.clone());
    until(|| relay.rows_requests.load(Ordering::SeqCst) == ids.len()).await;
    for id in ids {
        let handle = lock(&host.inner.handles).get(id).cloned().unwrap();
        assert_eq!(handle.room_gen, 2);
        assert!(handle.sync_started.load(Ordering::Acquire));
        assert!(
            store.sync_job_version(id, "wake").unwrap().is_some(),
            "retired before catch-up"
        );
    }
    assert!(workspace.chat("missing-row").unwrap().is_none());
    // Creation arrives after its notification has already started joining.
    workspace
        .create_chat("missing-row", None, Some("another-host"), None, None)
        .unwrap();
    relay.release.send_replace(true);
    until(|| store.sync_work_counts().unwrap().1 == 0).await;
    for id in ids {
        let snapshot = store.load_snapshot(id).unwrap().unwrap();
        let doc = loro::LoroDoc::new();
        doc.import(&snapshot).unwrap();
        assert_eq!(doc.get_text("slow-checkpoint").to_string(), "received");
    }
    host.shutdown_workers().await;
}

#[tokio::test]
async fn legacy_admission_rechecks_owner_and_retires_only_the_captured_wake() {
    let _budget_guard = SYNC_TEST_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(DocsStore::open(dir.path()).unwrap());
    let workspace = workspace(store.clone());
    legacy_chat(&workspace, "reassigned", "host", None);
    let host = DocHost::new(
        store.clone(),
        DocHostConfig {
            device_id: "host".into(),
            default_harness: HarnessId::Mock,
            edge: None,
        },
    );
    host.set_workspace(workspace.clone());
    let handle = host.open_local("reassigned").unwrap();
    assert_eq!(
        host.prepare_sync_admission(&handle, None).unwrap(),
        Some(SyncAdmission::Seed)
    );
    handle.sync_requested.store(true, Ordering::Release);
    handle
        .doc
        .doc()
        .get_text("local-edit")
        .insert(0, "keep this edit")
        .unwrap();
    handle.doc.doc().commit();
    let updates = crate::chat2_host::publication_updates(handle.doc.doc()).unwrap();
    store
        .enqueue_chat_update("reassigned", "local-batch", &updates[0])
        .unwrap();
    let outbox = store.pending_chat_updates("reassigned").unwrap();
    host.enqueue_wakeup("reassigned").unwrap();
    let old = store.sync_job_version("reassigned", "wake").unwrap();
    // Ownership changes while this handle is waiting for admission.
    workspace
        .set_chat_host("reassigned", "another-host")
        .unwrap();
    host.enqueue_wakeup("reassigned").unwrap();
    let newer = store.sync_job_version("reassigned", "wake").unwrap();
    assert_ne!(old, newer);
    assert_eq!(host.prepare_sync_admission(&handle, old).unwrap(), None);
    assert_eq!(store.sync_job_version("reassigned", "wake").unwrap(), newer);
    assert!(!handle.sync_requested.load(Ordering::Acquire));
    assert!(!handle.sync_started.load(Ordering::Acquire));
    assert_eq!(host.prepare_sync_admission(&handle, newer).unwrap(), None);
    assert_eq!(store.sync_job_version("reassigned", "wake").unwrap(), None);
    assert_eq!(store.pending_chat_updates("reassigned").unwrap(), outbox);
    assert_eq!(
        handle.doc.doc().get_text("local-edit").to_string(),
        "keep this edit"
    );
    // A cutover on another host can overtake this old cached handle: defer,
    // rather than retire the notification for the now-modern room.
    workspace.set_chat_room_gen("reassigned", 2).unwrap();
    host.enqueue_wakeup("reassigned").unwrap();
    let cutover = store.sync_job_version("reassigned", "wake").unwrap();
    assert_eq!(host.prepare_sync_admission(&handle, cutover).unwrap(), None);
    assert_eq!(
        store.sync_job_version("reassigned", "wake").unwrap(),
        cutover
    );
    host.shutdown_workers().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forty_eight_chats_drain_without_restarting_views_or_writers() {
    let _budget_guard = SYNC_TEST_LOCK.lock().await;
    const CHATS: usize = 48;
    const PROTECTED: usize = 8;
    let relay = relay(Hold::Backfill).await;
    relay.release.send_replace(true);
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(DocsStore::open(dir.path()).unwrap());
    let host = host(store.clone(), &relay);
    let mut handles = Vec::new();
    let mut views = Vec::new();
    let mut writers = Vec::new();
    for i in 0..CHATS {
        let id = format!("load-{i:02}");
        store.initialize_chat_outbox(&id, &[]).unwrap();
        let handle = host.open_local(&id).unwrap();
        if i < PROTECTED / 2 {
            views.push(handle.watch_messages());
        } else if i < PROTECTED {
            writers.push(handle.writer());
        }
        handles.push(handle);
    }
    // Establish eight protected clients first, then contend for the remaining
    // four slots with forty chats. All 48 requests are outstanding together.
    for h in &handles[..PROTECTED] {
        host.activate_sync(h);
    }
    until(|| {
        handles[..PROTECTED]
            .iter()
            .all(|h| lock(&h.chat2).as_ref().is_some_and(|c| c.caught_up()))
    })
    .await;
    let mut peak = 0;
    for round in 0..3 {
        for h in &handles {
            h.sync_last_started
                .store(now_ms() - SYNC_QUANTUM_MS - 1, Ordering::Release);
            h.last_access
                .store(now_ms() - SYNC_IDLE_MS - 1, Ordering::Release);
            host.enqueue_wakeup(&h.chat_id).unwrap();
        }
        if round == 0 {
            for h in &handles[PROTECTED..] {
                host.activate_sync(h);
            }
        }
        // Local writes keep active outboxes busy while the backlog advances.
        for h in &handles[..PROTECTED] {
            h.doc.doc().get_text("load").insert(0, "edit").unwrap();
            h.doc.doc().commit();
        }
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let stats = host.sync_resources();
                let active = stats["retainedBy"]["connections"].as_u64().unwrap();
                peak = peak.max(active);
                assert!(active <= ACTIVE_SYNC_CAP as u64, "{stats}");
                for h in &handles[..PROTECTED] {
                    assert!(
                        h.sync_started.load(Ordering::Acquire),
                        "protected client retired"
                    );
                    assert_eq!(lock(&relay.joins_by_chat).get(&h.chat_id), Some(&1));
                }
                if store.sync_work_counts().unwrap() == (0, 0) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("48-chat backlog did not drain");
    }
    assert_eq!(lock(&relay.joins_by_chat).len(), CHATS);
    assert!(
        peak > PROTECTED as u64,
        "background never competed for capacity"
    );
    eprintln!(
        "48 chats, 3 wake waves, peak clients={peak}, protected reconnects=0, durable backlog=0"
    );
    drop((views, writers));
    host.shutdown_workers().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_outboxes_do_not_rotate_clients_even_at_capacity() {
    let _budget_guard = SYNC_TEST_LOCK.lock().await;
    let relay = relay(Hold::Backfill).await;
    relay.release.send_replace(true);
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(DocsStore::open(dir.path()).unwrap());
    let host = host(store.clone(), &relay);
    let mut handles = Vec::new();
    for i in 0..ACTIVE_SYNC_CAP {
        let id = format!("active-{i:02}");
        store.initialize_chat_outbox(&id, &[]).unwrap();
        handles.push(host.open(&id).unwrap());
    }
    until(|| {
        handles
            .iter()
            .all(|h| lock(&h.chat2).as_ref().is_some_and(|c| c.caught_up()))
    })
    .await;
    for h in &handles {
        // Simulate durable publication awaiting delivery. Keep it in SQLite
        // without notifying the client so the regression cannot miss a short
        // outbox window; no payload is applied to the document.
        store
            .enqueue_chat_update(&h.chat_id, "pending", b"held")
            .unwrap();
        h.sync_last_started
            .store(now_ms() - SYNC_QUANTUM_MS - 1, Ordering::Release);
    }
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(relay.joins.load(Ordering::SeqCst), ACTIVE_SYNC_CAP);
    assert!(
        handles
            .iter()
            .all(|h| h.sync_started.load(Ordering::Acquire))
    );
    host.shutdown_workers().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spare_capacity_admits_waiters_without_rotating_aged_clients() {
    let _budget_guard = SYNC_TEST_LOCK.lock().await;
    let relay = relay(Hold::Backfill).await;
    relay.release.send_replace(true);
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(DocsStore::open(dir.path()).unwrap());
    let host = host(store.clone(), &relay);
    let mut handles = Vec::new();
    for i in 0..10 {
        let id = format!("spare-{i:02}");
        store.initialize_chat_outbox(&id, &[]).unwrap();
        handles.push(host.open(&id).unwrap());
    }
    until(|| {
        handles
            .iter()
            .all(|h| lock(&h.chat2).as_ref().is_some_and(|c| c.caught_up()))
    })
    .await;
    for h in &handles {
        h.sync_last_started
            .store(now_ms() - SYNC_QUANTUM_MS - 1, Ordering::Release);
    }
    let extra = host.open("extra").unwrap();
    until(|| lock(&extra.chat2).as_ref().is_some_and(|c| c.caught_up())).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    for h in &handles {
        assert_eq!(lock(&relay.joins_by_chat).get(&h.chat_id), Some(&1));
    }
    host.shutdown_workers().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn contention_retires_only_one_unprotected_client_for_one_waiter() {
    let _budget_guard = SYNC_TEST_LOCK.lock().await;
    let relay = relay(Hold::Backfill).await;
    relay.release.send_replace(true);
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(DocsStore::open(dir.path()).unwrap());
    let host = host(store.clone(), &relay);
    let mut handles = Vec::new();
    for i in 0..ACTIVE_SYNC_CAP {
        let id = format!("rotate-{i:02}");
        store.initialize_chat_outbox(&id, &[]).unwrap();
        handles.push(host.open(&id).unwrap());
    }
    until(|| {
        handles
            .iter()
            .all(|h| lock(&h.chat2).as_ref().is_some_and(|c| c.caught_up()))
    })
    .await;
    // A repeated local open recently touched each otherwise quiet client.
    for h in &handles {
        h.sync_last_started
            .store(now_ms() - SYNC_QUANTUM_MS - 1, Ordering::Release);
        h.touch();
    }
    let extra = host.open("extra").unwrap();
    let _view = extra.watch_messages();
    until(|| lock(&extra.chat2).as_ref().is_some_and(|c| c.caught_up())).await;
    let stopped = handles
        .iter()
        .filter(|h| !h.sync_started.load(Ordering::Acquire))
        .count();
    assert_eq!(stopped, 1, "retired more clients than needed");
    host.shutdown_workers().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forty_eight_chats_preserve_twelve_views_then_drain_when_one_view_closes() {
    let _budget_guard = SYNC_TEST_LOCK.lock().await;
    let relay = relay(Hold::Backfill).await;
    relay.release.send_replace(true);
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(DocsStore::open(dir.path()).unwrap());
    let host = host(store.clone(), &relay);
    let mut handles = Vec::new();
    let mut views = Vec::new();
    for i in 0..ACTIVE_SYNC_CAP {
        let id = format!("view-{i:02}");
        store.initialize_chat_outbox(&id, &[]).unwrap();
        let h = host.open(&id).unwrap();
        views.push(h.watch_messages());
        handles.push(h);
    }
    until(|| {
        handles
            .iter()
            .all(|h| lock(&h.chat2).as_ref().is_some_and(|c| c.caught_up()))
    })
    .await;
    // Fresh clients are still inside the reuse grace. Once a view closes,
    // contention must reclaim its quiet slot without making newcomers wait.
    for i in 0..36 {
        let id = format!("background-{i:02}");
        store.initialize_chat_outbox(&id, &[]).unwrap();
        host.enqueue_wakeup(&id).unwrap();
    }
    // All slots are protected: keep their connections, retaining other work
    // durably instead of rotating a visible conversation to admit it.
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(relay.joins.load(Ordering::SeqCst), ACTIVE_SYNC_CAP);
    assert_eq!(store.sync_work_counts().unwrap(), (0, 36));
    drop(views.pop());
    tokio::time::timeout(Duration::from_secs(20), async {
        while store.sync_work_counts().unwrap() != (0, 0) {
            assert!(
                host.sync_resources()["retainedBy"]["connections"]
                    .as_u64()
                    .unwrap()
                    <= 12
            );
            for h in &handles[..11] {
                assert!(h.sync_started.load(Ordering::Acquire));
                assert_eq!(lock(&relay.joins_by_chat).get(&h.chat_id), Some(&1));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("background work did not resume when a view closed");
    assert_eq!(lock(&relay.joins_by_chat).len(), 48);
    host.shutdown_workers().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saturated_protected_clients_do_not_load_cold_backlog() {
    let _budget_guard = SYNC_TEST_LOCK.lock().await;
    let relay = relay(Hold::Backfill).await;
    relay.release.send_replace(true);
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(DocsStore::open(dir.path()).unwrap());
    let host = host(store.clone(), &relay);
    let mut views = Vec::new();
    for n in 0..ACTIVE_SYNC_CAP {
        let id = format!("view-{n}");
        store.initialize_chat_outbox(&id, &[]).unwrap();
        views.push(host.open(&id).unwrap().watch_messages());
    }
    until(|| relay.joins.load(Ordering::SeqCst) == ACTIVE_SYNC_CAP).await;
    let loads = host.inner.document_loads.load(Ordering::Relaxed);
    for n in 0..128 {
        let id = format!("cold-{n:03}");
        store.initialize_chat_outbox(&id, &[]).unwrap();
        host.enqueue_wakeup(&id).unwrap();
    }
    tokio::time::sleep(Duration::from_millis(1300)).await;
    assert_eq!(host.inner.document_loads.load(Ordering::Relaxed), loads);
    assert_eq!(lock(&host.inner.handles).len(), ACTIVE_SYNC_CAP);
    assert_eq!(store.sync_work_counts().unwrap(), (0, 128));
    host.shutdown_workers().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn newer_view_displaces_one_older_view_without_reconnect_ping_pong() {
    let _budget_guard = SYNC_TEST_LOCK.lock().await;
    let relay = relay(Hold::Backfill).await;
    relay.release.send_replace(true);
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(DocsStore::open(dir.path()).unwrap());
    let host = host(store.clone(), &relay);
    let mut handles = Vec::new();
    let mut views = Vec::new();
    let mut writers = Vec::new();
    for n in 0..ACTIVE_SYNC_CAP {
        let id = format!("view-{n:02}");
        store.initialize_chat_outbox(&id, &[]).unwrap();
        let h = host.open(&id).unwrap();
        views.push(h.watch_messages());
        if n == 0 {
            writers.push(h.writer());
        }
        handles.push(h);
    }
    until(|| {
        handles
            .iter()
            .all(|h| lock(&h.chat2).as_ref().is_some_and(|c| c.caught_up()))
    })
    .await;
    // Make the writer the oldest candidate: it must still never be retired.
    for (n, h) in handles.iter().enumerate() {
        h.last_access
            .store(now_ms() - 2000 + n as i64, Ordering::Relaxed);
    }
    for n in 0..32 {
        host.enqueue_wakeup(&format!("cold-{n}")).unwrap();
    }
    let fresh = host.open("new-view").unwrap();
    let _fresh_view = fresh.watch_messages();
    until(|| lock(&fresh.chat2).as_ref().is_some_and(|c| c.caught_up())).await;
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert_eq!(relay.joins.load(Ordering::SeqCst), ACTIVE_SYNC_CAP + 1);
    assert_eq!(
        handles
            .iter()
            .filter(|h| !h.sync_started.load(Ordering::Acquire))
            .count(),
        1
    );
    assert!(handles[0].sync_started.load(Ordering::Acquire));
    assert!(!handles[1].sync_started.load(Ordering::Acquire));
    // Refocusing the displaced view makes it an explicit interactive request.
    host.open(&handles[1].chat_id).unwrap();
    until(|| handles[1].sync_started.load(Ordering::Acquire)).await;
    assert!(handles[0].sync_started.load(Ordering::Acquire));
    host.shutdown_workers().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_waiter_cannot_displace_a_running_writer() {
    let _budget_guard = SYNC_TEST_LOCK.lock().await;
    let relay = relay(Hold::Backfill).await;
    relay.release.send_replace(true);
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(DocsStore::open(dir.path()).unwrap());
    let host = host(store.clone(), &relay);
    let mut writers = Vec::new();
    for n in 0..ACTIVE_SYNC_CAP {
        let id = format!("writer-{n}");
        store.initialize_chat_outbox(&id, &[]).unwrap();
        writers.push(host.open(&id).unwrap().writer());
    }
    until(|| relay.joins.load(Ordering::SeqCst) == ACTIVE_SYNC_CAP).await;
    let fresh = host.open("new-view").unwrap();
    let _view = fresh.watch_messages();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!fresh.sync_started.load(Ordering::Acquire));
    assert_eq!(relay.joins.load(Ordering::SeqCst), ACTIVE_SYNC_CAP);
    drop(writers.pop());
    until(|| lock(&fresh.chat2).as_ref().is_some_and(|c| c.caught_up())).await;
    host.shutdown_workers().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_handoff_prefers_an_unprotected_slot_over_a_view() {
    let _budget_guard = SYNC_TEST_LOCK.lock().await;
    let relay = relay(Hold::Backfill).await;
    relay.release.send_replace(true);
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(DocsStore::open(dir.path()).unwrap());
    let host = host(store.clone(), &relay);
    // Put the next admission after the twelve residents on the normal
    // background-fairness turn. The freed slot must still serve the handoff.
    for n in 0..3 {
        host.enqueue_wakeup(&format!("phase-{n}")).unwrap();
    }
    until(|| relay.joins.load(Ordering::SeqCst) == 3).await;
    until(|| host.sync_resources()["retainedBy"]["connections"] == 0).await;
    let mut handles = Vec::new();
    let mut views = Vec::new();
    for n in 0..ACTIVE_SYNC_CAP {
        let id = format!("existing-{n:02}");
        store.initialize_chat_outbox(&id, &[]).unwrap();
        let h = host.open(&id).unwrap();
        if n > 0 {
            views.push(h.watch_messages());
        }
        handles.push(h);
    }
    until(|| {
        handles
            .iter()
            .all(|h| lock(&h.chat2).as_ref().is_some_and(|c| c.caught_up()))
    })
    .await;
    for h in &handles {
        h.last_access.store(now_ms() - 1000, Ordering::Relaxed);
    }
    for n in 0..32 {
        host.enqueue_wakeup(&format!("cold-{n}")).unwrap();
    }
    let fresh = host.open("new-view").unwrap();
    let _view = fresh.watch_messages();
    until(|| lock(&fresh.chat2).as_ref().is_some_and(|c| c.caught_up())).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    for h in &handles[1..] {
        assert!(h.sync_started.load(Ordering::Acquire));
        assert_eq!(lock(&relay.joins_by_chat).get(h.chat_id()), Some(&1));
    }
    host.shutdown_workers().await;
}
