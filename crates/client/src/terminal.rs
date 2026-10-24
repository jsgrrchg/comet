//! Terminals on a chat's host: the engine's PTY sessions (`OpenTerminal`,
//! `SubscribeTerminal`, `WriteTerminal`, `ResizeTerminal`, `CloseTerminal`),
//! reached over the device relay like every other host RPC.
//!
//! A [`TerminalLink`] keeps one terminal attached: its output stream resumes
//! from the last sequence number after any drop (the host keeps a replay
//! window and the shell outlives its viewers), and input is numbered so a
//! write retried after a reconnect lands once. Typing never waits on the
//! network — keystrokes queue and go out batched, one call in flight.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use zeron_proto::TerminalEvent;
use zeron_rpc::methods;

use crate::client::{Client, ClientInner};
use crate::error::{ClientError, Result};
use crate::live::b64;

/// An open terminal and where it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalInfo {
    pub id: String,
    pub device_id: String,
    pub cwd: String,
    /// Shell basename (`zsh`, `bash`, …).
    pub shell: String,
    /// The host answers the terminal's queries (device attributes, cursor
    /// position…) itself. A host from before that leaves them to the
    /// viewer, which then writes its emulator's answers back.
    pub host_replies: bool,
}

/// The host takes at most this much input per write (`MAX_INPUT_BYTES`).
const MAX_WRITE: usize = 64 * 1024;

/// What a link reports as it runs.
#[derive(Debug, Clone, PartialEq)]
pub enum TerminalUpdate {
    /// The output stream is (re)attached.
    Connected,
    /// PTY output, raw bytes (not necessarily valid UTF-8).
    Output(Vec<u8>),
    /// The shell exited; the stream is over.
    Exited { code: i32, signal: Option<String> },
    /// The stream dropped; the next attempt is `retry_in_ms` away.
    Reconnecting { retry_in_ms: u64 },
    /// The host no longer has this terminal (it exited long ago, or the
    /// host restarted).
    Gone,
}

/// Receives a link's updates on the client runtime.
pub trait TerminalSink: Send + Sync {
    fn update(&self, update: TerminalUpdate);
}

enum Op {
    Input(Vec<u8>),
    Resize(u16, u16),
    Close,
}

/// One attached terminal. Dropping it detaches (the shell keeps running on
/// the host); [`TerminalLink::close`] ends it — the close still goes out if
/// the link is dropped right after (only the output stream stops at once).
pub struct TerminalLink {
    info: TerminalInfo,
    ops: mpsc::UnboundedSender<Op>,
    stream: CancellationToken,
}

/// Writes the terminal's own query answers back (hosts without host
/// replies). Doesn't keep the link alive.
#[derive(Clone)]
pub struct TerminalReplies(mpsc::WeakUnboundedSender<Op>);

impl TerminalReplies {
    pub fn write(&self, bytes: Vec<u8>) {
        if let (false, Some(ops)) = (bytes.is_empty(), self.0.upgrade()) {
            let _ = ops.send(Op::Input(bytes));
        }
    }
}

impl TerminalLink {
    pub fn info(&self) -> &TerminalInfo {
        &self.info
    }

    /// Bytes for the shell (typed keys, a paste). Queued in order.
    pub fn write(&self, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            let _ = self.ops.send(Op::Input(bytes));
        }
    }

    /// The viewport's grid size changed.
    pub fn resize(&self, cols: u16, rows: u16) {
        let _ = self.ops.send(Op::Resize(cols.max(2), rows.max(1)));
    }

    /// End the shell on the host, then stop.
    pub fn close(&self) {
        let _ = self.ops.send(Op::Close);
    }

    /// For answering the terminal's queries when the host doesn't.
    pub fn replies(&self) -> TerminalReplies {
        TerminalReplies(self.ops.downgrade())
    }
}

impl Drop for TerminalLink {
    /// The output stream stops; queued writes (a close above all) drain,
    /// then the writer ends with the channel.
    fn drop(&mut self) {
        self.stream.cancel();
    }
}

