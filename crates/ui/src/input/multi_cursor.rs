use std::{collections::BTreeMap, ops::Range};

use gpui::{Context, EntityInputHandler, Window};
use sum_tree::Bias;

use crate::input::{
    AddCursorAbove, AddCursorBelow, EditorSelection, InputEvent, InputState,
    RemoveSecondaryCursors, RopeExt as _, Selection,
};

pub(super) const MAX_EDITOR_SELECTIONS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MultiCursorHistoryEntry {
    pub(super) before: Vec<EditorSelection>,
    pub(super) after: Vec<EditorSelection>,
}

#[derive(Debug, Clone)]
struct CursorEdit {
    selection_index: Option<usize>,
    range: Range<usize>,
    text: String,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum MultiCursorDelete {
    Backward,
    Forward,
    ToLineStart,
    ToLineEnd,
    PreviousWord,
    NextWord,
}

impl InputState {
    fn commit_multi_cursor_movement(
        &mut self,
        selections: Vec<EditorSelection>,
        cx: &mut Context<Self>,
    ) {
        self.set_editor_selections(selections, cx);
        self.pause_blink_cursor(cx);
    }

    pub(super) fn move_all_cursors_horizontal(&mut self, forward: bool, cx: &mut Context<Self>) {
        let selections = self
            .selections()
            .into_iter()
            .map(|selection| {
                let offset = if selection.is_empty() {
                    if forward {
                        self.next_boundary(selection.head())
                    } else {
                        self.previous_boundary(selection.head())
                    }
                } else if forward {
                    selection.range.end
                } else {
                    selection.range.start
                };
                EditorSelection::caret(offset)
            })
            .collect();
        self.commit_multi_cursor_movement(selections, cx);
    }

    pub(super) fn select_all_cursors_horizontal(&mut self, forward: bool, cx: &mut Context<Self>) {
        let selections = self
            .selections()
            .into_iter()
            .map(|selection| {
                let head = if forward {
                    self.next_boundary(selection.head())
                } else {
                    self.previous_boundary(selection.head())
                };
                EditorSelection::from_anchor_and_head(selection.anchor(), head)
            })
            .collect();
        self.commit_multi_cursor_movement(selections, cx);
    }

    pub(super) fn move_all_cursors_vertical(&mut self, direction: isize, cx: &mut Context<Self>) {
        let selections = self
            .selections()
            .into_iter()
            .map(|selection| {
                self.offset_on_adjacent_line(selection.head(), direction)
                    .map(EditorSelection::caret)
                    .unwrap_or(selection)
            })
            .collect();
        self.commit_multi_cursor_movement(selections, cx);
    }

    pub(super) fn select_all_cursors_vertical(&mut self, direction: isize, cx: &mut Context<Self>) {
        let selections = self
            .selections()
            .into_iter()
            .map(|selection| {
                let head = self
                    .offset_on_adjacent_line(selection.head(), direction)
                    .unwrap_or_else(|| selection.head());
                EditorSelection::from_anchor_and_head(selection.anchor(), head)
            })
            .collect();
        self.commit_multi_cursor_movement(selections, cx);
    }

    pub(super) fn move_all_cursors_to_line_boundary(
        &mut self,
        end: bool,
        extend: bool,
        cx: &mut Context<Self>,
    ) {
        let current = self.selections();
        let saved_primary = self.primary_editor_selection();
        let selections = current
            .into_iter()
            .map(|selection| {
                self.set_primary_editor_selection(selection);
                let target = if end {
                    self.end_of_line()
                } else {
                    self.start_of_line()
                };
                if extend {
                    EditorSelection::from_anchor_and_head(selection.anchor(), target)
                } else {
                    EditorSelection::caret(target)
                }
            })
            .collect();
        self.set_primary_editor_selection(saved_primary);
        self.commit_multi_cursor_movement(selections, cx);
    }

