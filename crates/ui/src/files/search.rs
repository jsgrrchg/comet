use std::time::Duration;

use gpui::{
    AnyElement, Context, ListSizingBehavior, SharedString, Task, Window, div, list, prelude::*, px,
};
use zeron_proto::{
    ListWorkspaceDirectoryRequest, SearchWorkspaceFilesRequest, WorkspaceEntryKind,
    WorkspaceFileSearchMatch,
};

use super::{FilesSurface, client::WorkspaceFilesClient, model::parent_path};
use crate::{
    icons::{self, icon},
    theme::Theme,
};

pub const SEARCH_ROW_HEIGHT: f32 = 42.0;

#[derive(Default)]
pub(super) struct FileSearchState {
    pub query: String,
    pub results: Vec<WorkspaceFileSearchMatch>,
    pub loading: bool,
    pub error: Option<SharedString>,
    pub generation: u64,
    pub active: usize,
    pub task: Option<Task<()>>,
    pub reveal_task: Option<Task<()>>,
}

impl FileSearchState {
    fn accepts(&self, generation: u64, query: &str) -> bool {
        self.generation == generation && self.query == query
    }
}

impl FilesSurface {
    pub(super) fn on_search_edited(&mut self, cx: &mut Context<Self>) {
        let query = self.search.read(cx).text().trim().to_string();
        if self.search_state.query == query {
            return;
        }
        self.search_state.generation = self.search_state.generation.wrapping_add(1);
        self.search_state.query = query.clone();
        self.search_state.active = 0;
        self.search_state.error = None;
        self.search_state.task = None;
        if query.is_empty() {
            self.search_state.loading = false;
            self.search_state.results.clear();
            self.search_list.reset(0);
            self.search.update(cx, |search, cx| {
                search.set_mention_controls(false, false, cx)
            });
            cx.notify();
            return;
        }
        self.search.update(cx, |search, cx| {
            search.set_mention_controls(true, false, cx)
        });
        let Some(context) = self.request_context.clone() else {
            self.search_state.loading = false;
            self.search_state.error = Some("No workspace available for this chat.".into());
            cx.notify();
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.search_state.loading = false;
            self.search_state.error = Some("Workspace service is still starting.".into());
            cx.notify();
            return;
        };
        self.search_state.loading = true;
        let generation = self.search_state.generation;
        let request = SearchWorkspaceFilesRequest {
            target: context.target.clone(),
            query: query.clone(),
            include_ignored: self.tree.include_ignored(),
            limit: Some(200),
        };
        let client = WorkspaceFilesClient::new(engine, context);
        self.search_state.task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(200))
                .await;
            let result = client.search(request).await;
            let _ = this.update(cx, |surface, cx| {
                if !surface.search_state.accepts(generation, &query) {
                    return;
                }
                surface.search_state.loading = false;
                match result {
                    Ok(results) => {
                        surface.search_state.error = None;
                        surface.search_state.results = results;
                        surface.search_state.active = 0;
                    }
                    Err(error) => {
                        surface.search_state.error = Some(error.to_string().into());
                    }
                }
                surface.search_list.reset_with_uniform_height(
                    surface.search_state.results.len(),
                    px(SEARCH_ROW_HEIGHT),
                );
                let has_results = !surface.search_state.results.is_empty();
                surface.search.update(cx, |search, cx| {
                    search.set_mention_controls(true, has_results, cx)
                });
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(super) fn clear_search(&mut self, cx: &mut Context<Self>) {
        self.search.update(cx, |search, cx| search.set_text("", cx));
    }

    pub(super) fn activate_search_result(&mut self, cx: &mut Context<Self>) {
        let Some(result) = self
            .search_state
            .results
            .get(self.search_state.active)
            .cloned()
        else {
            return;
        };
        self.reveal_search_result(result, cx);
    }

    pub(super) fn reveal_search_result(
        &mut self,
        result: WorkspaceFileSearchMatch,
        cx: &mut Context<Self>,
    ) {
        let Some(context) = self.request_context.clone() else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let mut directories = vec![String::new()];
        let mut ancestors = Vec::new();
        let mut current = parent_path(&result.path);
        while let Some(path) = current {
            if path.is_empty() {
                break;
            }
            ancestors.push(path.clone());
            current = parent_path(&path);
        }
        ancestors.reverse();
        directories.extend(ancestors.clone());
        let generation = self.tree.generation();
        let include_ignored = self.tree.include_ignored();
        let client = WorkspaceFilesClient::new(engine, context.clone());
        self.search_state.reveal_task = Some(cx.spawn(async move |this, cx| {
            let mut pages = Vec::with_capacity(directories.len());
            for directory in directories {
                match client
                    .list_directory(ListWorkspaceDirectoryRequest {
                        target: context.target.clone(),
                        directory,
                        include_ignored,
                        cursor: None,
                    })
                    .await
                {
                    Ok(page) => pages.push(page),
                    Err(error) => {
                        let _ = this.update(cx, |surface, cx| {
                            if surface.tree.generation() == generation {
                                surface.search_state.error = Some(error.to_string().into());
                                cx.notify();
                            }
                        });
                        return;
                    }
                }
            }
            let _ = this.update(cx, |surface, cx| {
                if surface.tree.generation() != generation {
                    return;
                }
                for (index, page) in pages.into_iter().enumerate() {
                    surface.tree.apply_page(page, generation);
                    if let Some(next) = ancestors.get(index) {
                        surface.tree.expand(next);
                    }
                }
                surface.tree.select(result.path.clone());
                surface.sync_tree_list();
                surface
                    .search
                    .update(cx, |search, cx| search.set_text("", cx));
                surface.reveal_tree_selection();
                if result.kind == WorkspaceEntryKind::Directory {
                    surface.show_tree_sidebar(cx);
                } else {
                    surface.open_tree_file(result.path.clone(), cx);
                }
                cx.notify();
            });
        }));
    }

    pub(super) fn render_search_results(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        if let Some(error) = self.search_state.error.clone() {
            return centered_search_message(error, theme.danger.opacity(0.82));
        }
        if self.search_state.results.is_empty() {
            let label = if self.search_state.loading {
                "Searching…"
            } else {
                "No files found."
            };
            return centered_search_message(label.into(), theme.text_faint);
        }
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                list(
                    self.search_list.clone(),
                    cx.processor(Self::render_search_row),
                )
                .flex_1()
                .min_h_0()
                .with_sizing_behavior(ListSizingBehavior::Auto),
            )
            .into_any_element()
    }

    fn render_search_row(
        &mut self,
        index: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(result) = self.search_state.results.get(index).cloned() else {
            return gpui::Empty.into_any_element();
        };
        let theme = Theme::of(cx).clone();
        let selected = self.search_state.active == index;
        let parent = parent_path(&result.path).unwrap_or_default();
        div()
            .id(("files-search-result", index))
            .h(px(SEARCH_ROW_HEIGHT))
            .w_full()
            .flex_none()
            .px(px(10.0))
            .flex()
            .items_center()
            .gap(px(7.0))
            .cursor_pointer()
            .when(selected, |element| element.bg(crate::theme::wash(0.1)))
            .when(!selected, |element| {
                element.hover(|style| style.bg(crate::theme::wash(0.055)))
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.search_state.active = index;
                this.activate_search_result(cx);
            }))
            .child(
                icon(if result.kind == WorkspaceEntryKind::Directory {
                    icons::FOLDER
                } else {
                    icons::DOCUMENT
                })
                .size(px(13.0))
                .flex_none()
                .text_color(theme.text_muted),
            )
            .child(
                div()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .truncate()
                            .font_family(theme.font_sans.clone())
                            .text_size(px(11.5))
                            .text_color(theme.text)
                            .child(result.name),
                    )
                    .child(
                        div()
                            .truncate()
                            .font_family(theme.font_sans.clone())
                            .text_size(px(9.5))
                            .text_color(theme.text_faint)
                            .child(parent),
                    ),
            )
            .into_any_element()
    }
}

fn centered_search_message(message: SharedString, color: gpui::Hsla) -> AnyElement {
    div()
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .px(px(24.0))
        .text_center()
        .text_size(px(11.5))
        .text_color(color)
        .child(message)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_state_rejects_stale_queries() {
        let state = FileSearchState {
            query: "shell".into(),
            generation: 4,
            ..Default::default()
        };
        assert!(state.accepts(4, "shell"));
        assert!(!state.accepts(3, "shell"));
        assert!(!state.accepts(4, "other"));
    }
}