impl Client {
    /// Open a shell for `chat_id` on its host, in the chat's working folder.
    pub async fn open_terminal(&self, chat_id: &str, cols: u16, rows: u16) -> Result<TerminalInfo> {
        let device_id = self.chat_host(chat_id)?;
        let reply = self
            .inner
            .host_rpc(
                &device_id,
                methods::OPEN_TERMINAL,
                json!({ "chatId": chat_id, "cols": cols.max(2), "rows": rows.max(1) }),
            )
            .await?;
        let session: zeron_proto::TerminalSession = serde_json::from_value(reply)
            .map_err(|e| ClientError::HostError(format!("OpenTerminal: {e}")))?;
        Ok(TerminalInfo {
            id: session.id,
            device_id,
            cwd: session.cwd,
            shell: session.shell,
            host_replies: session.host_replies,
        })
    }

    /// Attach to an open terminal: replays everything after `after_seq`
    /// (0 = the host's whole replay window), then follows live output.
    pub fn attach_terminal(
        &self,
        info: TerminalInfo,
        after_seq: u64,
        sink: Arc<dyn TerminalSink>,
    ) -> TerminalLink {
        let stream = CancellationToken::new();
        let (ops, rx) = mpsc::unbounded_channel();
        let ended = Arc::new(AtomicBool::new(false));
        let weak = Arc::downgrade(&self.inner);
        crate::runtime::shared().spawn(read_loop(
            weak.clone(),
            info.clone(),
            after_seq,
            sink,
            ended.clone(),
            stream.clone(),
        ));
        crate::runtime::shared().spawn(write_loop(weak, info.clone(), rx, ended));
        TerminalLink { info, ops, stream }
    }

    /// The device hosting `chat_id`.
    pub(crate) fn chat_host(&self, chat_id: &str) -> Result<String> {
        self.inner
            .workspace
            .chat(chat_id)
            .map(|c| c.device_id)
            .ok_or_else(|| ClientError::NotFound(chat_id.to_owned()))
    }
}

/// Follow the output stream, resuming from the last sequence seen after
/// every drop (0.4s → 5s backoff).
async fn read_loop(
    weak: std::sync::Weak<ClientInner>,
    info: TerminalInfo,
    mut last_seq: u64,
    sink: Arc<dyn TerminalSink>,
    ended: Arc<AtomicBool>,
    cancel: CancellationToken,
) {
    let mut backoff = Duration::from_millis(400);
    loop {
        let Some(inner) = weak.upgrade() else { return };
        let params = json!({
            "terminalId": info.id,
            "afterSeq": last_seq,
            // A host that answers terminal queries itself does, so a phone
            // over a slow link never answers them a second time, late.
            "hostReplies": info.host_replies,
        });
        let subscribe = inner.host_subscribe(&info.device_id, methods::SUBSCRIBE_TERMINAL, params);
        let stream = tokio::select! {
            _ = cancel.cancelled() => return,
            stream = subscribe => stream,
        };
        drop(inner);
        match stream {
            Ok(mut stream) => {
                sink.update(TerminalUpdate::Connected);
                loop {
                    let item = tokio::select! {
                        _ = cancel.cancelled() => return,
                        item = stream.recv() => item,
                    };
                    let Some(item) = item else { break };
                    backoff = Duration::from_millis(400);
                    match serde_json::from_value::<TerminalEvent>(item) {
                        Ok(TerminalEvent::Data { seq, data }) => {
                            if seq <= last_seq && last_seq > 0 {
                                continue;
                            }
                            last_seq = seq;
                            if let Some(bytes) = b64::decode(&data) {
                                sink.update(TerminalUpdate::Output(bytes));
                            }
                        }
                        Ok(TerminalEvent::Exit {
                            exit_code, signal, ..
                        }) => {
                            ended.store(true, Ordering::Release);
                            sink.update(TerminalUpdate::Exited {
                                code: exit_code,
                                signal,
                            });
                            return;
                        }
                        Err(_) => {}
                    }
                }
            }
            Err(ClientError::HostError(message)) if message.contains("Terminal not found") => {
                ended.store(true, Ordering::Release);
                sink.update(TerminalUpdate::Gone);
                return;
            }
            Err(ClientError::Unsupported(_)) => {
                ended.store(true, Ordering::Release);
                sink.update(TerminalUpdate::Gone);
                return;
            }
            Err(_) => {}
        }
        sink.update(TerminalUpdate::Reconnecting {
            retry_in_ms: backoff.as_millis() as u64,
        });
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(Duration::from_secs(5));
    }
}