    pub(super) fn move_all_cursors_to_document_boundary(
        &mut self,
        end: bool,
        extend: bool,
        cx: &mut Context<Self>,
    ) {
        let target = if end { self.text.len() } else { 0 };
        let selections = self
            .selections()
            .into_iter()
            .map(|selection| {
                if extend {
                    EditorSelection::from_anchor_and_head(selection.anchor(), target)
                } else {
                    EditorSelection::caret(target)
                }
            })
            .collect();
        self.commit_multi_cursor_movement(selections, cx);
    }

    pub(super) fn move_all_cursors_by_word(
        &mut self,
        forward: bool,
        extend: bool,
        cx: &mut Context<Self>,
    ) {
        let current = self.selections();
        let saved_primary = self.primary_editor_selection();
        let selections = current
            .into_iter()
            .map(|selection| {
                self.set_primary_editor_selection(selection);
                let target = if forward {
                    self.next_end_of_word()
                } else {
                    self.previous_start_of_word()
                };
                if extend {
                    EditorSelection::from_anchor_and_head(selection.anchor(), target)
                } else {
                    EditorSelection::caret(target)
                }
            })
            .collect();
        self.set_primary_editor_selection(saved_primary);
        self.commit_multi_cursor_movement(selections, cx);
    }

    pub(super) fn on_add_cursor_above(
        &mut self,
        _: &AddCursorAbove,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.add_cursor_above(cx);
    }

    pub(super) fn on_add_cursor_below(
        &mut self,
        _: &AddCursorBelow,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.add_cursor_below(cx);
    }

    pub(super) fn remove_secondary_cursors(
        &mut self,
        _: &RemoveSecondaryCursors,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_secondary_selections(cx);
    }

    pub(super) fn primary_editor_selection(&self) -> EditorSelection {
        EditorSelection {
            range: self.selected_range,
            reversed: self.selection_reversed,
        }
    }

    pub(super) fn set_primary_editor_selection(&mut self, selection: EditorSelection) {
        self.selected_range = selection.range;
        self.selection_reversed = selection.reversed;
    }

    /// Return every selection with the primary selection first.
    pub fn selections(&self) -> Vec<EditorSelection> {
        let mut selections = Vec::with_capacity(1 + self.secondary_selections.len());
        selections.push(self.primary_editor_selection());
        selections.extend(self.secondary_selections.iter().copied());
        selections
    }

    pub fn has_multiple_selections(&self) -> bool {
        !self.secondary_selections.is_empty()
    }

    /// Add a collapsed secondary caret on the logical line above each current
    /// selection, preserving the primary selection.
    pub fn add_cursor_above(&mut self, cx: &mut Context<Self>) {
        self.add_cursors_vertically(-1, cx);
    }

    /// Add a collapsed secondary caret on the logical line below each current
    /// selection, preserving the primary selection.
    pub fn add_cursor_below(&mut self, cx: &mut Context<Self>) {
        self.add_cursors_vertically(1, cx);
    }

    /// Add a collapsed secondary caret at a UTF-8 byte offset. Invalid offsets
    /// are clipped to a scalar boundary and duplicate/overlapping selections
    /// are normalized away.
    pub fn add_cursor_at_offset(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = self
            .text
            .clip_offset(offset.min(self.text.len()), Bias::Left);
        let mut selections = self.selections();
        selections.push(EditorSelection::caret(offset));
        self.set_editor_selections(selections, cx);
    }

    pub(super) fn secondary_editor_selections(&self) -> &[EditorSelection] {
        &self.secondary_selections
    }

    fn clip_editor_selection(&self, selection: EditorSelection) -> EditorSelection {
        let start = self
            .text
            .clip_offset(selection.range.start.min(self.text.len()), Bias::Left);
        let end = self
            .text
            .clip_offset(selection.range.end.min(self.text.len()), Bias::Right)
            .max(start);
        EditorSelection {
            range: Selection::new(start, end),
            reversed: selection.reversed && start != end,
        }
    }

    fn selections_should_merge(left: EditorSelection, right: EditorSelection) -> bool {
        if left.is_empty() || right.is_empty() {
            right.range.start <= left.range.end
        } else {
            right.range.start < left.range.end
        }
    }

