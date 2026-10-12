use super::*;
use crate::split_view::{self, SplitSide};

/// A pill carried out of a split's sidebar row.
#[derive(Clone)]
pub(super) struct SplitPillDrag {
    pub(super) chat_id: String,
    pub(super) title: SharedString,
}

/// The pill under the pointer while a [`SplitPillDrag`] is carried.
pub(super) struct SplitPillGhost {
    pub(super) title: SharedString,
}

impl Render for SplitPillGhost {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).for_popup();
        div()
            .h(px(24.0))
            .max_w(px(220.0))
            .px(px(8.0))
            .rounded(px(6.0))
            .flex()
            .items_center()
            .bg(crate::popover::surface_bg(&theme))
            .border_1()
            .border_color(theme.border)
            .shadow_md()
            .text_size(crate::typography::ui_rems(12.0))
            .text_color(theme.text)
            .child(div().truncate().child(self.title.clone()))
    }
}

/// Where a chat dragged over a conversation lands: in its composer as a
/// mention, or beside it as a split pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ChatDropKind {
    Mention,
    Beside(SplitSide),
}

/// The drop a carried chat would make over the conversation showing
/// `target`, with the bounds its overlay draws in.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct ChatDropZone {
    pub(super) target: String,
    pub(super) kind: ChatDropKind,
    bounds: gpui::Bounds<Pixels>,
}

impl Shell {
    /// The entire conversation receives attachments in its own composer,
    /// regardless of which pane had keyboard focus when the drag started.
    /// `split_anchor` is the chat the conversation shows when a session
    /// carried over it may also open beside it: over its composer the drop
    /// mentions the session, over either half of the transcript it opens
    /// the session on that side. Without one (a side chat, the canvas) the
    /// whole area mentions.
    pub(super) fn chat_dropzone(
        &mut self,
        id: impl Into<gpui::ElementId>,
        composer: Entity<Composer>,
        split_anchor: Option<String>,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let images = composer.clone();
        let workspace = composer.clone();
        let mention = composer.clone();
        let pill_mention = composer.clone();
        let (session_anchor, pill_anchor) = (split_anchor.clone(), split_anchor.clone());
        let el = div()
            .id(id)
            .relative()
            .on_drop(cx.listener(move |_, paths: &gpui::ExternalPaths, _, cx| {
                images.update(cx, |composer, cx| {
                    composer.add_paths(paths.paths().to_vec(), cx)
                });
                cx.notify();
            }))
            .on_drop::<WorkspacePathDrag>(cx.listener(
                move |this, payload: &WorkspacePathDrag, window, cx| {
                    this.attach_workspace_drag(payload, &workspace, window, cx);
                    cx.notify();
                },
            ))
            // A session dragged from the sidebar becomes a chat chip here, or
            // a pane beside this conversation, and its row slides home.
            .on_drop::<SidebarSessionDrag>(cx.listener(
                move |this, payload: &SidebarSessionDrag, window, cx| {
                    this.drop_dragged_chat(
                        session_anchor.as_deref(),
                        &payload.chat_id,
                        &mention,
                        window,
                        cx,
                    );
                    this.cancel_sidebar_session_transfer(cx);
                    cx.stop_propagation();
                    cx.notify();
                },
            ))
            .on_drop::<SplitPillDrag>(cx.listener(
                move |this, payload: &SplitPillDrag, window, cx| {
                    this.drop_dragged_chat(
                        pill_anchor.as_deref(),
                        &payload.chat_id,
                        &pill_mention,
                        window,
                        cx,
                    );
                    cx.stop_propagation();
                    cx.notify();
                },
            ))
            .on_drop::<RightTabDrag>(cx.listener(
                move |this, payload: &RightTabDrag, window, cx| {
                    if let Some(path) = &payload.workspace_path {
                        this.attach_workspace_drag(path, &composer, window, cx);
                    }
                    cx.notify();
                },
            ));
        let Some(anchor) = split_anchor else {
            return el;
        };
        let pill_anchor = anchor.clone();
        el.on_drag_move::<SidebarSessionDrag>(cx.listener(
            move |this, event: &gpui::DragMoveEvent<SidebarSessionDrag>, _, cx| {
                let dragged = event.drag(cx).chat_id.clone();
                this.track_chat_drop(&anchor, &dragged, event.bounds, event.event.position, cx);
            },
        ))
        .on_drag_move::<SplitPillDrag>(cx.listener(
            move |this, event: &gpui::DragMoveEvent<SplitPillDrag>, _, cx| {
                let dragged = event.drag(cx).chat_id.clone();
                this.track_chat_drop(
                    &pill_anchor,
                    &dragged,
                    event.bounds,
                    event.event.position,
                    cx,
                );
            },
        ))
    }

