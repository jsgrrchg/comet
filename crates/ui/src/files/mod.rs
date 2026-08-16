//! Workspace file browsing surface.

use std::{collections::HashMap, time::Duration};

use gpui::{App, Context, Entity, SharedString, Subscription, Task};
use zeron_proto::ListWorkspaceDirectoryRequest;

use crate::state::AppState;

pub mod client;
pub mod model;

use client::{FilesRequestContext, WorkspaceFilesClient};
use model::FileTreeModel;

pub struct FilesSurface {
    state: Entity<AppState>,
    chat_id: String,
    request_context: Option<FilesRequestContext>,
    tree: FileTreeModel,
    loads: HashMap<(String, Option<String>), Task<()>>,
    error: Option<SharedString>,
    started: bool,
    _observe: Subscription,
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
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.tree.fail_load(
                &directory,
                cursor,
                "Workspace service is still starting.",
                generation,
            );
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
        self.error = if next.is_none() {
            Some("No workspace available for this chat.".into())
        } else {
            None
        };
        self.request_context = next;
        self.started = false;
        true
    }
}