/// Input, resizes and close, in order, one call in flight. Keystrokes that
/// queue behind a call go out together in the next (up to the host's cap).
/// Runs until the link's channel closes and drains — a close queued right
/// before the link was dropped still reaches the host.
async fn write_loop(
    weak: std::sync::Weak<ClientInner>,
    info: TerminalInfo,
    mut ops: mpsc::UnboundedReceiver<Op>,
    ended: Arc<AtomicBool>,
) {
    let client_id = crate::new_id();
    let mut input_seq = 0u64;
    // Ops taken off the channel but not run yet, in order.
    let mut front: VecDeque<Op> = VecDeque::new();
    loop {
        let op = match front.pop_front() {
            Some(op) => op,
            None => match ops.recv().await {
                Some(op) => op,
                None => return,
            },
        };
        let Some(inner) = weak.upgrade() else { return };
        match op {
            Op::Input(mut bytes) => {
                // Batch what was typed meanwhile (only straight off the
                // channel, so nothing jumps the queue); a resize or close
                // waits its turn behind this write.
                while front.is_empty()
                    && bytes.len() < MAX_WRITE
                    && let Ok(next) = ops.try_recv()
                {
                    match next {
                        Op::Input(more) => bytes.extend(more),
                        other => front.push_back(other),
                    }
                }
                if bytes.len() > MAX_WRITE {
                    let rest = bytes.split_off(floor_char_boundary(&bytes, MAX_WRITE));
                    front.push_front(Op::Input(rest));
                }
                send_input(
                    &inner,
                    &info,
                    &client_id,
                    &mut input_seq,
                    bytes,
                    &ops,
                    &ended,
                )
                .await;
            }
            Op::Resize(mut cols, mut rows) => {
                // Only the latest size matters.
                while front.is_empty()
                    && let Ok(next) = ops.try_recv()
                {
                    match next {
                        Op::Resize(c, r) => (cols, rows) = (c, r),
                        other => front.push_back(other),
                    }
                }
                if !ended.load(Ordering::Acquire) {
                    let _ = inner
                        .host_rpc(
                            &info.device_id,
                            methods::RESIZE_TERMINAL,
                            json!({ "terminalId": info.id, "cols": cols, "rows": rows }),
                        )
                        .await;
                }
            }
            Op::Close => {
                let _ = inner
                    .host_rpc(
                        &info.device_id,
                        methods::CLOSE_TERMINAL,
                        json!({ "terminalId": info.id }),
                    )
                    .await;
                // The host ends the stream with the shell's exit, which the
                // reader reports; a dropped link already stopped it.
                return;
            }
        }
    }
}

/// A cut at or below `at` that doesn't split a UTF-8 sequence (a paste's
/// text; raw control bytes are single bytes either way).
fn floor_char_boundary(bytes: &[u8], at: usize) -> usize {
    let mut cut = at.min(bytes.len());
    while cut > 0 && (bytes[cut] & 0xC0) == 0x80 {
        cut -= 1;
    }
    cut
}

/// One numbered write. A retry after a dropped reply lands once (the host
/// dedupes by client and sequence), so it keeps trying through a relay
/// reconnect while the link is alive; once the link is gone it gives up
/// after a few tries. A host refusal (an ended shell) isn't retried.
async fn send_input(
    inner: &Arc<ClientInner>,
    info: &TerminalInfo,
    client_id: &str,
    input_seq: &mut u64,
    bytes: Vec<u8>,
    ops: &mpsc::UnboundedReceiver<Op>,
    ended: &AtomicBool,
) {
    if ended.load(Ordering::Acquire) || bytes.is_empty() {
        return;
    }
    *input_seq += 1;
    let params = json!({
        "terminalId": info.id,
        "data": b64::encode(&bytes),
        "clientId": client_id,
        "inputSeq": *input_seq,
    });
    let mut delay = Duration::from_millis(250);
    for attempt in 0u32.. {
        match inner
            .host_rpc(&info.device_id, methods::WRITE_TERMINAL, params.clone())
            .await
        {
            Ok(_) | Err(ClientError::HostError(_) | ClientError::Unsupported(_)) => return,
            Err(_) => {}
        }
        if ended.load(Ordering::Acquire) || (ops.is_closed() && attempt >= 3) {
            return;
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(2));
    }
}
