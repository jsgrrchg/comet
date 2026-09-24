//! The Agents page's device-addressed worktree destination settings.

use gpui::{AnyElement, Context, Entity, Render, Subscription, Task, Window, div, prelude::*, px};
use zeron_proto::{FolderListing, WorktreeSettings, WorktreeSettingsStatus};
use zeron_rpc::methods;

use super::widgets;
use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::icons;
use crate::popover::{self, Loadable};
use crate::state::AppState;
use crate::theme::Theme;
use zeron_proto::device_paths::{
    child_folder as child_path, expand_home, parent_folder as folder_parent,
};

pub(super) struct WorktreeSettingsCard {
    state: Entity<AppState>,
    target: Option<String>,
    settings: Loadable<WorktreeSettingsStatus>,
    enabled: bool,
    input: Entity<ComposerInput>,
    error: Option<String>,
    saving: bool,
    task: Option<Task<()>>,
    browsing: bool,
    folders: Loadable<FolderListing>,
    folder_task: Option<Task<()>>,
    _input_events: Subscription,
}

impl WorktreeSettingsCard {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            ComposerInput::new("Folder on the selected device", cx)
                .with_single_line()
                .with_text_metrics(12.0, 20.0)
        });
        let events = cx.subscribe(&input, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                if this.browsing {
                    this.browse_input(cx);
                } else {
                    this.save(cx);
                }
            }
            cx.notify();
        });
        Self {
            state,
            target: None,
            settings: Loadable::Idle,
            enabled: false,
            input,
            error: None,
            saving: false,
            task: None,
            browsing: false,
            folders: Loadable::Idle,
            folder_task: None,
            _input_events: events,
        }
    }

    pub fn load(&mut self, target: Option<String>, cx: &mut Context<Self>) {
        self.task = None;
        self.folder_task = None;
        self.target = target;
        self.settings = Loadable::Loading;
        self.enabled = false;
        self.saving = false;
        self.error = None;
        self.browsing = false;
        self.folders = Loadable::Idle;
        self.input.update(cx, |input, cx| input.set_text("", cx));
        self.request(None, cx);
    }

    fn request(&mut self, save: Option<WorktreeSettings>, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.settings = Loadable::Error("Device is not connected".into());
            cx.notify();
            return;
        };
        self.saving = save.is_some();
        let saving = self.saving;
        let method = if saving {
            methods::SET_WORKTREE_SETTINGS
        } else {
            methods::GET_WORKTREE_SETTINGS
        };
        let mut params = save
            .map(|settings| serde_json::to_value(settings).unwrap())
            .unwrap_or_else(|| serde_json::json!({}));
        params["targetDeviceId"] = serde_json::json!(self.target);
        self.error = None;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(method, params)
                .await
                .map_err(|error| match error {
                    zeron_rpc::RpcError::UnknownMethod(_) => {
                        "Update Zeron on the selected device to configure worktree locations."
                            .to_string()
                    }
                    _ => error.to_string(),
                })
                .and_then(|value| {
                    serde_json::from_value::<WorktreeSettingsStatus>(value)
                        .map_err(|error| error.to_string())
                });
            this.update(cx, |card, cx| {
                card.saving = false;
                match result {
                    Ok(status) => {
                        card.settings = Loadable::Ready(status);
                        card.reset_draft(cx);
                    }
                    Err(error) if saving => card.error = Some(error),
                    Err(error) => card.settings = Loadable::Error(error),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn reset_draft(&mut self, cx: &mut Context<Self>) {
        if let Loadable::Ready(status) = &self.settings {
            self.enabled = status.settings.use_custom_directory;
            let path = status.settings.custom_directory.clone().unwrap_or_default();
            self.input.update(cx, |input, cx| input.set_text(path, cx));
        }
        self.browsing = false;
        self.folder_task = None;
        self.error = None;
        cx.notify();
    }

    fn draft(&self, cx: &Context<Self>) -> WorktreeSettings {
        let path = self.input.read(cx).text().trim().to_owned();
        WorktreeSettings {
            use_custom_directory: self.enabled,
            custom_directory: (!path.is_empty()).then_some(path),
        }
    }

    fn has_changes(&self, cx: &Context<Self>) -> bool {
        let Loadable::Ready(status) = &self.settings else {
            return false;
        };
        let draft = self.draft(cx);
        draft.use_custom_directory != status.settings.use_custom_directory
            || (draft.use_custom_directory
                && draft.custom_directory != status.settings.custom_directory)
    }

    fn can_save(&self, cx: &Context<Self>) -> bool {
        let Loadable::Ready(status) = &self.settings else {
            return false;
        };
        !self.saving
            && status.environment_override.is_none()
            && (!self.enabled || self.draft(cx).custom_directory.is_some())
            && self.has_changes(cx)
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        if !self.can_save(cx) {
            return;
        }
        self.browsing = false;
        self.folder_task = None;
        self.request(Some(self.draft(cx)), cx);
    }

    fn browse_input(&mut self, cx: &mut Context<Self>) {
        let path = self.input.read(cx).text().trim().to_owned();
        self.browse((!path.is_empty()).then_some(path), cx);
    }

    fn browse(&mut self, path: Option<String>, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.browsing = true;
        self.folders = Loadable::Loading;
        let target = self.target.clone();
        self.folder_task = Some(cx.spawn(async move |this, cx| {
            let result: Result<FolderListing, String> = async {
                let mut path = path;
                if let Some(query) = path
                    .as_deref()
                    .filter(|p| *p == "~" || p.starts_with("~/") || p.starts_with(r"~\"))
                {
                    let home = engine
                        .client()
                        .call(
                            methods::LIST_FOLDERS,
                            serde_json::json!({"targetDeviceId": target}),
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                    let home: FolderListing =
                        serde_json::from_value(home).map_err(|error| error.to_string())?;
                    path =
                        Some(expand_home(query, &home.path).unwrap_or_else(|| query.to_string()));
                }
                let value = engine
                    .client()
                    .call(
                        methods::LIST_FOLDERS,
                        serde_json::json!({"path": path, "targetDeviceId": target}),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                serde_json::from_value(value).map_err(|error| error.to_string())
            }
            .await;
            this.update(cx, |card, cx| {
                card.folders = match result {
                    Ok(listing) => {
                        card.input
                            .update(cx, |input, cx| input.set_text(listing.path.clone(), cx));
                        Loadable::Ready(listing)
                    }
                    Err(error) => Loadable::Error(error),
                };
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn render_browser(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let parent = match &self.folders {
            Loadable::Ready(listing) => folder_parent(&listing.path),
            _ => None,
        };
        let has_parent = parent.is_some();
        let mut browser = div()
            .mt(px(12.0))
            .overflow_hidden()
            .border_1()
            .border_color(theme.border)
            .rounded(px(10.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(2.0))
                    .p(px(4.0))
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        widgets::ghost_action(theme)
                            .id("worktrees-home")
                            .hover(|style| widgets::ghost_hover(theme, style))
                            .child(
                                icons::icon(icons::HOME)
                                    .size(px(14.0))
                                    .text_color(theme.text_muted),
                            )
                            .child("Home")
                            .on_click(cx.listener(|card, _, _, cx| card.browse(None, cx))),
                    )
                    .child(
                        widgets::ghost_action(theme)
                            .id("worktrees-up")
                            .child(
                                icons::icon(icons::ARROW_UP)
                                    .size(px(14.0))
                                    .text_color(theme.text_muted),
                            )
                            .child("Up")
                            .when(has_parent, |el| {
                                el.hover(|style| widgets::ghost_hover(theme, style))
                                    .on_click(cx.listener(move |card, _, _, cx| {
                                        card.browse(parent.clone(), cx)
                                    }))
                            })
                            .when(!has_parent, |el| el.opacity(0.5)),
                    )
                    .child(
                        widgets::ghost_action(theme)
                            .id("worktrees-open-path")
                            .ml_auto()
                            .hover(|style| widgets::ghost_hover(theme, style))
                            .child("Open path")
                            .on_click(cx.listener(|card, _, _, cx| card.browse_input(cx))),
                    ),
            );
        match &self.folders {
            Loadable::Ready(listing) => {
                let mut entries = div()
                    .id("worktree-folder-list")
                    .p(px(4.0))
                    .max_h(px(200.0))
                    .overflow_y_scroll();
                for (index, entry) in listing
                    .entries
                    .iter()
                    .filter(|entry| entry.is_dir)
                    .enumerate()
                {
                    let path = child_path(&listing.path, &entry.name);
                    entries = entries.child(
                        widgets::ghost_action(theme)
                            .id(("worktree-folder", index))
                            .w_full()
                            .gap(px(8.0))
                            .hover(|style| widgets::ghost_hover(theme, style))
                            .child(
                                icons::icon(icons::FOLDER)
                                    .size(px(16.0))
                                    .flex_none()
                                    .text_color(theme.text_muted),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .child(entry.name.clone()),
                            )
                            .child(
                                icons::icon(icons::ALT_ARROW_RIGHT)
                                    .size(px(12.0))
                                    .flex_none()
                                    .text_color(theme.text_muted.opacity(0.5)),
                            )
                            .on_click(cx.listener(move |card, _, _, cx| {
                                card.browse(Some(path.clone()), cx)
                            })),
                    );
                }
                if !listing.entries.iter().any(|entry| entry.is_dir) {
                    entries = entries.child(
                        div()
                            .px(px(10.0))
                            .py(px(8.0))
                            .child(description(theme, "No subfolders")),
                    );
                }
                browser = browser
                    .child(
                        div()
                            .px(px(14.0))
                            .pt(px(10.0))
                            .pb(px(6.0))
                            .child(description(theme, &listing.path).truncate()),
                    )
                    .child(entries);
                if listing.truncated {
                    browser = browser.child(div().px(px(14.0)).py(px(6.0)).child(description(
                        theme,
                        "More folders exist. Enter a path to open one directly.",
                    )));
                }
                browser = browser.child(
                    div()
                        .flex()
                        .justify_end()
                        .p(px(4.0))
                        .border_t_1()
                        .border_color(theme.border)
                        .child(
                            widgets::ghost_action(theme)
                                .id("worktrees-use-folder")
                                .hover(|style| widgets::ghost_hover(theme, style))
                                .child("Use this folder")
                                .on_click(cx.listener(|card, _, _, cx| {
                                    if let Loadable::Ready(listing) = &card.folders {
                                        card.input.update(cx, |input, cx| {
                                            input.set_text(listing.path.clone(), cx)
                                        });
                                    }
                                    card.browsing = false;
                                    cx.notify();
                                })),
                        ),
                );
            }
            Loadable::Error(error) => {
                browser = browser.child(
                    div()
                        .p(px(10.0))
                        .child(widgets::error_strip(theme, error.clone())),
                )
            }
            _ => {
                browser = browser.child(
                    div()
                        .px(px(14.0))
                        .py(px(10.0))
                        .child(description(theme, "Loading folders…")),
                )
            }
        }
        browser.into_any_element()
    }
}

impl Render for WorktreeSettingsCard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let mut card = widgets::section_card(&theme).mt(px(20.0));
        let Loadable::Ready(status) = &self.settings else {
            let header = widgets::card_row(&theme, true)
                .child(widgets::row_tile(&theme, icons::FOLDER))
                .child(widgets::row_title(&theme, "Custom worktree location"));
            card = card.child(header);
            return match &self.settings {
                Loadable::Error(error) => card
                    .child(
                        div()
                            .px(px(20.0))
                            .pb(px(14.0))
                            .child(widgets::error_strip(&theme, error.clone()))
                            .child(
                                widgets::ghost_action(&theme)
                                    .id("worktrees-retry")
                                    .hover(|style| widgets::ghost_hover(&theme, style))
                                    .child("Retry")
                                    .on_click(cx.listener(|card, _, _, cx| {
                                        card.load(card.target.clone(), cx)
                                    })),
                            ),
                    )
                    .into_any_element(),
                _ => card
                    .child(
                        div()
                            .pl(px(70.0))
                            .pr(px(20.0))
                            .pb(px(14.0))
                            .child(description(&theme, "Loading worktree settings…")),
                    )
                    .into_any_element(),
            };
        };
        let overridden = status.environment_override.is_some();
        let interactive = !self.saving && !overridden;
        let enabled = self.enabled;
        let can_save = self.can_save(cx);
        let show_actions = !overridden && (self.has_changes(cx) || self.saving);
        let show_editor = enabled && !overridden;
        let subtitle = if !enabled && !overridden {
            status.default_directory.clone()
        } else {
            status.effective_directory.clone()
        };
        card = card.child(
            widgets::card_row(&theme, true)
                .child(widgets::row_tile(&theme, icons::FOLDER))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(widgets::row_title(&theme, "Custom worktree location"))
                        .when(!show_editor, |el| {
                            el.child(widgets::meta_line(
                                &theme,
                                vec![
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .child(subtitle)
                                        .into_any_element(),
                                ],
                            ))
                        }),
                )
                .child(
                    widgets::toggle_switch(&theme, enabled)
                        .id("worktrees-custom-toggle")
                        .flex_none()
                        .when(interactive, |el| {
                            el.cursor_pointer().on_click(cx.listener(|card, _, _, cx| {
                                card.enabled = !card.enabled;
                                card.browsing = false;
                                card.folder_task = None;
                                card.error = None;
                                cx.notify();
                            }))
                        })
                        .when(!interactive, |el| el.opacity(0.5)),
                ),
        );

        // Align the expanded controls with the label, after the shared 36px
        // icon tile and 14px gap used by the other Agents rows.
        let mut detail = div().pl(px(70.0)).pr(px(20.0)).pb(px(14.0));
        if overridden {
            return card
                .child(detail.child(description(
                    &theme,
                    "Location set by ZERON_WORKTREES_DIR on this device.",
                )))
                .into_any_element();
        }
        if show_editor {
            detail = detail.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .when(interactive, |el| {
                                el.child(popover::dialog_field(
                                    self.input.clone().into_any_element(),
                                ))
                            })
                            .when(!interactive, |el| {
                                el.child(description(&theme, self.input.read(cx).text()))
                            }),
                    )
                    .child(
                        widgets::ghost_action(&theme)
                            .id("worktrees-browse")
                            .flex_none()
                            .hover(|style| widgets::ghost_hover(&theme, style))
                            .child("Browse…")
                            .when(interactive, |el| {
                                el.on_click(cx.listener(|card, _, _, cx| card.browse_input(cx)))
                            })
                            .when(!interactive, |el| el.opacity(0.5)),
                    ),
            );
        }
        if self.browsing {
            detail = detail.child(self.render_browser(&theme, cx));
        }
        if let Some(error) = &self.error {
            detail = detail.child(
                div()
                    .mt(px(8.0))
                    .child(widgets::error_strip(&theme, error.clone())),
            );
        }
        if show_actions {
            detail = detail.child(
                div()
                    .mt(px(12.0))
                    .flex()
                    .justify_end()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        widgets::ghost_action(&theme)
                            .id("worktrees-cancel")
                            .child("Cancel")
                            .when(interactive, |el| {
                                el.hover(|style| widgets::ghost_hover(&theme, style))
                                    .on_click(cx.listener(|card, _, _, cx| card.reset_draft(cx)))
                            })
                            .when(!interactive, |el| el.opacity(0.5)),
                    )
                    .child(
                        widgets::ghost_action(&theme)
                            .id("worktrees-save")
                            .child(if self.saving { "Saving…" } else { "Save" })
                            .when(can_save, |el| {
                                el.text_color(theme.accent)
                                    .hover(|style| style.bg(theme.accent.opacity(0.08)))
                                    .on_click(cx.listener(|card, _, _, cx| card.save(cx)))
                            })
                            .when(!can_save, |el| el.opacity(0.5)),
                    ),
            );
        }
        card.when(
            show_editor || self.browsing || self.error.is_some() || show_actions,
            |card| card.child(detail),
        )
        .into_any_element()
    }
}

fn description(theme: &Theme, text: &str) -> gpui::Div {
    div()
        .text_size(crate::typography::ui_rems(widgets::ROW_DESCRIPTION_SIZE))
        .text_color(theme.text_muted.opacity(0.65))
        .child(text.to_owned())
}
