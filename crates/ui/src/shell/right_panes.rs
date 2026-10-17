//! Split surface hosts: each pane's tabs and shown surface, and keeping a
//! chat's split in step with its tab list.
//!
//! A host splits only when the user drags a tab onto a pane's edge. While
//! split, the chat's flat tab list stays the membership record every open
//! and close path already maintains; [`right_layout::SplitLayout`] files
//! those tabs into panes. `ChatPanels::right_active` is the focused pane's
//! tab, so everything that reads "the active surface" acts on that pane.

use super::right_layout::{Edge, PaneId, SplitLayout};
use super::*;

impl Shell {
    /// The current chat's split, while its host holds several panes.
    pub(super) fn right_split(&self, cx: &App) -> Option<&SplitLayout<RightSurface>> {
        self.panels.right_layout(&self.panel_key(cx))
    }

    /// One pane's live rows in strip order; `None` is the unsplit host.
    pub(super) fn right_pane_rows(&self, pane: Option<PaneId>, cx: &App) -> Vec<RightRow> {
        match (pane, self.right_split(cx)) {
            (Some(pane), Some(layout)) => layout
                .pane(pane)
                .map(|pane| self.right_rows_for(&pane.tabs, cx))
                .unwrap_or_default(),
            _ => self.right_surface_rows(cx),
        }
    }

    /// The surface a pane shows: its pick while that is live, else its most
    /// recently visited live tab, else its first.
    pub(super) fn resolved_pane_active(&self, pane: PaneId, cx: &App) -> RightSurface {
        let key = self.panel_key(cx);
        let rows = self.right_pane_rows(Some(pane), cx);
        let picked = self
            .right_split(cx)
            .and_then(|layout| layout.pane(pane))
            .and_then(|pane| pane.active);
        match picked {
            Some(picked) if rows.iter().any(|(surface, ..)| *surface == picked) => picked,
            _ => self
                .panels
                .right_surface_fallback(&key, rows.iter().map(|(surface, ..)| *surface)),
        }
    }

    /// The pane holding `surface` (the unsplit host is `None`).
    pub(super) fn right_pane_of(&self, surface: RightSurface, cx: &App) -> Option<PaneId> {
        self.right_split(cx)
            .and_then(|layout| layout.pane_of(surface))
    }

    /// Reconcile `key`'s split with its tab list: closed and dead tabs leave
    /// their panes, an emptied pane closes, and a split left with one pane
    /// dissolves back into the plain tab list. The chat's shown surface
    /// stays one the focused pane holds.
    pub(super) fn sync_right_layout(&mut self, key: &str, cx: &mut Context<Self>) {
        if self.panels.right_layout(key).is_none() {
            return;
        }
        let members: Vec<RightSurface> = self.right_tabs.get(key).cloned().unwrap_or_default();
        // Entities can only be checked for the chat on screen; another
        // chat's tabs are reconciled when it shows again.
        let live: Option<Vec<RightSurface>> = (key == self.panel_key(cx)).then(|| {
            self.right_rows_for(&members, cx)
                .into_iter()
                .map(|(surface, ..)| surface)
                .collect()
        });
        let keep = |tab: &RightSurface| {
            members.contains(tab) && live.as_ref().is_none_or(|live| live.contains(tab))
        };
        let Some(layout) = self.panels.right_layouts.get_mut(key) else {
            return;
        };
        layout.retain(&keep);
        for tab in members.iter().filter(|tab| keep(tab)) {
            layout.adopt(*tab);
        }
        if let Some(sole) = layout.sole_pane() {
            // Back to one pane: its strip order becomes the chat's tab order.
            let mut tabs = sole.tabs.clone();
            let active = sole.active;
            let unfiled: Vec<RightSurface> = members
                .iter()
                .filter(|tab| !tabs.contains(tab))
                .copied()
                .collect();
            tabs.extend(unfiled);
            self.panels.right_layouts.remove(key);
            self.right_tabs.insert(key.to_string(), tabs);
            if let Some(active) = active {
                self.panels.update(key, |panel| panel.right_active = active);
            }
            cx.notify();
            return;
        }
        let focused = layout.focused();
        let shown = layout.pane(focused).and_then(|pane| pane.active);
        let current = self.panels.get(key).right_active;
        let target = match shown {
            Some(shown) => shown,
            None if key == self.panel_key(cx) => self.resolved_pane_active(focused, cx),
            None => return,
        };
        if current != target {
            self.panels.update(key, |panel| panel.right_active = target);
        }
        cx.notify();
    }

    /// Focus a pane of the current chat's split (a press anywhere in it).
    pub(super) fn focus_right_pane(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        let Some(layout) = self.right_split(cx) else {
            return;
        };
        if layout.focused() == pane || layout.pane(pane).is_none() {
            return;
        }
        let surface = self.resolved_pane_active(pane, cx);
        if surface != RightSurface::Picker {
            self.set_right_active(surface, cx);
        }
    }

    /// Suspend image previews only in the file a change of tab hides: in a
    /// split, the other panes keep showing theirs.
    pub(super) fn suspend_replaced_file_images(
        &mut self,
        surface: RightSurface,
        cx: &mut Context<Self>,
    ) {
        let Some(layout) = self.right_split(cx) else {
            if self.resolved_right_active(cx) != surface {
                self.suspend_file_images(cx);
            }
            return;
        };
        let pane = layout.pane_of(surface).unwrap_or(layout.focused());
        let replaced = self.resolved_pane_active(pane, cx);
        if replaced != surface
            && let RightSurface::File(id) = replaced
            && let Some(file) = self.file_surfaces.get(&id).cloned()
        {
            file.update(cx, |file, cx| file.suspend_images(cx));
        }
    }

    /// Move `surface` into a new pane on `edge` of `target` (the unsplit
    /// host is `None`). The first split turns the host's tab list into a
    /// two-pane layout.
    pub(super) fn split_right_tab(
        &mut self,
        surface: RightSurface,
        target: Option<PaneId>,
        edge: Edge,
        cx: &mut Context<Self>,
    ) -> bool {
        let key = self.panel_key(cx);
        let split = match (target, self.panels.right_layouts.get_mut(&key)) {
            (Some(target), Some(layout)) => layout.split(surface, target, edge),
            (None, None) => {
                let tabs: Vec<RightSurface> = self
                    .right_surface_rows(cx)
                    .into_iter()
                    .map(|(tab, ..)| tab)
                    .collect();
                let active = Some(self.resolved_right_active(cx));
                match SplitLayout::split_off(tabs, active, surface, edge) {
                    Some(layout) => {
                        self.panels.right_layouts.insert(key.clone(), layout);
                        true
                    }
                    None => false,
                }
            }
            _ => false,
        };
        if split {
            self.panels
                .update(&key, |panel| panel.right_active = surface);
            self.sync_right_layout(&key, cx);
            cx.notify();
        }
        split
    }

    /// Move `surface` into pane `target` at `index` (its end when `None`).
    pub(super) fn move_right_tab(
        &mut self,
        surface: RightSurface,
        target: PaneId,
        index: Option<usize>,
        cx: &mut Context<Self>,
    ) -> bool {
        let key = self.panel_key(cx);
        let moved = self
            .panels
            .right_layouts
            .get_mut(&key)
            .is_some_and(|layout| layout.move_to(surface, target, index));
        if moved {
            self.panels
                .update(&key, |panel| panel.right_active = surface);
            self.sync_right_layout(&key, cx);
            cx.notify();
        }
        moved
    }
}
