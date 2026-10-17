//! Move a terminal's stable session between views only when the drop commits.

use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct TerminalTabInsertion {
    pub chat: String,
    pub key: u64,
    /// The strip it would land in: a split pane, or `None` for the unsplit host.
    pub pane: Option<right_layout::PaneId>,
    pub index: usize,
}

/// Insertions have count + 1 slots; reorders use a different count-slot rule.
fn insertion_index(content_x: f32, slot_width: f32, count: usize) -> usize {
    ((content_x / slot_width + 0.5).floor().max(0.0) as usize).min(count)
}

impl Shell {
    fn accepts_terminal_transfer(&self, payload: &TerminalTabDrag, cx: &App) -> bool {
        matches!(self.route, Route::Chat)
            && self.right_pane_open(cx)
            && self.terminal_open(cx)
            && self.panel_key(cx) == payload.chat
            && self.terminal.as_ref().is_some_and(|origin| {
                origin.downgrade() == payload.origin && origin.read(cx).accepts_drag(payload, cx)
            })
    }

    pub(super) fn update_terminal_tab_insertion(
        &mut self,
        event: &gpui::DragMoveEvent<TerminalTabDrag>,
        pane: Option<right_layout::PaneId>,
        scroll: &gpui::ScrollHandle,
        slot_width: f32,
        count: usize,
        cx: &mut Context<Self>,
    ) {
        let payload = event.drag(cx);
        let inside = event.bounds.contains(&event.event.position);
        // Split hosts draw several strips and every one hears the move: one
        // the pointer is not over must not clear the strip that holds it.
        if pane.is_some()
            && !inside
            && self
                .terminal_tab_insertion
                .as_ref()
                .is_none_or(|drop| drop.pane != pane)
        {
            return;
        }
        let next = if inside && self.accepts_terminal_transfer(payload, cx) {
            let x = f32::from(event.event.position.x - event.bounds.left() - scroll.offset().x);
            Some(TerminalTabInsertion {
                chat: payload.chat.clone(),
                key: payload.key,
                pane,
                index: insertion_index(x, slot_width, count),
            })
        } else {
            None
        };
        if self.terminal_tab_insertion != next {
            self.terminal_tab_insertion = next;
            cx.notify();
        }
    }

    /// Commit a drawer terminal dropped on `pane`'s strip (`None` is the
    /// unsplit host): its session moves into a fresh embedded panel of its
    /// own, filed as a Terminal surface at the drop slot.
    pub(super) fn transfer_terminal_to_right(
        &mut self,
        payload: &TerminalTabDrag,
        pane: Option<right_layout::PaneId>,
        fallback_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let insertion = self.terminal_tab_insertion.take();
        if !self.accepts_terminal_transfer(payload, cx) {
            cx.notify();
            return;
        }
        // A split pane vanished since the strip was drawn: land unsplit.
        let pane = pane.filter(|pane| {
            self.right_split(cx)
                .is_some_and(|layout| layout.pane(*pane).is_some())
        });
        let rows = self.right_pane_rows(pane, cx);
        let index = insertion
            .filter(|drop| drop.chat == payload.chat && drop.key == payload.key)
            .filter(|drop| drop.pane == pane)
            .map(|drop| drop.index)
            .unwrap_or(fallback_index)
            .min(rows.len());
        let origin = self.terminal.clone().expect("validated terminal origin");
        let Some(session) = origin.update(cx, |panel, cx| panel.take_session(payload, cx)) else {
            return;
        };
        let key = session.read(cx).key;
        let destination = self.new_right_terminal_panel(cx);
        destination.update(cx, |panel, cx| {
            panel.insert_session(payload.chat.clone(), session, cx);
        });
        let surface = self.register_right_terminal(destination.clone(), key, cx);
        if !origin.read(cx).is_open() {
            self.toggle_terminal(window, cx);
        }
        let tabs = self.right_tabs.entry(payload.chat.clone()).or_default();
        match pane {
            Some(pane) => {
                // A split pane keeps its own strip order: file the surface,
                // show it (it joins the focused pane), then move it into
                // the drop pane at the drop slot.
                tabs.push(surface);
                self.right_tab_drag = None;
                self.set_right_active(surface, cx);
                self.move_right_tab(surface, pane, Some(index), cx);
            }
            None => {
                // Stored lists may include stale surfaces; the drop position
                // indexes only visible rows. Resolve its neighboring surface
                // before inserting.
                let stored_index = rows
                    .get(index)
                    .and_then(|(next, _, _, _)| tabs.iter().position(|tab| tab == next))
                    .unwrap_or(tabs.len());
                tabs.insert(stored_index, surface);
                self.right_tab_drag = None;
                self.set_right_active(surface, cx);
            }
        }
        window.focus(&destination.read(cx).focus_handle(), cx);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::insertion_index;

    #[test]
    fn insertions_include_both_edges_and_use_the_tab_midpoint() {
        assert_eq!(insertion_index(100., 116., 0), 0);
        assert_eq!(insertion_index(-20., 116., 3), 0);
        assert_eq!(insertion_index(57., 116., 3), 0);
        assert_eq!(insertion_index(59., 116., 3), 1);
        assert_eq!(insertion_index(232., 116., 3), 2);
        assert_eq!(insertion_index(900., 116., 3), 3);
    }
}
