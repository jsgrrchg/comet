//! A split view in the sidebar: one row holding a pill per chat, side by
//! side as the panes are. The row sits where its first member's row would
//! (so a pinned member pins the split), and the members don't also list on
//! their own. A pill opens its chat's pane; the row opens the pane focused
//! last.
use super::*;
use crate::split_view::{self, SplitView};
use std::collections::HashSet;

/// The row's height: one line of pills compact, two otherwise.
pub(super) fn split_row_height(compact: bool) -> f32 {
    if compact { 29.0 } else { 45.0 }
}

/// What a pill shows for one member.
struct SplitPill {
    chat_id: String,
    title: SharedString,
    harness: Option<zeron_proto::HarnessId>,
    status: zeron_proto::ChatIndicator,
    time_ago: SharedString,
    /// Outside the sidebar's project filter: the pill recedes.
    filtered: bool,
}

impl Shell {
    /// Keep one row per split among `rows`, already in display order: the
    /// first member's. Returns the splits by the chat whose row stands for
    /// them.
    pub(super) fn collapse_split_rows<T>(
        &self,
        rows: &mut Vec<T>,
        chat_id: impl Fn(&T) -> &str,
        cx: &App,
    ) -> std::collections::HashMap<String, SplitView> {
        let views = self.split_views(cx);
        let mut anchors = std::collections::HashMap::new();
        if views.is_empty() {
            return anchors;
        }
        let mut seen = HashSet::new();
        rows.retain(|row| {
            let id = chat_id(row);
            match split_view::split_of(views, id) {
                Some(view) if seen.insert(view.id.clone()) => {
                    anchors.insert(id.to_string(), view.clone());
                    true
                }
                Some(_) => false,
                None => true,
            }
        });
        anchors
    }

    /// The chat a sidebar slot opens: for a split's row, its last focused
    /// pane.
    pub(super) fn sidebar_landing_chat(&self, chat_id: &str, cx: &App) -> String {
        self.split_of_chat(chat_id, cx)
            .and_then(SplitView::landing_chat)
            .unwrap_or(chat_id)
            .to_string()
    }

