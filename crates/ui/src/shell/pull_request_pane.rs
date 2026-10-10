//! The Pull requests route's side pane: opened pull requests as tabs beside
//! the board, so the list stays in view while one is inspected.

use super::*;
use crate::pull_request_detail::PullRequestDetailPage;
use crate::settings::PULL_REQUEST_PANE_DEFAULT;

/// Drag marker for the pull request pane's resize handle.
pub(super) struct PullRequestPaneResize;

/// Width the board keeps beside a hand-sized pane. Below this plus the
/// pane's own minimum the pane covers the board instead of squeezing both.
pub(super) const PULL_REQUEST_BOARD_MIN: f32 = 360.0;
/// Opening more closes the least recently viewed tab.
pub(super) const PULL_REQUEST_TABS_MAX: usize = 8;

pub(super) struct PullRequestTab {
    pub(super) url: String,
    pub(super) page: Entity<PullRequestDetailPage>,
    /// Activation stamp; the smallest is the least recently viewed.
    viewed: u64,
    _subscription: Subscription,
}

/// Session-local view state; tabs are not persisted across launches.
#[derive(Default)]
pub(super) struct PullRequestPane {
    pub(super) tabs: Vec<PullRequestTab>,
    pub(super) active: Option<String>,
    pub(super) open: bool,
    /// Takeover: the pane covers the board, as the detail did before tabs.
    pub(super) expanded: bool,
    pub(super) tween: Option<WidthTween>,
    pub(super) edge_bounce: Option<motion::ResizeEdgeBounce>,
    pub(super) resize_edge: Option<motion::ResizeEdge>,
    /// The last closed tab's view, kept on screen while the pane animates
    /// shut.
    closing: Option<Entity<PullRequestDetailPage>>,
    views: u64,
}

impl PullRequestPane {
    pub(super) fn active_index(&self) -> Option<usize> {
        let active = self.active.as_deref()?;
        self.tabs.iter().position(|tab| tab.url == active)
    }

    pub(super) fn active_page(&self) -> Option<Entity<PullRequestDetailPage>> {
        self.active_index().map(|ix| self.tabs[ix].page.clone())
    }

    fn activate(&mut self, ix: usize) {
        self.views += 1;
        self.tabs[ix].viewed = self.views;
        self.active = Some(self.tabs[ix].url.clone());
    }

    /// Drop tabs past the cap, least recently viewed first, never the
    /// active one.
    fn trim(&mut self) {
        while self.tabs.len() > PULL_REQUEST_TABS_MAX {
            let active = self.active_index();
            let Some(oldest) = (0..self.tabs.len())
                .filter(|ix| Some(*ix) != active)
                .min_by_key(|ix| self.tabs[*ix].viewed)
            else {
                return;
            };
            self.tabs.remove(oldest);
        }
    }

    /// Remove a tab. Closing the active one activates the tab that takes
    /// its place, else the one before it.
    fn remove(&mut self, ix: usize) {
        if ix >= self.tabs.len() {
            return;
        }
        let was_active = self.active_index() == Some(ix);
        self.tabs.remove(ix);
        if !was_active {
            return;
        }
        self.active = None;
        if !self.tabs.is_empty() {
            self.activate(ix.min(self.tabs.len() - 1));
        }
    }
}

/// The pane's width budget: a hand-sized pane leaves the board its minimum;
/// when even that cannot fit, the pane takes the whole content area.
fn pull_request_pane_width(available: f32, preferred: f32, expanded: bool) -> f32 {
    let max = (available - PULL_REQUEST_BOARD_MIN).max(0.0);
    if expanded || max < RIGHT_PANE_MIN {
        available
    } else {
        preferred.clamp(RIGHT_PANE_MIN, max)
    }
}

impl Shell {
    /// The board's side pane shows: on its route, with at least one tab.
    pub(super) fn pull_request_pane_open(&self) -> bool {
        matches!(self.route, Route::PullRequests)
            && self.pull_request_pane.open
            && !self.pull_request_pane.tabs.is_empty()
    }

    fn pull_request_pane_available(&self) -> f32 {
        (self.viewport_width - self.sidebar_now()).max(0.0)
    }

    /// Whether the pane can be resized by hand: not in takeover, and the
    /// window leaves room for the board beside it.
    pub(super) fn pull_request_pane_resizable(&self) -> bool {
        !self.pull_request_pane.expanded
            && self.pull_request_pane_available() - PULL_REQUEST_BOARD_MIN >= RIGHT_PANE_MIN
    }

    pub(super) fn pull_request_pane_target(&self) -> f32 {
        if !self.pull_request_pane_open() {
            return 0.0;
        }
        pull_request_pane_width(
            self.pull_request_pane_available(),
            self.settings.pull_request_pane_width,
            self.pull_request_pane.expanded,
        )
    }