    /// The paint-time bounds of the composer showing `chat_id`, for telling
    /// a mention drop from a split drop.
    pub(super) fn chat_drop_composer_probe(&mut self, chat_id: &str) -> AnyElement {
        let cell = self
            .chat_drop_composers
            .entry(chat_id.to_string())
            .or_default()
            .clone();
        gpui::canvas(move |bounds, _, _| cell.set(Some(bounds)), |_, _, _, _| {})
            .absolute()
            .inset_0()
            .into_any_element()
    }

    /// Follow a carried chat over the conversation showing `anchor`.
    fn track_chat_drop(
        &mut self,
        anchor: &str,
        dragged: &str,
        bounds: gpui::Bounds<Pixels>,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let over = bounds.contains(&position) && dragged != anchor;
        let zone = over.then(|| {
            let over_composer = self
                .chat_drop_composers
                .get(anchor)
                .and_then(|cell| cell.get())
                .is_some_and(|composer| composer.contains(&position));
            let kind = if over_composer || !self.chat_can_split(anchor, dragged, cx) {
                ChatDropKind::Mention
            } else if position.x < bounds.center().x {
                ChatDropKind::Beside(SplitSide::Left)
            } else {
                ChatDropKind::Beside(SplitSide::Right)
            };
            ChatDropZone {
                target: anchor.to_string(),
                kind,
                bounds,
            }
        });
        let current = self
            .chat_drop_zone
            .as_ref()
            .filter(|zone| zone.target == anchor);
        if zone.is_none() && current.is_none() {
            return;
        }
        if zone.as_ref() != current {
            self.chat_drop_zone = zone;
            cx.notify();
        }
    }

    /// Whether `dragged` may open beside `anchor`: both are splittable and
    /// the anchor's split has room for it (or already holds it).
    fn chat_can_split(&self, anchor: &str, dragged: &str, cx: &App) -> bool {
        if !self.splittable(anchor, cx) || !self.splittable(dragged, cx) {
            return false;
        }
        self.split_of_chat(anchor, cx).is_none_or(|view| {
            view.contains(dragged) || view.chats.len() < split_view::SPLIT_MAX_PANES
        })
    }

    /// Land a carried chat on the conversation showing `anchor`.
    fn drop_dragged_chat(
        &mut self,
        anchor: Option<&str>,
        chat_id: &str,
        composer: &Entity<Composer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let zone = self.chat_drop_zone.take();
        if anchor == Some(chat_id) {
            return;
        }
        let kind = match (anchor, zone) {
            (Some(anchor), Some(zone)) if zone.target == anchor => zone.kind,
            _ => ChatDropKind::Mention,
        };
        match (kind, anchor) {
            (ChatDropKind::Beside(side), Some(anchor)) => {
                self.split_chat_beside(anchor, chat_id, side, cx);
            }
            _ => {
                let title = self
                    .state
                    .read(cx)
                    .chats
                    .iter()
                    .find(|chat| chat.id == chat_id)
                    .map(|chat| chat.title.clone().unwrap_or_default());
                if let Some(title) = title {
                    composer.update(cx, |composer, cx| {
                        composer.add_chat_mention(chat_id, &title, window, cx)
                    });
                }
            }
        }
    }

