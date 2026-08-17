//! Boundary between the Files surface and the `gpui-base` editor.

use gpui::{AppContext as _, Context, Entity, Window};
use gpui_base::input::{EditorState, InputEvent};

/// Creates the editor state used by the compatibility spike.
///
/// Production documents remain on the existing read-only preview path until
/// autosave and optimistic concurrency are wired in later commits.
pub fn new_spike_editor(
    text: impl Into<gpui::SharedString>,
    window: &mut Window,
    cx: &mut Context<impl gpui::Render>,
) -> Entity<EditorState> {
    let editor = cx.new(|cx| {
        EditorState::new(window, cx)
            .line_number(true)
            .soft_wrap(false)
            .default_value(text)
    });
    editor.update(cx, |state, cx| state.set_readonly(true, cx));
    editor
}

/// Keeps the event type visible at the integration boundary while the spike is
/// isolated from the production preview.
pub fn is_change_event(event: &InputEvent) -> bool {
    matches!(event, InputEvent::Change)
}
