//! Split views: chats shown side by side in the conversation column.
//!
//! A split view is a device-local viewport onto the synced session list,
//! like the tabs before it: the sidebar lists it as one row of pills, and a
//! chat belongs to at most one. Opening a member shows its split with that
//! pane focused; opening any other chat leaves the split, which stays in the
//! sidebar to come back to. A split never holds fewer than two chats — the
//! last one left reverts to an ordinary session row.

use serde::{Deserialize, Serialize};

/// Panes per split. Three already crowd a 242px sidebar row and need
/// 3 × [`crate::settings::CHAT_PANEL_MIN`] of conversation column.
pub const SPLIT_MAX_PANES: usize = 3;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SplitView {
    pub id: String,
    /// Chat ids, left to right.
    pub chats: Vec<String>,
    /// Pane widths as fractions of the column, in `chats` order. Read through
    /// [`Self::normalized_ratios`]: a hand-edited file may not sum to one.
    #[serde(default)]
    pub ratios: Vec<f32>,
    /// The pane focused last: where a click on the row itself lands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitSide {
    Left,
    Right,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaceOutcome {
    /// The chat is now beside the anchor in the split with this id.
    Placed(String),
    /// The anchor's split already has [`SPLIT_MAX_PANES`] panes.
    Full,
    /// Nothing to do: the chat is the anchor.
    Unchanged,
}

impl SplitView {
    fn new(id: String, chats: Vec<String>) -> Self {
        let ratios = equal_ratios(chats.len());
        Self {
            id,
            chats,
            ratios,
            focused: None,
        }
    }

    pub fn contains(&self, chat_id: &str) -> bool {
        self.chats.iter().any(|id| id == chat_id)
    }

    pub fn position(&self, chat_id: &str) -> Option<usize> {
        self.chats.iter().position(|id| id == chat_id)
    }

    /// The chat a click on the split's row opens: its last focused pane.
    pub fn landing_chat(&self) -> Option<&str> {
        self.focused
            .as_deref()
            .filter(|id| self.contains(id))
            .or_else(|| self.chats.first().map(String::as_str))
    }

    /// One positive fraction per pane, summing to one.
    pub fn normalized_ratios(&self) -> Vec<f32> {
        let n = self.chats.len();
        if n == 0 {
            return Vec::new();
        }
        let valid = self.ratios.len() == n && self.ratios.iter().all(|r| r.is_finite() && *r > 0.0);
        if !valid {
            return equal_ratios(n);
        }
        let sum: f32 = self.ratios.iter().sum();
        self.ratios.iter().map(|r| r / sum).collect()
    }

    /// Move the divider after pane `divider` by `delta` (a fraction of the
    /// column), keeping both neighbours at least `min` wide.
    pub fn resize(&mut self, divider: usize, delta: f32, min: f32) -> bool {
        let mut ratios = self.normalized_ratios();
        if divider + 1 >= ratios.len() {
            return false;
        }
        let pair = ratios[divider] + ratios[divider + 1];
        let min = min.min(pair / 2.0);
        let left = (ratios[divider] + delta).clamp(min, pair - min);
        if (left - ratios[divider]).abs() < f32::EPSILON {
            return false;
        }
        ratios[divider] = left;
        ratios[divider + 1] = pair - left;
        self.ratios = ratios;
        true
    }

    fn insert(&mut self, index: usize, chat_id: String) {
        let ratios = self.normalized_ratios();
        let n = ratios.len() + 1;
        let share = 1.0 / n as f32;
        let mut ratios: Vec<f32> = ratios.iter().map(|r| r * (1.0 - share)).collect();
        let index = index.min(self.chats.len());
        ratios.insert(index, share);
        self.chats.insert(index, chat_id);
        self.ratios = ratios;
    }

    fn remove_at(&mut self, index: usize) -> String {
        let mut ratios = self.normalized_ratios();
        let removed = ratios.remove(index);
        let rest = 1.0 - removed;
        if rest > 0.0 {
            ratios.iter_mut().for_each(|r| *r /= rest);
        }
        self.ratios = ratios;
        let chat = self.chats.remove(index);
        if self.focused.as_deref() == Some(chat.as_str()) {
            self.focused = None;
        }
        chat
    }
}

fn equal_ratios(n: usize) -> Vec<f32> {
    vec![1.0 / n.max(1) as f32; n]
}

pub fn split_of<'a>(views: &'a [SplitView], chat_id: &str) -> Option<&'a SplitView> {
    views.iter().find(|view| view.contains(chat_id))
}