    fn normalize_editor_selections(
        &self,
        selections: impl IntoIterator<Item = EditorSelection>,
    ) -> Vec<EditorSelection> {
        let mut indexed = selections
            .into_iter()
            .take(MAX_EDITOR_SELECTIONS)
            .enumerate()
            .map(|(index, selection)| (index, self.clip_editor_selection(selection)))
            .collect::<Vec<_>>();
        if indexed.is_empty() {
            indexed.push((0, self.primary_editor_selection()));
        }
        indexed.sort_by_key(|(_, selection)| {
            (selection.range.start, selection.range.end, selection.head())
        });

        let mut index = 0;
        while index + 1 < indexed.len() {
            let (left_id, left) = indexed[index];
            let (right_id, right) = indexed[index + 1];
            if !Self::selections_should_merge(left, right) {
                index += 1;
                continue;
            }

            let winner = if left_id == 0 {
                (left_id, left)
            } else if right_id == 0 {
                (right_id, right)
            } else if left_id < right_id {
                (left_id, left)
            } else {
                (right_id, right)
            };
            let range = Selection::new(
                left.range.start.min(right.range.start),
                left.range.end.max(right.range.end),
            );
            indexed[index] = (
                winner.0,
                EditorSelection {
                    range,
                    reversed: winner.1.reversed && !range.is_empty(),
                },
            );
            indexed.remove(index + 1);
            index = index.saturating_sub(1);
        }

        let primary_index = indexed
            .iter()
            .position(|(original_index, _)| *original_index == 0)
            .unwrap_or(0);
        let primary = indexed.remove(primary_index).1;
        let mut normalized = Vec::with_capacity(1 + indexed.len());
        normalized.push(primary);
        normalized.extend(indexed.into_iter().map(|(_, selection)| selection));
        normalized
    }

    pub(super) fn set_editor_selections_internal(
        &mut self,
        selections: impl IntoIterator<Item = EditorSelection>,
    ) {
        let mut selections = self.normalize_editor_selections(selections);
        let primary = selections.remove(0);
        self.set_primary_editor_selection(primary);
        self.secondary_selections = selections;
    }

    /// Replace the current editor selections. The first item remains primary;
    /// invalid UTF-8 offsets are clipped, duplicates are removed, and
    /// overlapping selections are merged.
    pub fn set_editor_selections(
        &mut self,
        selections: impl IntoIterator<Item = EditorSelection>,
        cx: &mut Context<Self>,
    ) {
        self.cancel_linked_editing(cx);
        self.snippet_session = None;
        self.selected_word_range = None;
        self.set_editor_selections_internal(selections);
        self.clear_inline_completion(cx);
        self.close_signature_help(cx);
        self.update_preferred_column();
        self.scroll_to(self.cursor(), None, cx);
        self.refresh_bracket_match();
        cx.emit(InputEvent::SelectionChange);
        cx.notify();
    }

    /// Remove every secondary selection while preserving the primary one.
    pub fn clear_secondary_selections(&mut self, cx: &mut Context<Self>) -> bool {
        if self.secondary_selections.is_empty() {
            return false;
        }
        self.secondary_selections.clear();
        self.refresh_bracket_match();
        cx.emit(InputEvent::SelectionChange);
        cx.notify();
        true
    }

    pub(super) fn toggle_cursor_at_offset(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = self
            .text
            .clip_offset(offset.min(self.text.len()), Bias::Left);
        if let Some(index) = self
            .secondary_selections
            .iter()
            .position(|selection| selection.is_empty() && selection.head() == offset)
        {
            self.secondary_selections.remove(index);
            cx.emit(InputEvent::SelectionChange);
            cx.notify();
            return;
        }
        if self.primary_editor_selection().is_empty() && self.cursor() == offset {
            return;
        }

        self.add_cursor_at_offset(offset, cx);
    }

