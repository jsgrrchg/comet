use std::{cell::Cell, collections::HashMap, rc::Rc, sync::Arc};

use gpui::{
    AnyElement, Context, Focusable as _, ListAlignment, ListSizingBehavior, ListState, Point,
    Render, ScrollHandle, SharedString, Window, div, font, list, prelude::*, px,
};
use zeron_proto::{ReadWorkspaceFileRequest, WorkspaceFileSearchMatch, WorkspaceReadOnlyReason};

use super::{
    FilesSurface,
    client::{FilesRequestContext, WorkspaceFilesClient},
    document::{DocumentKey, DocumentPhase, FileDocument},
};
use crate::{
    icons::{self, icon},
    syntax_cache::{DocumentHighlightKey, SyntaxHighlightCache},
    theme::Theme,
};

const PREVIEW_LINE_HEIGHT: f32 = 20.0;
const WIDE_BREAKPOINT: f32 = 680.0;
const TREE_SPLIT_DEFAULT: f32 = 286.0;

struct HighlightedFile {
    content_hash: String,
    document: Arc<zeron_syntax::HighlightedDocument>,
}

pub(super) struct FilePreviewState {
    documents: HashMap<String, FileDocument>,
    active: Option<String>,
    highlights: HashMap<String, HighlightedFile>,
    syntax_cache: SyntaxHighlightCache,
    list: ListState,
    horizontal_scroll: ScrollHandle,
    surface_width: Rc<Cell<f32>>,
    word_wrap: bool,
    tree_sidebar_visible: bool,
    tree_width: f32,
}

impl FilePreviewState {
    pub(super) fn new() -> Self {
        Self {
            documents: HashMap::new(),
            active: None,
            highlights: HashMap::new(),
            syntax_cache: SyntaxHighlightCache::default(),
            list: ListState::new(0, ListAlignment::Top, px(520.0)),
            horizontal_scroll: ScrollHandle::new(),
            surface_width: Rc::new(Cell::new(520.0)),
            word_wrap: false,
            tree_sidebar_visible: false,
            tree_width: TREE_SPLIT_DEFAULT,
        }
    }

