//! Boundary between the Files surface and the `gpui-base` editor.

use gpui::{
    AnyElement, AppContext as _, Context, Entity, IntoElement as _, SharedString, Subscription,
    Window,
};
use gpui_base::input::{EditorState, InputEvent};

use super::FilesSurface;
use crate::theme::Theme;

pub(super) type FileEditorState = EditorState;

/// Creates a stable editor entity for an open workspace document.
pub(super) fn new_file_editor(
    text: impl Into<SharedString>,
    path: &str,
    soft_wrap: bool,
    theme: &Theme,
    window: &mut Window,
    cx: &mut Context<impl gpui::Render>,
) -> Entity<EditorState> {
    let editor = cx.new(|cx| {
        EditorState::new(window, cx)
            .language(path.to_string())
            .line_number(true)
            .soft_wrap(soft_wrap)
            .default_value(text)
    });
    editor.update(cx, |state, cx| {
        state.set_editor_style(super::editor_adapter::editor_style(theme));
        state.set_readonly(false, cx);
    });
    editor
}

pub(super) fn editor_element(editor: &Entity<FileEditorState>) -> AnyElement {
    gpui_base::input::Editor::new(editor).into_any_element()
}

pub(super) fn subscribe_to_changes(
    editor: &Entity<FileEditorState>,
    path: String,
    cx: &mut Context<FilesSurface>,
) -> Subscription {
    cx.subscribe(editor, move |surface, _, event, cx| {
        if matches!(event, InputEvent::Change) {
            surface.on_editor_change(&path, cx);
        }
    })
}
