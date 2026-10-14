//! The session hover card: dwell on a sidebar row and a frosted card floats
//! out beside it with what the row has no room for — the whole title, the
//! project and device, the branch, and the pull request's title.
//! Compact rows drop all of that, so the card is where it lives on.
//!
//! The card waits out a short dwell so sweeping the pointer down the list
//! stays quiet. Once one has shown, the card is warm: the next row's card
//! follows the pointer at once, until it rests off the list for a moment.
use std::time::Instant;

use super::*;

use crate::change_requests::ChangeRequestBadgeModel;

/// How long the pointer rests on a row before its card shows.
const DWELL: Duration = Duration::from_millis(550);
/// How long after a card hides the next row's card skips the dwell.
const WARM: Duration = Duration::from_millis(400);
const CARD_WIDTH: f32 = 284.0;
/// The gap between the row's right edge and the card.
const CARD_GAP: f32 = 10.0;
const CARD_PAD: f32 = 12.0;

/// Which row's card shows, and which is waiting out its dwell.
#[derive(Default)]
pub(super) struct SessionHoverCard {
    shown: Option<String>,
    pending: Option<(String, Task<()>)>,
    hidden_at: Option<Instant>,
}

impl SessionHoverCard {
    pub(super) fn shown(&self) -> Option<&str> {
        self.shown.as_deref()
    }

    /// The pointer entered `chat_id`'s row. True when the card should show
    /// at once: another card is up, or one hid moments ago.
    fn enter(&mut self, chat_id: &str, now: Instant) -> bool {
        if self.shown.as_deref() == Some(chat_id) {
            return false;
        }
        let warm = self.shown.is_some()
            || self
                .hidden_at
                .is_some_and(|hidden| now.duration_since(hidden) < WARM);
        if warm {
            self.pending = None;
            self.shown = Some(chat_id.to_owned());
        }
        warm
    }

    /// The pointer left `chat_id`'s row. True when a shown card hid.
    fn leave(&mut self, chat_id: &str, now: Instant) -> bool {
        if self
            .pending
            .as_ref()
            .is_some_and(|(pending, _)| pending == chat_id)
        {
            self.pending = None;
        }
        if self.shown.as_deref() != Some(chat_id) {
            return false;
        }
        self.shown = None;
        self.hidden_at = Some(now);
        true
    }

    /// The dwell on `chat_id` ran out with the pointer still there.
    fn settle(&mut self, chat_id: &str) -> bool {
        if self
            .pending
            .as_ref()
            .is_none_or(|(pending, _)| pending != chat_id)
        {
            return false;
        }
        self.pending = None;
        self.shown = Some(chat_id.to_owned());
        true
    }

    /// A click, menu, drag or scroll: hide now, and go cold so the card does
    /// not chase the pointer straight back.
    pub(super) fn dismiss(&mut self) -> bool {
        self.pending = None;
        self.hidden_at = None;
        self.shown.take().is_some()
    }
}

/// "21h ago" for the card's footer line ("now" stays as it is).
fn time_label(time_ago: &str) -> String {
    if time_ago == "now" {
        "just now".to_owned()
    } else {
        format!("{time_ago} ago")
    }
}

impl Shell {
    /// Row hover in or out (the row's own hover listener forwards here).
    pub(super) fn hover_session_card(
        &mut self,
        chat_id: &str,
        hovered: bool,
        cx: &mut Context<Self>,
    ) {
        let now = Instant::now();
        if !hovered {
            if self.session_hover_card.leave(chat_id, now) {
                cx.notify();
            }
            return;
        }
        if cx.has_active_drag() || self.chat_menu.is_open() {
            return;
        }
        if self.session_hover_card.enter(chat_id, now) {
            cx.notify();
            return;
        }
        if self.session_hover_card.shown() == Some(chat_id) {
            return;
        }
        let target = chat_id.to_owned();
        let task = cx.spawn({
            let target = target.clone();
            async move |this, cx| {
                cx.background_executor().timer(DWELL).await;
                let _ = this.update(cx, |this, cx| {
                    if this.session_hover_card.settle(&target) {
                        cx.notify();
                    }
                });
            }
        });
        self.session_hover_card.pending = Some((target, task));
    }

    pub(super) fn dismiss_session_card(&mut self, cx: &mut Context<Self>) {
        if self.session_hover_card.dismiss() {
            cx.notify();
        }
    }

