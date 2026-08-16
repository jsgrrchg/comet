//! Workspace file browsing surface.

use std::{collections::HashMap, time::Duration};

use gpui::{
    App, Context, Entity, FocusHandle, ListAlignment, ListState, Render, SharedString,
    Subscription, Task, div, prelude::*, px,
};
use zeron_proto::ListWorkspaceDirectoryRequest;

use crate::state::AppState;

pub mod client;
pub mod model;
pub mod tree;

use client::{FilesRequestContext, WorkspaceFilesClient};
use model::{DirectoryLoadState, FileTreeModel};

pub struct FilesSurface {
    state: Entity<AppState>,
    chat_id: String,
    request_context: Option<FilesRequestContext>,
    tree: FileTreeModel,
    tree_list: ListState,
    tree_focus: FocusHandle,
    loads: HashMap<(String, Option<String>), Task<()>>,
    error: Option<SharedString>,
    started: bool,
    _observe: Subscription,
}

impl Render for FilesSurface {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::theme::Theme::of(cx).clone();
        let phase = self.tree.node("").map(|root| &root.load);
        let content = if let Some(error) = self.error.clone() {
            div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(10.0))
                .px(px(28.0))
                .child(
                    div()
                        .text_center()
                        .text_size(px(12.0))
                        .text_color(theme.text_muted)
                        .child(error),
                )
                .child(
                    div()
                        .id("files-retry-root")
                        .h(px(28.0))
                        .px(px(12.0))
                        .rounded(px(7.0))
                        .border_1()
                        .border_color(theme.border)
                        .bg(crate::theme::wash(0.04))
                        .hover(|style| style.bg(crate::theme::wash(0.09)))
                        .cursor_pointer()
                        .flex()
                        .items_center()
                        .text_size(px(11.5))
                        .text_color(theme.text)
                        .child("Retry")
                        .on_click(cx.listener(|this, _, _, cx| this.retry_root(cx))),
                )
                .into_any_element()
        } else if matches!(
            phase,
            Some(DirectoryLoadState::Unloaded | DirectoryLoadState::Loading { .. })
        ) {
            div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(10.0))
                .child(crate::loaders::gradient_spinner(
                    "files-loading-root",
                    &theme,
                    3.0,
                    cx.entity_id(),
                    cx,
                ))
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(theme.text_faint)
                        .child("Loading workspace…"),
                )
                .into_any_element()
        } else {
            self.render_tree(cx)
        };
        let header_bg = if theme.is_glass() {
            theme.surface.opacity(0.26)
        } else {
            theme.surface
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(crate::theme::ink(0.0))
            .child(
                div()
                    .h(px(36.0))
                    .flex_none()
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .border_b_1()
                    .border_color(theme.border)
                    .bg(header_bg)
                    .child(
                        crate::icons::icon(crate::icons::FOLDER_WITH_FILES)
                            .size(px(13.0))
                            .text_color(theme.text_muted),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child("Files"),
                    ),
            )
            .child(content)
    }
}

impl FilesSurface {
    pub fn new(state: Entity<AppState>, chat_id: String, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |this: &mut Self, _, cx| {
            if this.sync_target(cx) {
                this.ensure_loaded(cx);
            }
        });
        let mut surface = Self {
            state,
            chat_id,
            request_context: None,
            tree: FileTreeModel::new(),
            tree_list: ListState::new(0, ListAlignment::Top, px(560.0)),
            tree_focus: cx.focus_handle(),
            loads: HashMap::new(),
            error: None,
            started: false,
            _observe: observe,
        };
        surface.sync_target(cx);
        surface
    }

    pub fn ensure_loaded(&mut self, cx: &mut Context<Self>) {
        self.sync_target(cx);
        if self.started || self.request_context.is_none() {
            return;
        }
        self.started = true;
        self.load_directory(String::new(), None, cx);
    }

    pub fn retry_root(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        self.started = true;
        self.load_directory(String::new(), None, cx);
    }

    pub fn load_directory(
        &mut self,
        directory: String,
        cursor: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(request_context) = self.request_context.clone() else {
            self.error = Some("No workspace available for this chat.".into());
            cx.notify();
            return;
        };
        let generation = self.tree.generation();
        if !self.tree.begin_load(&directory, cursor.clone(), generation) {
            return;
        }
        self.sync_tree_list();
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.tree.fail_load(
                &directory,
                cursor,
                "Workspace service is still starting.",
                generation,
            );
            self.sync_tree_list();
            cx.notify();
            return;
        };
        let key = (directory.clone(), cursor.clone());
        let request = ListWorkspaceDirectoryRequest {
            target: request_context.target.clone(),
            directory: directory.clone(),
            include_ignored: self.tree.include_ignored(),
            cursor: cursor.clone(),
        };
        let client = WorkspaceFilesClient::new(engine, request_context);
        let task = cx.spawn(async move |this, cx| {
            let mut result = client.list_directory(request.clone()).await;
            if result.as_ref().is_err_and(|error| error.retryable()) {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                result = client.list_directory(request).await;
            }
            let _ = this.update(cx, |surface, cx| {
                if surface.tree.generation() != generation {
                    return;
                }
                match result {
                    Ok(page) => {
                        surface.error = None;
                        surface.tree.apply_page(page, generation);
                    }
                    Err(error) => {
                        let message = error.to_string();
                        if directory.is_empty() {
                            surface.error = Some(message.clone().into());
                        }
                        surface
                            .tree
                            .fail_load(&directory, cursor, message, generation);
                    }
                }
                surface.sync_tree_list();
                cx.notify();
            });
        });
        self.loads.insert(key, task);
        cx.notify();
    }

    pub fn tree(&self) -> &FileTreeModel {
        &self.tree
    }

    pub fn error(&self) -> Option<&SharedString> {
        self.error.as_ref()
    }

    pub fn chat_id(&self) -> &str {
        &self.chat_id
    }

    fn sync_target(&mut self, cx: &App) -> bool {
        let next = FilesRequestContext::for_chat(&self.state.read(cx), &self.chat_id);
        if self.request_context == next {
            return false;
        }
        self.loads.clear();
        self.tree.reset();
        self.sync_tree_list();
        self.error = if next.is_none() {
            Some("No workspace available for this chat.".into())
        } else {
            None
        };
        self.request_context = next;
        self.started = false;
        true
    }

    fn sync_tree_list(&self) {
        self.tree_list
            .reset_with_uniform_height(self.tree.visible_rows().len(), px(tree::TREE_ROW_HEIGHT));
    }
}