    pub(super) fn pull_request_pane_visible_width(&self) -> f32 {
        if !matches!(self.route, Route::PullRequests) {
            return 0.0;
        }
        let now = self.eval_tween(
            self.pull_request_pane.tween,
            self.pull_request_pane_target(),
        ) + self.eval_resize_edge_bounce(
            self.pull_request_pane.edge_bounce,
            self.pull_request_pane_open() && self.pull_request_pane_resizable(),
        );
        now.max(0.0).min(self.pull_request_pane_available())
    }

    /// Animate the pane to its current target from wherever it is now.
    fn retarget_pull_request_pane(&mut self, from: f32) {
        self.pull_request_pane.edge_bounce = None;
        self.pull_request_pane.resize_edge = None;
        self.finish_pane_resize(PaneResizeKind::PullRequests);
        self.pull_request_pane.tween = Some(WidthTween::new(from, self.pull_request_pane_target()));
    }

    /// Show or hide the pane, keeping its tabs. A no-op when already there.
    pub(super) fn set_pull_request_pane_open(&mut self, open: bool, cx: &mut Context<Self>) {
        let open = open && !self.pull_request_pane.tabs.is_empty();
        if self.pull_request_pane.open == open {
            return;
        }
        let from = self.pull_request_pane_visible_width();
        self.pull_request_pane.open = open;
        if !open {
            // Reopening at full bleed with the board gone reads as broken.
            self.pull_request_pane.expanded = false;
        }
        self.retarget_pull_request_pane(from);
        self.sync_pull_request_selection(cx);
        cx.notify();
    }

    /// The board highlights the pull request shown beside it.
    pub(super) fn sync_pull_request_selection(&mut self, cx: &mut Context<Self>) {
        let selected = self
            .pull_request_pane
            .open
            .then(|| self.pull_request_pane.active.clone())
            .flatten();
        if let Some(page) = &self.pull_requests_page {
            page.update(cx, |page, cx| page.select_url(selected, cx));
        }
    }

    /// The active tab's detail view, if the pane has one.
    pub(super) fn active_pull_request_page(&self) -> Option<Entity<PullRequestDetailPage>> {
        self.pull_request_pane.active_page()
    }

    /// Open `url` as a tab beside the board (or activate its tab) without
    /// recording a navigation. Re-showing an open pull request keeps its
    /// view state.
    pub(super) fn show_pull_request_detail(
        &mut self,
        url: String,
        target: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending_pull_request = None;
        if self.pull_requests_page.is_none() {
            self.pull_requests_page =
                Some(cx.new(|cx| PullRequestsPage::new(self.state.clone(), cx)));
        }
        let existing = self
            .pull_request_pane
            .tabs
            .iter()
            .position(|tab| tab.url == url);
        let ix = match existing {
            Some(ix) => ix,
            None => {
                let preview = self
                    .pull_requests_page
                    .as_ref()
                    .and_then(|page| page.read(cx).preview(&url));
                let page = cx.new(|cx| {
                    PullRequestDetailPage::new(
                        self.state.clone(),
                        url.clone(),
                        target,
                        self.pull_request_cache.clone(),
                        preview,
                        window,
                        cx,
                    )
                });
                let subscription = cx.observe(&page, |_, _, cx| cx.notify());
                let pane = &mut self.pull_request_pane;
                pane.tabs.push(PullRequestTab {
                    url: url.clone(),
                    page,
                    viewed: 0,
                    _subscription: subscription,
                });
                pane.tabs.len() - 1
            }
        };
        self.pull_request_pane.closing = None;
        self.pull_request_pane.activate(ix);
        self.pull_request_pane.trim();
        self.set_pull_request_pane_open(true, cx);
        self.sync_pull_request_selection(cx);
        cx.notify();
    }

