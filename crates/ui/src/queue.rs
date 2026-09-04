//! The pending-message queue, docked above the composer.
//!
//! Everything you typed while the agent was busy, in the order it will be sent.
//! The rows live on the session doc ([`zeron_doc::QueuedMessage`]), so the phone
//! shows the same queue and either device can reorder it.
//!
//! Each row exposes one honest primary action: `Steer` when the selected agent
//! can accept text inside its live turn, otherwise `Send now`. Editing a row to
//! nothing IS dropping it: emptying the box you just filled is a clear enough
//! statement that "delete" would only be a second way to say it.

use gpui::{
    AnyElement, Context, Focusable as _, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, SharedString, StatefulInteractiveElement as _, Styled, Window, div, prelude::*, px,
};

use zeron_doc::QueuedMessage;
use zeron_rpc::methods;

use crate::composer::Composer;
use crate::icons::{self, icon};
use crate::motion::{self, AnimationExt as _, TAB_SLIDE};
use crate::terminal::panel::{drop_index, slide_offset};
use crate::theme::Theme;

/// Queue rows are replicated CRDT state, so ordinary mutations deliberately
/// land on the local engine. Only operations that hand a row to the agent must
/// execute on the chat's owning device.
fn queue_action_needs_host(method: &str) -> bool {
    matches!(
        method,
        methods::SEND_QUEUED_MESSAGE_NOW | methods::STEER_QUEUED_MESSAGE_NOW
    )
}

/// Queue mutation replies are deliberately explicit. A false or malformed
/// acknowledgement means an optimistic local edit may not match the document
/// (for example, another device removed the same row first).
fn queue_mutation_acknowledged(method: &str, reply: &serde_json::Value) -> bool {
    let field = match method {
        methods::UPDATE_QUEUED_MESSAGE | methods::MOVE_QUEUED_MESSAGE => "changed",
        methods::REMOVE_QUEUED_MESSAGE => "removed",
        methods::SEND_QUEUED_MESSAGE_NOW | methods::STEER_QUEUED_MESSAGE_NOW => "sent",
        _ => return true,
    };
    reply.get(field).and_then(serde_json::Value::as_bool) == Some(true)
}

struct QueueActionTooltip {
    label: SharedString,
}

impl Render for QueueActionTooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        div()
            .px(px(8.0))
            .py(px(5.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface_overlay)
            .text_size(px(10.5))
            .text_color(theme.text_muted)
            .child(self.label.clone())
    }
}

/// Each queued prompt is a compact raised card inside the outlined queue.
const ROW_HEIGHT: f32 = 38.0;
const ROW_GAP: f32 = 6.0;
const ROW_SLOT: f32 = ROW_HEIGHT + ROW_GAP;
const ROW_PAD_X: f32 = 10.0;
const LEAD: f32 = 14.0;
const PANEL_RADIUS: f32 = 14.0;
/// The body begins one pixel before the open-bottom tab ends, so the tab's
/// fill masks the body's top border and both read as one continuous outline.
const PANEL_TAB_HEIGHT: f32 = 27.0;
const PANEL_TOP_PAD: f32 = PANEL_TAB_HEIGHT - 1.0;
const BODY_ROWS_PAD_TOP: f32 = 13.0;

/// The single trailing action a queue row advertises and executes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueuePrimaryAction {
    Steer,
    SendNow,
}

impl QueuePrimaryAction {
    fn label(self) -> &'static str {
        match self {
            Self::Steer => "Steer",
            Self::SendNow => "Send now",
        }
    }

    fn tooltip(self) -> &'static str {
        match self {
            Self::Steer => "Steer without interrupting",
            Self::SendNow => "Send now (interrupt)",
        }
    }

    fn glyph(self) -> &'static str {
        match self {
            Self::Steer => icons::RETURN,
            Self::SendNow => icons::ARROW_RIGHT,
        }
    }
}

