use std::collections::BTreeSet;

use gpui::{Context, Window};

use super::{
    EditorSelection, Fold, FoldAll, FoldRange, InputState, RopeExt as _, Unfold, UnfoldAll,
};

const MAX_FOLDING_SNAPSHOT_RANGES: usize = 256;

/// Bounded, framework-neutral folding state for menus, commands, and debug tools.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EditorFoldingSnapshot {
    pub enabled: bool,
    pub candidate_count: usize,
    pub folded_count: usize,
    pub folded_ranges: Vec<FoldRange>,
    pub truncated: bool,
    pub can_fold: bool,
    pub can_unfold: bool,
    pub can_fold_all: bool,
    pub can_unfold_all: bool,
}

impl InputState {
    /// Fold the innermost unfolded range containing each active caret.
    /// Repeating the command folds the next unfolded parent range.
    pub fn fold_at_selections(&mut self, cx: &mut Context<Self>) -> bool {
        if self.disabled || !self.mode.is_folding() {
            return false;
        }

        let rows = self.active_selection_rows();
        let candidates = self.display_map.fold_candidates().to_vec();
        let folded = self.display_map.folded_ranges().to_vec();
        let targets = rows
            .into_iter()
            .filter_map(|row| fold_candidate_for_line(row, &candidates, &folded))
            .map(|range| range.start_line)
            .collect::<BTreeSet<_>>();
        if targets.is_empty() {
            return false;
        }

        for start_line in targets {
            self.display_map.set_folded(start_line, true);
        }
        self.reconcile_selections_after_folding(cx);
        true
    }

    /// Unfold the innermost folded range containing each active caret.
    pub fn unfold_at_selections(&mut self, cx: &mut Context<Self>) -> bool {
        if self.disabled || !self.mode.is_folding() {
            return false;
        }

        let rows = self.active_selection_rows();
        let folded = self.display_map.folded_ranges().to_vec();
        let targets = rows
            .into_iter()
            .filter_map(|row| unfold_candidate_for_line(row, &folded))
            .map(|range| range.start_line)
            .collect::<BTreeSet<_>>();
        if targets.is_empty() {
            return false;
        }

        for start_line in targets {
            self.display_map.set_folded(start_line, false);
        }
        self.reconcile_selections_after_folding(cx);
        true
    }

    /// Fold every current syntax- and LSP-provided range.
    pub fn fold_all(&mut self, cx: &mut Context<Self>) -> bool {
        if self.disabled || !self.mode.is_folding() || !self.display_map.set_all_folded(true) {
            return false;
        }
        self.reconcile_selections_after_folding(cx);
        true
    }

    /// Unfold every current range.
    pub fn unfold_all(&mut self, cx: &mut Context<Self>) -> bool {
        if self.disabled || !self.mode.is_folding() || !self.display_map.set_all_folded(false) {
            return false;
        }
        self.reconcile_selections_after_folding(cx);
        true
    }

    /// Return bounded folding state suitable for host UI and automation.
    pub fn folding_snapshot(&self) -> EditorFoldingSnapshot {
        let enabled = !self.disabled && self.mode.is_folding();
        let candidates = self.display_map.fold_candidates();
        let folded = self.display_map.folded_ranges();
        let rows = self.active_selection_rows();
        let folded_ranges = folded
            .iter()
            .copied()
            .take(MAX_FOLDING_SNAPSHOT_RANGES)
            .collect::<Vec<_>>();

        EditorFoldingSnapshot {
            enabled,
            candidate_count: candidates.len(),
            folded_count: folded.len(),
            truncated: folded_ranges.len() < folded.len(),
            folded_ranges,
            can_fold: enabled
                && rows
                    .iter()
                    .any(|row| fold_candidate_for_line(*row, candidates, folded).is_some()),
            can_unfold: enabled
                && rows
                    .iter()
                    .any(|row| unfold_candidate_for_line(*row, folded).is_some()),
            can_fold_all: enabled && candidates != folded,
            can_unfold_all: enabled && !folded.is_empty(),
        }
    }

    fn active_selection_rows(&self) -> Vec<usize> {
        self.selections()
            .into_iter()
            .map(|selection| self.text.offset_to_point(selection.head()).row)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    fn reconcile_selections_after_folding(&mut self, cx: &mut Context<Self>) {
        let current = self.selections();
        let adjusted = current
            .iter()
            .map(|selection| {
                EditorSelection::from_anchor_and_head(
                    self.clamp_offset_to_visible_backward(selection.anchor()),
                    self.clamp_offset_to_visible_backward(selection.head()),
                )
            })
            .collect::<Vec<_>>();

        if adjusted != current {
            self.set_editor_selections(adjusted, cx);
        } else {
            self.scroll_to(self.cursor(), None, cx);
            cx.notify();
        }
    }

    pub(super) fn on_action_fold(&mut self, _: &Fold, _: &mut Window, cx: &mut Context<Self>) {
        self.fold_at_selections(cx);
    }

    pub(super) fn on_action_unfold(&mut self, _: &Unfold, _: &mut Window, cx: &mut Context<Self>) {
        self.unfold_at_selections(cx);
    }

    pub(super) fn on_action_fold_all(
        &mut self,
        _: &FoldAll,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.fold_all(cx);
    }

    pub(super) fn on_action_unfold_all(
        &mut self,
        _: &UnfoldAll,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.unfold_all(cx);
    }
}

fn fold_candidate_for_line(
    line: usize,
    candidates: &[FoldRange],
    folded: &[FoldRange],
) -> Option<FoldRange> {
    candidates
        .iter()
        .filter(|candidate| {
            candidate.start_line <= line
                && line <= candidate.end_line
                && !folded
                    .iter()
                    .any(|range| range.start_line == candidate.start_line)
        })
        .max_by_key(|candidate| candidate.start_line)
        .copied()
}

fn unfold_candidate_for_line(line: usize, folded: &[FoldRange]) -> Option<FoldRange> {
    folded
        .iter()
        .filter(|range| range.start_line <= line && line <= range.end_line)
        .max_by_key(|range| range.start_line)
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nested_candidates() -> Vec<FoldRange> {
        vec![
            FoldRange::new(0, 10),
            FoldRange::new(2, 5),
            FoldRange::new(12, 20),
        ]
    }

    #[test]
    fn fold_selects_innermost_unfolded_range_then_parent() {
        let candidates = nested_candidates();
        assert_eq!(
            fold_candidate_for_line(3, &candidates, &[]),
            Some(FoldRange::new(2, 5))
        );
        assert_eq!(
            fold_candidate_for_line(3, &candidates, &[FoldRange::new(2, 5)]),
            Some(FoldRange::new(0, 10))
        );
    }

    #[test]
    fn unfold_selects_innermost_folded_range() {
        let folded = vec![FoldRange::new(0, 10), FoldRange::new(2, 5)];
        assert_eq!(
            unfold_candidate_for_line(3, &folded),
            Some(FoldRange::new(2, 5))
        );
    }

    #[test]
    fn folding_helpers_ignore_unrelated_lines() {
        let candidates = nested_candidates();
        assert_eq!(fold_candidate_for_line(11, &candidates, &[]), None);
        assert_eq!(
            unfold_candidate_for_line(11, &[FoldRange::new(0, 10)]),
            None
        );
    }
}
