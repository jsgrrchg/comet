use super::*;
use zeron_proto::{HarnessId, NativeForkAvailability};

pub(super) fn eligible(entry: &SessionMessageEntry, harness: HarnessId, subagent: bool) -> bool {
    !subagent
        && zeron_proto::native_fork_provider(harness)
        && entry.role == MessageRole::Assistant
        && entry.status == Some(MessageStatus::Complete)
}

impl Transcript {
    /// Availability is resolved on state changes, never while rendering a row.
    pub(super) fn refresh_native_forks(&mut self, cx: &mut Context<Self>) {
        let state = self.state.read(cx);
        let Some(chat) = state
            .selected_chat_row()
            .filter(|_| self.doc_override.is_none())
            .cloned()
        else {
            self.native_forks.clear();
            self.native_fork_key.clear();
            return;
        };
        let harness = chat.config.as_ref().map(|c| c.harness).or_else(|| {
            state
                .transcript
                .iter()
                .find_map(|e| e.native_fork_point.as_ref().map(|p| p.harness))
        });
        let Some(harness) = harness else {
            self.native_forks.clear();
            return;
        };
        let supported = state.device_supports(
            &chat.device_id,
            zeron_proto::capabilities::NATIVE_MESSAGE_FORK_V1,
        );
        let entries: Vec<_> = state
            .transcript
            .iter()
            .filter(|e| eligible(e, harness, false))
            .collect();
        let points: Vec<_> = entries
            .iter()
            .map(|e| (&e.id, &e.native_fork_point))
            .collect();
        let versions: Vec<_> = state
            .harness_updates
            .iter()
            .filter(|s| s.harness == harness)
            .map(|s| (&s.installed_version, &s.phase))
            .collect();
        let key = serde_json::to_string(&(
            &chat.id,
            &chat.device_id,
            harness,
            supported,
            format!("{:?}", state.connection),
            points,
            versions,
        ))
        .unwrap_or_default();
        if self.native_fork_key == key {
            return;
        }
        self.native_fork_key = key.clone();
        self.native_forks.clear();
        let mut ids = Vec::new();
        for entry in entries {
            let reason = if !supported {
                "Update the chat host to fork this message"
            } else if entry.native_fork_point.is_none() {
                "Native fork point unavailable for this message"
            } else {
                ids.push(entry.id.clone());
                "Checking native fork availability…"
            };
            self.native_forks.insert(
                entry.id.clone(),
                NativeForkAvailability::unavailable(reason),
            );
        }
        let engine = state.engine().cloned();
        cx.notify();
        if !supported || ids.is_empty() {
            return;
        }
        let Some(engine) = engine else {
            return;
        };
        cx.spawn(async move |this, cx| {
            for chunk in ids.chunks(512) {
                let result = engine.client().call_as::<HashMap<String, NativeForkAvailability>>(
                    zeron_rpc::methods::GET_NATIVE_FORK_AVAILABILITY,
                    serde_json::json!({"sourceChatId":chat.id,"targetDeviceId":chat.device_id,"messageIds":chunk}),
                ).await;
                let _ = this.update(cx, |this, cx| {
                    if this.native_fork_key != key { return; }
                    match result {
                        Ok(values) => this.native_forks.extend(values),
                        Err(error) => for id in chunk {
                            this.native_forks.insert(id.clone(), NativeForkAvailability::unavailable(error.to_string()));
                        }
                    }
                    cx.notify();
                });
            }
        }).detach();
    }

    pub(crate) fn native_fork_finished(
        &mut self,
        chat: &str,
        message: &str,
        error: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.native_fork_pending
            .remove(&(chat.to_owned(), message.to_owned()));
        if let Some(error) = error {
            self.native_fork_errors
                .insert((chat.to_owned(), message.to_owned()), error);
        }
        cx.notify();
    }

    pub(super) fn native_fork_button(
        &self,
        entry: &SharedString,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let availability = self.native_forks.get(entry.as_ref())?;
        let chat = self.chat_id.clone()?;
        let message = entry.to_string();
        let identity = (chat.clone(), message.clone());
        let busy = self.native_fork_pending.contains(&identity);
        let enabled = availability.available && !busy;
        let label = if busy {
            "Creating side chat…"
        } else {
            "Fork in side chat"
        };
        let tooltip = if busy {
            label.to_owned()
        } else {
            self.native_fork_errors
                .get(&identity)
                .cloned()
                .or_else(|| availability.reason.clone())
                .unwrap_or_else(|| label.to_owned())
        };
        Some(
            div()
                .id(SharedString::from(format!("native-fork-{entry}")))
                .role(gpui::Role::Button)
                .aria_label(label)
                .aria_description(tooltip.clone())
                .tab_index(0)
                .focus_visible(|s| s.border_1().border_color(theme.accent))
                .size(px(Theme::SPACE_MD * 2.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(Theme::CONTROL_RADIUS))
                .when(enabled, |el| {
                    el.cursor_pointer().hover(|s| s.bg(crate::theme::ink(0.08)))
                })
                .tooltip(crate::settings::widgets::text_tooltip(tooltip))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if !enabled || this.native_fork_pending.contains(&identity) {
                        return;
                    }
                    this.native_fork_pending.insert(identity.clone());
                    this.native_fork_errors.remove(&identity);
                    cx.emit(TranscriptEvent::ForkMessage {
                        chat_id: chat.clone(),
                        message_id: message.clone(),
                    });
                    cx.notify();
                }))
                .child(
                    crate::icons::icon(crate::icons::GIT_BRANCH)
                        .size(px(14.0))
                        .text_color(theme.text_muted.opacity(if enabled { 1.0 } else { 0.4 })),
                )
                .into_any_element(),
        )
    }
}