    pub(super) fn reset(&mut self) {
        self.documents.clear();
        self.active = None;
        self.highlights.clear();
        self.list.reset(0);
        self.tree_sidebar_visible = false;
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

    pub(super) fn tree_sidebar_visible(&self) -> bool {
        self.tree_sidebar_visible
    }

    fn word_wrap(&self) -> bool {
        self.word_wrap
    }

    pub(super) fn tree_width(&self) -> f32 {
        self.tree_width
    }

    pub(super) fn narrow_tree_width(&self) -> f32 {
        (self.surface_width.get() * 0.44).clamp(152.0, self.tree_width)
    }

    pub(super) fn mark_external(&mut self, path: &str) {
        if let Some(document) = self.documents.get_mut(path) {
            document.phase = DocumentPhase::ExternallyModified { disk_hash: None };
        }
    }
}

fn document_key(context: &FilesRequestContext, path: String) -> DocumentKey {
    DocumentKey {
        chat_id: context.target.chat_id.clone().unwrap_or_default(),
        checkout_id: context.checkout_id.clone(),
        path,
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
    pub(super) fn show_tree_sidebar(&mut self, cx: &mut Context<Self>) {
        self.preview.tree_sidebar_visible = true;
        cx.notify();
    }

    fn toggle_tree_sidebar(&mut self, cx: &mut Context<Self>) {
        self.preview.tree_sidebar_visible = !self.preview.tree_sidebar_visible;
        cx.notify();
    }

    fn toggle_word_wrap(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.preview.word_wrap = !self.preview.word_wrap;
        self.preview.list.remeasure();
        if let Some(editor) = self
            .preview
            .active
            .as_deref()
            .and_then(|path| self.preview.documents.get(path))
            .and_then(|document| document.editor.clone())
        {
            let word_wrap = self.preview.word_wrap;
            editor.update(cx, |state, cx| state.set_soft_wrap(word_wrap, window, cx));
        }
        cx.notify();
    }

    pub(super) fn open_file(&mut self, path: String, cx: &mut Context<Self>) {
        self.preview.active = Some(path.clone());
        self.preview.tree_sidebar_visible = false;
        if !self.preview.documents.contains_key(&path) {
            let Some(context) = self.request_context.as_ref() else {
                return;
            };
            self.preview.documents.insert(
                path.clone(),
                FileDocument::loading(document_key(context, path.clone())),
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
        let Some(document) = self.preview.documents.get_mut(&path) else {
            return;
        };
        let key = document_key(&context, path.clone());
        document.key = key.clone();
        let generation = document.begin_load();
        let request = ReadWorkspaceFileRequest {
            target: context.target.clone(),
            path: path.clone(),
        };
        let client = WorkspaceFilesClient::new(engine, context.clone());
        let task_path = path.clone();
        let task_key = key.clone();
        let task = cx.spawn(async move |this, cx| {
            let mut result = client.read_file(request.clone()).await;
            if result.as_ref().is_err_and(|error| error.retryable()) {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(250))
                    .await;
                result = client.read_file(request).await;
            }
            let _ = this.update(cx, |surface, cx| {
                if surface.request_context.as_ref() != Some(&context) {
                    return;
                }
                let Some(document) = surface.preview.documents.get_mut(&task_path) else {
                    return;
                };
                if !document.accepts(&task_key, generation) {
                    return;
                }
                match result {
                    Ok(file) => {
                        let highlight = file
                            .text
                            .as_ref()
                            .zip(file.content_hash.as_ref())
                            .map(|(source, hash)| (source.clone(), hash.clone()));
                        document.set_loaded(file);
                        surface.sync_preview_list();
                        if let Some((source, hash)) = highlight {
                            surface.request_file_highlight(task_path.clone(), source, hash, cx);
                        }
                    }
                    Err(error) => {
                        document.set_error(error.to_string());
                        surface.sync_preview_list();
                    }
                }
                cx.notify();
            });
        });
        if let Some(document) = self.preview.documents.get_mut(&path)
            && document.accepts(&key, generation)
        {
            document.read_task = Some(task);
        }
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
        let Some((document_key, generation)) = self
            .preview
            .documents
            .get(&path)
            .map(|document| (document.key.clone(), document.generation))
        else {
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
        let task_document_key = document_key.clone();
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
                        .is_some_and(|document| {
                            document.accepts(&task_document_key, generation)
                                && document.content_hash() == Some(content_hash.as_str())
                        });
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
        if let Some(document) = self.preview.documents.get_mut(&path)
            && document.accepts(&document_key, generation)
        {
            document.highlight_task = Some(task);
        }
    }

    fn sync_preview_list(&self) {
        let count = self
            .preview
            .active
            .as_deref()
            .and_then(|path| self.preview.documents.get(path))
            .map(|document| document.lines.len())
            .unwrap_or(0);
        self.preview
            .list
            .reset_with_uniform_height(count, px(PREVIEW_LINE_HEIGHT));
    }

    fn reload_active_document(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = self.preview.active.clone() {
            self.read_file(path, cx);
        }
    }

    pub(super) fn render_preview(
        &mut self,
        show_sidebar_toggle: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let Some(active) = self.preview.active.clone() else {
            return gpui::Empty.into_any_element();
        };
        let breadcrumb = self.render_breadcrumb(&active, show_sidebar_toggle, &theme, cx);
        let external = self.preview.documents.get(&active).is_some_and(|document| {
            matches!(
                document.phase,
                DocumentPhase::ExternallyModified { .. } | DocumentPhase::Conflict { .. }
            )
        });
        let body = self.render_document_body(&active, &theme, window, cx);
        div()
            .size_full()
            .min_w_0()
            .flex()
            .flex_col()
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

    fn render_breadcrumb(
        &mut self,
        path: &str,
        show_sidebar_toggle: bool,
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
                    .font_family(theme.font_sans.clone())
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
            .child(
                div()
                    .id("files-toggle-word-wrap")
                    .size(px(22.0))
                    .flex_none()
                    .rounded(px(5.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .when(self.preview.word_wrap(), |element| {
                        element.bg(crate::theme::wash(0.1))
                    })
                    .hover(|style| style.bg(crate::theme::wash(0.07)))
                    .role(gpui::Role::Button)
                    .aria_label(if self.preview.word_wrap() {
                        "Disable word wrap"
                    } else {
                        "Enable word wrap"
                    })
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_word_wrap(window, cx)))
                    .child(icon(icons::LIST).size(px(11.0)).text_color(
                        if self.preview.word_wrap() {
                            theme.text
                        } else {
                            theme.text_muted
                        },
                    )),
            )
            .when(show_sidebar_toggle, |element| {
                element.child(
                    div()
                        .id("files-toggle-tree-sidebar")
                        .size(px(22.0))
                        .flex_none()
                        .rounded(px(5.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .hover(|style| style.bg(crate::theme::wash(0.07)))
                        .role(gpui::Role::Button)
                        .aria_label(if self.preview.tree_sidebar_visible {
                            "Hide files sidebar"
                        } else {
                            "Show files sidebar"
                        })
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_tree_sidebar(cx)))
                        .child(
                            icon(icons::SIDEBAR_MINIMALISTIC)
                                .size(px(11.0))
                                .text_color(theme.text_muted),
                        ),
                )
            })
            .into_any_element()
    }

    fn render_document_body(
        &mut self,
        path: &str,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let editor = self.ensure_editor(path, theme, window, cx);
        let Some(document) = self.preview.documents.get(path) else {
            return gpui::Empty.into_any_element();
        };
        if matches!(document.phase, DocumentPhase::Loading) {
            return centered_state("Loading file…", theme.text_faint);
        }
        if let DocumentPhase::Error(error) | DocumentPhase::SaveFailed(error) = &document.phase {
            return centered_state(error.clone(), theme.danger_muted);
        }
        if let Some(editor) = editor {
            editor.update(cx, |state, _| {
                state.set_editor_style(super::editor_adapter::editor_style(theme));
            });
            return div()
                .id("files-editor-body")
                .flex_1()
                .min_w_0()
                .min_h_0()
                .font_family(theme.font_mono.clone())
                .text_size(px(11.5))
                .line_height(px(PREVIEW_LINE_HEIGHT))
                .child(super::editor::editor_element(&editor))
                .into_any_element();
        }
        let Some(file) = document.file.as_ref() else {
            return centered_state("This file cannot be previewed.", theme.text_muted);
        };
        if file.text.is_none() {
            return centered_state(read_only_message(file.read_only_reason), theme.text_muted);
        }
        let truncated = file.truncated;
        let word_wrap = self.preview.word_wrap();
        let code_scroll = if word_wrap {
            div()
                .id("files-preview-code-scroll")
                .flex_1()
                .min_w_0()
                .min_h_0()
                .flex()
                .child(
                    list(
                        self.preview.list.clone(),
                        cx.processor(Self::render_preview_line),
                    )
                    .flex_1()
                    .min_h_0()
                    .with_sizing_behavior(ListSizingBehavior::Auto),
                )
        } else {
            let mut scroll = div()
                .id("files-preview-code-scroll")
                .flex_1()
                .min_w_0()
                .min_h_0()
                .flex()
                .overflow_x_scroll()
                .track_scroll(&self.preview.horizontal_scroll)
                .child(
                    div().flex_none().min_w_full().h_full().child(
                        list(
                            self.preview.list.clone(),
                            cx.processor(Self::render_preview_line),
                        )
                        .h_full()
                        .with_sizing_behavior(ListSizingBehavior::Infer),
                    ),
                );
            // GPUI otherwise maps a vertical wheel gesture to X for an x-only
            // scroller, preventing the list from receiving it.
            scroll.style().restrict_scroll_to_axis = Some(true);
            scroll
        };

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
            .child(code_scroll)
            .into_any_element()
    }

    fn ensure_editor(
        &mut self,
        path: &str,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Entity<super::editor::FileEditorState>> {
        let document = self.preview.documents.get(path)?;
        if !matches!(document.phase, DocumentPhase::Ready) {
            return None;
        }
        if let Some(editor) = document.editor.clone() {
            return Some(editor);
        }
        let text = document.file.as_ref()?.text.clone()?;
        let editor =
            super::editor::new_file_editor(text, self.preview.word_wrap, theme, window, cx);
        let focus = editor.focus_handle(cx);
        window.defer(cx, move |window, cx| focus.focus(window, cx));
        self.preview.documents.get_mut(path)?.editor = Some(editor.clone());
        Some(editor)
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
        let Some(file) = document.file.as_ref() else {
            return gpui::Empty.into_any_element();
        };
        let Some(line) = document.lines.get(index) else {
            return gpui::Empty.into_any_element();
        };
        let theme = Theme::of(cx).clone();
        let word_wrap = self.preview.word_wrap();
        let spans = self
            .preview
            .highlights
            .get(path)
            .filter(|highlight| {
                file.content_hash.as_deref() == Some(highlight.content_hash.as_str())
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
            .min_h(px(PREVIEW_LINE_HEIGHT))
            .flex_none()
            .flex()
            .when(word_wrap, |element| element.w_full().items_stretch())
            .when(!word_wrap, |element| {
                element
                    .h(px(PREVIEW_LINE_HEIGHT))
                    .min_w_full()
                    .items_center()
            })
            .child(
                div()
                    .w(px(48.0))
                    .when(!word_wrap, |element| element.h_full())
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
                    .when(word_wrap, |element| {
                        element.flex_1().min_w_0().py(px(2.0)).whitespace_normal()
                    })
                    .pl(px(12.0))
                    .pr(px(18.0))
                    .when(!word_wrap, |element| element.whitespace_nowrap())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_reasons_have_specific_messages() {
        assert!(read_only_message(Some(WorkspaceReadOnlyReason::Binary)).contains("Binary"));
        assert!(
            read_only_message(Some(WorkspaceReadOnlyReason::UnsupportedEncoding))
                .contains("encoding")
        );
    }

    #[test]
    fn reset_drops_documents_and_active_preview_from_the_previous_target() {
        let mut preview = FilePreviewState::new();
        preview.active = Some("private.env".into());
        preview.documents.insert(
            "private.env".into(),
            FileDocument::loading(DocumentKey {
                chat_id: "chat-1".into(),
                checkout_id: Some("checkout-1".into()),
                path: "private.env".into(),
            }),
        );
        preview.mark_external("private.env");
        preview.tree_sidebar_visible = true;

        preview.reset();

        assert!(preview.documents.is_empty());
        assert!(preview.active.is_none());
        assert!(!preview.tree_sidebar_visible);
    }
}