    /// The card for `chat_id`'s row, mounted from the row itself: it floats
    /// from the row's right edge above everything else, clear of the
    /// sidebar's clip.
    pub(super) fn render_session_hover_card(
        &self,
        chat_id: &str,
        time_ago: SharedString,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::of(cx).for_popup();
        let now = Utc::now();
        let (title, project, device, local, online, branch, change_request) = {
            let state = self.state.read(cx);
            let chat = state.chats.iter().find(|chat| chat.id == chat_id)?;
            let space = state.space_for_chat(chat);
            let project = space.map(|space| SharedString::from(space.display_name().to_owned()));
            let device = state
                .device_name(&chat.device_id)
                .map(|name| SharedString::from(name.to_owned()));
            let local = state.local_device_id.as_deref() == Some(chat.device_id.as_str());
            let online = state.device_online(&chat.device_id, now);
            let branch = crate::change_requests::conversation_branch(chat, &state.spaces)
                .map(str::trim)
                .filter(|branch| !branch.is_empty())
                .map(|branch| SharedString::from(branch.to_owned()));
            let change_request = state
                .change_request_for_chat(chat)
                .map(ChangeRequestBadgeModel::from_summary);
            let title = SharedString::from(
                chat.title
                    .as_deref()
                    .map(|title| title.split_whitespace().collect::<Vec<_>>().join(" "))
                    .filter(|title| !title.is_empty())
                    .unwrap_or_else(|| "New session".to_owned()),
            );
            (
                title,
                project,
                device,
                local,
                online,
                branch,
                change_request,
            )
        };

        let muted = theme.text_muted;
        let meta_row = |glyph: AnyElement, label: SharedString| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(7.0))
                .min_w_0()
                .h(px(18.0))
                .child(
                    div()
                        .w(px(14.0))
                        .flex_none()
                        .flex()
                        .justify_center()
                        .child(glyph),
                )
                .child(div().flex_1().min_w_0().truncate().child(label))
        };
        let glyph = |path: &'static str| {
            icon(path)
                .size(px(13.0))
                .text_color(muted.opacity(0.8))
                .into_any_element()
        };

        // ── Title, then its age on one quiet line.
        let header = div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(
                div()
                    .w_full()
                    .text_size(crate::typography::ui_rems(13.0))
                    .line_height(px(18.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .line_clamp(3)
                    .child(title),
            )
            .child(
                div()
                    .text_size(crate::typography::ui_rems(11.0))
                    .text_color(muted)
                    .child(time_label(&time_ago)),
            );

        // ── Where it runs: project and device, branch.
        let project_row = meta_row(
            match &project {
                Some(_) => self.render_project_icon(chat_id, 14.0, false, cx),
                None => glyph(icons::FOLDER),
            },
            project.clone().unwrap_or_else(|| "No project".into()),
        )
        .text_color(theme.text)
        .when_some(device, |row, device| {
            row.child(
                div()
                    .flex_none()
                    .max_w(px(120.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    .text_color(muted)
                    .child(
                        icon(if !online {
                            icons::WIFI_OFF
                        } else if local {
                            icons::LAPTOP
                        } else {
                            icons::REMOTE_SERVER
                        })
                        .size(px(12.0))
                        .flex_none()
                        .text_color(if online {
                            muted.opacity(0.8)
                        } else {
                            theme.warning.opacity(0.8)
                        }),
                    )
                    .child(div().min_w_0().truncate().child(device)),
            )
        });
        let location = div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .text_size(crate::typography::ui_rems(12.0))
            .text_color(muted)
            .child(project_row)
            .children(branch.map(|branch| meta_row(glyph(icons::GIT_BRANCH), branch)));

        // ── The pull request on one line: its state lives in the tint and
        // the glyph's color (the badge's), so only the title and number
        // need words.
        let pull_request = change_request.map(|model| {
            let tone = model.tone.color(&theme);
            div()
                .flex()
                .flex_row()
                .items_start()
                .gap(px(7.0))
                .px(px(9.0))
                .py(px(7.0))
                .rounded(px(8.0))
                .bg(tone.opacity(0.08))
                .text_size(crate::typography::ui_rems(12.0))
                .line_height(px(16.0))
                .child(
                    div().h(px(16.0)).flex_none().flex().items_center().child(
                        icon(icons::PULL_REQUEST)
                            .size(px(12.0))
                            .text_color(tone.opacity(0.9)),
                    ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(theme.text)
                        .line_clamp(2)
                        .child(model.title),
                )
                .child(
                    div()
                        .flex_none()
                        .font_family(theme.font_mono.clone())
                        .text_size(crate::typography::ui_rems(11.0))
                        .text_color(tone.opacity(0.85))
                        .child(SharedString::from(format!("#{}", model.number))),
                )
        });

        let card = div()
            .w(px(CARD_WIDTH))
            .p(px(CARD_PAD))
            .flex()
            .flex_col()
            .gap(px(10.0))
            .rounded(px(popover::CARD_RADIUS))
            .overflow_hidden()
            .border_1()
            .border_color(theme.border)
            .bg(popover::surface_bg(&theme))
            .when(!theme.is_frost(), |card| card.shadow_lg())
            .child(header)
            .child(location)
            .children(pull_request);
        let card = div()
            .debug_selector({
                let chat_id = chat_id.to_owned();
                move || format!("session-hover-card-{chat_id}")
            })
            .child(crate::frost::frosted(
                popover::CARD_RADIUS,
                crate::frost::MENU_BLUR,
                card,
            ));
        // Drifts in from the row's side, like a submenu leaving its trigger.
        let card = if self.reduced_motion {
            card.into_any_element()
        } else {
            card.with_animation(
                SharedString::from(format!("session-hover-card-{chat_id}")),
                motion::MENU_IN.animation(),
                |el, t| el.relative().opacity(t).left(px(-4.0 * (1.0 - t))),
            )
            .into_any_element()
        };
        Some(
            div()
                .absolute()
                .top_0()
                .left_full()
                .child(
                    gpui::deferred(
                        gpui::anchored()
                            .anchor(gpui::Anchor::TopLeft)
                            .snap_to_window_with_margin(px(8.0))
                            .child(div().pl(px(CARD_GAP)).mt(px(-4.0)).child(card)),
                    )
                    .with_priority(1),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card() -> SessionHoverCard {
        SessionHoverCard::default()
    }

    #[test]
    fn a_cold_card_waits_out_the_dwell() {
        let mut card = card();
        let now = Instant::now();
        assert!(!card.enter("a", now));
        assert_eq!(card.shown(), None);
        // The dwell only settles a row the pointer is still on.
        assert!(!card.settle("a"));
        card.pending = Some(("a".into(), Task::ready(())));
        assert!(card.settle("a"));
        assert_eq!(card.shown(), Some("a"));
    }

    #[test]
    fn leaving_before_the_dwell_cancels_it() {
        let mut card = card();
        card.pending = Some(("a".into(), Task::ready(())));
        assert!(!card.leave("a", Instant::now()));
        assert!(!card.settle("a"));
        assert_eq!(card.shown(), None);
    }

    #[test]
    fn a_warm_card_follows_the_pointer_in_either_event_order() {
        let now = Instant::now();
        // Leave fires first: the card hid a moment ago, so it is still warm.
        let mut card = card();
        card.shown = Some("a".into());
        assert!(card.leave("a", now));
        assert!(card.enter("b", now + Duration::from_millis(16)));
        assert_eq!(card.shown(), Some("b"));
        // Enter fires first: the stale leave must not hide the new card.
        let mut card = SessionHoverCard {
            shown: Some("a".into()),
            ..Default::default()
        };
        assert!(card.enter("b", now));
        assert!(!card.leave("a", now));
        assert_eq!(card.shown(), Some("b"));
    }

    #[test]
    fn the_card_cools_after_a_rest_or_a_dismissal() {
        let now = Instant::now();
        let mut card = card();
        card.shown = Some("a".into());
        card.leave("a", now);
        assert!(!card.enter("b", now + WARM + Duration::from_millis(1)));
        let mut card = SessionHoverCard {
            shown: Some("a".into()),
            ..Default::default()
        };
        assert!(card.dismiss());
        assert!(!card.enter("a", now));
        assert_eq!(card.shown(), None);
    }

    #[test]
    fn the_age_reads_as_a_sentence() {
        assert_eq!(time_label("21h"), "21h ago");
        assert_eq!(time_label("now"), "just now");
    }
}
