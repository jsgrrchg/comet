//! The session peek: the session card's facts for one pane of a split, on
//! hover over its title. The docked card belongs to a single conversation,
//! so a split hides its toggle; the peek takes its place there. It opens
//! below the title after a short rest, stays while the pointer travels into
//! it, and closes on leaving both, a press elsewhere, Escape or a drag.
//! Its rows act on the pane's chat, focusing that pane first.
use super::*;
use super::session_info::{SessionFacts, info_row, row_glyph};
use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

/// The rest on a title before its peek opens; once one is open, moving to
/// another title switches at once.
const OPEN_DELAY: Duration = Duration::from_millis(350);
/// The grace for crossing from a title into its peek, or back.
const CLOSE_DELAY: Duration = Duration::from_millis(150);
const PEEK_WIDTH: f32 = 272.0;
/// The head's top corners: the pane title's rounding.
const HEAD_RADIUS: f32 = 6.0;
/// The card sliding out from under the head.
const OPEN_MOTION: Duration = Duration::from_millis(180);

#[derive(Default)]
pub(super) struct SessionPeekState {
    /// The chat whose peek shows.
    open: Option<String>,
    /// A pending open or close.
    task: Option<Task<()>>,
    /// Each pane title's bounds as last painted, by chat id.
    anchors: HashMap<String, Rc<Cell<Option<gpui::Bounds<Pixels>>>>>,
}

impl Shell {
    /// The chat whose peek shows.
    pub(super) fn session_peek_chat(&self) -> Option<&str> {
        self.session_peek.open.as_deref()
    }

    /// The pointer entered or left the title of `chat_id`'s pane.
    pub(super) fn hover_session_peek_title(
        &mut self,
        chat_id: &str,
        hovered: bool,
        cx: &mut Context<Self>,
    ) {
        if !hovered {
            if self.session_peek.open.is_some() {
                self.schedule_session_peek(CLOSE_DELAY, None, cx);
            } else {
                self.session_peek.task = None;
            }
            return;
        }
        if cx.has_active_drag() {
            return;
        }
        if self.session_peek.open.is_some() {
            self.session_peek.task = None;
            if self.session_peek.open.as_deref() != Some(chat_id) {
                self.session_peek.open = Some(chat_id.to_string());
                cx.notify();
            }
        } else {
            self.schedule_session_peek(OPEN_DELAY, Some(chat_id.to_string()), cx);
        }
    }

    /// The pointer entered or left the peek itself.
    fn hover_session_peek_card(&mut self, hovered: bool, cx: &mut Context<Self>) {
        if hovered {
            self.session_peek.task = None;
        } else {
            self.schedule_session_peek(CLOSE_DELAY, None, cx);
        }
    }

