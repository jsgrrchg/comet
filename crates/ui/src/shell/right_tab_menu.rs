//! Context actions for the session-owned right surface tabs.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RightTabCloseMode {
    Left,
    Right,
    Others,
}

/// Work from the currently rendered order, never a captured tab index.
fn close_targets(
    order: &[RightSurface],
    target: RightSurface,
    mode: RightTabCloseMode,
) -> Vec<RightSurface> {
    if target == RightSurface::Picker {
        return Vec::new();
    }
    let Some(at) = order.iter().position(|surface| *surface == target) else {
        return Vec::new();
    };
    order
        .iter()
        .enumerate()
        .filter(|(index, surface)| {
            **surface != target
                && **surface != RightSurface::Picker
                && match mode {
                    RightTabCloseMode::Left => *index < at,
                    RightTabCloseMode::Right => *index > at,
                    RightTabCloseMode::Others => true,
                }
        })
        .map(|(_, surface)| *surface)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use RightSurface::{Browser, Diff, File, Picker, Subagent, Terminal};
    use RightTabCloseMode::{Left, Others, Right};

    #[test]
    fn targets_follow_visible_order_for_every_surface_kind() {
        let order = [File(1), Terminal(2), Diff(3), Browser(4), Subagent(5)];
        for (at, target) in order.iter().enumerate() {
            assert_eq!(close_targets(&order, *target, Left), order[..at]);
            assert_eq!(close_targets(&order, *target, Right), order[at + 1..]);
            assert_eq!(
                close_targets(&order, *target, Others),
                order
                    .iter()
                    .copied()
                    .filter(|s| s != target)
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn empty_single_missing_and_picker_targets_cannot_close_neighbors() {
        for mode in [Left, Right, Others] {
            assert!(close_targets(&[], File(1), mode).is_empty());
            assert!(close_targets(&[File(1)], File(1), mode).is_empty());
            assert!(close_targets(&[File(1)], File(2), mode).is_empty());
            assert!(close_targets(&[Picker, File(1)], Picker, mode).is_empty());
            assert!(close_targets(&[Picker, File(1), Picker], File(1), mode).is_empty());
        }
    }

    #[test]
    fn reordering_changes_neighbors_without_changing_target_identity() {
        let before = [File(1), File(2), File(3)];
        let after = [File(3), File(1), File(2)];
        assert_eq!(close_targets(&before, File(2), Left), [File(1)]);
        assert_eq!(close_targets(&after, File(2), Left), [File(3), File(1)]);
        assert!(close_targets(&after, File(2), Right).is_empty());
    }
}
