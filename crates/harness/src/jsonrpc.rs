//! Minimal JSON-RPC 2.0 client over a child agent's stdio (newline-delimited
//! frames, id-multiplexed), ported from codex.ts's `startAppServer`. Shared by
//! the Codex app-server harness and the ACP harness — both protocols are
//! newline-framed JSON-RPC 2.0 over stdio.
//!
//! - Responses are matched to callers by numeric id (a shared pending map the
//!   reader task resolves directly, so requests can be awaited from anywhere —
//!   including inside the session loop — without starving notifications).
//! - Notifications and server→client requests (approvals) are pumped into an
//!   [`Incoming`] channel the session loop drains.
//! - Writes to a dead child's stdin (EPIPE) are tolerated and logged, matching
//!   the TS harness's swallowed-EPIPE behavior.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use futures::StreamExt;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};
use tokio_util::codec::{FramedRead, LinesCodec};

use crate::HarnessError;
use crate::process::{ChildStdin, ChildStdout};

/// A non-response line from the app server, in stdout order.
#[derive(Debug)]
pub(crate) enum Incoming {
    Notification {
        method: String,
        params: Value,
    },
    /// Server→client request (approvals); must be answered via
    /// [`RpcClient::respond`] / [`RpcClient::respond_error`].
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// stdout EOF: the app server exited. All pending requests fail.
    Eof,
}

type StdoutObserver = Box<dyn Fn(&str) + Send>;

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, HarnessError>>>>>;

/// Why stdout became unreadable, when it failed rather than reached EOF. The
/// app server may still be alive, so callers must not be told it exited.
type ReadFailure = Arc<OnceLock<String>>;

fn closed_error(method: &str, failure: &ReadFailure) -> HarnessError {
    HarnessError::Protocol(match failure.get() {
        Some(cause) => format!("{method}: app-server stdout unreadable: {cause}"),
        None => format!("{method}: app-server exited before responding"),
    })
}

struct VoiceRouter {
    sender: mpsc::Sender<Incoming>,
    overflow: Arc<AtomicBool>,
    abort_media: Box<dyn Fn() + Send + Sync>,
}

#[derive(Clone)]
pub(crate) struct RpcClient {
    next_id: Arc<AtomicI64>,
    pending: Pending,
    writer: mpsc::Sender<String>,
    media_writer: mpsc::Sender<String>,
    closed: Arc<AtomicBool>,
    read_failure: ReadFailure,
    voice_router: Arc<Mutex<Option<VoiceRouter>>>,
}

impl RpcClient {
    /// Spawn the writer + reader tasks over the child's stdio; returns the
    /// client and the incoming (notification/request) channel.
    pub fn new(stdin: ChildStdin, stdout: ChildStdout) -> (Self, mpsc::Receiver<Incoming>) {
        Self::with_stdout_observer(stdin, stdout, None)
    }

    pub(crate) fn with_stdout_observer(
        stdin: ChildStdin,
        stdout: ChildStdout,
        observer: Option<StdoutObserver>,
    ) -> (Self, mpsc::Receiver<Incoming>) {
        let (writer_tx, writer_rx) = mpsc::channel::<String>(256);
        let (media_writer, media_rx) = mpsc::channel::<String>(8);
        let pending: Pending = Arc::default();
        let (incoming_tx, incoming_rx) = mpsc::channel(256);
        let closed = Arc::new(AtomicBool::new(false));
        let read_failure = ReadFailure::default();
        let voice_router = Arc::default();
        tokio::spawn(write_loop(
            stdin,
            writer_rx,
            media_rx,
            pending.clone(),
            closed.clone(),
        ));
        tokio::spawn(read_loop(
            stdout,
            Arc::clone(&pending),
            incoming_tx,
            closed.clone(),
            read_failure.clone(),
            observer,
            Arc::clone(&voice_router),
        ));
        (
            Self {
                next_id: Arc::new(AtomicI64::new(0)),
                pending,
                writer: writer_tx,
                media_writer,
                closed,
                read_failure,
                voice_router,
            },
            incoming_rx,
        )
    }