    fn offset_on_adjacent_line(&self, offset: usize, direction: isize) -> Option<usize> {
        let point = self.text.offset_to_point(offset.min(self.text.len()));
        let target_row = point.row.checked_add_signed(direction)?;
        if target_row >= self.text.lines_len() {
            return None;
        }
        let start = self.text.line_start_offset(target_row);
        let end = self.text.line_end_offset(target_row);
        Some(start + point.column.min(end.saturating_sub(start)))
    }

    pub(super) fn add_cursors_vertically(&mut self, direction: isize, cx: &mut Context<Self>) {
        if self.mode.is_single_line() || direction == 0 {
            return;
        }
        let current = self.selections();
        let mut next = current.clone();
        next.extend(current.into_iter().filter_map(|selection| {
            self.offset_on_adjacent_line(selection.head(), direction)
                .map(EditorSelection::caret)
        }));
        if next.len() == self.selections().len() {
            return;
        }
        self.set_editor_selections(next, cx);
    }

    fn shift_selection_after_edit(
        selection: EditorSelection,
        edit_range: &Range<usize>,
        replacement_len: usize,
    ) -> EditorSelection {
        let delta = replacement_len as isize - edit_range.len() as isize;
        let shift = |offset: usize| {
            if offset >= edit_range.end {
                offset.saturating_add_signed(delta)
            } else {
                offset
            }
        };
        EditorSelection {
            range: Selection::new(shift(selection.range.start), shift(selection.range.end)),
            reversed: selection.reversed,
        }
    }

    fn apply_cursor_edits(
        &mut self,
        mut edits: Vec<CursorEdit>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if edits.is_empty() {
            return false;
        }
        edits.sort_by_key(|edit| (edit.range.start, edit.range.end));
        if edits
            .windows(2)
            .any(|pair| pair[0].range.end > pair[1].range.start)
        {
            return false;
        }
        let completion_trigger = edits
            .iter()
            .find(|edit| edit.selection_index == Some(0))
            .map(|edit| edit.text.clone())
            .unwrap_or_default();

        let before = self.selections();
        let was_silent = self.silent_replace_text;
        self.cancel_linked_editing(cx);
        self.snippet_session = None;
        self.secondary_selections.clear();
        self.multi_cursor_editing = true;
        self.start_undo_transaction();
        let history_version = self.history.version();
        let mut results = BTreeMap::<usize, EditorSelection>::new();

        for edit in edits.into_iter().rev() {
            let replacement_len = edit.text.len();
            for result in results.values_mut() {
                *result = Self::shift_selection_after_edit(*result, &edit.range, replacement_len);
            }
            self.silent_replace_text = was_silent;
            if let Some(selection_index) = edit.selection_index {
                self.set_primary_editor_selection(EditorSelection {
                    range: edit.range.into(),
                    reversed: false,
                });
                EntityInputHandler::replace_text_in_range(self, None, &edit.text, window, cx);
                results.insert(selection_index, self.primary_editor_selection());
            } else {
                let range_utf16 = self.range_to_utf16(&edit.range);
                EntityInputHandler::replace_text_in_range(
                    self,
                    Some(range_utf16),
                    &edit.text,
                    window,
                    cx,
                );
            }
        }

        self.end_undo_transaction();
        self.multi_cursor_editing = false;
        self.silent_replace_text = was_silent;
        let after = (0..before.len())
            .filter_map(|selection_index| results.remove(&selection_index))
            .collect::<Vec<_>>();
        self.set_editor_selections_internal(after);
        self.update_preferred_column();
        self.scroll_to(self.cursor(), None, cx);
        self.refresh_bracket_match();

        let after = self.selections();
        if before != after {
            let transaction_before = self
                .multi_cursor_history
                .get(&history_version)
                .map(|entry| entry.before.clone())
                .unwrap_or(before);
            self.multi_cursor_history.insert(
                history_version,
                MultiCursorHistoryEntry {
                    before: transaction_before,
                    after: after.clone(),
                },
            );
            while self.multi_cursor_history.len() > 1_000 {
                if let Some(version) = self.multi_cursor_history.keys().next().copied() {
                    self.multi_cursor_history.remove(&version);
                }
            }
        }

        if !was_silent {
            self.handle_signature_help_text_change(true, cx);
            self.handle_completion_trigger(&completion_trigger, window, cx);
        }
        if self.emit_events {
            cx.emit(InputEvent::Change);
            cx.emit(InputEvent::SelectionChange);
        }
        cx.notify();
        true
    }

