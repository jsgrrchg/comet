use gpui::{Context, Entity, IntoElement, Render, Window, div, prelude::*, px};

use crate::state::AppState;
use crate::theme::Theme;

/// Lazily-owned central surface for authored pull requests.
pub struct PullRequestsPage {
    #[allow(dead_code)]
    state: Entity<AppState>,
}

impl PullRequestsPage {
    pub fn new(state: Entity<AppState>) -> Self {
        Self { state }
    }
}

impl Render for PullRequestsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(20.0))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(theme.text)
            .child("Pull requests")
    }
}
