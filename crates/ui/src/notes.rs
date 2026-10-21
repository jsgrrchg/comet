//! Session notes: one Markdown scratchpad per main chat, edited in a
//! right-pane tab. The text lives on the chat doc (`zeron_doc::notes`) and
//! this device's own engine serves it, never the chat's host, so notes stay
//! editable while the host is offline.
//!
//! Writes are compare-and-set against the hash of the text this surface last
//! saw from the engine, its base. One write is in flight at a time. Changes
//! from other devices that land meanwhile are held, then merged into the
//! editor with `rebase_notes`, which keeps both sides' typing; whatever local
//! text remains unwritten goes out with the next write. The watch also reports
//! this surface's own writes back, sometimes after a newer one has landed;
//! `NotesEchoes` keeps those from merging as reverts.

use std::time::Duration;

use gpui::{
    App, AppContext as _, Context, Entity, Focusable as _, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Subscription, Task, Window, div, px,
};
use zeron_doc::{
    NotesEchoes, NotesSnapshot, NotesWrite, notes_hash, rebase_notes, single_range_diff,
};
use zeron_rpc::methods;

use crate::composer::{ComposerInput, ComposerInputEvent, DOCUMENT_INPUT_CONTEXT};
use crate::state::AppState;
use crate::theme::Theme;

/// Quiet time after the last keystroke before the notes are written.
const WRITE_DEBOUNCE: Duration = Duration::from_millis(750);
/// Wait before redialing a dropped watch or retrying a failed write.
const RETRY_DELAY: Duration = Duration::from_secs(2);