    pub(super) fn replace_all_selections(
        &mut self,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let edits = self
            .selections()
            .into_iter()
            .enumerate()
            .map(|(selection_index, selection)| CursorEdit {
                selection_index: Some(selection_index),
                range: selection.range.into(),
                text: new_text.to_string(),
            })
            .collect();
        self.apply_cursor_edits(edits, window, cx);
    }

    pub(super) fn replace_selection_ranges(
        &mut self,
        ranges: Vec<Range<usize>>,
        texts: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if ranges.len() != self.selections().len() || texts.len() != ranges.len() {
            return;
        }
        let edits = ranges
            .into_iter()
            .zip(texts)
            .enumerate()
            .map(|(selection_index, (range, text))| CursorEdit {
                selection_index: Some(selection_index),
                range,
                text,
            })
            .collect();
        self.apply_cursor_edits(edits, window, cx);
    }

    /// Map a completion's primary replacement range onto every collapsed
    /// caret when the same prefix appears immediately before each caret.
    pub(crate) fn multi_cursor_completion_ranges(
        &self,
        primary_range: Range<usize>,
    ) -> Option<Vec<Range<usize>>> {
        if !self.has_multiple_selections() {
            return Some(vec![primary_range]);
        }
        if primary_range.start > primary_range.end || primary_range.end > self.text.len() {
            return None;
        }
        let prefix = self.text.slice(primary_range.clone()).to_string();
        let mut ranges = Vec::with_capacity(self.selections().len());
        for (index, selection) in self.selections().into_iter().enumerate() {
            if index == 0 {
                ranges.push(primary_range.clone());
                continue;
            }
            if !selection.is_empty() || selection.head() < prefix.len() {
                return None;
            }
            let start = selection.head() - prefix.len();
            if self.text.clip_offset(start, Bias::Left) != start
                || self.text.slice(start..selection.head()).to_string() != prefix
            {
                return None;
            }
            ranges.push(start..selection.head());
        }
        Some(ranges)
    }

    /// Apply one resolved completion to every mapped caret while applying the
    /// server's additional edits exactly once. All edits share one undo step.
    pub(crate) fn apply_multi_cursor_completion(
        &mut self,
        ranges: Vec<Range<usize>>,
        new_text: &str,
        additional_edits: Vec<(Range<usize>, String)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if ranges.len() != self.selections().len() {
            return false;
        }
        let mut edits = ranges
            .into_iter()
            .enumerate()
            .map(|(selection_index, range)| CursorEdit {
                selection_index: Some(selection_index),
                range,
                text: new_text.to_string(),
            })
            .collect::<Vec<_>>();
        edits.extend(
            additional_edits
                .into_iter()
                .map(|(range, text)| CursorEdit {
                    selection_index: None,
                    range,
                    text,
                }),
        );
        let was_silent = self.silent_replace_text;
        self.silent_replace_text = true;
        let applied = self.apply_cursor_edits(edits, window, cx);
        self.silent_replace_text = was_silent;
        applied
    }