    /// Ephemeral realtime notifications bypass the durable/control channel.
    /// Overflow is terminal and observable; stdout never waits for playout.
    pub fn subscribe_voice(
        &self,
        abort_media: impl Fn() + Send + Sync + 'static,
    ) -> (mpsc::Receiver<Incoming>, Arc<AtomicBool>) {
        let (tx, rx) = mpsc::channel(32);
        let overflow = Arc::new(AtomicBool::new(false));
        *self.voice_router.lock().expect("voice router") = Some(VoiceRouter {
            sender: tx,
            overflow: overflow.clone(),
            abort_media: Box::new(abort_media),
        });
        (rx, overflow)
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Send a request and await its response (resolved by the reader task).
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, HarnessError> {
        self.request_now(method, params).await
    }

    /// [`Self::request`], but the line is queued for writing before this
    /// returns rather than on first poll — so a notification sent afterwards
    /// (a steer's `session/cancel`) can never overtake it on the wire.
    pub fn request_now(
        &self,
        method: &str,
        params: Value,
    ) -> futures::future::BoxFuture<'static, Result<Value, HarnessError>> {
        self.queue_request(method, params, false)
    }

    /// Bounded low-priority media lane. Never retries an accepted chunk.
    pub fn request_media(
        &self,
        method: &str,
        params: Value,
    ) -> futures::future::BoxFuture<'static, Result<Value, HarnessError>> {
        self.queue_request(method, params, true)
    }

    fn queue_request(
        &self,
        method: &str,
        params: Value,
        media: bool,
    ) -> futures::future::BoxFuture<'static, Result<Value, HarnessError>> {
        let method = method.to_owned();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().expect("pending lock");
            // Check under the same lock as EOF cleanup: a request racing the
            // reader exit must either be rejected here or cleared by it.
            if self.is_closed() || pending.len() >= 256 {
                let error = closed_error(&method, &self.read_failure);
                return Box::pin(async move { Err(error) });
            }
            pending.insert(id, tx);
        }
        let line = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let encoded = line.to_string();
        let writer = if media {
            &self.media_writer
        } else {
            &self.writer
        };
        if (media && encoded.len() > 32_768) || writer.try_send(encoded).is_err() {
            self.pending.lock().expect("pending lock").remove(&id);
            return Box::pin(async move {
                Err(HarnessError::Protocol(format!(
                    "{method}: app-server stdin closed"
                )))
            });
        }
        let cleanup = PendingGuard {
            id,
            pending: self.pending.clone(),
        };
        let read_failure = self.read_failure.clone();
        Box::pin(async move {
            let _cleanup = cleanup;
            match rx.await {
                Ok(Ok(result)) => Ok(result),
                Ok(Err(error)) => Err(error),
                // Sender dropped: the reader stopped and failed all pending.
                Err(_) => Err(closed_error(&method, &read_failure)),
            }
        })
    }

    /// Fire a notification (no id, no response).
    pub fn notify(&self, method: &str, params: Option<Value>) {
        let line = match params {
            Some(params) => json!({ "jsonrpc": "2.0", "method": method, "params": params }),
            None => json!({ "jsonrpc": "2.0", "method": method }),
        };
        self.send_control(line.to_string());
    }

    fn send_control(&self, line: String) {
        if self.writer.try_send(line).is_err() {
            // Never silently lose approvals or cancellation. Retire the peer
            // on control overflow; both I/O loops observe this terminal flag.
            self.closed.store(true, Ordering::Release);
            self.pending.lock().expect("pending lock").clear();
        }
    }

    /// Answer a server→client request.
    pub fn respond(&self, id: &Value, result: Value) {
        let line = json!({ "jsonrpc": "2.0", "id": id, "result": result });
        self.send_control(line.to_string());
    }

    /// Reject a server→client request (e.g. unknown method).
    pub fn respond_error(&self, id: &Value, code: i64, message: &str) {
        let line = json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        });
        self.send_control(line.to_string());
    }
}