pub struct NotesSurface {
    chat_id: String,
    state: Entity<AppState>,
    input: Entity<ComposerInput>,
    /// The notes as the engine last reported them: the base of the next
    /// write. `None` until the first snapshot arrives.
    base: Option<NotesSnapshot>,
    /// A snapshot from another device that arrived during a write.
    held: Option<NotesSnapshot>,
    /// What this surface wrote recently, so the watch echoing it back is not
    /// mistaken for another device's change.
    echoes: NotesEchoes,
    in_flight: bool,
    error: Option<SharedString>,
    focus_pending: bool,
    write_task: Option<Task<()>>,
    _watch: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl NotesSurface {
    pub fn new(
        chat_id: String,
        state: Entity<AppState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            let mut input = ComposerInput::with_context(
                "Notes for this session. Markdown works here.",
                DOCUMENT_INPUT_CONTEXT,
                cx,
            )
            .with_markdown()
            .with_fill_height();
            // Nothing to edit until the engine says what is there.
            input.read_only = true;
            input
        });
        let edited = cx.subscribe(&input, |this, _, event: &ComposerInputEvent, cx| {
            if matches!(event, ComposerInputEvent::Edited) && this.base.is_some() {
                this.schedule_write(WRITE_DEBOUNCE, cx);
            }
        });
        // Leaving the editor writes now rather than after the debounce.
        let blurred = cx.on_blur(&input.focus_handle(cx), window, |this, _, cx| {
            this.flush(cx);
        });
        let watch = Self::spawn_watch(chat_id.clone(), cx);
        Self {
            chat_id,
            state,
            input,
            base: None,
            held: None,
            echoes: NotesEchoes::default(),
            in_flight: false,
            error: None,
            focus_pending: true,
            write_task: None,
            _watch: watch,
            _subscriptions: vec![edited, blurred],
        }
    }

    pub fn chat_id(&self) -> &str {
        &self.chat_id
    }

    /// Put the caret in the editor on the next frame.
    pub fn request_focus(&mut self, cx: &mut Context<Self>) {
        self.focus_pending = true;
        cx.notify();
    }

    /// Write any unwritten edits now (tab closing, editor blurred). The write
    /// outlives this surface.
    pub fn flush(&mut self, cx: &mut Context<Self>) {
        self.write_task = None;
        self.write(cx);
    }

    fn spawn_watch(chat_id: String, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                let Ok(engine) = this.update(cx, |this, cx| this.state.read(cx).engine().cloned())
                else {
                    return;
                };
                if let Some(engine) = engine {
                    let params = serde_json::json!({ "chatId": chat_id });
                    match engine
                        .client()
                        .subscribe_checked(methods::WATCH_SESSION_NOTES, params)
                        .await
                    {
                        Ok(mut rx) => {
                            while let Some(value) = rx.recv().await {
                                let snapshot: NotesSnapshot = match serde_json::from_value(value) {
                                    Ok(snapshot) => snapshot,
                                    Err(err) => {
                                        tracing::warn!(error = %err, "dropping malformed notes frame");
                                        continue;
                                    }
                                };
                                if this
                                    .update(cx, |this, cx| this.receive(snapshot, cx))
                                    .is_err()
                                {
                                    return;
                                }
                            }
                        }
                        Err(err) => {
                            tracing::debug!(%chat_id, error = %err, "notes watch failed; retrying");
                        }
                    }
                }
                if this.update(cx, |_, _| {}).is_err() {
                    return;
                }
                cx.background_executor().timer(RETRY_DELAY).await;
            }
        })
    }

    fn receive(&mut self, remote: NotesSnapshot, cx: &mut Context<Self>) {
        let Some(base) = &self.base else {
            self.input.update(cx, |input, cx| {
                input.read_only = false;
                input.set_text(remote.text.clone(), cx);
            });
            self.base = Some(remote);
            cx.notify();
            return;
        };
        if remote.hash == base.hash || self.echoes.contains(&remote.hash) {
            return;
        }
        if self.in_flight {
            self.held = Some(remote);
            return;
        }
        self.merge(remote, cx);
    }

    /// Fold another device's notes into the editor, keeping local edits that
    /// the engine has not seen yet.
    fn merge(&mut self, remote: NotesSnapshot, cx: &mut Context<Self>) {
        let base = self.base.take().unwrap_or_default();
        let local = self.input.read(cx).text().to_owned();
        let merged = rebase_notes(&base.text, &local, &remote.text);
        if let Some(edit) = single_range_diff(&local, &merged) {
            self.input.update(cx, |input, cx| {
                input.apply_external_edit(edit.range, &edit.insert, cx)
            });
        }
        let unwritten = merged != remote.text;
        self.base = Some(remote);
        if unwritten {
            self.schedule_write(Duration::ZERO, cx);
        }
        cx.notify();
    }

    fn schedule_write(&mut self, delay: Duration, cx: &mut Context<Self>) {
        self.write_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |this, cx| this.write(cx)).ok();
        }));
    }

    fn write(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some((text, params)) = self.begin_write(cx) else {
            return;
        };
        // Detached: a closing tab's last write still lands.
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call_as::<NotesWrite>(methods::SET_SESSION_NOTES, params)
                .await;
            this.update(cx, |this, cx| this.written(text, result, cx))
                .ok();
        })
        .detach();
    }

    /// The text to write and its request, if there is anything to write and
    /// no write in flight (that one checks for newer edits when it lands).
    fn begin_write(&mut self, cx: &App) -> Option<(String, serde_json::Value)> {
        if self.in_flight {
            return None;
        }
        let base = self.base.as_ref()?;
        let text = self.input.read(cx).text().to_owned();
        if text == base.text {
            return None;
        }
        let params = serde_json::json!({
            "chatId": self.chat_id,
            "baseHash": base.hash,
            "text": text,
        });
        // Before sending: the watch can report the write before it returns.
        self.echoes.record(notes_hash(&text));
        self.in_flight = true;
        Some((text, params))
    }

    fn written(
        &mut self,
        text: String,
        result: Result<NotesWrite, zeron_rpc::RpcError>,
        cx: &mut Context<Self>,
    ) {
        self.in_flight = false;
        let mut retry = Some(Duration::ZERO);
        match result {
            Ok(NotesWrite::Applied { hash }) => {
                self.error = None;
                self.base = Some(NotesSnapshot { text, hash });
            }
            Ok(NotesWrite::Conflict { current }) if self.echoes.contains(&current.hash) => {
                // An earlier write of ours landed though its reply was lost:
                // the editor already holds it, so it is the base now.
                self.error = None;
                self.base = Some(current);
            }
            Ok(NotesWrite::Conflict { current }) => {
                self.error = None;
                self.held = Some(current);
            }
            Ok(NotesWrite::TooLarge { limit }) => {
                self.error = Some(
                    format!(
                        "Notes are limited to {} KB. The latest changes aren't saved.",
                        limit / 1024
                    )
                    .into(),
                );
                // The next edit tries again; retrying the same text can't help.
                retry = None;
            }
            Err(err) => {
                tracing::debug!(chat = %self.chat_id, error = %err, "notes write failed");
                self.error = Some("Couldn't save the notes. Retrying…".into());
                retry = Some(RETRY_DELAY);
            }
        }
        if let Some(remote) = self.held.take()
            && !self.echoes.contains(&remote.hash)
            && self
                .base
                .as_ref()
                .is_none_or(|base| base.hash != remote.hash)
        {
            self.merge(remote, cx);
        }
        let unwritten = self
            .base
            .as_ref()
            .is_some_and(|base| base.text != self.input.read(cx).text());
        if unwritten && let Some(delay) = retry {
            self.schedule_write(delay, cx);
        }
        cx.notify();
    }
}