    pub(super) fn delete_at_all_selections(
        &mut self,
        operation: MultiCursorDelete,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let selections = self.selections();
        let saved_primary = self.primary_editor_selection();
        let mut ranges = Vec::with_capacity(selections.len());
        for selection in selections.iter().copied() {
            self.set_primary_editor_selection(selection);
            let cursor = selection.head();
            let range = if !selection.is_empty() {
                selection.range.into()
            } else {
                match operation {
                    MultiCursorDelete::Backward => self
                        .paired_backspace_range_at(cursor)
                        .unwrap_or_else(|| self.previous_boundary(cursor)..cursor),
                    MultiCursorDelete::Forward => cursor..self.next_boundary(cursor),
                    MultiCursorDelete::ToLineStart => {
                        let mut start = self.start_of_line();
                        if start == cursor {
                            start = start.saturating_sub(1);
                        }
                        start..cursor
                    }
                    MultiCursorDelete::ToLineEnd => {
                        let mut end = self.end_of_line();
                        if end == cursor {
                            end = (end + 1).min(self.text.len());
                        }
                        cursor..end
                    }
                    MultiCursorDelete::PreviousWord => self.previous_start_of_word()..cursor,
                    MultiCursorDelete::NextWord => cursor..self.next_end_of_word(),
                }
            };
            ranges.push(range);
        }
        self.set_primary_editor_selection(saved_primary);
        self.replace_selection_ranges(ranges, vec![String::new(); selections.len()], window, cx);
        self.pause_blink_cursor(cx);
    }

    pub(super) fn newline_texts_for_selections(&mut self) -> Vec<String> {
        let selections = self.selections();
        let saved_primary = self.primary_editor_selection();
        let texts = selections
            .iter()
            .copied()
            .map(|selection| {
                self.set_primary_editor_selection(selection);
                let indent = if self.mode.is_code_editor() {
                    self.indent_of_next_line()
                } else {
                    String::new()
                };
                format!("\n{indent}")
            })
            .collect();
        self.set_primary_editor_selection(saved_primary);
        texts
    }

    pub(super) fn selected_text_for_all_selections(&self) -> Option<String> {
        let mut selections = self.selections();
        if selections.iter().all(EditorSelection::is_empty) {
            return None;
        }
        selections.sort_by_key(|selection| (selection.range.start, selection.range.end));
        Some(
            selections
                .into_iter()
                .filter(|selection| !selection.is_empty())
                .map(|selection| {
                    self.text
                        .slice(Range::<usize>::from(selection.range))
                        .to_string()
                })
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }

    pub(super) fn restore_multi_cursor_history(
        &mut self,
        version: usize,
        undo: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.multi_cursor_history.get(&version).cloned() else {
            self.secondary_selections.clear();
            return;
        };
        let selections = if undo { entry.before } else { entry.after };
        self.set_editor_selections_internal(selections);
        self.update_preferred_column();
        self.scroll_to(self.cursor(), None, cx);
        self.refresh_bracket_match();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Root, input::Undo, theme::Theme};
    use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext};

    struct InputView {
        input: Entity<InputState>,
        window_handle: gpui::WindowHandle<Root>,
    }

    impl InputView {
        fn new(cx: &mut TestAppContext) -> Self {
            let mut input = None;
            let window_handle = cx.update(|cx| {
                cx.open_window(Default::default(), |window, cx| {
                    cx.set_global(Theme::default());
                    super::super::init(cx);
                    input = Some(cx.new(|cx| InputState::new(window, cx).code_editor("rust")));
                    cx.new(|cx| Root::new(input.clone().unwrap(), window, cx))
                })
                .unwrap()
            });
            Self {
                input: input.unwrap(),
                window_handle,
            }
        }
    }

    #[test]
    fn editor_selection_tracks_anchor_and_active_head() {
        let forward = EditorSelection::from_anchor_and_head(2, 8);
        assert_eq!(forward.range, Selection::new(2, 8));
        assert_eq!(forward.anchor(), 2);
        assert_eq!(forward.head(), 8);

        let reversed = EditorSelection::from_anchor_and_head(8, 2);
        assert_eq!(reversed.range, Selection::new(2, 8));
        assert_eq!(reversed.anchor(), 8);
        assert_eq!(reversed.head(), 2);
    }

