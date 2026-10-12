//! Split views on screen: each chat of the focused chat's split gets a pane
//! of its own — transcript and composer over a chat-view state that mirrors
//! the shell's (see [`AppState::side_chat_state`]). The shell's selection is
//! the focused pane, so the right pane, the explorer, the terminal and the
//! titlebar follow focus exactly as they follow navigation; only their
//! visibility belongs to the split as a whole (see [`Shell::layout_key`]).
use super::*;
use crate::split_view::{self, PlaceOutcome, SplitSide, SplitView};

pub(super) struct ChatPane {
    pub(super) state: Entity<AppState>,
    pub(super) transcript: Entity<Transcript>,
    pub(super) composer: Entity<Composer>,
    _events: Vec<Subscription>,
}

/// The hairline between two panes.
pub(super) const SPLIT_DIVIDER_WIDTH: f32 = 1.0;

/// Narrowest a pane gets from dragging a divider, and the room each pane
/// claims in the window fit.
pub(super) const SPLIT_PANE_MIN: f32 = 240.0;

/// Half the divider's grab target, either side of the hairline.
const SPLIT_DIVIDER_HITBOX_HALF: f32 = 4.0;

/// Drag marker for the divider after pane `divider` of split `split`.
pub(super) struct SplitDividerDrag {
    split: String,
    divider: usize,
}

/// Per-pane animation keys for the jump pill (they must be `'static`).
const JUMP_KEYS: [(&str, &str); split_view::SPLIT_MAX_PANES] = [
    ("split-jump-0", "split-jump-pill-0"),
    ("split-jump-1", "split-jump-pill-1"),
    ("split-jump-2", "split-jump-pill-2"),
];

/// Panel visibility for a split is shared by its panes.
pub(super) fn split_layout_key(split_id: &str) -> String {
    format!("split:{split_id}")
}

