//! Zeron-owned styling and highlighting adapters for `gpui-base`.

use std::sync::Arc;

use gpui::HighlightStyle;
use gpui_base::input::{HighlightStyleResolver, InputEditorStyle};

use crate::theme::Theme;

#[derive(Default)]
struct EmptyHighlightResolver;

impl HighlightStyleResolver for EmptyHighlightResolver {
    fn style(&self, _name: &str) -> Option<HighlightStyle> {
        None
    }
}

/// Maps the current Zeron palette onto the editor without importing the
/// `gpui-component` theme layer.
pub fn editor_style(theme: &Theme) -> InputEditorStyle {
    InputEditorStyle {
        foreground: theme.text,
        muted_foreground: theme.text_faint,
        background: gpui::transparent_black(),
        border: theme.border,
        selection: theme.accent.opacity(0.22),
        caret: theme.text,
        highlight_styles: Arc::new(EmptyHighlightResolver),
        editor_active_line: Some(crate::theme::wash(0.025)),
        editor_gutter_background: Some(gpui::transparent_black()),
        ..Default::default()
    }
}