    #[gpui::test]
    fn typing_at_multiple_carets_is_one_undoable_transaction(cx: &mut TestAppContext) {
        let input_view = InputView::new(cx);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value("ab\nab", window, cx);
                state.set_editor_selections(
                    [EditorSelection::caret(2), EditorSelection::caret(5)],
                    cx,
                );
                EntityInputHandler::replace_text_in_range(state, None, "X", window, cx);
                assert_eq!(state.value(), "abX\nabX");
                assert_eq!(
                    state
                        .selections()
                        .into_iter()
                        .map(|selection| selection.head())
                        .collect::<Vec<_>>(),
                    vec![3, 7]
                );
            });
        });

        cx.update(|window, cx| {
            input.update(cx, |state, cx| state.undo(&Undo, window, cx));
        });
        input.read_with(&cx, |state, _| {
            assert_eq!(state.value(), "ab\nab");
            assert_eq!(
                state
                    .selections()
                    .into_iter()
                    .map(|selection| selection.head())
                    .collect::<Vec<_>>(),
                vec![2, 5]
            );
        });
    }

    #[gpui::test]
    fn add_cursor_below_and_overlapping_selection_normalization_are_stable(
        cx: &mut TestAppContext,
    ) {
        let input_view = InputView::new(cx);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value("ab\nab", window, cx);
                state.set_editor_selections([EditorSelection::caret(2)], cx);
                state.add_cursors_vertically(1, cx);
                assert_eq!(
                    state
                        .selections()
                        .into_iter()
                        .map(|selection| selection.head())
                        .collect::<Vec<_>>(),
                    vec![2, 5]
                );

                state.set_editor_selections(
                    [
                        EditorSelection::from_anchor_and_head(1, 4),
                        EditorSelection::caret(2),
                    ],
                    cx,
                );
                assert_eq!(
                    state.selections(),
                    vec![EditorSelection::from_anchor_and_head(1, 4)]
                );
            });
        });
    }

    #[gpui::test]
    fn completion_replaces_matching_prefixes_and_applies_additional_edits_once(
        cx: &mut TestAppContext,
    ) {
        let input_view = InputView::new(cx);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value("fo\nfo", window, cx);
                state.set_editor_selections(
                    [EditorSelection::caret(2), EditorSelection::caret(5)],
                    cx,
                );
                let ranges = state.multi_cursor_completion_ranges(0..2).unwrap();
                state.completion_inserting = true;
                assert!(state.apply_multi_cursor_completion(
                    ranges,
                    "format",
                    vec![(0..0, "use\n".to_string())],
                    window,
                    cx,
                ));
                state.completion_inserting = false;
                assert_eq!(state.value(), "use\nformat\nformat");
                assert_eq!(
                    state
                        .selections()
                        .into_iter()
                        .map(|selection| selection.head())
                        .collect::<Vec<_>>(),
                    vec![10, 17]
                );
            });
        });

        cx.update(|window, cx| {
            input.update(cx, |state, cx| state.undo(&Undo, window, cx));
        });
        input.read_with(&cx, |state, _| {
            assert_eq!(state.value(), "fo\nfo");
            assert_eq!(
                state
                    .selections()
                    .into_iter()
                    .map(|selection| selection.head())
                    .collect::<Vec<_>>(),
                vec![2, 5]
            );
        });
    }

    #[gpui::test]
    fn backspace_deletes_at_every_caret_and_restores_selections_on_undo(cx: &mut TestAppContext) {
        let input_view = InputView::new(cx);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value("ab\nab", window, cx);
                state.set_editor_selections(
                    [EditorSelection::caret(2), EditorSelection::caret(5)],
                    cx,
                );
                state.delete_at_all_selections(MultiCursorDelete::Backward, window, cx);
                assert_eq!(state.value(), "a\na");
                assert_eq!(
                    state
                        .selections()
                        .into_iter()
                        .map(|selection| selection.head())
                        .collect::<Vec<_>>(),
                    vec![1, 3]
                );
                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "ab\nab");
                assert_eq!(
                    state
                        .selections()
                        .into_iter()
                        .map(|selection| selection.head())
                        .collect::<Vec<_>>(),
                    vec![2, 5]
                );
            });
        });
    }
}