/// Put `chat_id` beside `anchor` on `side`: into the anchor's split, or a new
/// split of the two. A chat taken from another split leaves it first.
pub fn place(
    views: &mut Vec<SplitView>,
    anchor: &str,
    chat_id: &str,
    side: SplitSide,
    new_id: impl FnOnce() -> String,
) -> PlaceOutcome {
    if anchor == chat_id {
        return PlaceOutcome::Unchanged;
    }
    let anchor_split = views.iter().position(|view| view.contains(anchor));
    if let Some(ix) = anchor_split {
        let view = &views[ix];
        if !view.contains(chat_id) && view.chats.len() >= SPLIT_MAX_PANES {
            return PlaceOutcome::Full;
        }
    }
    let same_split = anchor_split.is_some_and(|ix| views[ix].contains(chat_id));
    if same_split {
        // Rearranging within the split keeps the moved pane's width.
        let view = &mut views[anchor_split.unwrap()];
        let ratios = view.normalized_ratios();
        let from = view.position(chat_id).unwrap();
        let share = ratios[from];
        let chat = view.chats.remove(from);
        let mut ratios = ratios;
        ratios.remove(from);
        let at = view.position(anchor).unwrap() + usize::from(side == SplitSide::Right);
        view.chats.insert(at, chat);
        ratios.insert(at, share);
        view.ratios = ratios;
        view.focused = Some(chat_id.to_string());
        return PlaceOutcome::Placed(view.id.clone());
    }
    remove(views, chat_id);
    // Removing may have dissolved or shifted splits: look the anchor up again.
    match views.iter().position(|view| view.contains(anchor)) {
        Some(ix) => {
            let view = &mut views[ix];
            let at = view.position(anchor).unwrap() + usize::from(side == SplitSide::Right);
            view.insert(at, chat_id.to_string());
            view.focused = Some(chat_id.to_string());
            PlaceOutcome::Placed(view.id.clone())
        }
        None => {
            let chats = match side {
                SplitSide::Left => vec![chat_id.to_string(), anchor.to_string()],
                SplitSide::Right => vec![anchor.to_string(), chat_id.to_string()],
            };
            let mut view = SplitView::new(new_id(), chats);
            view.focused = Some(chat_id.to_string());
            let id = view.id.clone();
            views.push(view);
            PlaceOutcome::Placed(id)
        }
    }
}

/// Take `chat_id` out of its split, dissolving a split left with one chat.
/// Returns whether any split changed.
pub fn remove(views: &mut Vec<SplitView>, chat_id: &str) -> bool {
    let Some(ix) = views.iter().position(|view| view.contains(chat_id)) else {
        return false;
    };
    let view = &mut views[ix];
    let at = view.position(chat_id).unwrap();
    view.remove_at(at);
    if view.chats.len() < 2 {
        views.remove(ix);
    }
    true
}

/// Drop chats that no longer qualify (archived, deleted), dissolving splits
/// left with fewer than two. Returns whether anything changed.
pub fn prune(views: &mut Vec<SplitView>, keep: impl Fn(&str) -> bool) -> bool {
    let mut changed = false;
    let mut seen = std::collections::HashSet::new();
    for view in views.iter_mut() {
        let mut ix = 0;
        while ix < view.chats.len() {
            let id = &view.chats[ix];
            // A chat belongs to at most one split; a hand-edited file keeps
            // its first.
            if keep(id) && seen.insert(id.clone()) {
                ix += 1;
            } else {
                view.remove_at(ix);
                changed = true;
            }
        }
    }
    let before = views.len();
    views.retain(|view| view.chats.len() >= 2);
    changed || views.len() != before
}