/// Attachments cannot travel through the text-only steering channel. Unknown
/// catalogs are conservative too: never promise a non-interrupting steer until
/// the selected provider has advertised it.
fn queue_primary_action(
    resolved_mid_turn_steering: Option<bool>,
    has_attachments: bool,
) -> Option<QueuePrimaryAction> {
    match resolved_mid_turn_steering {
        Some(true) if !has_attachments => Some(QueuePrimaryAction::Steer),
        Some(_) => Some(QueuePrimaryAction::SendNow),
        None => None,
    }
}

/// Translate a pointer inside the whole outlined panel into a row slot. The
/// label and the body's top padding both belong to slot zero; the bottom pad
/// clamps to the final row.
fn queue_drop_index(panel_y: f32, count: usize) -> usize {
    drop_index(panel_y - PANEL_TOP_PAD - BODY_ROWS_PAD_TOP, ROW_SLOT, count)
}

/// Paint-only start and target positions for the PR #90 reorder treatment.
/// The dragged row travels to the hovered slot while every row in its path
/// slides into the space it leaves behind.
fn queue_drag_offsets(ix: usize, from: usize, prev_over: usize, over: usize) -> (f32, f32) {
    if ix == from {
        (
            (prev_over as f32 - from as f32) * ROW_SLOT,
            (over as f32 - from as f32) * ROW_SLOT,
        )
    } else {
        (
            slide_offset(ix, from, prev_over) * ROW_SLOT,
            slide_offset(ix, from, over) * ROW_SLOT,
        )
    }
}

/// A queue row being dragged (gpui drag-and-drop). Scoped to its chat so a
/// drag can't land in a queue it didn't come from.
pub struct QueueDragPayload {
    chat: String,
    from: usize,
}

/// Where the dragged row would land, including the previous slot needed to
/// restart the short PR #90-style slide from its current visual position.
pub struct QueueDragState {
    pub from: usize,
    pub over: usize,
    pub prev_over: usize,
    pub epoch: usize,
}

/// Invisible cursor ghost: the real row stays in the queue and moves between
/// slots, instead of following the pointer as a detached tooltip.
struct QueueGhost;

impl Render for QueueGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

/// One line of a queued message: the newlines that make it a paragraph in the
/// composer make it three rows here, and the row is one line tall.
fn one_line(text: &str) -> SharedString {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    SharedString::from(flat)
}

/// The fieldset-style legend shown on the queue's top edge.
pub fn queue_label(count: usize) -> Option<String> {
    match count {
        0 => None,
        n => Some(format!("Queue {n}")),
    }
}