/// Owns the child's stdin; a write failure (EPIPE after the child died) is
/// tolerated and logged.
struct PendingGuard {
    id: i64,
    pending: Pending,
}
impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.pending.lock().expect("pending lock").remove(&self.id);
    }
}

async fn write_loop(
    mut stdin: ChildStdin,
    mut control: mpsc::Receiver<String>,
    mut media: mpsc::Receiver<String>,
    pending: Pending,
    closed: Arc<AtomicBool>,
) {
    let mut health = tokio::time::interval(std::time::Duration::from_millis(100));
    loop {
        if closed.load(Ordering::Acquire)
            || (control.is_closed() && control.is_empty() && media.is_closed() && media.is_empty())
        {
            break;
        }
        let line = tokio::select! {
            biased;
            _ = health.tick() => continue,
            line = control.recv(), if !control.is_closed() || !control.is_empty() => line,
            line = media.recv(), if !media.is_closed() || !media.is_empty() => line,
            else => break,
        };
        let Some(line) = line else { continue };
        let write = async {
            stdin.write_all(line.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await
        };
        if tokio::time::timeout(std::time::Duration::from_secs(10), write)
            .await
            .map_or(true, |result| result.is_err())
        {
            closed.store(true, Ordering::Release);
            pending.lock().expect("pending lock").clear();
            break;
        }
    }
}

struct ReaderGuard {
    pending: Pending,
    closed: Arc<AtomicBool>,
}
impl Drop for ReaderGuard {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
        self.pending.lock().expect("pending lock").clear();
    }
}

/// The id of a response, tolerantly. zeron always sends numeric ids, but
/// JSON-RPC lets a server echo them re-encoded — a string `"5"` or float
/// `5.0` still names request 5. Dropping such a response would strand its
/// caller forever (the session would spin Working with no per-turn timeout).
fn response_id(id: &Value) -> Option<i64> {
    if let Some(n) = id.as_i64() {
        return Some(n);
    }
    if let Some(s) = id.as_str() {
        return s.parse().ok();
    }
    id.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64)
}

/// Preserve the agent's rejection reason as well as the generic RPC label.
fn response_error(error: &Value) -> String {
    let Some(message) = error.get("message").and_then(Value::as_str) else {
        return error.to_string();
    };
    let mut rendered = message.to_owned();
    if let Some(code) = error.get("code").and_then(Value::as_i64) {
        rendered.push_str(&format!(" (code {code})"));
    }
    if let Some(data) = error.get("data").filter(|v| !v.is_null()) {
        let detail = data
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| data.to_string());
        if !detail.is_empty() {
            rendered.push_str(": ");
            rendered.push_str(&detail);
        }
    }
    rendered
}