    /// A split's sidebar row. `jump_label` replaces the trailing corner
    /// while the jump hints show.
    pub(super) fn render_split_row(
        &self,
        view: &SplitView,
        filtered_in: &HashSet<String>,
        jump_label: Option<SharedString>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let compact = self.settings.sidebar_compact;
        let now = Utc::now();
        let selected = self.state.read(cx).selected_chat.clone();
        let shown = selected.as_deref().is_some_and(|id| view.contains(id));
        let pills: Vec<SplitPill> = {
            let state = self.state.read(cx);
            let statuses: std::collections::HashMap<&str, zeron_proto::ChatIndicator> = state
                .overview_chats(now)
                .into_iter()
                .map(|(status, chat)| (chat.id.as_str(), status))
                .collect();
            view.chats
                .iter()
                .filter_map(|id| {
                    let chat = state.chats.iter().find(|chat| &chat.id == id)?;
                    Some(SplitPill {
                        chat_id: id.clone(),
                        title: transcript::single_line(
                            &chat.title.clone().unwrap_or_else(|| "New session".into()),
                        )
                        .into(),
                        harness: self
                            .settings
                            .sidebar_show_harness
                            .then(|| chat.config.as_ref().map(|c| c.harness))
                            .flatten(),
                        status: statuses
                            .get(id.as_str())
                            .copied()
                            .unwrap_or(zeron_proto::ChatIndicator::Idle),
                        time_ago: format_time_ago(
                            chat.last_message_at.unwrap_or(chat.created_at),
                            now,
                        )
                        .into(),
                        filtered: !filtered_in.contains(id),
                    })
                })
                .collect()
        };
        let landing = view.landing_chat().unwrap_or_default().to_string();
        let row_id = format!("split-row-{}", view.id);
        let fade_key = format!("{row_id}-hover");
        let selected_wash = crate::theme::glass_selected_bg();
        let rest_bg = if shown {
            selected_wash
        } else {
            crate::theme::wash(0.0)
        };
        let hover_bg = if shown {
            selected_wash
        } else {
            theme.glass_hover()
        };
        let mut row = div()
            .id(SharedString::from(row_id.clone()))
            .debug_selector({
                let row_id = row_id.clone();
                move || row_id.clone()
            })
            .h(px(split_row_height(compact)))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(3.0))
            .px(px(3.0))
            .rounded(px(8.0))
            .bg(motion::hover_blend(&fade_key, rest_bg, hover_bg))
            .on_hover(motion::hover_listener(fade_key))
            .cursor_pointer()
            .role(gpui::Role::Button)
            .aria_label(SharedString::from(format!(
                "Split view: {}",
                pills
                    .iter()
                    .map(|pill| pill.title.as_ref())
                    .collect::<Vec<_>>()
                    .join(", ")
            )))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.open_chat(landing.clone(), cx);
            }));
        for (ix, pill) in pills.into_iter().enumerate() {
            let focused = shown && selected.as_deref() == Some(pill.chat_id.as_str());
            row = row.child(self.render_split_pill(&view.id, ix, pill, focused, theme, cx));
        }
        row.when_some(jump_label, |row, label| {
            row.child(
                div()
                    .flex_none()
                    .pr(px(4.0))
                    .whitespace_nowrap()
                    .font_family(crate::popover::KEY_GLYPH_FONT)
                    .text_size(crate::typography::ui_rems(11.0))
                    .text_color(theme.text_muted.opacity(0.5))
                    .child(label),
            )
        })
        .into_any_element()
    }

    fn render_split_pill(
        &self,
        split_id: &str,
        ix: usize,
        pill: SplitPill,
        focused: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let compact = self.settings.sidebar_compact;
        let SplitPill {
            chat_id,
            title,
            harness,
            status,
            time_ago,
            filtered,
        } = pill;
        let pill_id = format!("split-pill-{split_id}-{ix}");
        let (queued, undelivered) = {
            let state = self.state.read(cx);
            let now = Utc::now();
            (
                state.send_queued(&chat_id, now),
                state.send_undelivered(&chat_id, now),
            )
        };
        let status_color = if undelivered {
            theme.danger
        } else if queued {
            theme.warning
        } else {
            spaces::status_dot_color(status, theme)
        };
        let working = status == zeron_proto::ChatIndicator::Working && !queued && !undelivered;
        let status_label: Option<&'static str> = if undelivered {
            Some("Failed")
        } else if queued {
            Some("Queued")
        } else {
            match status {
                zeron_proto::ChatIndicator::Working => Some("Working"),
                zeron_proto::ChatIndicator::AwaitingInput => Some("Input"),
                zeron_proto::ChatIndicator::Errored => Some("Failed"),
                zeron_proto::ChatIndicator::Completed => Some("Done"),
                zeron_proto::ChatIndicator::Idle => None,
            }
        };
        // Activity first, as on a session row; an idle pill wears its
        // harness, or a quiet dot without one.
        let lead: AnyElement = if working {
            loaders::mini_glyph_spinner(
                format!("{pill_id}-working"),
                2.0,
                theme.glyph,
                self.sidebar_pane.entity_id(),
                cx,
            )
            .into_any_element()
        } else if status == zeron_proto::ChatIndicator::Completed && !queued && !undelivered {
            icon(icons::CHECK)
                .size(px(11.0))
                .text_color(status_color)
                .into_any_element()
        } else if status_label.is_some() {
            div()
                .size(px(6.0))
                .rounded_full()
                .bg(status_color)
                .into_any_element()
        } else if let Some((path, tint)) = harness.map(crate::pickers::harness_brand_icon) {
            icon(path)
                .size(px(SIDEBAR_ACTIVE_HARNESS_ICON_SIZE))
                .text_color(tint.unwrap_or(theme.text_muted.opacity(0.8)))
                .into_any_element()
        } else {
            div()
                .size(px(6.0))
                .rounded_full()
                .bg(status_color)
                .into_any_element()
        };
        let label = crate::shell::sidebar_faded_label(
            SharedString::from(format!("{pill_id}-title")),
            true,
            div()
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(if focused {
                    theme.text
                } else {
                    theme.text.opacity(0.8)
                })
                .child(title.clone()),
        );
        let body: AnyElement = if compact {
            label.into_any_element()
        } else {
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(label)
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(11.0))
                        .whitespace_nowrap()
                        .text_color(if status_label.is_some() {
                            status_color
                        } else {
                            theme.text_muted.opacity(0.5)
                        })
                        .child(SharedString::from(
                            status_label.map_or(time_ago.to_string(), str::to_string),
                        )),
                )
                .into_any_element()
        };
        let open_id = chat_id.clone();
        let menu_id = chat_id.clone();
        let close_id = chat_id.clone();
        crate::surface_chrome::tab(SharedString::from(pill_id.clone()), focused, theme)
            .debug_selector({
                let id = chat_id.clone();
                move || format!("split-pill-{id}")
            })
            .flex_1()
            .min_w_0()
            .h(px(if compact { 23.0 } else { 37.0 }))
            .px(px(6.0))
            .rounded(px(6.0))
            .gap(px(6.0))
            .when(!focused, |el| el.bg(crate::theme::wash(0.04)))
            .when(filtered, |el| el.opacity(0.45))
            .aria_label(title)
            .child(
                div()
                    .size(px(13.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(lead),
            )
            .child(body)
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.open_chat(open_id.clone(), cx);
            }))
            // Middle-click takes the chat out of the split, like closing a tab.
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.close_split_pane(&close_id, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.chat_menu.open(ChatMenuState {
                        tab: None,
                        chat_id: menu_id.clone(),
                        position: event.position,
                        page: ChatMenuPage::Root,
                    });
                    cx.notify();
                }),
            )
            .into_any_element()
    }
}
