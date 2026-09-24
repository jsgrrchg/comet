//! The Agents page's device-addressed worktree destination settings.

use gpui::{AnyElement, Context, Entity, Render, Subscription, Task, Window, div, prelude::*, px};
use zeron_proto::{FolderListing, WorktreeSettings, WorktreeSettingsStatus};
use zeron_rpc::methods;

use super::widgets;
use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::pickers::{child_path, parent_path, typed_path_target};
use crate::popover::{self, Loadable};
use crate::state::AppState;
use crate::theme::Theme;

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

    fn can_save(&self, cx: &Context<Self>) -> bool {
        let Loadable::Ready(status) = &self.settings else {
            return false;
        };
        let draft = self.draft(cx);
        !self.saving
            && status.environment_override.is_none()
            && (!draft.use_custom_directory || draft.custom_directory.is_some())
            && (draft.use_custom_directory != status.settings.use_custom_directory
                || (draft.use_custom_directory
                    && draft.custom_directory != status.settings.custom_directory))
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
                if let Some(query) = path.as_deref().filter(|p| *p == "~" || p.starts_with("~/")) {
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
                    path = typed_path_target(query, Some(&home.path));
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
            .mt(px(8.0))
            .p(px(8.0))
            .border_1()
            .border_color(theme.border)
            .rounded(px(8.0))
            .child(
                div()
                    .flex()
                    .gap(px(8.0))
                    .child(
                        widgets::ghost_action(theme)
                            .id("worktrees-home")
                            .child("Home")
                            .on_click(cx.listener(|card, _, _, cx| card.browse(None, cx))),
                    )
                    .child(
                        widgets::ghost_action(theme)
                            .id("worktrees-up")
                            .child("Up")
                            .when(has_parent, |el| {
                                el.on_click(cx.listener(move |card, _, _, cx| {
                                    card.browse(parent.clone(), cx)
                                }))
                            })
                            .when(!has_parent, |el| el.opacity(0.5)),
                    )
                    .child(
                        widgets::ghost_action(theme)
                            .id("worktrees-open-path")
                            .child("Open path")
                            .on_click(cx.listener(|card, _, _, cx| card.browse_input(cx))),
                    ),
            );
        match &self.folders {
            Loadable::Ready(listing) => {
                let mut entries = div()
                    .id("worktree-folder-list")
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
                            .child(entry.name.clone())
                            .on_click(cx.listener(move |card, _, _, cx| {
                                card.browse(Some(path.clone()), cx)
                            })),
                    );
                }
                browser = browser
                    .child(
                        div()
                            .mt(px(8.0))
                            .text_size(px(12.0))
                            .child(listing.path.clone()),
                    )
                    .child(entries);
                if listing.truncated {
                    browser = browser.child(description(
                        theme,
                        "More folders exist. Enter a path to open one directly.",
                    ));
                }
                browser = browser.child(
                    widgets::ghost_action(theme)
                        .id("worktrees-use-folder")
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
                );
            }
            Loadable::Error(error) => {
                browser = browser.child(widgets::error_strip(theme, error.clone()))
            }
            _ => browser = browser.child("Loading folders…"),
        }
        browser.into_any_element()
    }
}

impl Render for WorktreeSettingsCard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let mut card = widgets::section_card(&theme).mt(px(20.0)).p(px(16.0))
            .child(widgets::row_title(&theme, "Worktrees"))
            .child(widgets::page_subtitle(&theme, "Choose where new worktrees are created on the selected device. Existing worktrees stay in their current locations."));
        let Loadable::Ready(status) = &self.settings else {
            return match &self.settings {
                Loadable::Error(error) => card
                    .child(widgets::error_strip(
                        &theme,
                        format!("Could not load worktree settings: {error}"),
                    ))
                    .child(
                        widgets::ghost_action(&theme)
                            .id("worktrees-retry")
                            .child("Retry")
                            .on_click(
                                cx.listener(|card, _, _, cx| card.load(card.target.clone(), cx)),
                            ),
                    )
                    .into_any_element(),
                _ => card.child("Loading worktree settings…").into_any_element(),
            };
        };
        let interactive = !self.saving && status.environment_override.is_none();
        let enabled = self.enabled;
        let can_save = self.can_save(cx);
        card = card
            .child(
                div()
                    .mt(px(8.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(widgets::row_title(&theme, "Use custom location"))
                    .child(
                        widgets::toggle_switch(&theme, enabled)
                            .id("worktrees-custom-toggle")
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
            )
            .child(div().mt(px(8.0)).child(description(
                &theme,
                &format!("Current location: {}", status.effective_directory),
            )));
        if status.environment_override.is_some() {
            return card
                .child(description(
                    &theme,
                    "This location is controlled by ZERON_WORKTREES_DIR on the selected device.",
                ))
                .into_any_element();
        }
        if enabled {
            card = card.child(div().mt(px(12.0)).child(widgets::row_title(&theme, "Folder")))
                .child(div().mt(px(6.0)).flex().items_center().gap(px(8.0))
                    .child(div().flex_1().min_w_0().when(interactive, |el| el.child(popover::dialog_field(self.input.clone().into_any_element())))
                        .when(!interactive, |el| el.child(self.input.read(cx).text().to_string())))
                    .child(widgets::ghost_action(&theme).id("worktrees-browse").child("Browse…")
                        .when(interactive, |el| el.on_click(cx.listener(|card, _, _, cx| card.browse_input(cx))))
                        .when(!interactive, |el| el.opacity(0.5))))
                .child(description(&theme, "New worktrees use <folder>/<repository>/<worktree>. Save to apply this location."));
        } else {
            card = card.child(description(
                &theme,
                &format!("Default location: {}", status.default_directory),
            ));
        }
        if self.browsing {
            card = card.child(self.render_browser(&theme, cx));
        }
        if let Some(error) = &self.error {
            card = card.child(widgets::error_strip(&theme, error.clone()));
        }
        card.child(
            div()
                .mt(px(12.0))
                .flex()
                .gap(px(8.0))
                .child(
                    widgets::ghost_action(&theme)
                        .id("worktrees-save")
                        .child(if self.saving { "Saving…" } else { "Save" })
                        .when(can_save, |el| {
                            el.on_click(cx.listener(|card, _, _, cx| card.save(cx)))
                        })
                        .when(!can_save, |el| el.opacity(0.5)),
                )
                .child(
                    widgets::ghost_action(&theme)
                        .id("worktrees-cancel")
                        .child("Cancel")
                        .when(interactive, |el| {
                            el.on_click(cx.listener(|card, _, _, cx| card.reset_draft(cx)))
                        })
                        .when(!interactive, |el| el.opacity(0.5)),
                ),
        )
        .into_any_element()
    }
}

fn description(theme: &Theme, text: &str) -> gpui::Div {
    div()
        .text_size(px(12.0))
        .text_color(theme.text_muted)
        .child(text.to_owned())
}

/// Interpret the host's paths, including Windows paths when this UI runs on Unix.
fn folder_parent(path: &str) -> Option<String> {
    if path.starts_with("\\\\") || path.as_bytes().get(1) == Some(&b':') {
        let path = path.replace('\\', "/");
        let path = path
            .strip_prefix("//?/UNC/")
            .map(|path| format!("//{path}"))
            .unwrap_or_else(|| path.strip_prefix("//?/").unwrap_or(&path).to_owned());
        if path.starts_with("//") && path.trim_matches('/').split('/').count() <= 2 {
            return None;
        }
        return parent_path(&path).map(|parent| {
            if parent.ends_with(':') {
                format!("{parent}/")
            } else {
                parent
            }
        });
    }
    parent_path(path)
}