impl Composer {
    /// The queue panel, or `None` when nothing is waiting. Its legend sits in
    /// the outline like a fieldset, keeping it visually separate from the
    /// composer pill below.
    pub(crate) fn render_queue_panel(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        // A drop outside the panel ends GPUI's active drag without invoking our
        // `on_drop`. Never leave the source row replaced by a stale gap.
        if self.queue_drag.is_some() && !cx.has_active_drag() {
            self.queue_drag = None;
        }
        let items = self.state.read(cx).queue.clone();
        let label = queue_label(items.len())?;
        let theme = Theme::of(cx).clone();
        let chat_id = self.state.read(cx).selected_chat.clone()?;
        let count = items.len();
        let drag = self
            .queue_drag
            .as_ref()
            .map(|d| (d.from, d.over, d.prev_over, d.epoch));
        let editing = self.editing_queued.clone();
        let mid_turn_steering = self.pickers().read(cx).resolved_mid_turn_steering(cx);

        let list_chat = chat_id.clone();
        let drop_chat = chat_id.clone();
        let rows = div()
            .flex()
            .flex_col()
            .gap(px(ROW_GAP))
            .children(items.iter().enumerate().map(|(ix, item)| {
                self.queue_row(
                    &chat_id,
                    ix,
                    item,
                    drag,
                    &editing,
                    mid_turn_steering,
                    &theme,
                    cx,
                )
            }));

        let body = div()
            .rounded(px(PANEL_RADIUS))
            .bg(theme.input_glass_bg())
            .border_1()
            .border_color(theme.border)
            .when(!theme.is_glass(), |el| el.shadow_lg())
            .px(px(8.0))
            .pt(px(BODY_ROWS_PAD_TOP))
            .pb(px(8.0))
            .child(rows);
        // Open-bottom tab joined to the body's top edge. A fully-rounded,
        // independently-filled pill here reads as an object laid on top; the
        // reference is one notched silhouette shared by tab and body.
        let legend = div()
            .absolute()
            .top_0()
            .left(px(12.0))
            .h(px(PANEL_TAB_HEIGHT))
            .px(px(10.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .rounded_t(px(9.0))
            .border_t_1()
            .border_l_1()
            .border_r_1()
            .border_color(theme.border)
            .bg(theme.input_glass_bg())
            .text_size(px(11.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(theme.text_muted.opacity(0.78))
            .child(
                icon(icons::LIST)
                    .size(px(12.0))
                    .text_color(theme.text_muted.opacity(0.68)),
            )
            .child(SharedString::from(label));
        let panel = div()
            .relative()
            .pt(px(PANEL_TOP_PAD))
            // The complete outlined panel is a drop target, not just the
            // surviving row hitboxes. This includes the legend and padding.
            .on_drag_move::<QueueDragPayload>(cx.listener(
                move |this, event: &gpui::DragMoveEvent<QueueDragPayload>, _, cx| {
                    let payload = event.drag(cx);
                    if payload.chat != list_chat {
                        return;
                    }
                    let from = payload.from;
                    let rel_y = f32::from(event.event.position.y) - f32::from(event.bounds.top());
                    let over = queue_drop_index(rel_y, count);
                    this.update_queue_drag_over(from, over, cx);
                },
            ))
            .on_drop::<QueueDragPayload>(cx.listener(
                move |this, payload: &QueueDragPayload, _, cx| {
                    if payload.chat != drop_chat {
                        this.queue_drag = None;
                        cx.notify();
                        return;
                    }
                    let to = this
                        .queue_drag
                        .as_ref()
                        .map(|d| d.over)
                        .unwrap_or(payload.from);
                    this.queue_drag = None;
                    this.move_queued(payload.from, to, cx);
                },
            ))
            .on_mouse_up_out(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, cx| this.cancel_queue_drag(cx)),
            )
            .child(body)
            .child(crate::frost::layered(legend));
        Some(crate::frost::frosted(PANEL_RADIUS, 16.0, panel).into_any_element())
    }

    /// One queued message: a drag grip, its place in line, the text, quiet edit
    /// controls, and one explicit primary delivery action.
    #[allow(clippy::too_many_arguments)]
    fn queue_row(
        &self,
        chat_id: &str,
        ix: usize,
        item: &QueuedMessage,
        drag: Option<(usize, usize, usize, usize)>,
        editing: &Option<String>,
        mid_turn_steering: Option<bool>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = SharedString::from(format!("queue-{}", item.id));
        let being_edited = editing.as_deref() == Some(item.id.as_str());
        let text = one_line(&item.text);

        let edit_id = item.id.clone();
        let edit = self.queue_action(
            &key,
            "edit",
            "Edit",
            icons::PEN,
            theme,
            cx.listener(move |this, _, window, cx| {
                this.begin_queue_edit(edit_id.clone(), window, cx);
            }),
        );
        let drop_id = item.id.clone();
        let discard = self.queue_action(
            &key,
            "drop",
            "Remove",
            icons::CLOSE,
            theme,
            cx.listener(move |this, _, _, cx| {
                this.remove_queued(drop_id.clone(), cx);
            }),
        );
        let resolved_primary =
            queue_primary_action(mid_turn_steering, !item.attachments.is_empty());
        let primary_action = resolved_primary.unwrap_or(QueuePrimaryAction::SendNow);
        let primary_id = item.id.clone();
        let primary = self.queue_primary_action_button(
            &key,
            primary_action,
            resolved_primary.is_some(),
            theme,
            cx.listener(move |this, _, _, cx| {
                this.activate_queued_primary(primary_id.clone(), primary_action, cx);
            }),
        );
        let save = self.queue_action(
            &key,
            "save",
            "Save",
            icons::CHECK,
            theme,
            cx.listener(|this, _, _, cx| {
                this.commit_queue_edit(cx);
            }),
        );
        let cancel = self.queue_action(
            &key,
            "cancel",
            "Cancel",
            icons::CLOSE,
            theme,
            cx.listener(|this, _, _, cx| {
                this.cancel_queue_edit(cx);
            }),
        );

        let drag_chat = chat_id.to_string();
        let drag_handle = div()
            .id(SharedString::from(format!("{key}-drag")))
            .w(px(14.0))
            .h(px(22.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(4.0))
            .cursor_pointer()
            .when(being_edited, |el| {
                el.cursor(gpui::CursorStyle::Arrow).opacity(0.35)
            })
            .child(
                icon(icons::DRAG_HANDLE)
                    .size(px(12.0))
                    .text_color(theme.text_muted.opacity(0.5)),
            );

        let row = div()
            .id(SharedString::from(format!("{key}-row")))
            .h(px(ROW_HEIGHT))
            .flex_none()
            .px(px(ROW_PAD_X))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .rounded(px(9.0))
            .border_1()
            .border_color(theme.border.opacity(0.72))
            .bg(theme.surface_raised.opacity(0.72))
            .when(being_edited, |el| el.bg(crate::theme::ink(0.08)))
            .when(!being_edited, |el| {
                el.hover(|s| s.bg(theme.surface_raised_hover.opacity(0.78)))
            })
            .cursor(gpui::CursorStyle::Arrow)
            // Keep the grip as the visual affordance, but retain the proven
            // full-row drag hitbox. Editing disables it so text selection can
            // never accidentally become a reorder gesture.
            .when(!being_edited, |el| {
                el.on_drag(
                    QueueDragPayload {
                        chat: drag_chat,
                        from: ix,
                    },
                    move |_payload, _point, _, cx| {
                        cx.stop_propagation();
                        cx.new(|_| QueueGhost)
                    },
                )
            })
            .child(drag_handle)
            .child(
                div()
                    .w(px(LEAD))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(11.0))
                    .text_color(theme.text_muted.opacity(0.64))
                    .child(SharedString::from(format!("{}", ix + 1))),
            )
            .when(!being_edited, |el| {
                el.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(12.5))
                        .text_color(theme.text.opacity(0.9))
                        .child(text),
                )
            })
            .when(being_edited, |el| {
                el.child(
                    div()
                        .id(SharedString::from(format!("{key}-editor")))
                        .flex_1()
                        .min_w_0()
                        .h(px(26.0))
                        .px(px(6.0))
                        .flex()
                        .items_center()
                        .overflow_hidden()
                        .rounded(px(6.0))
                        .border_1()
                        .border_color(theme.border.opacity(0.82))
                        .bg(theme.bg.opacity(0.38))
                        .child(self.queue_edit_input.clone()),
                )
            })
            // Files are why a row can sit through a steerable turn, so say so.
            .when(!item.attachments.is_empty(), |el| {
                el.child(
                    crate::icons::icon(crate::icons::PAPERCLIP)
                        .size(px(11.0))
                        .text_color(theme.text_muted.opacity(0.7)),
                )
            })
            .when(being_edited, |el| {
                el.child(
                    div()
                        .flex_none()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(3.0))
                        .child(save)
                        .child(cancel),
                )
            })
            .when(!being_edited, |el| {
                el.child(
                    div()
                        .flex_none()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(3.0))
                        .child(edit)
                        .child(discard)
                        .child(primary),
                )
            });

