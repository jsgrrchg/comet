//! Exercise the dispatcher against slow but live loopback transports.
use super::*;
use futures::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zeron_sync::chat_frames::{decode, encode, frame_type};

#[derive(Clone, Copy)]
enum Hold {
    Checkpoint,
    Backfill,
}

struct Relay {
    url: String,
    release: watch::Sender<bool>,
    joins: Arc<AtomicUsize>,
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
    let relay = relay(Hold::Backfill).await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(DocsStore::open(dir.path()).unwrap());
    store.initialize_chat_outbox("stalled", &[]).unwrap();
    let host = host(store, &relay);
    let stalled = host.open("stalled").unwrap();
    until(|| relay.rows_requests.load(Ordering::SeqCst) == 1).await;
    host.enqueue_wakeup("stalled").unwrap();
    // The new wake forces the stalled client's retirement on the next tick.
    tokio::time::sleep(Duration::from_millis(250)).await;
    let other = host.open("unrelated").unwrap();
    until(|| lock(&other.chat2).is_some()).await;
    assert!(other.sync_started.load(Ordering::Acquire));
    assert!(!lock(&stalled.chat2).as_ref().is_some_and(|c| c.caught_up()));
    tokio::time::timeout(Duration::from_secs(2), host.shutdown_workers())
        .await
        .expect("host shutdown waited on a peer holding ROWS_DONE");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_connection_closes_while_caller_keeps_document() {
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
