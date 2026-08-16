use std::{cell::Cell, collections::HashMap, rc::Rc, sync::Arc};

use gpui::{
    AnyElement, Context, ListAlignment, ListSizingBehavior, ListState, Point, Render, ScrollHandle,
    SharedString, Task, Window, div, font, list, prelude::*, px,
};
use zeron_proto::{
    ReadWorkspaceFileRequest, WorkspaceFileSearchMatch, WorkspaceFileText, WorkspaceReadOnlyReason,
};

use super::{FilesSurface, client::WorkspaceFilesClient};
use crate::{
    icons::{self, icon},
    syntax_cache::{DocumentHighlightKey, SyntaxHighlightCache},
    theme::Theme,
};

const PREVIEW_LINE_HEIGHT: f32 = 20.0;
const WIDE_BREAKPOINT: f32 = 680.0;
const TREE_SPLIT_DEFAULT: f32 = 286.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FilesNarrowView {
    Tree,
    Preview,
}

enum DocumentPhase {
    Loading,
    Ready(ReadyDocument),
    Error(SharedString),
}

struct ReadyDocument {
    file: WorkspaceFileText,
    lines: Arc<Vec<SharedString>>,
}

struct FileDocument {
    phase: DocumentPhase,
    externally_modified: bool,
}

struct HighlightedFile {
    content_hash: String,
    document: Arc<zeron_syntax::HighlightedDocument>,
}

pub(super) struct FilePreviewState {
    documents: HashMap<String, FileDocument>,
    tabs: Vec<String>,
    active: Option<String>,
    reads: HashMap<String, Task<()>>,
    highlight_tasks: HashMap<String, Task<()>>,
    highlights: HashMap<String, HighlightedFile>,
    syntax_cache: SyntaxHighlightCache,
    list: ListState,
    horizontal_scroll: ScrollHandle,
    tab_scroll: ScrollHandle,
    surface_width: Rc<Cell<f32>>,
    narrow_view: FilesNarrowView,
    tree_width: f32,
}

impl FilePreviewState {
    pub(super) fn new() -> Self {
        Self {
            documents: HashMap::new(),
            tabs: Vec::new(),
            active: None,
            reads: HashMap::new(),
            highlight_tasks: HashMap::new(),
            highlights: HashMap::new(),
            syntax_cache: SyntaxHighlightCache::default(),
            list: ListState::new(0, ListAlignment::Top, px(520.0)),
            horizontal_scroll: ScrollHandle::new(),
            tab_scroll: ScrollHandle::new(),
            surface_width: Rc::new(Cell::new(520.0)),
            narrow_view: FilesNarrowView::Tree,
            tree_width: TREE_SPLIT_DEFAULT,
        }
    }

    pub(super) fn reset(&mut self) {
        self.documents.clear();
        self.tabs.clear();
        self.active = None;
        self.reads.clear();
        self.highlight_tasks.clear();
        self.highlights.clear();
        self.list.reset(0);
        self.narrow_view = FilesNarrowView::Tree;
    }

    pub(super) fn has_active(&self) -> bool {
        self.active.is_some()
    }

    pub(super) fn is_wide(&self) -> bool {
        self.surface_width.get() >= WIDE_BREAKPOINT
    }

    pub(super) fn width_cell(&self) -> Rc<Cell<f32>> {
        self.surface_width.clone()
    }

    pub(super) fn narrow_view(&self) -> FilesNarrowView {
        self.narrow_view
    }

    pub(super) fn tree_width(&self) -> f32 {
        self.tree_width
    }

    pub(super) fn mark_external(&mut self, path: &str) {
        if let Some(document) = self.documents.get_mut(path) {
            document.externally_modified = true;
        }
    }
}

pub(super) struct PreviewSplitResize;

struct PreviewDragGhost;

impl Render for PreviewDragGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size(px(1.0))
    }
}

impl FilesSurface {
    pub(super) fn show_tree(&mut self, cx: &mut Context<Self>) {
        self.preview.narrow_view = FilesNarrowView::Tree;
        cx.notify();
    }

    pub(super) fn open_file(&mut self, path: String, cx: &mut Context<Self>) {
        if !self.preview.tabs.contains(&path) {
            self.preview.tabs.push(path.clone());
        }
        self.preview.active = Some(path.clone());
        self.preview.narrow_view = FilesNarrowView::Preview;
        if !self.preview.documents.contains_key(&path) {
            self.preview.documents.insert(
                path.clone(),
                FileDocument {
                    phase: DocumentPhase::Loading,
                    externally_modified: false,
                },
            );
            self.read_file(path, cx);
        } else {
            self.sync_preview_list();
        }
        cx.notify();
    }