impl Shell {
    /// Splits of the active account profile; none until it is known.
    pub(super) fn split_views(&self, cx: &App) -> &[SplitView] {
        self.active_sidebar_pin_profile_key(cx)
            .and_then(|key| self.settings.split_views_by_profile.get(&key))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// Edit the active profile's splits, saving when anything changed.
    pub(super) fn update_split_views<R>(
        &mut self,
        cx: &mut Context<Self>,
        update: impl FnOnce(&mut Vec<SplitView>) -> R,
    ) -> Option<R> {
        let key = self.active_sidebar_pin_profile_key(cx)?;
        let layout_before = self.layout_key(cx);
        let views = self
            .settings
            .split_views_by_profile
            .entry(key.clone())
            .or_default();
        let before = views.clone();
        let result = update(views);
        let changed = *views != before;
        if views.is_empty() {
            self.settings.split_views_by_profile.remove(&key);
        }
        if changed {
            self.refresh_split_layout(cx);
            // The conversation on screen joined or left a split: its columns
            // stay as they are rather than switching to another entry's.
            let layout_after = self.layout_key(cx);
            if layout_after != layout_before && !self.active_chat.is_empty() {
                self.panels.carry_layout(&layout_before, &layout_after);
            }
            self.schedule_save(cx);
            cx.notify();
        }
        Some(result)
    }

    pub(super) fn split_of_chat(&self, chat_id: &str, cx: &App) -> Option<&SplitView> {
        split_view::split_of(self.split_views(cx), chat_id)
    }

    /// The split on screen: the focused chat's.
    pub(super) fn shown_split(&self, cx: &App) -> Option<&SplitView> {
        if !matches!(self.route, Route::Chat) || self.active_chat.is_empty() {
            return None;
        }
        self.split_of_chat(&self.active_chat, cx)
    }

    /// The key panel VISIBILITY lives under (open flags, open order,
    /// takeover): a split's panes share one, so moving focus between them
    /// never opens or closes a column. Panel CONTENT (surface tabs, the
    /// explorer) stays under [`Self::panel_key`], the focused chat's.
    pub(super) fn layout_key(&self, cx: &App) -> String {
        match self.shown_split(cx) {
            Some(view) => split_layout_key(&view.id),
            None => self.panel_key(cx),
        }
    }

    /// The panels entry the window fit reads (it runs without an `App`):
    /// the shown split's, else the active chat's.
    pub(super) fn fit_panels_key(&self) -> &str {
        self.split_layout
            .as_ref()
            .map_or(&self.active_chat, |(key, _)| key)
    }

    /// The width the conversation column must keep: one chat's, or room
    /// for each pane of the split on screen.
    pub(super) fn conversation_min(&self) -> f32 {
        match &self.split_layout {
            Some((_, panes)) => (SPLIT_PANE_MIN * *panes as f32).max(CHAT_PANEL_MIN),
            None => CHAT_PANEL_MIN,
        }
    }

    /// Re-derive [`Self::fit_panels_key`] after the selection or the splits
    /// changed.
    pub(super) fn refresh_split_layout(&mut self, cx: &App) {
        self.split_layout = (!self.active_chat.is_empty())
            .then(|| self.split_of_chat(&self.active_chat, cx))
            .flatten()
            .map(|view| (split_layout_key(&view.id), view.chats.len()));
    }

    /// [`Self::layout_key`] for a chat that may not be the active one.
    pub(super) fn layout_key_for(&self, chat_id: &str, cx: &App) -> String {
        match self.split_of_chat(chat_id, cx) {
            Some(view) if !chat_id.is_empty() => split_layout_key(&view.id),
            _ if chat_id.is_empty() => {
                crate::state::canvas_panel_key(self.state.read(cx).selected_space.as_deref())
            }
            _ => chat_id.to_string(),
        }
    }

    /// The composer typing goes to: the focused pane's in a split.
    pub(super) fn active_composer(&self) -> Entity<Composer> {
        self.split_panes
            .get(&self.active_chat)
            .map(|pane| pane.composer.clone())
            .unwrap_or_else(|| self.composer.clone())
    }

    /// The composer showing `chat_id`: its pane's in a split.
    pub(super) fn composer_for(&self, chat_id: &str) -> Entity<Composer> {
        self.split_panes
            .get(chat_id)
            .map(|pane| pane.composer.clone())
            .unwrap_or_else(|| self.composer.clone())
    }

    /// Whether `chat_id` is in a pane on screen.
    pub(super) fn split_pane_open(&self, chat_id: &str) -> bool {
        self.split_panes.contains_key(chat_id)
    }

    /// Chats that may join a split: listed, unarchived, top-level sessions.
    pub(super) fn splittable(&self, chat_id: &str, cx: &App) -> bool {
        self.state
            .read(cx)
            .chats
            .iter()
            .any(|chat| chat.id == chat_id && !chat.archived && chat.parent_chat_id.is_none())
    }

    /// Show `chat_id` beside `anchor`, focused: in the anchor's split or a
    /// new split of the two. Returns whether it was placed.
    pub(super) fn split_chat_beside(
        &mut self,
        anchor: &str,
        chat_id: &str,
        side: SplitSide,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.splittable(anchor, cx) || !self.splittable(chat_id, cx) {
            return false;
        }
        let outcome = self.update_split_views(cx, |views| {
            split_view::place(views, anchor, chat_id, side, || {
                uuid::Uuid::new_v4().to_string()
            })
        });
        match outcome {
            Some(PlaceOutcome::Placed(_)) => {
                self.open_chat(chat_id.to_string(), cx);
                // Placing beside the focused chat changes the split without a
                // selection change to announce it.
                self.sync_split_panes(cx);
                true
            }
            Some(PlaceOutcome::Full) => {
                self.show_notice(
                    crate::toast::ToastKind::Info,
                    format!("A split shows up to {} chats", split_view::SPLIT_MAX_PANES),
                    cx,
                );
                false
            }
            _ => false,
        }
    }

    /// Open `chat_id` beside the conversation on screen (Cmd/Ctrl-click,
    /// middle-click, the chat menu). With nothing open it just opens; a chat
    /// already on screen is focused.
    pub(super) fn open_chat_in_split(&mut self, chat_id: String, cx: &mut Context<Self>) {
        // The selection, not `active_chat`: that follows a frame later.
        let anchor = self
            .state
            .read(cx)
            .selected_chat
            .clone()
            .unwrap_or_default();
        if anchor.is_empty()
            || anchor == chat_id
            || self.split_pane_open(&chat_id)
            || !matches!(self.route, Route::Chat)
        {
            self.open_chat(chat_id, cx);
            return;
        }
        if !self.split_chat_beside(&anchor, &chat_id, SplitSide::Right, cx)
            && !self.splittable(&chat_id, cx)
        {
            self.open_chat(chat_id, cx);
        }
    }

    /// The chat menu's split row: "Remove from split view" for a member,
    /// "Open in split view" beside the conversation on screen otherwise.
    pub(super) fn chat_menu_split_row(
        &self,
        chat_id: &str,
        is_side_chat: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Stateful<gpui::Div>> {
        if chat_id.is_empty() || is_side_chat {
            return None;
        }
        let member = self.split_of_chat(chat_id, cx).is_some();
        let can_open = !member
            && matches!(self.route, Route::Chat)
            && !self.active_chat.is_empty()
            && self.active_chat != chat_id
            && self.splittable(chat_id, cx)
            && self.splittable(&self.active_chat, cx);
        if !member && !can_open {
            return None;
        }
        let id = chat_id.to_string();
        let label = if member {
            "Remove from split view"
        } else {
            "Open in split view"
        };
        Some(
            popover::menu_row(theme, false, format!("chat-menu-split-{chat_id}"))
                .id("chat-menu-split")
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.close_chat_menu(cx);
                    if member {
                        this.close_split_pane(&id, cx);
                    } else {
                        this.open_chat_in_split(id.clone(), cx);
                    }
                }))
                .child(
                    icon(icons::SPLIT_COLUMNS)
                        .size(px(16.0))
                        .text_color(theme.text_muted),
                )
                .child(SharedString::from(label)),
        )
    }

    /// Take a chat out of its split. A focused pane hands focus to its
    /// neighbour; a split left with one chat dissolves into it.
    pub(super) fn close_split_pane(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        let Some(view) = self.split_of_chat(chat_id, cx).cloned() else {
            return;
        };
        let neighbour = view.position(chat_id).and_then(|ix| {
            let other = if ix > 0 { ix - 1 } else { 1 };
            view.chats.get(other).cloned()
        });
        self.update_split_views(cx, |views| split_view::remove(views, chat_id));
        if self.active_chat == chat_id
            && let Some(neighbour) = neighbour
        {
            self.open_chat(neighbour, cx);
        }
        self.sync_split_panes(cx);
        cx.notify();
    }

    /// Focus a pane of the split on screen.
    pub(super) fn focus_split_pane(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        self.select_split_pane(chat_id, true, cx);
    }

    /// A press inside a pane focuses whatever it lands on, so only the
    /// keyboard and the titlebar move the caret into the pane's composer.
    fn select_split_pane(&mut self, chat_id: &str, focus_composer: bool, cx: &mut Context<Self>) {
        if self.state.read(cx).selected_chat.as_deref() == Some(chat_id) {
            return;
        }
        if focus_composer {
            self.open_chat(chat_id.to_string(), cx);
            return;
        }
        self.nav.push(NavEntry::Chat(chat_id.to_string()));
        self.state.update(cx, |state, cx| {
            state.select_chat(Some(chat_id.to_string()), cx)
        });
        self.sync_split_panes(cx);
        cx.notify();
    }

    /// Move focus to the pane left or right of the focused one, stopping at
    /// the ends.
    pub(super) fn step_split_focus(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(view) = self.shown_split(cx) else {
            return;
        };
        let Some(at) = view.position(&self.active_chat) else {
            return;
        };
        let target = if forward {
            view.chats.get(at + 1)
        } else {
            at.checked_sub(1).and_then(|ix| view.chats.get(ix))
        };
        if let Some(target) = target.cloned() {
            self.focus_split_pane(&target, cx);
        }
    }

    /// Keep the panes in step with the selection: a pane for each chat of
    /// the selected chat's split and none otherwise. Each chat's draft moves
    /// between the main composer and its pane as it enters or leaves.
    pub(super) fn sync_split_panes(&mut self, cx: &mut Context<Self>) {
        // Runs on every app-state change: nothing to do without splits.
        if self.split_panes.is_empty() && self.split_views(cx).is_empty() {
            return;
        }
        let (synced, keep) = {
            let state = self.state.read(cx);
            let keep = state
                .chats
                .iter()
                .filter(|chat| !chat.archived && chat.parent_chat_id.is_none())
                .map(|chat| chat.id.clone())
                .collect::<std::collections::HashSet<_>>();
            (state.chats_synced, keep)
        };
        if synced {
            self.update_split_views(cx, |views| split_view::prune(views, |id| keep.contains(id)));
        }
        let selected = self.state.read(cx).selected_chat.clone();
        if let Some(selected) = &selected {
            self.update_split_views(cx, |views| split_view::set_focused(views, selected));
        }
        let wanted = selected
            .as_deref()
            .and_then(|id| self.split_of_chat(id, cx))
            .map(|view| view.chats.clone())
            .unwrap_or_default();
        let gone = self
            .split_panes
            .keys()
            .filter(|id| !wanted.contains(id))
            .cloned()
            .collect::<Vec<_>>();
        for id in gone {
            let Some(pane) = self.split_panes.remove(&id) else {
                continue;
            };
            let draft = pane
                .composer
                .update(cx, |composer, cx| composer.take_chat_draft(&id, cx));
            self.composer.update(cx, |composer, cx| {
                composer.restore_chat_draft(&id, draft, cx)
            });
        }
        for id in wanted {
            if self.split_panes.contains_key(&id) {
                continue;
            }
            let Some(chat) = self
                .state
                .read(cx)
                .chats
                .iter()
                .find(|chat| chat.id == id)
                .cloned()
            else {
                continue;
            };
            let pane = self.new_chat_pane(chat, cx);
            let draft = self
                .composer
                .update(cx, |composer, cx| composer.take_chat_draft(&id, cx));
            pane.composer.update(cx, |composer, cx| {
                composer.restore_chat_draft(&id, draft, cx)
            });
            if selected.as_deref() == Some(id.as_str()) {
                pane.composer.update(cx, |composer, cx| {
                    composer.focus_pending = true;
                    cx.notify();
                });
            }
            self.split_panes.insert(id, pane);
        }
    }

    fn new_chat_pane(&mut self, chat: zeron_proto::Chat, cx: &mut Context<Self>) -> ChatPane {
        let chat_id = chat.id.clone();
        let parent = self.state.clone();
        let state = cx.new(|cx| AppState::side_chat_state(&parent, chat, false, cx));
        let transcript = cx.new(|cx| Transcript::new(state.clone(), cx));
        let links = Self::session_links(Some(chat_id), cx);
        transcript.update(cx, |transcript, _| {
            transcript.set_workspace_link_handler(links)
        });
        let composer = cx.new(|cx| {
            let mut composer = Composer::new(state.clone(), cx);
            // Only the selected chat's pane takes focus (see
            // `sync_split_panes`), not every pane as it mounts.
            composer.focus_pending = false;
            composer
        });
        let events = vec![
            cx.subscribe(&transcript, Self::on_transcript_event),
            cx.subscribe(&composer, {
                let transcript = transcript.clone();
                move |this: &mut Self, _, event, cx| match event {
                    ComposerEvent::WorkspaceCommand(command) => {
                        this.pending_workspace_command = Some(*command);
                        cx.notify();
                    }
                    // A pane always shows an existing chat: no canvas launch,
                    // no worktree of its own to set up.
                    ComposerEvent::NewThreadTransitionStarted
                    | ComposerEvent::WorktreeSetup { .. } => {}
                    ComposerEvent::Sent {
                        chat_id,
                        message_id,
                    } => transcript.update(cx, |t, cx| {
                        t.on_own_send(chat_id.clone(), message_id.clone(), cx)
                    }),
                    ComposerEvent::Queued {
                        chat_id,
                        message_id,
                    } => transcript.update(cx, |t, cx| {
                        t.on_own_queued_send(chat_id.clone(), message_id.clone(), cx)
                    }),
                }
            }),
            cx.observe(&state, |_, _, cx| cx.notify()),
        ];
        ChatPane {
            state,
            transcript,
            composer,
            _events: events,
        }
    }

    /// Pane widths for a column `width` wide, left to right.
    pub(super) fn split_pane_widths(view: &SplitView, width: f32) -> Vec<f32> {
        let n = view.chats.len();
        let available = (width - SPLIT_DIVIDER_WIDTH * n.saturating_sub(1) as f32).max(0.0);
        view.normalized_ratios()
            .into_iter()
            .map(|ratio| available * ratio)
            .collect()
    }

    /// The conversation column as the split's panes, side by side, with the
    /// terminal dock spanning all of them.
    pub(super) fn render_split(
        &mut self,
        view: SplitView,
        width: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let transcript_setting = settings::transcript_width(cx);
        let widths = Self::split_pane_widths(&view, width);
        let terminal_geometry =
            std::rc::Rc::new(std::cell::Cell::new(crate::terminal::dock::Geometry::new(
                self.eval_tween(self.terminal_tween, self.terminal_target(cx)),
                self.settings.terminal_height,
                (self.viewport_height * TERMINAL_MAX_VH).min(
                    (self.viewport_height - Theme::TITLEBAR_HEIGHT - Theme::STATUS_STRIP_HEIGHT)
                        .max(0.0),
                ),
            )));
        let mut row = div()
            .id("split-panes")
            .debug_selector(|| "split-panes".into())
            .flex_1()
            .min_h_0()
            .w_full()
            .flex()
            .flex_row();
        let count = view.chats.len();
        for (ix, chat_id) in view.chats.iter().enumerate() {
            let pane_width = widths.get(ix).copied().unwrap_or_default();
            let last = ix + 1 == count;
            let pane =
                self.render_split_pane(ix, chat_id, pane_width, last, transcript_setting, cx);
            if ix > 0 {
                row = row.child(self.render_split_divider(&view.id, ix - 1, &theme, cx));
            }
            row = row.child(pane);
        }
        div()
            .id("split-view")
            .debug_selector(|| "split-view".into())
            .on_drag_move::<SplitDividerDrag>(cx.listener(Self::drag_split_divider))
            .track_focus(&self.navigation_focus.main)
            .capture_any_mouse_down(cx.listener(|this, _, window, cx| {
                this.capture_navigation_focus(false, false, window, cx);
            }))
            .on_key_down(|event, _, cx| navigation_focus::copy_transcript_selection(event, cx))
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .child(row)
            .child(self.render_terminal_container(terminal_geometry, window, cx))
            .into_any_element()
    }

    /// The hairline between two panes, with a wider grab target over it.
    /// Dragging moves it; a double-click shares the width out evenly again.
    fn render_split_divider(
        &self,
        split: &str,
        divider: usize,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let hover_key = format!("split-divider-{divider}");
        let highlight = motion::hover_blend(&hover_key, theme.border, theme.border_strong);
        let (drag_split, reset_split) = (split.to_string(), split.to_string());
        div()
            .w(px(SPLIT_DIVIDER_WIDTH))
            .h_full()
            .flex_none()
            .relative()
            .bg(highlight)
            .child(
                div()
                    .id(("split-divider", divider))
                    .debug_selector(move || format!("split-divider-{divider}"))
                    .absolute()
                    .top(px(Theme::TITLEBAR_HEIGHT))
                    .bottom_0()
                    .left(px(-SPLIT_DIVIDER_HITBOX_HALF))
                    .w(px(SPLIT_DIVIDER_HITBOX_HALF * 2.0 + SPLIT_DIVIDER_WIDTH))
                    .occlude()
                    .cursor_col_resize()
                    .on_hover(motion::hover_listener(hover_key))
                    .on_drag(
                        SplitDividerDrag {
                            split: drag_split,
                            divider,
                        },
                        |_, _point: Point<Pixels>, _, cx| {
                            cx.stop_propagation();
                            cx.new(|_| DragGhost)
                        },
                    )
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseUpEvent, _, cx| {
                            if event.click_count == 2 {
                                let id = reset_split.clone();
                                this.update_split_views(cx, |views| {
                                    if let Some(view) = views.iter_mut().find(|v| v.id == id) {
                                        view.ratios.clear();
                                    }
                                });
                            }
                        }),
                    ),
            )
            .into_any_element()
    }

    fn drag_split_divider(
        &mut self,
        event: &gpui::DragMoveEvent<SplitDividerDrag>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let drag = event.drag(cx);
        let (split, divider) = (drag.split.clone(), drag.divider);
        let width = f32::from(event.bounds.size.width);
        if width <= 0.0 {
            return;
        }
        let pointer = (f32::from(event.event.position.x) - f32::from(event.bounds.left())) / width;
        self.update_split_views(cx, |views| {
            let Some(view) = views.iter_mut().find(|view| view.id == split) else {
                return;
            };
            let ratios = view.normalized_ratios();
            let edge: f32 = ratios.iter().take(divider + 1).sum();
            let min = (SPLIT_PANE_MIN / width).min(0.9 / ratios.len().max(1) as f32);
            view.resize(divider, pointer - edge, min);
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn render_split_pane(
        &mut self,
        ix: usize,
        chat_id: &str,
        width: f32,
        last: bool,
        transcript_setting: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(pane) = self.split_panes.get(chat_id) else {
            return div().w(px(width)).flex_none().h_full().into_any_element();
        };
        let transcript = pane.transcript.clone();
        let composer = pane.composer.clone();
        composer.update(cx, |composer, cx| {
            composer.set_dock_frame(crate::composer_dock::DockFrame::settled(true), cx);
            composer
                .set_available_width(composer_target_width(width, transcript_setting, true), cx);
        });
        let pill = transcript.read(cx).jump_button_shown().then(|| {
            let (anim, hover) = JUMP_KEYS[ix.min(JUMP_KEYS.len() - 1)];
            div()
                .absolute()
                .bottom(px(12.0))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(self.jump_pill(anim, hover, transcript.clone(), cx))
        });
        let focus_id = chat_id.to_string();
        let attach_overlay = Self::attachment_drop_overlay(Theme::of(cx));
        let drop_overlay = self.chat_drop_overlay(chat_id, cx);
        let composer_probe = self.chat_drop_composer_probe(chat_id);
        self.chat_dropzone(
            ("split-pane", ix),
            composer.clone(),
            Some(chat_id.to_string()),
            cx,
        )
        .debug_selector({
            let id = chat_id.to_string();
            move || format!("split-pane-{id}")
        })
        .capture_any_mouse_down(cx.listener(move |this, _, _, cx| {
            this.select_split_pane(&focus_id, false, cx);
        }))
        .h_full()
        .min_w_0()
        .when(last, |el| el.flex_1())
        .when(!last, |el| el.w(px(width)).flex_none())
        .flex()
        .flex_col()
        .child(
            div()
                .flex_1()
                .min_h_0()
                .relative()
                .child(
                    crate::edge_fade::edge_faded(
                        Theme::TRANSCRIPT_FADE_BAND,
                        true,
                        false,
                        div().size_full().child(transcript),
                    )
                    .inset_top(Theme::TITLEBAR_HEIGHT),
                )
                .children(pill),
        )
        .child(
            div()
                .flex_none()
                .relative()
                .child(composer_probe)
                .child(composer),
        )
        .child(attach_overlay)
        .children(drop_overlay)
        .into_any_element()
    }

    /// The titlebar over a split: each pane's title above it, the focused
    /// one raised. A title focuses its pane; its ✕ takes the chat out of the
    /// split. `row_left` is where the titlebar row starts.
    pub(super) fn render_split_titles(
        &mut self,
        view: &SplitView,
        row_left: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let column =
            self.main_content_width(self.main_target_width(self.right_visible_width(cx), cx), cx);
        let widths = Self::split_pane_widths(view, column);
        // The row starts inset from the first pane's left edge.
        let lead = (row_left - self.sidebar_now()).max(0.0);
        let mut row = div()
            .id("split-titles")
            .debug_selector(|| "split-titles".into())
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_row()
            .items_center()
            .overflow_hidden();
        let count = view.chats.len();
        for (ix, chat_id) in view.chats.iter().enumerate() {
            let (title, harness) = {
                let state = self.state.read(cx);
                let chat = state.chats.iter().find(|chat| &chat.id == chat_id);
                (
                    SharedString::from(transcript::single_line(
                        &chat
                            .and_then(|chat| chat.title.clone())
                            .unwrap_or_else(|| "New session".into()),
                    )),
                    chat.and_then(|chat| chat.config.as_ref().map(|config| config.harness)),
                )
            };
            let focused = *chat_id == self.active_chat;
            let mut width = widths.get(ix).copied().unwrap_or_default();
            if ix == 0 {
                width -= lead;
            } else {
                width += SPLIT_DIVIDER_WIDTH;
            }
            let group = SharedString::from(format!("split-title-{ix}"));
            let focus_id = chat_id.clone();
            let close_id = chat_id.clone();
            let title_el = div()
                .id(("split-title", ix))
                .debug_selector({
                    let id = chat_id.clone();
                    move || format!("split-title-{id}")
                })
                .group(group.clone())
                .max_w_full()
                .min_w_0()
                .h(px(24.0))
                .px(px(6.0))
                .rounded(px(6.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .cursor_pointer()
                .role(gpui::Role::Button)
                .aria_label(title.clone())
                .when(focused, |el| el.bg(crate::theme::wash(0.07)))
                .hover(|el| el.bg(crate::theme::wash(0.09)))
                // Keep the titlebar's drag region from claiming the press.
                .block_mouse_except_scroll()
                .on_mouse_down(gpui::MouseButton::Left, |_, window, _| {
                    window.prevent_default()
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.focus_split_pane(&focus_id, cx);
                }))
                .when_some(
                    harness.map(crate::pickers::harness_brand_icon),
                    |el, (path, tint)| {
                        el.child(
                            icon(path)
                                .size(px(14.0))
                                .flex_none()
                                .text_color(tint.unwrap_or(theme.text_muted)),
                        )
                    },
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(crate::typography::ui_rems(12.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(if focused {
                            theme.text.opacity(0.85)
                        } else {
                            theme.text_muted.opacity(0.75)
                        })
                        .child(title),
                )
                .child(
                    div()
                        .id(("split-title-close", ix))
                        .debug_selector({
                            let id = chat_id.clone();
                            move || format!("split-title-close-{id}")
                        })
                        .flex_none()
                        .size(px(18.0))
                        .rounded(px(4.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .opacity(0.0)
                        .group_hover(group, |el| el.opacity(1.0))
                        .hover(|el| el.bg(crate::theme::wash(0.12)))
                        .role(gpui::Role::Button)
                        .aria_label("Close pane")
                        .tooltip(crate::settings::widgets::text_tooltip("Close pane"))
                        .on_mouse_down(gpui::MouseButton::Left, |_, window, cx| {
                            window.prevent_default();
                            cx.stop_propagation();
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.close_split_pane(&close_id, cx);
                        }))
                        .child(
                            icon(icons::CLOSE)
                                .size(px(12.0))
                                .text_color(theme.text_muted),
                        ),
                );
            let slot = div()
                .h_full()
                .min_w_0()
                .flex()
                .items_center()
                .when(ix > 0, |el| el.pl(px(Theme::SPACE_LG - 6.0)))
                .when(ix + 1 == count, |el| el.flex_1())
                .when(ix + 1 < count, |el| el.w(px(width.max(0.0))).flex_none())
                .child(title_el);
            row = row.child(slot);
        }
        row.into_any_element()
    }
}