        let Some((from, over, prev_over, epoch)) = drag else {
            return row.into_any_element();
        };
        let (start, target) = queue_drag_offsets(ix, from, prev_over, over);
        if cx.reduce_motion() {
            return div()
                .relative()
                .top(px(target))
                .child(row)
                .into_any_element();
        }
        div()
            .child(row)
            .with_animation(
                ("queue-row-slide", (ix as u64) | ((epoch as u64) << 32)),
                TAB_SLIDE.animation(),
                move |el, t| el.relative().top(px(motion::lerp(start, target, t))),
            )
            .into_any_element()
    }

    /// A permanently-visible trailing glyph button. The queue reference keeps
    /// edit and remove present instead of revealing them only on hover.
    fn queue_action(
        &self,
        key: &SharedString,
        slot: &str,
        label: &'static str,
        glyph: &'static str,
        theme: &Theme,
        on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
    ) -> AnyElement {
        let own = SharedString::from(format!("{key}-{slot}-grp"));
        div()
            .id(SharedString::from(format!("{key}-{slot}")))
            .group(own.clone())
            .size(px(18.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(5.0))
            .cursor_pointer()
            .opacity(0.72)
            .hover(|s| s.opacity(1.0).bg(crate::theme::ink(0.07)))
            .on_click(on_click)
            .tooltip(move |_, cx| {
                cx.new(|_| QueueActionTooltip {
                    label: label.into(),
                })
                .into()
            })
            .tooltip_show_delay(std::time::Duration::from_millis(350))
            .child(
                icon(glyph)
                    .size(px(11.0))
                    .text_color(theme.text_muted.opacity(0.8))
                    .group_hover(own, |s| s.text_color(theme.text)),
            )
            .into_any_element()
    }

    /// The row's only delivery control. Text and behavior are both supplied by
    /// the same resolved enum so the label can never conceal an interrupt.
    fn queue_primary_action_button(
        &self,
        key: &SharedString,
        action: QueuePrimaryAction,
        enabled: bool,
        theme: &Theme,
        on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
    ) -> AnyElement {
        let own = SharedString::from(format!("{key}-primary-grp"));
        let tooltip = if enabled {
            action.tooltip()
        } else {
            "Waiting for provider capabilities"
        };
        div()
            .id(SharedString::from(format!("{key}-primary")))
            .group(own.clone())
            .h(px(22.0))
            .flex_none()
            .px(px(6.0))
            .flex()
            .items_center()
            .gap(px(4.0))
            .rounded(px(6.0))
            .text_size(px(11.5))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(theme.text_muted.opacity(0.82))
            .when(enabled, |el| {
                el.cursor_pointer()
                    .hover(|s| s.bg(crate::theme::ink(0.07)).text_color(theme.text))
                    .on_click(on_click)
            })
            .when(!enabled, |el| {
                el.cursor(gpui::CursorStyle::Arrow).opacity(0.5)
            })
            .tooltip(move |_, cx| {
                cx.new(|_| QueueActionTooltip {
                    label: tooltip.into(),
                })
                .into()
            })
            .tooltip_show_delay(std::time::Duration::from_millis(350))
            .child(
                icon(action.glyph())
                    .size(px(11.0))
                    .text_color(theme.text_muted.opacity(0.72))
                    .group_hover(own, |s| s.text_color(theme.text)),
            )
            .child(action.label())
            .into_any_element()
    }

    /// Track the drop slot while a row is dragged over the list.
    fn update_queue_drag_over(&mut self, from: usize, over: usize, cx: &mut Context<Self>) {
        match &mut self.queue_drag {
            Some(drag) if drag.from == from => {
                if drag.over != over {
                    drag.prev_over = drag.over;
                    drag.over = over;
                    drag.epoch = drag.epoch.wrapping_add(1);
                    cx.notify();
                }
            }
            _ => {
                self.queue_drag = Some(QueueDragState {
                    from,
                    over,
                    prev_over: from,
                    epoch: 0,
                });
                cx.notify();
            }
        }
    }

    /// Restore a row whose pointer was released outside the queue's drop zone.
    fn cancel_queue_drag(&mut self, cx: &mut Context<Self>) {
        if self.queue_drag.take().is_some() {
            cx.notify();
        }
    }

    /// Move the row at `from` to `to`, optimistically here and for real on the
    /// doc (the watch frame is what everyone else sees).
    pub(crate) fn move_queued(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        if from == to {
            cx.notify();
            return;
        }
        let Some(id) = self
            .state
            .read(cx)
            .queue
            .get(from)
            .map(|item| item.id.clone())
        else {
            return;
        };
        self.state.update(cx, |state, cx| {
            if from < state.queue.len() {
                let item = state.queue.remove(from);
                state.queue.insert(to.min(state.queue.len()), item);
                cx.notify();
            }
        });
        self.queue_rpc(
            methods::MOVE_QUEUED_MESSAGE,
            serde_json::json!({ "id": id, "toIndex": to }),
            "Couldn't reorder the queue",
            cx,
        );
    }

    /// Drop a queued message.
    pub(crate) fn remove_queued(&mut self, id: String, cx: &mut Context<Self>) {
        if self.editing_queued.as_deref() == Some(id.as_str()) {
            self.clear_queue_edit(cx);
        }
        self.state.update(cx, |state, cx| {
            state.queue.retain(|item| item.id != id);
            cx.notify();
        });
        self.queue_rpc(
            methods::REMOVE_QUEUED_MESSAGE,
            serde_json::json!({ "id": id }),
            "Couldn't remove the message",
            cx,
        );
    }

    /// Send one now: the host stops the turn and hands this message over. Not
    /// optimistic — the row leaves the queue when the host has actually taken
    /// it, so a failed interrupt doesn't lose the text.
    pub(crate) fn send_queued_now(&mut self, id: String, cx: &mut Context<Self>) {
        if self.editing_queued.as_deref() == Some(id.as_str()) {
            self.clear_queue_edit(cx);
        }
        self.queue_rpc(
            methods::SEND_QUEUED_MESSAGE_NOW,
            serde_json::json!({ "id": id }),
            "Couldn't send that message",
            cx,
        );
    }

    /// Steer one row into the current turn without stopping it. If the turn
    /// ends during the click, the engine sends it as the next turn instead.
    pub(crate) fn steer_queued_now(&mut self, id: String, cx: &mut Context<Self>) {
        if self.editing_queued.as_deref() == Some(id.as_str()) {
            self.clear_queue_edit(cx);
        }
        self.queue_rpc(
            methods::STEER_QUEUED_MESSAGE_NOW,
            serde_json::json!({ "id": id }),
            "Couldn't steer that message",
            cx,
        );
    }

    /// Execute the same resolved action advertised on the row. Both pointer
    /// clicks and the empty-composer Enter gesture come through here.
    fn activate_queued_primary(
        &mut self,
        id: String,
        action: QueuePrimaryAction,
        cx: &mut Context<Self>,
    ) {
        match action {
            QueuePrimaryAction::Steer => self.steer_queued_now(id, cx),
            QueuePrimaryAction::SendNow => self.send_queued_now(id, cx),
        }
    }

    /// Hitting Enter on an empty composer activates the same action shown on
    /// the first queued row: non-interrupting Steer when possible, Send now
    /// otherwise.
    pub(crate) fn queue_pop_head(&mut self, cx: &mut Context<Self>) {
        let Some((id, has_attachments)) = self
            .state
            .read(cx)
            .queue
            .first()
            .map(|item| (item.id.clone(), !item.attachments.is_empty()))
        else {
            return;
        };
        let Some(action) = queue_primary_action(
            self.pickers().read(cx).resolved_mid_turn_steering(cx),
            has_attachments,
        ) else {
            return;
        };
        self.activate_queued_primary(id, action, cx);
    }

    /// Turn one queue row into its inline editor. The main composer is a
    /// separate draft and is deliberately left untouched.
    pub(crate) fn begin_queue_edit(
        &mut self,
        id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = self
            .state
            .read(cx)
            .queue
            .iter()
            .find(|item| item.id == id)
            .cloned()
        else {
            return;
        };
        self.editing_queued = Some(id);
        self.queue_drag = None;
        self.queue_edit_focus_pending = false;
        self.queue_edit_input
            .update(cx, |input, cx| input.set_text(item.text.clone(), cx));
        let focus = self.queue_edit_input.focus_handle(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Commit the inline edit. Empty text removes the row — emptying a
    /// message is how you take it back. `true` when this consumed the submit.
    pub(crate) fn commit_queue_edit(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(id) = self.editing_queued.take() else {
            return false;
        };
        let text = self.queue_edit_input.read(cx).text().trim().to_string();
        self.queue_edit_input
            .update(cx, |input, cx| input.set_text(String::new(), cx));
        self.queue_edit_focus_pending = true;
        if text.is_empty() {
            self.remove_queued(id, cx);
        } else {
            self.state.update(cx, |state, cx| {
                if let Some(item) = state.queue.iter_mut().find(|item| item.id == id) {
                    item.text = text.clone();
                    cx.notify();
                }
            });
            self.queue_rpc(
                methods::UPDATE_QUEUED_MESSAGE,
                serde_json::json!({ "id": id, "text": text }),
                "Couldn't save that edit",
                cx,
            );
        }
        cx.notify();
        true
    }

    /// Escape out of an edit, leaving the row as it was.
    pub(crate) fn cancel_queue_edit(&mut self, cx: &mut Context<Self>) -> bool {
        if self.editing_queued.is_none() {
            return false;
        }
        self.clear_queue_edit(cx);
        true
    }

    pub(crate) fn clear_queue_edit(&mut self, cx: &mut Context<Self>) {
        self.editing_queued = None;
        self.queue_edit_input
            .update(cx, |input, cx| input.set_text(String::new(), cx));
        self.queue_edit_focus_pending = true;
        cx.notify();
    }

    /// Fire one queue mutation at the chat's doc host.
    fn queue_rpc(
        &mut self,
        method: &'static str,
        params: serde_json::Value,
        failure: &'static str,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let (chat_id, host_device_id, host_supports_action) = {
            let state = self.state.read(cx);
            let Some(chat_id) = state.selected_chat.clone() else {
                return;
            };
            let host = queue_action_needs_host(method)
                .then(|| state.selected_chat_row().map(|chat| chat.device_id.clone()))
                .flatten();
            let supported = !queue_action_needs_host(method)
                || state.chat_host_supports(
                    &chat_id,
                    zeron_proto::capabilities::MESSAGE_QUEUE_ACTIONS_V1,
                );
            (chat_id, host, supported)
        };
        if !host_supports_action {
            self.failure = Some("The chat host does not support queue actions".into());
            cx.notify();
            return;
        }
        let mut params = params;
        if let Some(object) = params.as_object_mut() {
            object.insert("chatId".into(), serde_json::Value::String(chat_id));
            if let Some(host) = host_device_id {
                object.insert("targetDeviceId".into(), serde_json::Value::String(host));
            }
        }
        // Detached, not held: these are independent one-shot mutations, and
        // parking them in a single slot meant the next arrow tap dropped — and
        // so cancelled — the move still in flight, leaving the optimistic list
        // showing an order the doc never got.
        cx.spawn(
            async move |this, cx| match engine.client().call(method, params).await {
                Ok(reply) if queue_mutation_acknowledged(method, &reply) => {}
                Ok(reply) => {
                    tracing::debug!(
                        method,
                        ?reply,
                        "queue mutation was not applied; reconciling"
                    );
                    this.update(cx, |composer, cx| {
                        composer
                            .state
                            .update(cx, |state, cx| state.refresh_selected_queue(cx));
                    })
                    .ok();
                }
                Err(err) => {
                    tracing::warn!(method, error = %err, "queue mutation failed");
                    this.update(cx, |composer, cx| {
                        composer.failure = Some(failure.into());
                        composer
                            .state
                            .update(cx, |state, cx| state.refresh_selected_queue(cx));
                        cx.notify();
                    })
                    .ok();
                }
            },
        )
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use zeron_rpc::methods;

    use super::{
        BODY_ROWS_PAD_TOP, PANEL_TOP_PAD, QueuePrimaryAction, ROW_SLOT, one_line,
        queue_action_needs_host, queue_drag_offsets, queue_drop_index, queue_label,
        queue_mutation_acknowledged, queue_primary_action,
    };

    #[test]
    fn label_counts_or_says_nothing() {
        assert_eq!(queue_label(0), None);
        assert_eq!(queue_label(1).as_deref(), Some("Queue 1"));
        assert_eq!(queue_label(4).as_deref(), Some("Queue 4"));
    }

    #[test]
    fn primary_action_only_promises_steer_when_the_row_can_use_it() {
        assert_eq!(
            queue_primary_action(Some(true), false),
            Some(QueuePrimaryAction::Steer)
        );
        assert_eq!(
            queue_primary_action(Some(false), false),
            Some(QueuePrimaryAction::SendNow)
        );
        assert_eq!(queue_primary_action(None, false), None);
        assert_eq!(
            queue_primary_action(Some(true), true),
            Some(QueuePrimaryAction::SendNow)
        );
    }

    #[test]
    fn only_agent_execution_actions_route_to_the_host() {
        assert!(queue_action_needs_host(methods::SEND_QUEUED_MESSAGE_NOW));
        assert!(queue_action_needs_host(methods::STEER_QUEUED_MESSAGE_NOW));
        assert!(!queue_action_needs_host(methods::QUEUE_MESSAGE));
        assert!(!queue_action_needs_host(methods::UPDATE_QUEUED_MESSAGE));
        assert!(!queue_action_needs_host(methods::MOVE_QUEUED_MESSAGE));
        assert!(!queue_action_needs_host(methods::REMOVE_QUEUED_MESSAGE));
    }

    #[test]
    fn mutation_acknowledgements_detect_conflicts_and_malformed_replies() {
        assert!(queue_mutation_acknowledged(
            methods::MOVE_QUEUED_MESSAGE,
            &serde_json::json!({ "changed": true })
        ));
        assert!(!queue_mutation_acknowledged(
            methods::MOVE_QUEUED_MESSAGE,
            &serde_json::json!({ "changed": false })
        ));
        assert!(!queue_mutation_acknowledged(
            methods::REMOVE_QUEUED_MESSAGE,
            &serde_json::json!({})
        ));
        assert!(queue_mutation_acknowledged(
            methods::SEND_QUEUED_MESSAGE_NOW,
            &serde_json::json!({ "sent": true })
        ));
    }

    #[test]
    fn the_whole_panel_maps_to_a_clamped_queue_drop_slot() {
        assert_eq!(queue_drop_index(0.0, 2), 0, "legend targets the head");
        assert_eq!(
            queue_drop_index(PANEL_TOP_PAD + BODY_ROWS_PAD_TOP + ROW_SLOT - 0.1, 2),
            0
        );
        assert_eq!(
            queue_drop_index(PANEL_TOP_PAD + BODY_ROWS_PAD_TOP + ROW_SLOT, 2),
            1
        );
        assert_eq!(queue_drop_index(10_000.0, 2), 1);
    }

    #[test]
    fn drag_offsets_move_the_real_row_and_open_its_destination() {
        assert_eq!(queue_drag_offsets(0, 0, 0, 2), (0.0, 2.0 * ROW_SLOT));
        assert_eq!(queue_drag_offsets(1, 0, 0, 2), (0.0, -ROW_SLOT));
        assert_eq!(queue_drag_offsets(2, 0, 0, 2), (0.0, -ROW_SLOT));

        // Moving the pointer back one slot restarts only the rows whose
        // visual destination actually changed.
        assert_eq!(queue_drag_offsets(0, 0, 2, 1), (2.0 * ROW_SLOT, ROW_SLOT));
        assert_eq!(queue_drag_offsets(1, 0, 2, 1), (-ROW_SLOT, -ROW_SLOT));
        assert_eq!(queue_drag_offsets(2, 0, 2, 1), (-ROW_SLOT, 0.0));
    }

    /// A row is one line tall, so a multi-line message has to read as one line
    /// — otherwise the panel's rows stop lining up.
    #[test]
    fn rows_flatten_multi_line_messages() {
        assert_eq!(
            one_line("fix the test\n\nthen ship it").as_ref(),
            "fix the test then ship it"
        );
        assert_eq!(one_line("  spaced   out  ").as_ref(), "spaced out");
    }
}