    fn schedule_session_peek(
        &mut self,
        delay: Duration,
        target: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.session_peek.task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |this, cx| {
                this.session_peek.task = None;
                if this.session_peek.open != target {
                    this.session_peek.open = target;
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// Close the peek now, dropping any pending open.
    pub(super) fn close_session_peek(&mut self, cx: &mut Context<Self>) {
        self.session_peek.task = None;
        if self.session_peek.open.take().is_some() {
            cx.notify();
        }
    }

    /// A layer inside the title of `chat_id`'s pane that records where it
    /// painted, for the peek to hang from.
    pub(super) fn session_peek_anchor_probe(&mut self, chat_id: &str) -> AnyElement {
        let cell = self
            .session_peek
            .anchors
            .entry(chat_id.to_string())
            .or_default()
            .clone();
        gpui::canvas(move |bounds, _, _| cell.set(Some(bounds)), |_, _, _, _| {})
            .absolute()
            .inset_0()
            .into_any_element()
    }

    /// Run `act` on `chat_id`'s pane: at once when it is focused, else once
    /// focusing it has made it the active chat.
    fn act_on_pane(
        &mut self,
        chat_id: &str,
        act: impl FnOnce(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) {
        self.close_session_peek(cx);
        if self.active_chat == chat_id {
            act(self, cx);
            return;
        }
        self.focus_split_pane(chat_id, cx);
        let (chat_id, shell) = (chat_id.to_string(), cx.entity().downgrade());
        cx.defer(move |cx| {
            shell
                .update(cx, |this, cx| {
                    if this.active_chat == chat_id {
                        act(this, cx);
                    }
                })
                .ok();
        });
    }

    /// The open peek, hanging below its pane's title; `None` while closed,
    /// during a drag, or once its chat left the split on screen.
    pub(super) fn render_session_peek(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let chat_id = self.session_peek.open.clone()?;
        // A drag (the head carries its pane like the title does) ends it.
        if cx.has_active_drag() {
            self.session_peek.open = None;
            self.session_peek.task = None;
            return None;
        }
        if !self
            .shown_split(cx)
            .is_some_and(|view| view.contains(&chat_id))
        {
            self.session_peek.open = None;
            self.session_peek.task = None;
            return None;
        }
        let anchor = self.session_peek.anchors.get(&chat_id)?.get()?;
        let theme = Theme::of(cx).for_popup();
        let SessionFacts {
            chat,
            device,
            local,
            online,
            project,
            branch,
        } = SessionFacts::for_chat(self.state.read(cx), &chat_id)?;
        let focused = self.active_chat == chat_id;

        // ── Where it runs.
        let device_row = info_row(
            &theme,
            row_glyph(if local { icons::LAPTOP } else { icons::MONITOR }, &theme),
            device,
        )
        .when(!online, |row| {
            row.child(
                icon(icons::WIFI_OFF)
                    .size(px(12.0))
                    .flex_none()
                    .text_color(theme.warning.opacity(0.8)),
            )
        });
        let project_row = match project {
            Some((name, path)) => info_row(
                &theme,
                self.render_project_icon(&chat.id, 16.0, false, cx),
                name,
            )
            .id("session-peek-project")
            .tooltip(crate::settings::widgets::text_tooltip(path))
            .into_any_element(),
            None => info_row(&theme, row_glyph(icons::HOME, &theme), "No project".into())
                .into_any_element(),
        };
        let branch_row = branch.map(|branch| {
            info_row(&theme, row_glyph(icons::GIT_BRANCH, &theme), branch.clone())
                .id("session-peek-branch")
                .tooltip(crate::settings::widgets::text_tooltip(branch))
        });

        // ── This conversation.
        let busy = self.side_chat_creating;
        let (side_id, fork_id) = (chat_id.clone(), chat_id.clone());
        let side_chat = popover::picker_row(&theme, false, false, "session-peek-side-chat")
            .id("session-peek-side-chat")
            .debug_selector(|| "session-peek-side-chat".into())
            .when(busy, |row| row.opacity(0.5))
            .on_click(cx.listener(move |this, _, _, cx| {
                let parent = side_id.clone();
                this.act_on_pane(
                    &side_id,
                    move |this, cx| this.create_child_chat(Some(parent), cx),
                    cx,
                );
            }))
            .child(popover::picker_icon_slot(row_glyph(icons::PLUS, &theme)))
            .child(div().flex_1().child("New side chat"));
        let fork = popover::picker_row(&theme, false, false, "session-peek-fork")
            .id("session-peek-fork")
            .debug_selector(|| "session-peek-fork".into())
            .when(busy, |row| row.opacity(0.5))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.act_on_pane(&fork_id, |this, cx| this.create_side_chat(cx), cx);
            }))
            .child(popover::picker_icon_slot(row_glyph(icons::FORK, &theme)))
            .child(div().flex_1().child("Fork chat"));

        // ── The project's run actions: they follow the focused chat, so
        // another pane's peek offers to focus it instead.
        let actions: Vec<AnyElement> = if focused {
            self.render_session_info_actions(&theme, cx)
        } else if chat.space_id.is_some() {
            let focus_id = chat_id.clone();
            vec![
                popover::picker_divider().into_any_element(),
                popover::picker_row(&theme, false, false, "session-peek-show-actions")
                    .id("session-peek-show-actions")
                    .debug_selector(|| "session-peek-show-actions".into())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        // Focusing keeps the peek open: its actions load in.
                        this.focus_split_pane(&focus_id, cx);
                    }))
                    .child(popover::picker_icon_slot(row_glyph(crate::icons::ACTION_PLAY, &theme)))
                    .child(div().flex_1().child("Show actions"))
                    .into_any_element(),
            ]
        } else {
            Vec::new()
        };

        let max_height =
            (self.viewport_height - f32::from(anchor.bottom()) - 12.0).max(120.0);
        let body = div()
            .id("session-peek-body")
            .max_h(px(max_height))
            .overflow_y_scroll()
            .p(px(popover::CARD_INSET))
            .flex()
            .flex_col()
            .gap(px(popover::MENU_GAP))
            .child(device_row)
            .child(project_row)
            .children(branch_row)
            .child(popover::picker_divider())
            .child(side_chat)
            .child(fork)
            .children(actions);

        // The peek grows out of the title: a head over the title, in the
        // card's surface, opening into the card below as one outline. The
        // card starts under the head's left edge, pulled left only as far as
        // the window needs; its top edge is open where the head joins it.
        let head_w = f32::from(anchor.size.width);
        let head_h = f32::from(anchor.size.height);
        let card_w = PEEK_WIDTH.max(head_w);
        let left = f32::from(anchor.left());
        let dx = (self.viewport_width - 8.0 - (left + card_w)).min(0.0).max(-left + 8.0);
        let (join_from, join_to) = (-dx, -dx + head_w);
        let radius = px(popover::CARD_RADIUS);
        let card_corners = gpui::Corners {
            top_left: if join_from <= 0.5 { px(0.0) } else { radius },
            top_right: if join_to >= card_w - 0.5 { px(0.0) } else { radius },
            bottom_right: radius,
            bottom_left: radius,
        };
        let head_corners = gpui::Corners {
            top_left: px(HEAD_RADIUS),
            top_right: px(HEAD_RADIUS),
            bottom_right: px(0.0),
            bottom_left: px(0.0),
        };
        let edge = |from: f32, to: f32| {
            (to - from > 0.5).then(|| {
                div()
                    .absolute()
                    .top_0()
                    .left(px(from))
                    .w(px(to - from))
                    .h(px(1.0))
                    .bg(theme.border)
            })
        };
        let card = div()
            .id("session-peek")
            .debug_selector({
                let id = chat_id.clone();
                move || format!("session-peek-{id}")
            })
            .relative()
            .w(px(card_w))
            .rounded_tl(card_corners.top_left)
            .rounded_tr(card_corners.top_right)
            .rounded_b(radius)
            .overflow_hidden()
            .border_l_1()
            .border_r_1()
            .border_b_1()
            .border_color(theme.border)
            .bg(popover::surface_bg(&theme))
            .when(!theme.is_frost(), |card| card.shadow_lg())
            .occlude()
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                this.hover_session_peek_card(*hovered, cx);
            }))
            // A press on the head is the head's own; elsewhere it dismisses.
            .on_mouse_down_out(cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                if !anchor.contains(&event.position) {
                    this.close_session_peek(cx);
                }
            }))
            .child(body)
            .children(edge(0.0, join_from))
            .children(edge(join_to, card_w));
        let card = crate::frost::frosted_corners(card_corners, crate::frost::MENU_BLUR, card);
        let card = div().absolute().top_0().left(px(dx)).child(card);
        let card: AnyElement = if self.reduced_motion {
            card.into_any_element()
        } else {
            // It slides out from under the head.
            card.with_animation(
                SharedString::from(format!("session-peek-open-{chat_id}")),
                gpui::Animation::new(OPEN_MOTION).with_easing(gpui::ease_out_quint()),
                |card, t| card.opacity(t).top(px(-8.0 * (1.0 - t))),
            )
            .into_any_element()
        };
        let card = div().absolute().top(px(head_h)).left_0().child(card);

        let head = self.render_session_peek_head(&chat_id, &theme, cx);
        let head = crate::frost::frosted_corners(head_corners, crate::frost::MENU_BLUR, head);
        Some(
            gpui::deferred(
                gpui::anchored().position(anchor.origin).child(
                    div()
                        .relative()
                        .w(px(head_w))
                        .h(px(head_h))
                        // The card first, so the head paints over its seam.
                        .child(card)
                        .child(div().absolute().inset_0().child(head)),
                ),
            )
            .with_priority(2)
            .into_any_element(),
        )
    }

    /// The peek's head: the pane's title again, in the card's surface, open
    /// at the bottom into the card. A click focuses the pane, and it drags
    /// the pane like the title.
    fn render_session_peek_head(
        &self,
        chat_id: &str,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (title, harness) = {
            let state = self.state.read(cx);
            let chat = state.chats.iter().find(|chat| chat.id == chat_id);
            (
                SharedString::from(transcript::single_line(
                    &chat
                        .and_then(|chat| chat.title.clone())
                        .unwrap_or_else(|| "New session".into()),
                )),
                chat.and_then(|chat| chat.config.as_ref().map(|config| config.harness)),
            )
        };
        let focus_id = chat_id.to_string();
        let drag = chat_dropzone::SplitPillDrag {
            chat_id: chat_id.to_string(),
            title: title.clone(),
        };
        div()
            .id("session-peek-head")
            .debug_selector(|| "session-peek-head".into())
            .size_full()
            // The title's padding, less the border.
            .px(px(5.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .rounded_t(px(HEAD_RADIUS))
            .border_t_1()
            .border_l_1()
            .border_r_1()
            .border_color(theme.border)
            .bg(popover::surface_bg(theme))
            .cursor_pointer()
            .occlude()
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                this.hover_session_peek_card(*hovered, cx);
            }))
            .on_mouse_down(gpui::MouseButton::Left, |_, window, _| window.prevent_default())
            .on_click(cx.listener(move |this, _, _, cx| {
                this.close_session_peek(cx);
                this.focus_split_pane(&focus_id, cx);
            }))
            .on_drag(drag, |payload, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| chat_dropzone::SplitPillGhost {
                    title: payload.title.clone(),
                })
            })
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
                    .text_color(theme.text.opacity(0.85))
                    .child(title),
            )
            .into_any_element()
    }
}

#[cfg(feature = "project-palette-fixture")]
impl Shell {
    /// Open the peek of `chat_id`'s pane for the isolated review fixture.
    pub fn fixture_open_session_peek(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        self.session_peek.open = Some(chat_id.to_string());
        cx.notify();
    }
}