/// Parse stdout lines: responses resolve the pending map, everything else is
/// forwarded in order. Non-JSON noise is skipped; on EOF all pending requests
/// fail (their senders drop) and one final [`Incoming::Eof`] is delivered.
async fn read_loop(
    stdout: ChildStdout,
    pending: Pending,
    tx: mpsc::Sender<Incoming>,
    closed: Arc<AtomicBool>,
    read_failure: ReadFailure,
    observer: Option<StdoutObserver>,
    voice_router: Arc<Mutex<Option<VoiceRouter>>>,
) {
    let _cleanup = ReaderGuard {
        pending: pending.clone(),
        closed: closed.clone(),
    };
    // No length cap: a resumed thread returns its whole history in one line,
    // tens of MiB for a long chat. Voice bounds its own messages.
    let mut lines = FramedRead::new(stdout, LinesCodec::new());
    // A read error ends the loop like EOF: either way the child's stdout is
    // unusable, pending requests must fail, and the session loop must know.
    // Its cause is kept, since the child may well be alive.
    let mut health = tokio::time::interval(std::time::Duration::from_millis(100));
    loop {
        if closed.load(Ordering::Acquire) {
            break;
        }
        let result = tokio::select! {
            biased;
            _ = health.tick() => continue,
            result = lines.next() => result,
        };
        let line = match result {
            Some(Ok(line)) => line,
            Some(Err(error)) => {
                tracing::warn!(target: "zeron_harness::rpc", %error, "app-server stdout unreadable");
                let _ = read_failure.set(error.to_string());
                break;
            }
            None => break,
        };
        let line = line.trim();
        if let Some(url) =
            line.strip_prefix("Open the following link to authenticate the ACP server: ")
        {
            if let Some(observer) = &observer {
                observer(url);
            }
            continue;
        }
        if line.is_empty() {
            continue;
        }
        let Ok(mut msg) = serde_json::from_str::<Value>(line) else {
            tracing::debug!(target: "zeron_harness::rpc", "non-JSON stdout line (skipped)");
            continue;
        };
        if !msg.is_object() || msg.get("jsonrpc").is_some_and(|version| version != "2.0") {
            continue;
        }
        let method = msg.get("method").and_then(Value::as_str);
        let id = msg.get("id");
        match (method, id) {
            // Response: resolve the awaiting request.
            (None, Some(id)) => {
                if msg.get("result").is_none() && msg.get("error").is_none() {
                    continue;
                }
                let Some(id) = response_id(id) else { continue };
                let Some(sender) = pending.lock().expect("pending lock").remove(&id) else {
                    continue;
                };
                let outcome = match msg.get("error") {
                    Some(err) => Err(HarnessError::Rpc {
                        code: err.get("code").and_then(Value::as_i64).unwrap_or(-32603),
                        message: response_error(err),
                        data: err.get("data").filter(|v| !v.is_null()).cloned(),
                    }),
                    None => Ok(msg
                        .get_mut("result")
                        .map(Value::take)
                        .unwrap_or(Value::Null)),
                };
                let _ = sender.send(outcome);
            }
            // Server→client request (approvals).
            (Some(method), Some(id)) => {
                let incoming = Incoming::Request {
                    id: id.clone(),
                    method: method.to_owned(),
                    params: msg
                        .get_mut("params")
                        .map(Value::take)
                        .unwrap_or(Value::Null),
                };
                if tx.send(incoming).await.is_err() {
                    break;
                }
            }
            // Notification.
            (Some(method), None) => {
                let realtime = method.starts_with("thread/realtime/");
                let account_updated = method == "account/updated";
                let incoming = Incoming::Notification {
                    method: method.to_owned(),
                    params: msg
                        .get_mut("params")
                        .map(Value::take)
                        .unwrap_or(Value::Null),
                };
                if account_updated {
                    if let Some(router) = voice_router.lock().expect("voice router").as_ref() {
                        (router.abort_media)();
                        if router
                            .sender
                            .try_send(Incoming::Notification {
                                method: "account/updated".to_owned(),
                                params: match &incoming {
                                    Incoming::Notification { params, .. } => params.clone(),
                                    _ => Value::Null,
                                },
                            })
                            .is_err()
                        {
                            router.overflow.store(true, Ordering::Release);
                            (router.abort_media)();
                        }
                    }
                }
                if realtime {
                    if let Some(router) = voice_router.lock().expect("voice router").as_ref() {
                        if router.sender.try_send(incoming).is_err() {
                            router.overflow.store(true, Ordering::Release);
                            (router.abort_media)();
                        }
                        continue;
                    }
                }
                if tx.send(incoming).await.is_err() {
                    break;
                }
            }
            (None, None) => {}
        }
    }
    // EOF/read error: fail every awaiting request, then signal the loop.
    closed.store(true, Ordering::Release);
    pending.lock().expect("pending lock").clear();
    if let Some(router) = voice_router.lock().expect("voice router").as_ref() {
        (router.abort_media)();
    }
    let _ = tx.send(Incoming::Eof).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_notification_wire_has_no_id() {
        let (writer, mut receiver) = mpsc::channel(256);
        let client = RpcClient {
            next_id: Arc::new(AtomicI64::new(0)),
            pending: Arc::default(),
            writer,
            media_writer: mpsc::channel(8).0,
            closed: Arc::new(AtomicBool::new(false)),
            read_failure: Arc::default(),
            voice_router: Arc::default(),
        };
        client.notify("session/cancel", Some(json!({"sessionId": "parent"})));
        let frame: Value = serde_json::from_str(&receiver.try_recv().unwrap()).unwrap();
        assert_eq!(
            frame,
            json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": "parent"}})
        );
        assert!(frame.get("id").is_none());
        assert!(client.pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn requests_after_eof_fail_without_entering_pending_map() {
        let (writer, mut receiver) = mpsc::channel(256);
        let client = RpcClient {
            next_id: Arc::new(AtomicI64::new(0)),
            pending: Arc::default(),
            writer,
            media_writer: mpsc::channel(8).0,
            closed: Arc::new(AtomicBool::new(true)),
            read_failure: Arc::default(),
            voice_router: Arc::default(),
        };
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            client.request("session/prompt", json!({})),
        )
        .await
        .expect("a request after EOF cannot wait for another EOF");
        assert!(result.unwrap_err().to_string().contains("exited"));
        assert!(client.pending.lock().unwrap().is_empty());
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn cancelled_unpolled_request_cleans_pending() {
        let (writer, mut rx) = mpsc::channel(256);
        let client = RpcClient {
            next_id: Arc::new(AtomicI64::new(0)),
            pending: Arc::default(),
            writer,
            media_writer: mpsc::channel(8).0,
            closed: Arc::new(AtomicBool::new(false)),
            read_failure: Arc::default(),
            voice_router: Arc::default(),
        };
        let future = client.request_now("test", json!({}));
        assert_eq!(client.pending.lock().unwrap().len(), 1);
        drop(future);
        assert!(client.pending.lock().unwrap().is_empty());
        assert!(rx.try_recv().is_ok()); // Queued once; a late ACK is harmless.
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn text_notification_burst_preserves_backpressure_and_pending_response() {
        use crate::process::{Command, Stdio};
        let script = r#"import json, sys
for i in range(600):
    print(json.dumps({'jsonrpc':'2.0','method':'text/delta','params':{'sequence':i}}))
sys.stdout.flush()
request = json.loads(sys.stdin.readline())
print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':True}), flush=True)
sys.stdin.read()
"#;
        let mut child = Command::new("python3")
            .args(["-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let (client, mut incoming) =
            RpcClient::new(child.stdin.take().unwrap(), child.stdout.take().unwrap());
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while incoming.len() < 256 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // Let stdout's reader observe the full channel before consuming it.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!client.is_closed());
        let response = client.request_now("text/status", json!({}));
        for expected in 0..600 {
            let event = tokio::time::timeout(std::time::Duration::from_secs(5), incoming.recv())
                .await
                .unwrap()
                .unwrap();
            let Incoming::Notification { method, params } = event else {
                panic!("text peer retired during burst");
            };
            assert_eq!(method, "text/delta");
            assert_eq!(params["sequence"], expected);
        }
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(5), response)
                .await
                .unwrap()
                .unwrap(),
            json!(true)
        );
        drop(client);
        tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap();
    }

    #[cfg(unix)]
    fn python_peer(script: &str) -> (crate::process::Child, RpcClient, mpsc::Receiver<Incoming>) {
        use crate::process::{Command, Stdio};
        let mut child = Command::new("python3")
            .args(["-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let (client, incoming) =
            RpcClient::new(child.stdin.take().unwrap(), child.stdout.take().unwrap());
        (child, client, incoming)
    }

    /// A resumed long chat returns tens of MiB of history in one line.
    #[cfg(unix)]
    #[tokio::test]
    async fn resume_history_larger_than_any_small_message_cap_arrives_whole() {
        let script = r#"import json, sys
request = json.loads(sys.stdin.readline())
print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':{'history':'x'*(24*1024*1024)}}), flush=True)
sys.stdin.read()
"#;
        let (_child, client, _incoming) = python_peer(script);
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            client.request("thread/resume", json!({})),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result["history"].as_str().unwrap().len(), 24 * 1024 * 1024);
        assert!(!client.is_closed());
    }

    /// Unreadable stdout is not an exit: callers learn the actual cause.
    #[cfg(unix)]
    #[tokio::test]
    async fn unreadable_stdout_reports_its_cause_rather_than_an_exit() {
        let script = r#"import sys, time
sys.stdin.readline()
sys.stdout.buffer.write(b'\xff\xfe\n'); sys.stdout.flush()
time.sleep(30)
"#;
        let (mut child, client, _incoming) = python_peer(script);
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            client.request("thread/resume", json!({})),
        )
        .await
        .unwrap()
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("thread/resume: app-server stdout unreadable"),
            "{error}"
        );
        assert!(
            child.try_wait().unwrap().is_none(),
            "the peer is still alive"
        );
        // A fallback request on the retired client keeps the same cause.
        let error = client
            .request("thread/start", json!({}))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("thread/start: app-server stdout unreadable"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn control_overflow_retires_peer_and_fails_pending() {
        let (writer, _control) = mpsc::channel(1);
        let client = RpcClient {
            next_id: Arc::new(AtomicI64::new(0)),
            pending: Arc::default(),
            writer,
            media_writer: mpsc::channel(8).0,
            closed: Arc::new(AtomicBool::new(false)),
            read_failure: Arc::default(),
            voice_router: Arc::default(),
        };
        let pending = client.request_now("pending", json!({}));
        client.respond(&json!(7), json!({"approved":true}));
        assert!(client.is_closed());
        assert!(pending.await.is_err());
        assert!(client.pending.lock().unwrap().is_empty());
        assert!(client.request("next", json!({})).await.is_err());
    }

    #[tokio::test]
    async fn media_flood_is_bounded_and_stop_has_separate_capacity() {
        let (writer, mut control) = mpsc::channel(256);
        let (media_writer, media) = mpsc::channel(8);
        let client = RpcClient {
            next_id: Arc::new(AtomicI64::new(0)),
            pending: Arc::default(),
            writer,
            media_writer,
            closed: Arc::new(AtomicBool::new(false)),
            read_failure: Arc::default(),
            voice_router: Arc::default(),
        };
        let mut requests = Vec::new();
        for _ in 0..8 {
            requests.push(client.request_media("append", json!({})));
        }
        assert_eq!(media.len(), 8);
        assert!(client.request_media("append", json!({})).await.is_err());
        let stop = client.request_now("stop", json!({}));
        let wire: Value = serde_json::from_str(&control.try_recv().unwrap()).unwrap();
        assert_eq!(wire["method"], "stop");
        drop(stop);
        drop(requests);
        assert!(client.pending.lock().unwrap().is_empty());
    }

    #[test]
    fn rpc_error_preserves_string_and_structured_details() {
        for data in [
            json!("A prompt is already running"),
            json!({"reason": "A prompt is already running"}),
            json!(["busy", 7]),
        ] {
            let rendered = response_error(
                &json!({"code": -32600, "message": "Invalid request", "data": data}),
            );
            assert!(rendered.starts_with("Invalid request (code -32600): "));
            assert!(rendered.ends_with(data.as_str().unwrap_or(&data.to_string())));
        }
    }

    #[test]
    fn rpc_error_handles_missing_null_and_unstructured_fields() {
        for error in [
            json!({"message": "busy"}),
            json!({"message": "busy", "data": null}),
            json!({"message": "busy", "data": ""}),
        ] {
            assert_eq!(response_error(&error), "busy");
        }
        for error in [
            json!({"code": -32600, "data": {"reason": "busy"}}),
            json!("unstructured error"),
        ] {
            assert_eq!(response_error(&error), error.to_string());
        }
    }
}