    /// What a chat carried over the conversation showing `anchor` would
    /// do: the half it opens in, or the composer it mentions in.
    pub(super) fn chat_drop_overlay(&self, anchor: &str, cx: &App) -> Option<AnyElement> {
        let zone = self
            .chat_drop_zone
            .as_ref()
            .filter(|zone| zone.target == anchor && cx.has_active_drag())?;
        let theme = Theme::of(cx);
        let label = |text: &'static str| {
            div()
                .px(px(10.0))
                .py(px(4.0))
                .rounded(px(6.0))
                .bg(crate::popover::surface_bg(theme))
                .text_size(crate::typography::ui_rems(13.0))
                .text_color(theme.text)
                .child(text)
        };
        let overlay = match zone.kind {
            ChatDropKind::Beside(side) => div()
                .debug_selector(move || {
                    format!(
                        "chat-drop-{}",
                        if side == SplitSide::Left {
                            "left"
                        } else {
                            "right"
                        }
                    )
                })
                .absolute()
                .top(px(Theme::TITLEBAR_HEIGHT))
                .bottom(px(8.0))
                .w(gpui::relative(0.5))
                .when(side == SplitSide::Left, |el| el.left(px(8.0)))
                .when(side == SplitSide::Right, |el| el.right(px(8.0)))
                .rounded(px(10.0))
                .border_1()
                .border_color(theme.accent.opacity(0.45))
                .bg(theme.accent.opacity(0.08))
                .flex()
                .items_center()
                .justify_center()
                .child(label("Open beside")),
            ChatDropKind::Mention => {
                let composer = self
                    .chat_drop_composers
                    .get(anchor)
                    .and_then(|cell| cell.get())
                    .unwrap_or(zone.bounds);
                div()
                    .debug_selector(|| "chat-drop-mention".into())
                    .absolute()
                    .left(composer.left() - zone.bounds.left())
                    .top(composer.top() - zone.bounds.top())
                    .w(composer.size.width)
                    .h(composer.size.height)
                    .rounded(px(12.0))
                    .bg(theme.scrim().opacity(0.4 / 0.6))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(label("Drop to mention"))
            }
        };
        Some(overlay.into_any_element())
    }

    pub(super) fn attachment_drop_overlay(theme: &Theme) -> gpui::Stateful<gpui::Div> {
        // Use typed drag styles rather than cached shell state: an external
        // FileDrop::Exited clears the payload without a final mouse move.
        // Resize markers and non-file tabs must never reveal the overlay.
        div()
            .id("attachment-drop-overlay")
            .absolute()
            .inset_0()
            .opacity(0.0)
            .bg(theme.scrim().opacity(0.4 / 0.6))
            .flex()
            .items_center()
            .justify_center()
            .text_size(crate::typography::ui_rems(13.0))
            .text_color(theme.text)
            .drag_over::<gpui::ExternalPaths>(|style, _, _, _| style.opacity(1.0))
            .drag_over::<WorkspacePathDrag>(|style, _, _, _| style.opacity(1.0))
            .drag_over::<RightTabDrag>(|style, tab, _, _| {
                if tab.workspace_path.is_some() {
                    style.opacity(1.0)
                } else {
                    style
                }
            })
            .child("Drop to attach")
    }

    /// The hint while a sidebar session is carried over a chat.
    pub(super) fn mention_drop_overlay(theme: &Theme) -> gpui::Stateful<gpui::Div> {
        div()
            .id("mention-drop-overlay")
            .absolute()
            .inset_0()
            .opacity(0.0)
            .bg(theme.scrim().opacity(0.4 / 0.6))
            .flex()
            .items_center()
            .justify_center()
            .text_size(crate::typography::ui_rems(13.0))
            .text_color(theme.text)
            .drag_over::<SidebarSessionDrag>(|style, _, _, _| style.opacity(1.0))
            .drag_over::<SplitPillDrag>(|style, _, _, _| style.opacity(1.0))
            .child("Drop to mention")
    }
}