impl Render for NotesSurface {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.focus_pending) {
            window.focus(&self.input.focus_handle(cx), cx);
        }
        let theme = Theme::of(cx);
        div()
            .id("session-notes")
            .size_full()
            .flex()
            .flex_col()
            .child(
                div().flex_1().min_h_0().relative().child(
                    div()
                        .absolute()
                        .inset_0()
                        .px(px(20.0))
                        .pt(px(14.0))
                        .pb(px(10.0))
                        .child(self.input.clone()),
                ),
            )
            .children(self.error.clone().map(|error| {
                div()
                    .flex_none()
                    .px(px(20.0))
                    .pb(px(12.0))
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.danger)
                    .child(error)
            }))
    }
}

impl gpui::Focusable for NotesSurface {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.input.focus_handle(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    fn surface(cx: &mut TestAppContext) -> (Entity<NotesSurface>, &mut gpui::VisualTestContext) {
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::dark());
            crate::composer::init(cx, Default::default());
        });
        let (surface, cx) = cx.add_window_view(|window, cx| {
            let state = cx.new(|_| AppState::new());
            NotesSurface::new("chat".into(), state, window, cx)
        });
        (surface, cx)
    }

    fn snapshot(text: &str) -> NotesSnapshot {
        NotesSnapshot::of(text)
    }

    fn type_text(surface: &mut NotesSurface, text: &str, cx: &mut Context<NotesSurface>) {
        surface.input.update(cx, |input, cx| {
            input.apply_external_edit(0..input.text().len(), text, cx)
        });
    }

    fn applied(text: &str) -> Result<NotesWrite, zeron_rpc::RpcError> {
        Ok(NotesWrite::Applied {
            hash: notes_hash(text),
        })
    }

    #[gpui::test]
    fn a_late_echo_of_an_older_write_does_not_revert_the_editor(cx: &mut TestAppContext) {
        let (surface, cx) = surface(cx);
        surface.update(cx, |surface, cx| {
            surface.receive(snapshot("a"), cx);
            assert!(!surface.input.read(cx).read_only);
            // First write lands; its echo is still on the way.
            type_text(surface, "ab", cx);
            let (text, _) = surface.begin_write(cx).unwrap();
            surface.written(text, applied("ab"), cx);
            // Second write goes out; the first one's echo arrives meanwhile.
            type_text(surface, "abc", cx);
            let (text, _) = surface.begin_write(cx).unwrap();
            surface.receive(snapshot("ab"), cx);
            surface.written(text, applied("abc"), cx);
            // And once more after it, as a straggler.
            surface.receive(snapshot("ab"), cx);
            assert_eq!(surface.input.read(cx).text(), "abc");
            assert_eq!(surface.base.as_ref().unwrap().text, "abc");
        });
    }

    #[gpui::test]
    fn another_devices_change_merges_with_unwritten_typing(cx: &mut TestAppContext) {
        let (surface, cx) = surface(cx);
        surface.update(cx, |surface, cx| {
            surface.receive(snapshot("line\n"), cx);
            type_text(surface, "line\nmine\n", cx);
            surface.receive(snapshot("theirs\nline\n"), cx);
            assert_eq!(surface.input.read(cx).text(), "theirs\nline\nmine\n");
            assert_eq!(surface.base.as_ref().unwrap().text, "theirs\nline\n");
            // During a write the change waits for it, then merges.
            let (text, _) = surface.begin_write(cx).unwrap();
            surface.receive(snapshot("theirs\nline\nmine\nmore\n"), cx);
            assert_eq!(surface.input.read(cx).text(), "theirs\nline\nmine\n");
            surface.written(
                text,
                Ok(NotesWrite::Conflict {
                    current: snapshot("theirs\nline\nmore\n"),
                }),
                cx,
            );
            // The conflict's current state is the newest: merged against it,
            // their insertion first where both typed at the same spot.
            assert_eq!(surface.input.read(cx).text(), "theirs\nline\nmore\nmine\n");
        });
    }

    #[gpui::test]
    fn a_conflict_naming_our_own_write_adopts_it(cx: &mut TestAppContext) {
        let (surface, cx) = surface(cx);
        surface.update(cx, |surface, cx| {
            surface.receive(snapshot("x"), cx);
            type_text(surface, "xy", cx);
            let (text, _) = surface.begin_write(cx).unwrap();
            // The write landed but its reply was lost.
            surface.written(text, Err(zeron_rpc::RpcError::Closed), cx);
            type_text(surface, "xyz", cx);
            let (text, _) = surface.begin_write(cx).unwrap();
            surface.written(
                text,
                Ok(NotesWrite::Conflict {
                    current: snapshot("xy"),
                }),
                cx,
            );
            // No duplicated "y": the editor already held that write.
            assert_eq!(surface.input.read(cx).text(), "xyz");
            assert_eq!(surface.base.as_ref().unwrap().text, "xy");
        });
    }
}
