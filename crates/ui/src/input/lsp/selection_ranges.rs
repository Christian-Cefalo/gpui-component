use std::ops::Range;

use anyhow::Result;
use gpui::{Context, Task, Window};
use lsp_types::{Position, SelectionRange};
use ropey::Rope;

use crate::input::{ExpandSelection, InputEvent, InputState, RopeExt, ShrinkSelection};

/// Supplies the nested syntax ranges used by smart expand selection.
pub trait SelectionRangeProvider {
    fn selection_ranges(
        &self,
        text: &Rope,
        positions: Vec<Position>,
        window: &mut Window,
        cx: &mut Context<InputState>,
    ) -> Task<Result<Vec<SelectionRange>>>;
}

fn valid_offset(text: &Rope, position: Position) -> Option<usize> {
    let offset = text.position_to_offset(&position);
    (offset <= text.len() && text.offset_to_position(offset) == position).then_some(offset)
}

fn nested_selection_ranges(text: &Rope, root: &SelectionRange) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut current = Some(root);
    while let Some(selection) = current {
        let start = valid_offset(text, selection.range.start);
        let end = valid_offset(text, selection.range.end);
        if let (Some(start), Some(end)) = (start, end)
            && start <= end
        {
            let range = start..end;
            if ranges.last() != Some(&range) {
                ranges.push(range);
            }
        }
        current = selection.parent.as_deref();
    }
    ranges
}

fn next_expanded_range(
    text: &Rope,
    root: &SelectionRange,
    current: Range<usize>,
) -> Option<Range<usize>> {
    nested_selection_ranges(text, root)
        .into_iter()
        .filter(|range| {
            range.start <= current.start
                && range.end >= current.end
                && (range.start < current.start || range.end > current.end)
        })
        .min_by_key(|range| range.end.saturating_sub(range.start))
}

impl InputState {
    pub(crate) fn expand_selection(
        &mut self,
        _: &ExpandSelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(provider) = self.lsp.selection_range_provider.as_ref().cloned() else {
            return;
        };
        let current = self.selected_range();
        if self.lsp.selection_range_last != Some(current.clone().into()) {
            self.lsp.selection_range_history.clear();
            self.lsp.selection_range_last = None;
        }
        let text = self.text.clone();
        let position = text.offset_to_position(self.cursor());
        let task = provider.selection_ranges(&text, vec![position], window, cx);
        let input = cx.entity();
        self.lsp._selection_range_task = cx.spawn_in(window, async move |_, cx| {
            let Ok(ranges) = task.await else {
                return;
            };
            let Some(next) = ranges
                .first()
                .and_then(|root| next_expanded_range(&text, root, current.clone()))
            else {
                return;
            };
            let _ = input.update(cx, |input, cx| {
                if input.text != text || input.selected_range() != current {
                    return;
                }
                input.lsp.selection_range_history.push(input.selected_range);
                input.selected_range = next.into();
                input.selection_reversed = false;
                input.selected_word_range = None;
                input.lsp.selection_range_last = Some(input.selected_range);
                input.cancel_snippet_session_if_selection_outside(cx);
                cx.emit(InputEvent::SelectionChange);
                cx.notify();
            });
        });
    }

    pub(crate) fn shrink_selection(
        &mut self,
        _: &ShrinkSelection,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.lsp.selection_range_last != Some(self.selected_range) {
            self.lsp.selection_range_history.clear();
            self.lsp.selection_range_last = None;
            return;
        }
        let Some(previous) = self.lsp.selection_range_history.pop() else {
            return;
        };
        self.selected_range = previous;
        self.selection_reversed = false;
        self.selected_word_range = None;
        self.cancel_snippet_session_if_selection_outside(cx);
        self.lsp.selection_range_last = if self.lsp.selection_range_history.is_empty() {
            None
        } else {
            Some(previous)
        };
        cx.emit(InputEvent::SelectionChange);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::Range as LspRange;

    fn selection(start: u32, end: u32, parent: Option<SelectionRange>) -> SelectionRange {
        SelectionRange {
            range: LspRange::new(Position::new(0, start), Position::new(0, end)),
            parent: parent.map(Box::new),
        }
    }

    #[test]
    fn chooses_the_smallest_strictly_containing_server_range() {
        let text = Rope::from_str("let answer = value;");
        let root = selection(4, 10, Some(selection(4, 18, Some(selection(0, 19, None)))));
        assert_eq!(next_expanded_range(&text, &root, 6..6), Some(4..10));
        assert_eq!(next_expanded_range(&text, &root, 4..10), Some(4..18));
        assert_eq!(next_expanded_range(&text, &root, 0..19), None);
    }

    #[test]
    fn malformed_and_non_nested_ranges_are_ignored() {
        let text = Rope::from_str("abc");
        let malformed = selection(8, 9, None);
        assert!(nested_selection_ranges(&text, &malformed).is_empty());

        let reversed = selection(3, 1, None);
        assert!(nested_selection_ranges(&text, &reversed).is_empty());
    }
}