    /// Close one tab; closing the last collapses the pane.
    pub(super) fn close_pull_request_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.pull_request_pane.tabs.len() {
            return;
        }
        if self.pull_request_pane.tabs.len() == 1 {
            self.pull_request_pane.closing = Some(self.pull_request_pane.tabs[ix].page.clone());
            self.set_pull_request_pane_open(false, cx);
        }
        self.pull_request_pane.remove(ix);
        self.sync_pull_request_selection(cx);
        cx.notify();
    }

    /// Return to the board alone without recording a navigation. Tabs stay
    /// for the next time the pane opens.
    pub(super) fn dismiss_pull_request_detail(&mut self, cx: &mut Context<Self>) {
        self.pending_pull_request = None;
        self.set_pull_request_pane_open(false, cx);
        self.sync_pull_request_selection(cx);
        cx.notify();
    }

    /// Forget every tab, e.g. when the browser profile changes.
    pub(super) fn reset_pull_request_pane(&mut self) {
        self.pending_pull_request = None;
        self.pull_request_pane = PullRequestPane::default();
    }

    pub(super) fn on_pull_request_pane_drag(
        &mut self,
        event: &gpui::DragMoveEvent<PullRequestPaneResize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let viewport = f32::from(window.viewport_size().width);
        let width = viewport - f32::from(event.event.position.x);
        let max = (self.pull_request_pane_available() - PULL_REQUEST_BOARD_MIN).max(0.0);
        if max < RIGHT_PANE_MIN {
            return;
        }
        let sample = motion::resize_drag_sample(
            width,
            RIGHT_PANE_MIN,
            max,
            self.pull_request_pane.resize_edge,
            self.reduced_motion,
        );
        self.settings.pull_request_pane_width = sample.width;
        self.pane_resize_dragging = Some(PaneResizeKind::PullRequests);
        if sample.starts_bounce {
            self.pull_request_pane.edge_bounce = sample.edge.map(motion::ResizeEdgeBounce::new);
        } else if sample.edge.is_none() {
            self.pull_request_pane.edge_bounce = None;
        }
        self.pane_resize_active = sample
            .edge
            .is_none()
            .then_some(PaneResizeKind::PullRequests);
        self.pull_request_pane.resize_edge = sample.edge;
        self.pull_request_pane.tween = None;
        self.schedule_save(cx);
        cx.notify();
    }

    /// The seam's resize handle, when the pane can be sized by hand.
    pub(super) fn pull_request_pane_handle(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Stateful<gpui::Div>> {
        (self.pull_request_pane_open()
            && self.pull_request_pane_resizable()
            && !self.tween_active(self.pull_request_pane.tween))
        .then(|| {
            self.resize_handle(
                "pull-request-pane-resize",
                PaneResizeKind::PullRequests,
                || PullRequestPaneResize,
                |shell, _| {
                    shell.settings.pull_request_pane_width = PULL_REQUEST_PANE_DEFAULT;
                    shell.pull_request_pane.edge_bounce = None;
                },
                cx,
            )
            .left(px(-PANE_RESIZE_HITBOX_HALF_WIDTH))
        })
    }

    /// The pane: the active tab's detail view in the right pane's flush
    /// panel, clipped to the animated width.
    pub(super) fn render_pull_request_pane(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let visible = self.pull_request_pane_visible_width();
        if !self.tween_active(self.pull_request_pane.tween) {
            self.pull_request_pane.closing = None;
            if visible <= 0.0 {
                return Empty.into_any_element();
            }
        }
        let theme = Theme::of(cx).clone();
        let target = self.pull_request_pane_target();
        let covers_board = target >= self.pull_request_pane_available() - 0.5;
        let corner = Self::window_corner_radius(window);
        let content = self
            .active_pull_request_page()
            .or_else(|| self.pull_request_pane.closing.clone())
            .map(IntoElement::into_any_element)
            .unwrap_or_else(|| Empty.into_any_element());
        let panel = div()
            .id("pull-request-pane")
            .debug_selector(|| "pull-request-pane".into())
            .size_full()
            .flex()
            .flex_col()
            // Covering the board, the pane's left edge is the sidebar seam,
            // which already carries a hairline.
            .when(!covers_board, |el| {
                el.border_l_1().border_color(theme.border)
            })
            .when(corner > 0.0, |el| {
                el.rounded_tr(px(corner)).rounded_br(px(corner))
            })
            .bg(theme.panel_bg())
            .overflow_hidden()
            .child(content);
        let transition = self.active_tween_endpoints(self.pull_request_pane.tween);
        let content_width = stable_panel_content_width(target, transition).max(visible);
        div()
            .h_full()
            .flex_none()
            .relative()
            .overflow_hidden()
            .w(px(visible))
            .child(
                div()
                    .absolute()
                    .top_0()
                    .right_0()
                    .h_full()
                    .w(px(content_width))
                    .child(panel),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_leaves_the_board_its_minimum_or_covers_it() {
        // Room for both: the preferred width, kept clear of the board.
        assert_eq!(pull_request_pane_width(1400.0, 640.0, false), 640.0);
        assert_eq!(
            pull_request_pane_width(1000.0, 900.0, false),
            1000.0 - PULL_REQUEST_BOARD_MIN
        );
        assert_eq!(
            pull_request_pane_width(1400.0, 100.0, false),
            RIGHT_PANE_MIN
        );
        // Too narrow for both: the pane covers the board.
        assert_eq!(pull_request_pane_width(600.0, 640.0, false), 600.0);
        // Takeover always covers it.
        assert_eq!(pull_request_pane_width(1400.0, 640.0, true), 1400.0);
    }
}