    fn read_file(&mut self, path: String, cx: &mut Context<Self>) {
        let Some(context) = self.request_context.clone() else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            if let Some(document) = self.preview.documents.get_mut(&path) {
                document.phase =
                    DocumentPhase::Error("Workspace service is still starting.".into());
            }
            return;
        };
        if let Some(document) = self.preview.documents.get_mut(&path) {
            document.phase = DocumentPhase::Loading;
            document.externally_modified = false;
        }
        let generation = self.tree.generation();
        let request = ReadWorkspaceFileRequest {
            target: context.target.clone(),
            path: path.clone(),
        };
        let client = WorkspaceFilesClient::new(engine, context);
        let task_path = path.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = client.read_file(request).await;
            let _ = this.update(cx, |surface, cx| {
                if surface.tree.generation() != generation {
                    return;
                }
                let Some(document) = surface.preview.documents.get_mut(&task_path) else {
                    return;
                };
                match result {
                    Ok(file) => {
                        let lines = Arc::new(
                            file.text
                                .as_deref()
                                .unwrap_or_default()
                                .split('\n')
                                .map(|line| SharedString::from(line.to_string()))
                                .collect::<Vec<_>>(),
                        );
                        let highlight = file
                            .text
                            .as_ref()
                            .zip(file.content_hash.as_ref())
                            .map(|(source, hash)| (source.clone(), hash.clone()));
                        document.phase = DocumentPhase::Ready(ReadyDocument { file, lines });
                        document.externally_modified = false;
                        surface.sync_preview_list();
                        if let Some((source, hash)) = highlight {
                            surface.request_file_highlight(task_path.clone(), source, hash, cx);
                        }
                    }
                    Err(error) => {
                        document.phase = DocumentPhase::Error(error.to_string().into());
                        surface.sync_preview_list();
                    }
                }
                cx.notify();
            });
        });
        self.preview.reads.insert(path, task);
        self.sync_preview_list();
        cx.notify();
    }

    fn request_file_highlight(
        &mut self,
        path: String,
        source: String,
        content_hash: String,
        cx: &mut Context<Self>,
    ) {
        let Some(language) = zeron_syntax::language_for_path(&path) else {
            return;
        };
        let key = DocumentHighlightKey::new(language, &source);
        if let Some(document) = self.preview.syntax_cache.get(&key) {
            self.preview.highlights.insert(
                path,
                HighlightedFile {
                    content_hash,
                    document,
                },
            );
            cx.notify();
            return;
        }
        let highlight_path = path.clone();
        let task = cx.spawn(async move |this, cx| {
            let request_path = highlight_path.clone();
            let highlighted = cx
                .background_executor()
                .spawn(async move {
                    zeron_syntax::highlight(zeron_syntax::HighlightRequest {
                        source: &source,
                        path: Some(&request_path),
                        fence_tag: None,
                    })
                    .ok()
                    .map(Arc::new)
                })
                .await;
            let _ = this.update(cx, |surface, cx| {
                let still_current =
                    surface
                        .preview
                        .documents
                        .get(&highlight_path)
                        .and_then(|document| match &document.phase {
                            DocumentPhase::Ready(ready) => ready.file.content_hash.as_deref(),
                            _ => None,
                        })
                        == Some(content_hash.as_str());
                if !still_current {
                    return;
                }
                if let Some(document) = highlighted {
                    surface.preview.syntax_cache.insert(key, document.clone());
                    surface.preview.highlights.insert(
                        highlight_path.clone(),
                        HighlightedFile {
                            content_hash,
                            document,
                        },
                    );
                    cx.notify();
                }
            });
        });
        self.preview.highlight_tasks.insert(path, task);
    }

    fn sync_preview_list(&self) {
        let count = self
            .preview
            .active
            .as_deref()
            .and_then(|path| self.preview.documents.get(path))
            .and_then(|document| match &document.phase {
                DocumentPhase::Ready(ready) => Some(ready.lines.len()),
                _ => None,
            })
            .unwrap_or(0);
        self.preview
            .list
            .reset_with_uniform_height(count, px(PREVIEW_LINE_HEIGHT));
    }

    pub(super) fn close_document(&mut self, path: &str, cx: &mut Context<Self>) {
        let Some(index) = self.preview.tabs.iter().position(|tab| tab == path) else {
            return;
        };
        self.preview.tabs.remove(index);
        self.preview.documents.remove(path);
        self.preview.reads.remove(path);
        self.preview.highlight_tasks.remove(path);
        self.preview.highlights.remove(path);
        if self.preview.active.as_deref() == Some(path) {
            self.preview.active = next_tab_after_close(&self.preview.tabs, index);
        }
        if self.preview.active.is_none() {
            self.preview.narrow_view = FilesNarrowView::Tree;
        }
        self.sync_preview_list();
        cx.notify();
    }

    fn select_document(&mut self, path: String, cx: &mut Context<Self>) {
        self.preview.active = Some(path);
        self.preview.narrow_view = FilesNarrowView::Preview;
        self.sync_preview_list();
        cx.notify();
    }

    fn reload_active_document(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = self.preview.active.clone() {
            self.read_file(path, cx);
        }
    }

    pub(super) fn render_preview(&mut self, narrow: bool, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let Some(active) = self.preview.active.clone() else {
            return gpui::Empty.into_any_element();
        };
        let tabs = self.render_document_tabs(narrow, &theme, cx);
        let breadcrumb = self.render_breadcrumb(&active, &theme, cx);
        let external = self
            .preview
            .documents
            .get(&active)
            .is_some_and(|document| document.externally_modified);
        let body = self.render_document_body(&active, &theme, cx);
        div()
            .size_full()
            .min_w_0()
            .flex()
            .flex_col()
            .child(tabs)
            .child(breadcrumb)
            .when(external, |element| {
                element.child(
                    div()
                        .h(px(30.0))
                        .flex_none()
                        .px(px(10.0))
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .border_b_1()
                        .border_color(theme.warning.opacity(0.25))
                        .bg(theme.warning.opacity(0.055))
                        .text_size(px(10.5))
                        .text_color(theme.warning_muted)
                        .child("This file changed outside Zeron.")
                        .child(
                            div()
                                .id("files-reload-external")
                                .ml_auto()
                                .cursor_pointer()
                                .text_color(theme.text)
                                .child("Reload")
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.reload_active_document(cx)),
                                ),
                        ),
                )
            })
            .child(body)
            .into_any_element()
    }

    fn render_document_tabs(
        &mut self,
        narrow: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active = self.preview.active.clone();
        let mut tabs = div()
            .id("files-preview-tabs")
            .flex()
            .items_center()
            .gap(px(2.0))
            .min_w_0()
            .overflow_x_scroll()
            .track_scroll(&self.preview.tab_scroll);
        for (index, path) in self.preview.tabs.clone().into_iter().enumerate() {
            let selected = active.as_deref() == Some(path.as_str());
            let name = path.rsplit('/').next().unwrap_or(&path).to_string();
            let select_path = path.clone();
            let close_path = path.clone();
            tabs = tabs.child(
                div()
                    .id(("files-document-tab", index))
                    .h(px(25.0))
                    .max_w(px(150.0))
                    .flex_none()
                    .pl(px(8.0))
                    .pr(px(4.0))
                    .rounded(px(6.0))
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .cursor_pointer()
                    .when(selected, |element| element.bg(crate::theme::wash(0.1)))
                    .when(!selected, |element| {
                        element.hover(|style| style.bg(crate::theme::wash(0.055)))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_document(select_path.clone(), cx)
                    }))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_family(theme.font_mono.clone())
                            .text_size(px(10.5))
                            .text_color(if selected {
                                theme.text
                            } else {
                                theme.text_muted
                            })
                            .child(name),
                    )
                    .child(
                        div()
                            .id(("files-document-close", index))
                            .size(px(16.0))
                            .flex_none()
                            .rounded(px(4.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .hover(|style| style.bg(crate::theme::wash(0.1)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.close_document(&close_path, cx);
                            }))
                            .child(
                                icon(icons::CLOSE)
                                    .size(px(10.0))
                                    .text_color(theme.text_faint),
                            ),
                    ),
            );
        }
        div()
            .h(px(34.0))
            .flex_none()
            .px(px(6.0))
            .border_b_1()
            .border_color(theme.border)
            .bg(if theme.is_glass() {
                theme.surface.opacity(0.24)
            } else {
                theme.surface
            })
            .flex()
            .items_center()
            .gap(px(4.0))
            .when(narrow, |element| {
                element.child(
                    div()
                        .id("files-preview-back")
                        .h(px(24.0))
                        .px(px(5.0))
                        .flex_none()
                        .rounded(px(5.0))
                        .flex()
                        .items_center()
                        .gap(px(2.0))
                        .cursor_pointer()
                        .hover(|style| style.bg(crate::theme::wash(0.07)))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.show_tree(cx);
                        }))
                        .child(
                            icon(icons::ALT_ARROW_LEFT)
                                .size(px(11.0))
                                .text_color(theme.text_muted),
                        )
                        .child(
                            div()
                                .text_size(px(10.5))
                                .text_color(theme.text_muted)
                                .child("Files"),
                        ),
                )
            })
            .child(div().min_w_0().flex_1().child(tabs))
            .into_any_element()
    }

    fn render_breadcrumb(
        &mut self,
        path: &str,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let parts = path.split('/').collect::<Vec<_>>();
        let reveal_path = path.to_string();
        let mut crumbs = div()
            .min_w_0()
            .flex_1()
            .flex()
            .items_center()
            .overflow_hidden();
        for (index, part) in parts.iter().enumerate() {
            if index > 0 {
                crumbs = crumbs.child(
                    div()
                        .mx(px(4.0))
                        .text_size(px(10.0))
                        .text_color(theme.text_faint.opacity(0.65))
                        .child("›"),
                );
            }
            crumbs = crumbs.child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_family(theme.font_mono.clone())
                    .text_size(px(10.0))
                    .text_color(if index + 1 == parts.len() {
                        theme.text_muted
                    } else {
                        theme.text_faint
                    })
                    .child((*part).to_string()),
            );
        }
        div()
            .h(px(31.0))
            .flex_none()
            .px(px(10.0))
            .border_b_1()
            .border_color(theme.border)
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(crumbs)
            .child(
                div()
                    .id("files-reveal-active")
                    .size(px(22.0))
                    .flex_none()
                    .rounded(px(5.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .hover(|style| style.bg(crate::theme::wash(0.07)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let name = reveal_path
                            .rsplit('/')
                            .next()
                            .unwrap_or(&reveal_path)
                            .to_string();
                        this.reveal_search_result(
                            WorkspaceFileSearchMatch {
                                path: reveal_path.clone(),
                                name,
                                kind: zeron_proto::WorkspaceEntryKind::File,
                                score: 0,
                            },
                            cx,
                        );
                    }))
                    .child(
                        icon(icons::FOLDER)
                            .size(px(11.5))
                            .text_color(theme.text_muted),
                    ),
            )
            .child(
                div()
                    .id("files-refresh-active")
                    .size(px(22.0))
                    .flex_none()
                    .rounded(px(5.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .hover(|style| style.bg(crate::theme::wash(0.07)))
                    .on_click(cx.listener(|this, _, _, cx| this.reload_active_document(cx)))
                    .child(
                        icon(icons::REFRESH)
                            .size(px(11.5))
                            .text_color(theme.text_muted),
                    ),
            )
            .into_any_element()
    }

    fn render_document_body(
        &mut self,
        path: &str,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(document) = self.preview.documents.get(path) else {
            return gpui::Empty.into_any_element();
        };
        match &document.phase {
            DocumentPhase::Loading => centered_state("Loading file…", theme.text_faint),
            DocumentPhase::Error(error) => centered_state(error.clone(), theme.danger_muted),
            DocumentPhase::Ready(ready) => {
                if ready.file.text.is_none() {
                    return centered_state(
                        read_only_message(ready.file.read_only_reason),
                        theme.text_muted,
                    );
                }
                let truncated = ready.file.truncated;
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .when(truncated, |element| {
                        element.child(
                            div()
                                .h(px(28.0))
                                .flex_none()
                                .px(px(10.0))
                                .border_b_1()
                                .border_color(theme.border)
                                .bg(theme.warning.opacity(0.045))
                                .flex()
                                .items_center()
                                .text_size(px(10.0))
                                .text_color(theme.warning_muted)
                                .child("Large file preview is truncated and read-only."),
                        )
                    })
                    .child(
                        div()
                            .id("files-preview-code-scroll")
                            .flex_1()
                            .min_h_0()
                            .overflow_x_scroll()
                            .track_scroll(&self.preview.horizontal_scroll)
                            .child(
                                list(
                                    self.preview.list.clone(),
                                    cx.processor(Self::render_preview_line),
                                )
                                .min_w_full()
                                .h_full()
                                .with_sizing_behavior(ListSizingBehavior::Auto),
                            ),
                    )
                    .into_any_element()
            }
        }
    }

    fn render_preview_line(
        &mut self,
        index: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(path) = self.preview.active.as_deref() else {
            return gpui::Empty.into_any_element();
        };
        let Some(document) = self.preview.documents.get(path) else {
            return gpui::Empty.into_any_element();
        };
        let DocumentPhase::Ready(ready) = &document.phase else {
            return gpui::Empty.into_any_element();
        };
        let Some(line) = ready.lines.get(index) else {
            return gpui::Empty.into_any_element();
        };
        let theme = Theme::of(cx).clone();
        let spans = self
            .preview
            .highlights
            .get(path)
            .filter(|highlight| {
                ready.file.content_hash.as_deref() == Some(highlight.content_hash.as_str())
            })
            .and_then(|highlight| highlight.document.lines.get(index))
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let mono = font(theme.font_mono.clone());
        let runs = crate::markdown::render::runs_for_syntax_line_with_plain(
            line.as_ref(),
            spans,
            &mono,
            theme.text.opacity(0.93),
            &theme,
        );
        div()
            .h(px(PREVIEW_LINE_HEIGHT))
            .min_w_full()
            .flex_none()
            .flex()
            .items_center()
            .child(
                div()
                    .w(px(48.0))
                    .h_full()
                    .flex_none()
                    .pr(px(10.0))
                    .border_r_1()
                    .border_color(theme.border.opacity(0.55))
                    .flex()
                    .items_center()
                    .justify_end()
                    .font_family(theme.font_mono.clone())
                    .text_size(px(10.0))
                    .text_color(theme.text_faint.opacity(0.7))
                    .child((index + 1).to_string()),
            )
            .child(
                div()
                    .pl(px(12.0))
                    .pr(px(18.0))
                    .whitespace_nowrap()
                    .font_family(theme.font_mono.clone())
                    .text_size(px(11.5))
                    .child(gpui::StyledText::new(line.clone()).with_runs(runs)),
            )
            .into_any_element()
    }

    pub(super) fn on_preview_split_drag(
        &mut self,
        event: &gpui::DragMoveEvent<PreviewSplitResize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let width = f32::from(event.bounds.right() - event.event.position.x);
        self.preview.tree_width = width.clamp(220.0, 360.0);
        cx.notify();
    }

    pub(super) fn preview_split_handle(&self, cx: &mut Context<Self>) -> AnyElement {
        let color = Theme::of(cx).border_strong;
        div()
            .id("files-preview-split")
            .w(px(5.0))
            .h_full()
            .flex_none()
            .cursor_col_resize()
            .hover(move |style| style.bg(color))
            .on_drag(
                PreviewSplitResize,
                |_, _point: Point<gpui::Pixels>, _, cx| {
                    cx.stop_propagation();
                    cx.new(|_| PreviewDragGhost)
                },
            )
            .into_any_element()
    }
}

fn centered_state(message: impl Into<SharedString>, color: gpui::Hsla) -> AnyElement {
    div()
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .px(px(24.0))
        .text_center()
        .text_size(px(11.5))
        .text_color(color)
        .child(message.into())
        .into_any_element()
}

fn read_only_message(reason: Option<WorkspaceReadOnlyReason>) -> SharedString {
    match reason {
        Some(WorkspaceReadOnlyReason::Binary) => "Binary files cannot be previewed.",
        Some(WorkspaceReadOnlyReason::UnsupportedEncoding) => {
            "This file encoding is not supported."
        }
        Some(WorkspaceReadOnlyReason::Symlink) => "Symlink targets are read-only.",
        Some(WorkspaceReadOnlyReason::PermissionDenied) => "Permission denied.",
        Some(WorkspaceReadOnlyReason::TooLarge) => "This file is too large to preview.",
        Some(WorkspaceReadOnlyReason::MixedLineEndings) => {
            "Files with mixed line endings are read-only."
        }
        Some(WorkspaceReadOnlyReason::NotRegularFile) | None => "This file cannot be previewed.",
    }
    .into()
}

pub(super) fn next_tab_after_close(tabs: &[String], closed: usize) -> Option<String> {
    if tabs.is_empty() {
        None
    } else {
        tabs.get(closed.min(tabs.len() - 1)).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_a_tab_selects_the_next_or_previous_neighbor() {
        let tabs = vec!["a".into(), "c".into()];
        assert_eq!(next_tab_after_close(&tabs, 1).as_deref(), Some("c"));
        assert_eq!(next_tab_after_close(&tabs, 2).as_deref(), Some("c"));
        assert_eq!(next_tab_after_close(&[], 0), None);
    }

    #[test]
    fn read_only_reasons_have_specific_messages() {
        assert!(read_only_message(Some(WorkspaceReadOnlyReason::Binary)).contains("Binary"));
        assert!(
            read_only_message(Some(WorkspaceReadOnlyReason::UnsupportedEncoding))
                .contains("encoding")
        );
    }
}