/// Remember `chat_id` as its split's focused pane.
pub fn set_focused(views: &mut [SplitView], chat_id: &str) -> bool {
    let Some(view) = views.iter_mut().find(|view| view.contains(chat_id)) else {
        return false;
    };
    if view.focused.as_deref() == Some(chat_id) {
        return false;
    }
    view.focused = Some(chat_id.to_string());
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(view: &SplitView) -> Vec<&str> {
        view.chats.iter().map(String::as_str).collect()
    }

    fn close(a: &[f32], b: &[f32]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(a, b)| (a - b).abs() < 1e-4)
    }

    #[test]
    fn placing_beside_a_lone_chat_makes_a_split_of_the_two() {
        let mut views = Vec::new();
        let outcome = place(&mut views, "a", "b", SplitSide::Right, || "s1".into());
        assert_eq!(outcome, PlaceOutcome::Placed("s1".into()));
        assert_eq!(ids(&views[0]), ["a", "b"]);
        assert_eq!(views[0].focused.as_deref(), Some("b"));
        place(&mut views, "a", "c", SplitSide::Left, || unreachable!());
        assert_eq!(ids(&views[0]), ["c", "a", "b"]);
        assert!(close(&views[0].normalized_ratios(), &[1.0 / 3.0; 3]));
        assert_eq!(
            place(&mut views, "a", "d", SplitSide::Right, || unreachable!()),
            PlaceOutcome::Full
        );
        assert_eq!(
            place(&mut views, "a", "a", SplitSide::Right, || unreachable!()),
            PlaceOutcome::Unchanged
        );
    }

    #[test]
    fn a_chat_moves_between_splits_and_a_lone_leftover_dissolves() {
        let mut views = Vec::new();
        place(&mut views, "a", "b", SplitSide::Right, || "s1".into());
        place(&mut views, "c", "d", SplitSide::Right, || "s2".into());
        // b leaves s1 (dissolving it) for s2.
        place(&mut views, "c", "b", SplitSide::Left, || unreachable!());
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].id, "s2");
        assert_eq!(ids(&views[0]), ["b", "c", "d"]);
        assert!(split_of(&views, "a").is_none());
    }

    #[test]
    fn rearranging_within_a_split_keeps_widths() {
        let mut views = vec![SplitView {
            id: "s".into(),
            chats: vec!["a".into(), "b".into(), "c".into()],
            ratios: vec![0.5, 0.3, 0.2],
            focused: None,
        }];
        place(&mut views, "a", "c", SplitSide::Left, || unreachable!());
        assert_eq!(ids(&views[0]), ["c", "a", "b"]);
        assert!(close(&views[0].ratios, &[0.2, 0.5, 0.3]));
    }

    #[test]
    fn removing_gives_the_width_back_and_prune_drops_dead_chats() {
        let mut views = vec![SplitView {
            id: "s".into(),
            chats: vec!["a".into(), "b".into(), "c".into()],
            ratios: vec![0.5, 0.25, 0.25],
            focused: Some("c".into()),
        }];
        assert!(remove(&mut views, "c"));
        assert!(close(&views[0].ratios, &[2.0 / 3.0, 1.0 / 3.0]));
        assert_eq!(views[0].landing_chat(), Some("a"));
        assert!(!prune(&mut views, |_| true));
        assert!(prune(&mut views, |id| id != "b"));
        assert!(views.is_empty());
        // Duplicates across splits keep their first.
        let mut views = vec![
            SplitView::new("1".into(), vec!["a".into(), "b".into()]),
            SplitView::new("2".into(), vec!["b".into(), "c".into(), "d".into()]),
        ];
        assert!(prune(&mut views, |_| true));
        assert_eq!(ids(&views[1]), ["c", "d"]);
    }

    #[test]
    fn resizing_respects_the_minimum_and_bad_ratios_heal() {
        let mut view = SplitView::new("s".into(), vec!["a".into(), "b".into()]);
        assert!(view.resize(0, 0.2, 0.25));
        assert!(close(&view.ratios, &[0.7, 0.3]));
        assert!(view.resize(0, 0.5, 0.25));
        assert!(close(&view.ratios, &[0.75, 0.25]));
        assert!(!view.resize(1, 0.1, 0.25));
        view.ratios = vec![f32::NAN, 1.0];
        assert!(close(&view.normalized_ratios(), &[0.5, 0.5]));
        view.ratios = vec![2.0, 6.0];
        assert!(close(&view.normalized_ratios(), &[0.25, 0.75]));
    }
}
