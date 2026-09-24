//! Built-in Drafts section; its entries are not sessions and cannot be pinned
//! or assigned to user sections before their first send.
use super::*;
use zeron_proto::{DraftChange, PromptDraft};

pub(super) struct PendingDraftChanges {
    engine: crate::state::EngineHandle,
    queue: std::collections::VecDeque<DraftChange>,
}
impl Shell {
    pub(super) fn visible_prompt_drafts(&self, cx: &App) -> Vec<PromptDraft> {
        let state = self.state.read(cx);
        let mut rows = state.prompt_drafts.drafts.clone();
        if let Some(pending) = &self.draft_changes {
            if state
                .engine()
                .is_some_and(|e| e.same_connection(&pending.engine))
            {
                for change in &pending.queue {
                    match change {
                        DraftChange::Discard { id } => rows.retain(|r| &r.id != id),
                        DraftChange::Move { id, before, after } => {
                            if let Some(i) = rows.iter().position(|r| &r.id == id) {
                                let row = rows.remove(i);
                                let at = before
                                    .as_ref()
                                    .and_then(|b| rows.iter().position(|r| &r.id == b))
                                    .or_else(|| {
                                        after.as_ref().and_then(|a| {
                                            rows.iter().position(|r| &r.id == a).map(|i| i + 1)
                                        })
                                    })
                                    .unwrap_or(rows.len());
                                rows.insert(at, row);
                            }
                        }
                    }
                }
            }
        }
        rows
    }
    pub(super) fn change_prompt_draft(&mut self, change: DraftChange, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        if self
            .draft_changes
            .as_ref()
            .is_some_and(|p| !p.engine.same_connection(&engine))
        {
            self.draft_changes = None;
        }
        if let Some(pending) = &mut self.draft_changes {
            pending.queue.push_back(change);
            cx.notify();
            return;
        }
        self.draft_changes = Some(PendingDraftChanges {
            engine: engine.clone(),
            queue: std::collections::VecDeque::from([change.clone()]),
        });
        cx.spawn(async move |this, cx| {
            let mut next = change;
            loop {
                let result = engine
                    .client()
                    .call(methods::CHANGE_DRAFT, serde_json::to_value(&next).unwrap())
                    .await;
                let following = this
                    .update(cx, |shell, cx| {
                        if !shell
                            .state
                            .read(cx)
                            .engine()
                            .is_some_and(|e| e.same_connection(&engine))
                        {
                            return None;
                        }
                        if let Err(error) = &result {
                            shell.sidebar_notice =
                                Some(format!("Couldn't save draft change: {error}").into());
                            shell.draft_changes = None;
                            cx.notify();
                            return None;
                        }
                        if let Ok(value) = result {
                            if let Ok(value) =
                                serde_json::from_value::<zeron_proto::DraftsState>(value)
                            {
                                shell.state.update(cx, |state, cx| {
                                    if value.revision >= state.prompt_drafts.revision {
                                        state.prompt_drafts = value;
                                        cx.notify();
                                    }
                                });
                            }
                        }
                        let pending = shell.draft_changes.as_mut()?;
                        pending.queue.pop_front();
                        let next = pending.queue.front().cloned();
                        if next.is_none() {
                            shell.draft_changes = None;
                        }
                        cx.notify();
                        next
                    })
                    .ok()
                    .flatten();
                let Some(following) = following else {
                    break;
                };
                next = following;
            }
        })
        .detach();
        cx.notify();
    }
    pub(super) fn render_drafts_section(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let rows = self.visible_prompt_drafts(cx);
        if rows.is_empty() {
            return None;
        }
        let open = self.drafts_open;
        let label = if open {
            "Drafts".into()
        } else {
            format!("Drafts ({})", rows.len()).into()
        };
        let height = rows.len() as f32 * 64.0 + spaces::SIDEBAR_DISCLOSURE_BODY_INSET;
        let chevron = self.sidebar_disclosure_chevron("drafts", open, theme);
        let header = spaces::sidebar_disclosure_header(theme, label, chevron)
            .id("drafts-toggle")
            .on_click(cx.listener(move |this, _, _, cx| {
                this.begin_sidebar_disclosure_motion(
                    "drafts",
                    if open { height } else { 0.0 },
                    if open { 0.0 } else { height },
                );
                this.drafts_open = !open;
                cx.notify();
            }));
        let items: Vec<_> = rows
            .into_iter()
            .map(|row| self.render_prompt_draft_row(row, theme, cx))
            .collect();
        let body = div()
            .flex()
            .flex_col()
            .pt(px(spaces::SIDEBAR_DISCLOSURE_BODY_INSET))
            .children(items)
            .into_any_element();
        Some(
            div()
                .id("sidebar-drafts-section")
                .flex()
                .flex_col()
                .pb(px(8.0))
                .child(header)
                .child(self.render_sidebar_disclosure_body("drafts", open, height, body))
                .into_any_element(),
        )
    }
    fn render_prompt_draft_row(
        &mut self,
        row: PromptDraft,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active = self.state.read(cx).selected_chat.is_none()
            && self.composer.read(cx).active_prompt_draft() == Some(row.id.as_str());
        let label = row
            .target
            .project_name
            .clone()
            .unwrap_or_else(|| "No project".into());
        let host = self
            .state
            .read(cx)
            .devices
            .iter()
            .find(|d| d.id == row.target.device_id)
            .map(|d| d.name.clone())
            .unwrap_or_else(|| row.target.device_id.clone());
        let target = if host.is_empty() {
            label
        } else {
            format!("{label} · {host}")
        };
        let id = row.id.clone();
        let discard = row.id.clone();
        let activate = row.clone();
        div()
            .id(SharedString::from(format!("draft-row-{id}")))
            .h(px(64.0))
            .w_full()
            .rounded(px(6.0))
            .px(px(10.0))
            .py(px(8.0))
            .flex()
            .flex_col()
            .gap(px(4.0))
            .cursor_pointer()
            .bg(if active {
                theme.accent.opacity(0.14)
            } else {
                theme.accent_wash
            })
            .hover(|style| style.bg(theme.accent.opacity(0.1)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.route = Route::Chat;
                this.composer.update(cx, |composer, cx| {
                    composer.open_prompt_draft(activate.clone(), cx)
                });
                cx.notify();
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(icon(icons::PEN).size(px(12.0)).text_color(theme.accent))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(crate::typography::ui_rems(11.0))
                            .text_color(theme.accent)
                            .child(target),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("discard-draft-{id}")))
                            .size(px(18.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.change_prompt_draft(
                                    DraftChange::Discard {
                                        id: discard.clone(),
                                    },
                                    cx,
                                );
                                if this.composer.read(cx).active_prompt_draft()
                                    == Some(discard.as_str())
                                {
                                    this.composer
                                        .update(cx, |composer, cx| composer.start_prompt_draft(cx));
                                }
                            }))
                            .child(
                                icon(icons::CLOSE)
                                    .size(px(12.0))
                                    .text_color(theme.text_muted),
                            ),
                    ),
            )
            .child(
                div()
                    .truncate()
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text)
                    .child(if row.conflict {
                        format!("Recovered edit · {}", row.preview)
                    } else {
                        row.preview
                    }),
            )
            .into_any_element()
    }
}
