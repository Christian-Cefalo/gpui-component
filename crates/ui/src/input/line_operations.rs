use std::{
    collections::{BTreeMap, HashSet},
    ops::RangeInclusive,
};

use gpui::{Context, Window};
use icu_collator::{Collator, CollatorOptions};
use icu_locid::Locale;

use crate::input::{
    CopyLineDown, CopyLineUp, DeleteDuplicateLines, DeleteLine, DuplicateSelection,
    EditorSelection, InputState, InsertLineAbove, InsertLineBelow, JoinLines, MoveLineDown,
    MoveLineUp, ReverseLines, Rope, RopeExt as _, Selection, SortLinesAscending,
    SortLinesDescending, comment::selection_line_range,
};

const MAX_JOIN_LINES: usize = 100_000;
const MAX_REORDER_LINES: usize = 100_000;

#[derive(Debug, Clone, PartialEq, Eq)]
struct LineOperationPlan {
    edits: Vec<(std::ops::Range<usize>, String)>,
    selections_after: Vec<EditorSelection>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InsertLineDirection {
    Above,
    Below,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineDirection {
    Up,
    Down,
}

#[derive(Debug, Clone)]
struct RewrittenBlock {
    selected_rows: RangeInclusive<usize>,
    edit: (std::ops::Range<usize>, String),
    relative_start_by_original_row: BTreeMap<usize, usize>,
    selected_end_relative: usize,
    post_edit_start: usize,
}

#[derive(Debug, Clone)]
struct DuplicateInsertion {
    selection_index: usize,
    offset: usize,
    replacement: String,
    selected_start_relative: usize,
    selected_end_relative: usize,
    reversed: bool,
}

#[derive(Debug, Clone)]
struct ReverseBlock {
    selection_index: usize,
    rows: RangeInclusive<usize>,
    edit: (std::ops::Range<usize>, String),
    relative_start_by_original_row: BTreeMap<usize, usize>,
    selection: EditorSelection,
}

#[derive(Debug, Clone)]
struct SortBlock {
    selection_index: usize,
    edit: (std::ops::Range<usize>, String),
    selection: EditorSelection,
}

#[derive(Debug, Clone)]
struct DeduplicateBlock {
    selection_index: usize,
    edit: (std::ops::Range<usize>, String),
}

#[derive(Debug, Clone)]
struct JoinRowMap {
    replacement_start: usize,
    indentation_len: usize,
    mapped_content_len: usize,
}

#[derive(Debug, Clone)]
struct JoinRewrite {
    rows: RangeInclusive<usize>,
    edit: (std::ops::Range<usize>, String),
    row_maps: BTreeMap<usize, JoinRowMap>,
    boundary_after_row: BTreeMap<usize, usize>,
    post_edit_start: usize,
}

impl InputState {
    /// Delete every logical line covered by the editor selections as one
    /// undoable mutation. Adjacent or overlapping selections own one block.
    pub fn delete_lines(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.mode.is_code_editor() {
            return false;
        }
        let Some(plan) = plan_delete_lines(&self.text, &self.selections()) else {
            return false;
        };
        self.apply_buffer_edits_with_selections(plan.edits, plan.selections_after, window, cx)
    }

    /// Insert one indented logical line above every distinct active line.
    pub fn insert_lines_above(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.insert_lines(InsertLineDirection::Above, window, cx)
    }

    /// Insert one indented logical line below every distinct active line.
    pub fn insert_lines_below(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.insert_lines(InsertLineDirection::Below, window, cx)
    }

    pub fn move_lines_up(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.move_lines(LineDirection::Up, window, cx)
    }

    pub fn move_lines_down(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.move_lines(LineDirection::Down, window, cx)
    }

    pub fn copy_lines_up(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.copy_lines(LineDirection::Up, window, cx)
    }

    pub fn copy_lines_down(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.copy_lines(LineDirection::Down, window, cx)
    }

    /// Duplicate each empty selection's logical line or each non-empty
    /// selection's exact text as one undoable edit transaction.
    pub fn duplicate_selections(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.mode.is_code_editor() {
            return false;
        }
        let Some(plan) = plan_duplicate_selections(&self.text, &self.selections()) else {
            return false;
        };
        self.apply_buffer_edits_with_selections(plan.edits, plan.selections_after, window, cx)
    }

    /// Reverse the selected logical lines, or the whole document for one
    /// single-line selection, while retaining cursor columns and direction.
    pub fn reverse_lines(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.mode.is_code_editor() {
            return false;
        }
        let Some(plan) = plan_reverse_lines(&self.text, &self.selections()) else {
            return false;
        };
        let changes_text = plan
            .edits
            .iter()
            .any(|(range, replacement)| self.text.slice(range.clone()).to_string() != *replacement);
        if !changes_text {
            if self.selections() == plan.selections_after {
                return false;
            }
            self.set_editor_selections(plan.selections_after, cx);
            return true;
        }
        self.apply_buffer_edits_with_selections(plan.edits, plan.selections_after, window, cx)
    }

    /// Sort the selected logical lines using the operating system locale, or
    /// the whole document when the editor has one single-line selection.
    pub fn sort_lines_ascending(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.sort_lines(false, window, cx)
    }

    /// Sort the selected logical lines in descending locale order, or the
    /// whole document when the editor has one single-line selection.
    pub fn sort_lines_descending(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.sort_lines(true, window, cx)
    }

    /// Keep only the first occurrence of each exact logical line inside each
    /// selection, or across the whole document for one single-line selection.
    pub fn delete_duplicate_lines(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.mode.is_code_editor() {
            return false;
        }
        let Some(plan) = plan_delete_duplicate_lines(&self.text, &self.selections()) else {
            return false;
        };
        let changes_text = plan
            .edits
            .iter()
            .any(|(range, replacement)| self.text.slice(range.clone()).to_string() != *replacement);
        if !changes_text {
            if self.selections() == plan.selections_after {
                return false;
            }
            self.set_editor_selections(plan.selections_after, cx);
            return true;
        }
        self.apply_buffer_edits_with_selections(plan.edits, plan.selections_after, window, cx)
    }

    /// Join the selected logical lines, or the active line with its successor,
    /// as one undoable transaction while retaining every cursor.
    pub fn join_lines(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.mode.is_code_editor() {
            return false;
        }
        let Some(plan) = plan_join_lines(&self.text, &self.selections()) else {
            return false;
        };
        let changes_text = plan
            .edits
            .iter()
            .any(|(range, replacement)| self.text.slice(range.clone()).to_string() != *replacement);
        if !changes_text {
            if self.selections() == plan.selections_after {
                return false;
            }
            self.set_editor_selections(plan.selections_after, cx);
            return true;
        }
        self.apply_buffer_edits_with_selections(plan.edits, plan.selections_after, window, cx)
    }

    fn insert_lines(
        &mut self,
        direction: InsertLineDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.mode.is_code_editor() {
            return false;
        }
        let Some(plan) = plan_insert_lines(&self.text, &self.selections(), direction) else {
            return false;
        };
        self.apply_buffer_edits_with_selections(plan.edits, plan.selections_after, window, cx)
    }

    fn move_lines(
        &mut self,
        direction: LineDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.mode.is_code_editor() {
            return false;
        }
        let Some(plan) = plan_move_lines(&self.text, &self.selections(), direction) else {
            return false;
        };
        self.apply_buffer_edits_with_selections(plan.edits, plan.selections_after, window, cx)
    }

    fn copy_lines(
        &mut self,
        direction: LineDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.mode.is_code_editor() {
            return false;
        }
        let Some(plan) = plan_copy_lines(&self.text, &self.selections(), direction) else {
            return false;
        };
        self.apply_buffer_edits_with_selections(plan.edits, plan.selections_after, window, cx)
    }

    fn sort_lines(
        &mut self,
        descending: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.mode.is_code_editor() {
            return false;
        }
        let Some(collator) = system_line_collator() else {
            return false;
        };
        let Some(plan) = plan_sort_lines(&self.text, &self.selections(), &collator, descending)
        else {
            return false;
        };
        self.apply_buffer_edits_with_selections(plan.edits, plan.selections_after, window, cx)
    }

    pub(super) fn on_delete_line(
        &mut self,
        _: &DeleteLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.delete_lines(window, cx);
    }

    pub(super) fn on_insert_line_above(
        &mut self,
        _: &InsertLineAbove,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.insert_lines_above(window, cx);
    }

    pub(super) fn on_insert_line_below(
        &mut self,
        _: &InsertLineBelow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.insert_lines_below(window, cx);
    }

    pub(super) fn on_move_line_up(
        &mut self,
        _: &MoveLineUp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_lines_up(window, cx);
    }

    pub(super) fn on_move_line_down(
        &mut self,
        _: &MoveLineDown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_lines_down(window, cx);
    }

    pub(super) fn on_copy_line_up(
        &mut self,
        _: &CopyLineUp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.copy_lines_up(window, cx);
    }

    pub(super) fn on_copy_line_down(
        &mut self,
        _: &CopyLineDown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.copy_lines_down(window, cx);
    }

    pub(super) fn on_duplicate_selection(
        &mut self,
        _: &DuplicateSelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.duplicate_selections(window, cx);
    }

    pub(super) fn on_reverse_lines(
        &mut self,
        _: &ReverseLines,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.reverse_lines(window, cx);
    }

    pub(super) fn on_sort_lines_ascending(
        &mut self,
        _: &SortLinesAscending,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sort_lines_ascending(window, cx);
    }

    pub(super) fn on_sort_lines_descending(
        &mut self,
        _: &SortLinesDescending,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sort_lines_descending(window, cx);
    }

    pub(super) fn on_delete_duplicate_lines(
        &mut self,
        _: &DeleteDuplicateLines,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.delete_duplicate_lines(window, cx);
    }

    pub(super) fn on_join_lines(
        &mut self,
        _: &JoinLines,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.join_lines(window, cx);
    }
}

fn plan_delete_lines(text: &Rope, selections: &[EditorSelection]) -> Option<LineOperationPlan> {
    if selections.is_empty() || text.lines_len() == 0 {
        return None;
    }
    let merged = selected_line_blocks(text, selections, true)?;

    let mut edits = Vec::new();
    let mut caret_by_row = BTreeMap::<usize, usize>::new();
    let mut delta = 0isize;
    for rows in &merged {
        let start_row = *rows.start();
        let end_row = *rows.end();
        let mut start = text.line_start_offset(start_row);
        let end = if end_row + 1 < text.lines_len() {
            text.line_start_offset(end_row + 1)
        } else {
            if start > 0 {
                start = preceding_line_ending_start(text, start);
            }
            text.len()
        };
        if start == end {
            continue;
        }
        let caret = start.saturating_add_signed(delta);
        for row in rows.clone() {
            caret_by_row.insert(row, caret);
        }
        delta -= (end - start) as isize;
        edits.push((start..end, String::new()));
    }
    if edits.is_empty() {
        return None;
    }
    let selections_after = selections
        .iter()
        .filter_map(|selection| {
            let row = text.offset_to_point(selection.range.start).row;
            caret_by_row.get(&row).copied().map(EditorSelection::caret)
        })
        .collect::<Vec<_>>();
    if selections_after.is_empty() {
        return None;
    }
    Some(LineOperationPlan {
        edits,
        selections_after,
    })
}

fn plan_move_lines(
    text: &Rope,
    selections: &[EditorSelection],
    direction: LineDirection,
) -> Option<LineOperationPlan> {
    let blocks = selected_line_blocks(text, selections, true)?;
    let last_row = text.lines_len().saturating_sub(1);
    let mut rewrites = Vec::new();
    for block in blocks {
        let movable = match direction {
            LineDirection::Up => *block.start() > 0,
            LineDirection::Down => *block.end() < last_row,
        };
        if !movable {
            continue;
        }
        let (affected, order) = match direction {
            LineDirection::Up => {
                let preceding = *block.start() - 1;
                let order = block
                    .clone()
                    .chain(std::iter::once(preceding))
                    .collect::<Vec<_>>();
                (preceding..=*block.end(), order)
            }
            LineDirection::Down => {
                let following = *block.end() + 1;
                let order = std::iter::once(following)
                    .chain(block.clone())
                    .collect::<Vec<_>>();
                (*block.start()..=following, order)
            }
        };
        let mut rewrite = compose_row_rewrite(text, block.clone(), affected, &order);
        rewrite.selected_end_relative = match direction {
            LineDirection::Up => rewrite
                .relative_start_by_original_row
                .get(&(*block.start() - 1))
                .copied()
                .unwrap_or(rewrite.edit.1.len()),
            LineDirection::Down => rewrite.edit.1.len(),
        };
        rewrites.push(rewrite);
    }
    finish_rewritten_blocks(text, selections, rewrites)
}

fn plan_copy_lines(
    text: &Rope,
    selections: &[EditorSelection],
    direction: LineDirection,
) -> Option<LineOperationPlan> {
    // VS Code copies adjacent caret lines independently. Merge only actual
    // overlap so two cursors on neighboring lines produce one duplicate of
    // each line rather than duplicating the pair as a single block.
    let blocks = selected_line_blocks(text, selections, false)?;
    let line_ending = document_line_ending(text);
    let mut rewrites = Vec::new();
    for block in blocks {
        let (offset, prefix, trailing_line_ending) = match direction {
            LineDirection::Up => (text.line_start_offset(*block.start()), "", true),
            LineDirection::Down if *block.end() + 1 < text.lines_len() => {
                (text.line_start_offset(*block.end() + 1), "", true)
            }
            LineDirection::Down => (text.len(), line_ending, false),
        };
        let mut replacement = prefix.to_string();
        let mut relative_start_by_original_row = BTreeMap::new();
        let block_len = block.clone().count();
        for (index, row) in block.clone().enumerate() {
            relative_start_by_original_row.insert(row, replacement.len());
            replacement.push_str(&line_content(text, row));
            if index + 1 < block_len || trailing_line_ending {
                replacement.push_str(line_ending);
            }
        }
        let selected_end_relative = replacement.len();
        rewrites.push(RewrittenBlock {
            selected_rows: block,
            edit: (offset..offset, replacement),
            relative_start_by_original_row,
            selected_end_relative,
            post_edit_start: 0,
        });
    }
    finish_rewritten_blocks(text, selections, rewrites)
}

fn selected_line_blocks(
    text: &Rope,
    selections: &[EditorSelection],
    merge_adjacent: bool,
) -> Option<Vec<RangeInclusive<usize>>> {
    if selections.is_empty() || text.lines_len() == 0 {
        return None;
    }
    let mut blocks = selections
        .iter()
        .copied()
        .map(|selection| selection_line_range(text, selection))
        .collect::<Option<Vec<_>>>()?;
    blocks.sort_by_key(|rows| (*rows.start(), *rows.end()));
    let mut merged = Vec::<RangeInclusive<usize>>::new();
    for rows in blocks {
        let overlaps_previous = merged.last().is_some_and(|previous| {
            let previous_end = *previous.end();
            *rows.start()
                <= if merge_adjacent {
                    previous_end.saturating_add(1)
                } else {
                    previous_end
                }
        });
        if overlaps_previous {
            let previous = merged.last_mut().expect("merged block exists");
            let start = *previous.start();
            let end = (*previous.end()).max(*rows.end());
            *previous = start..=end;
        } else {
            merged.push(rows);
        }
    }
    Some(merged)
}

fn compose_row_rewrite(
    text: &Rope,
    selected_rows: RangeInclusive<usize>,
    affected_rows: RangeInclusive<usize>,
    order: &[usize],
) -> RewrittenBlock {
    let line_ending = document_line_ending(text);
    let range_start = text.line_start_offset(*affected_rows.start());
    let range_end = if *affected_rows.end() + 1 < text.lines_len() {
        text.line_start_offset(*affected_rows.end() + 1)
    } else {
        text.len()
    };
    let trailing_line_ending = *affected_rows.end() + 1 < text.lines_len();
    let mut replacement = String::new();
    let mut relative_start_by_original_row = BTreeMap::new();
    for (index, row) in order.iter().copied().enumerate() {
        relative_start_by_original_row.insert(row, replacement.len());
        replacement.push_str(&line_content(text, row));
        if index + 1 < order.len() || trailing_line_ending {
            replacement.push_str(line_ending);
        }
    }
    RewrittenBlock {
        selected_rows,
        edit: (range_start..range_end, replacement),
        relative_start_by_original_row,
        selected_end_relative: 0,
        post_edit_start: 0,
    }
}

fn finish_rewritten_blocks(
    text: &Rope,
    selections: &[EditorSelection],
    mut rewrites: Vec<RewrittenBlock>,
) -> Option<LineOperationPlan> {
    if rewrites.is_empty() {
        return None;
    }
    rewrites.sort_by_key(|rewrite| rewrite.edit.0.start);
    let mut delta = 0isize;
    for rewrite in &mut rewrites {
        rewrite.post_edit_start = rewrite.edit.0.start.saturating_add_signed(delta);
        delta += rewrite.edit.1.len() as isize - rewrite.edit.0.len() as isize;
    }
    let edits = rewrites
        .iter()
        .map(|rewrite| rewrite.edit.clone())
        .collect::<Vec<_>>();
    let selections_after = selections
        .iter()
        .copied()
        .map(|selection| {
            let start_row = text.offset_to_point(selection.range.start).row;
            if let Some(rewrite) = rewrites
                .iter()
                .find(|rewrite| rewrite.selected_rows.contains(&start_row))
            {
                map_selection_into_rewrite(text, selection, rewrite, &edits)
            } else {
                transform_selection_through_edits(selection, &edits)
            }
        })
        .collect::<Vec<_>>();
    Some(LineOperationPlan {
        edits,
        selections_after,
    })
}

fn map_selection_into_rewrite(
    text: &Rope,
    selection: EditorSelection,
    rewrite: &RewrittenBlock,
    edits: &[(std::ops::Range<usize>, String)],
) -> EditorSelection {
    let map = |offset| {
        let point = text.offset_to_point(offset);
        if let Some(relative_start) = rewrite
            .relative_start_by_original_row
            .get(&point.row)
            .copied()
            .filter(|_| rewrite.selected_rows.contains(&point.row))
        {
            let column = point.column.min(line_content(text, point.row).len());
            return rewrite.post_edit_start + relative_start + column;
        }
        if point.column == 0 && point.row == *rewrite.selected_rows.end() + 1 {
            return rewrite.post_edit_start + rewrite.selected_end_relative;
        }
        transform_offset_through_edits(offset, edits)
    };
    let start = map(selection.range.start);
    let end = map(selection.range.end);
    EditorSelection {
        range: Selection::new(start.min(end), start.max(end)),
        reversed: selection.reversed,
    }
}

fn transform_selection_through_edits(
    selection: EditorSelection,
    edits: &[(std::ops::Range<usize>, String)],
) -> EditorSelection {
    let start = transform_offset_through_edits(selection.range.start, edits);
    let end = transform_offset_through_edits(selection.range.end, edits);
    EditorSelection {
        range: Selection::new(start.min(end), start.max(end)),
        reversed: selection.reversed,
    }
}

fn transform_offset_through_edits(
    offset: usize,
    edits: &[(std::ops::Range<usize>, String)],
) -> usize {
    let mut delta = 0isize;
    for (range, replacement) in edits {
        if offset < range.start {
            break;
        }
        if range.is_empty() {
            if offset >= range.start {
                delta += replacement.len() as isize;
            }
            continue;
        }
        if offset == range.start {
            return range.start.saturating_add_signed(delta);
        }
        if offset < range.end {
            return range
                .start
                .saturating_add_signed(delta)
                .saturating_add((offset - range.start).min(replacement.len()));
        }
        delta += replacement.len() as isize - range.len() as isize;
    }
    offset.saturating_add_signed(delta)
}

fn plan_duplicate_selections(
    text: &Rope,
    selections: &[EditorSelection],
) -> Option<LineOperationPlan> {
    if selections.is_empty() || text.lines_len() == 0 {
        return None;
    }
    let line_ending = document_line_ending(text);
    let mut insertions = Vec::with_capacity(selections.len());
    for (selection_index, selection) in selections.iter().copied().enumerate() {
        if selection.range.start > selection.range.end
            || selection.range.end > text.len()
            || !text.is_char_boundary(selection.range.start)
            || !text.is_char_boundary(selection.range.end)
        {
            return None;
        }
        if selection.is_empty() {
            let point = text.offset_to_point(selection.head());
            let content = line_content(text, point.row);
            let (offset, prefix, suffix) = if point.row + 1 < text.lines_len() {
                (text.line_start_offset(point.row + 1), "", line_ending)
            } else {
                (text.len(), line_ending, "")
            };
            let mut replacement = prefix.to_string();
            let selected_relative = replacement.len() + point.column.min(content.len());
            replacement.push_str(&content);
            replacement.push_str(suffix);
            insertions.push(DuplicateInsertion {
                selection_index,
                offset,
                replacement,
                selected_start_relative: selected_relative,
                selected_end_relative: selected_relative,
                reversed: false,
            });
        } else {
            let replacement = text.slice(selection.range.clone()).to_string();
            insertions.push(DuplicateInsertion {
                selection_index,
                offset: selection.range.end,
                selected_start_relative: 0,
                selected_end_relative: replacement.len(),
                replacement,
                reversed: selection.reversed,
            });
        }
    }
    insertions.sort_by_key(|insertion| (insertion.offset, insertion.selection_index));

    let mut delta = 0usize;
    let mut edits = Vec::with_capacity(insertions.len());
    let mut selections_after = vec![None; selections.len()];
    for insertion in insertions {
        let post_edit_start = insertion.offset + delta;
        selections_after[insertion.selection_index] = Some(EditorSelection {
            range: Selection::new(
                post_edit_start + insertion.selected_start_relative,
                post_edit_start + insertion.selected_end_relative,
            ),
            reversed: insertion.reversed,
        });
        delta += insertion.replacement.len();
        edits.push((insertion.offset..insertion.offset, insertion.replacement));
    }
    Some(LineOperationPlan {
        edits,
        selections_after: selections_after.into_iter().collect::<Option<Vec<_>>>()?,
    })
}

fn plan_reverse_lines(text: &Rope, selections: &[EditorSelection]) -> Option<LineOperationPlan> {
    if selections.is_empty() || text.lines_len() == 0 {
        return None;
    }
    if selections.iter().any(|selection| {
        selection.range.start > selection.range.end
            || selection.range.end > text.len()
            || !text.is_char_boundary(selection.range.start)
            || !text.is_char_boundary(selection.range.end)
    }) {
        return None;
    }
    let expand_to_document = selections.len() == 1 && {
        let selection = selections[0];
        text.offset_to_point(selection.range.start).row
            == text.offset_to_point(selection.range.end).row
    };
    let last_row = text.lines_len().saturating_sub(1);
    let line_ending = document_line_ending(text);
    let mut blocks = Vec::with_capacity(selections.len());
    let mut planned_rows = 0usize;
    for (selection_index, selection) in selections.iter().copied().enumerate() {
        let mut rows = if expand_to_document {
            0..=last_row
        } else {
            selection_line_range(text, selection)?
        };
        if *rows.end() == last_row && line_content(text, last_row).is_empty() {
            if *rows.start() < last_row {
                rows = *rows.start()..=last_row - 1;
            }
        }
        planned_rows = planned_rows.checked_add(rows.clone().count())?;
        if planned_rows > MAX_REORDER_LINES {
            return None;
        }
        let range_start = text.line_start_offset(*rows.start());
        let range_end = text.line_start_offset(*rows.end()) + line_content(text, *rows.end()).len();
        let mut replacement = String::new();
        let mut relative_start_by_original_row = BTreeMap::new();
        let row_count = rows.clone().count();
        for (index, row) in rows.clone().rev().enumerate() {
            relative_start_by_original_row.insert(row, replacement.len());
            replacement.push_str(&line_content(text, row));
            if index + 1 < row_count {
                replacement.push_str(line_ending);
            }
        }
        blocks.push(ReverseBlock {
            selection_index,
            rows,
            edit: (range_start..range_end, replacement),
            relative_start_by_original_row,
            selection,
        });
    }
    if blocks.is_empty() {
        return None;
    }
    blocks.sort_by_key(|block| (block.edit.0.start, block.edit.0.end));
    if blocks
        .windows(2)
        .any(|pair| pair[0].edit.0.end > pair[1].edit.0.start)
    {
        return None;
    }

    let mut selections_after = vec![None; selections.len()];
    for block in &blocks {
        let selection = block.selection;
        let rows = &block.rows;
        let range_start = block.edit.0.start;
        let relative_start_by_original_row = &block.relative_start_by_original_row;
        let mapped = {
            let map = |offset| {
                let point = text.offset_to_point(offset);
                if !rows.contains(&point.row) {
                    return offset;
                }
                let content = line_content(text, point.row);
                let mut column = point.column.min(content.len());
                while column > 0 && !content.is_char_boundary(column) {
                    column -= 1;
                }
                range_start
                    + relative_start_by_original_row
                        .get(&point.row)
                        .copied()
                        .unwrap_or(0)
                    + column
            };
            EditorSelection::from_anchor_and_head(map(selection.anchor()), map(selection.head()))
        };
        selections_after[block.selection_index] = Some(mapped);
    }
    let edits = blocks.into_iter().map(|block| block.edit).collect();
    Some(LineOperationPlan {
        edits,
        selections_after: selections_after.into_iter().collect::<Option<Vec<_>>>()?,
    })
}

fn system_line_collator() -> Option<Collator> {
    if let Some(locale) = sys_locale::get_locale().and_then(|locale| locale.parse::<Locale>().ok())
        && let Ok(collator) = Collator::try_new(&locale.into(), CollatorOptions::new())
    {
        return Some(collator);
    }
    Collator::try_new(&Default::default(), CollatorOptions::new()).ok()
}

fn plan_sort_lines(
    text: &Rope,
    selections: &[EditorSelection],
    collator: &Collator,
    descending: bool,
) -> Option<LineOperationPlan> {
    if selections.is_empty() || text.lines_len() == 0 {
        return None;
    }
    if selections.iter().any(|selection| {
        selection.range.start > selection.range.end
            || selection.range.end > text.len()
            || !text.is_char_boundary(selection.range.start)
            || !text.is_char_boundary(selection.range.end)
    }) {
        return None;
    }
    let expand_to_document = selections.len() == 1 && {
        let selection = selections[0];
        text.offset_to_point(selection.range.start).row
            == text.offset_to_point(selection.range.end).row
    };
    let last_row = text.lines_len().saturating_sub(1);
    let line_ending = document_line_ending(text);
    let mut blocks = Vec::with_capacity(selections.len());
    let mut planned_rows = 0usize;

    for (selection_index, selection) in selections.iter().copied().enumerate() {
        let rows = if expand_to_document {
            let end_row = if last_row > 0 && line_content(text, last_row).is_empty() {
                last_row - 1
            } else {
                last_row
            };
            0..=end_row
        } else {
            selection_line_range(text, selection)?
        };
        if rows.start() >= rows.end() {
            // VS Code refuses the complete multi-cursor action when any
            // selection contains fewer than two sortable lines.
            return None;
        }
        planned_rows = planned_rows.checked_add(rows.clone().count())?;
        if planned_rows > MAX_REORDER_LINES {
            return None;
        }

        let before = rows
            .clone()
            .map(|row| line_content(text, row))
            .collect::<Vec<_>>();
        let mut ordered = rows.clone().zip(before.iter().cloned()).collect::<Vec<_>>();
        ordered.sort_by(|left, right| collator.compare(&left.1, &right.1));
        if descending {
            // Match Intl.Collator + Array.reverse(), including the ordering
            // of strings that compare as equivalent at the active strength.
            ordered.reverse();
        }
        if before
            .iter()
            .zip(&ordered)
            .all(|(before, (_, after))| before == after)
        {
            return None;
        }

        let range_start = text.line_start_offset(*rows.start());
        let range_end = text.line_start_offset(*rows.end()) + line_content(text, *rows.end()).len();
        let mut replacement = String::new();
        let row_count = ordered.len();
        for (index, (_, content)) in ordered.into_iter().enumerate() {
            replacement.push_str(&content);
            if index + 1 < row_count {
                replacement.push_str(line_ending);
            }
        }
        blocks.push(SortBlock {
            selection_index,
            edit: (range_start..range_end, replacement),
            selection,
        });
    }

    blocks.sort_by_key(|block| (block.edit.0.start, block.edit.0.end));
    if blocks
        .windows(2)
        .any(|pair| pair[0].edit.0.end > pair[1].edit.0.start)
    {
        return None;
    }

    let mut selections_after = vec![None; selections.len()];
    for block in &blocks {
        let map = |offset: usize| {
            if offset < block.edit.0.start || offset > block.edit.0.end {
                return offset;
            }
            let mut relative = offset
                .saturating_sub(block.edit.0.start)
                .min(block.edit.1.len());
            while relative > 0 && !block.edit.1.is_char_boundary(relative) {
                relative -= 1;
            }
            block.edit.0.start + relative
        };
        selections_after[block.selection_index] = Some(EditorSelection::from_anchor_and_head(
            map(block.selection.anchor()),
            map(block.selection.head()),
        ));
    }
    let edits = blocks.into_iter().map(|block| block.edit).collect();
    Some(LineOperationPlan {
        edits,
        selections_after: selections_after.into_iter().collect::<Option<Vec<_>>>()?,
    })
}

fn plan_delete_duplicate_lines(
    text: &Rope,
    selections: &[EditorSelection],
) -> Option<LineOperationPlan> {
    if selections.is_empty()
        || text.lines_len() == 0
        || (text.lines_len() == 1 && line_content(text, 0).is_empty())
    {
        return None;
    }
    if selections.iter().any(|selection| {
        selection.range.start > selection.range.end
            || selection.range.end > text.len()
            || !text.is_char_boundary(selection.range.start)
            || !text.is_char_boundary(selection.range.end)
    }) {
        return None;
    }

    let expand_to_document = selections.len() == 1 && {
        let selection = selections[0];
        text.offset_to_point(selection.range.start).row
            == text.offset_to_point(selection.range.end).row
    };
    let last_row = text.lines_len().saturating_sub(1);
    let line_ending = document_line_ending(text);
    let mut blocks = Vec::with_capacity(selections.len());
    let mut planned_rows = 0usize;

    for (selection_index, selection) in selections.iter().copied().enumerate() {
        let rows = if expand_to_document {
            0..=last_row
        } else {
            text.offset_to_point(selection.range.start).row
                ..=text.offset_to_point(selection.range.end).row
        };
        planned_rows = planned_rows.checked_add(rows.clone().count())?;
        if planned_rows > MAX_REORDER_LINES {
            return None;
        }
        let range_start = text.line_start_offset(*rows.start());
        let range_end = text.line_start_offset(*rows.end()) + line_content(text, *rows.end()).len();
        let mut seen = HashSet::with_capacity(rows.clone().count());
        let mut retained_rows = Vec::new();
        for row in rows {
            let content = line_content(text, row);
            if seen.insert(content) {
                retained_rows.push(row);
            }
        }

        let mut replacement = String::new();
        let retained_count = retained_rows.len();
        for (index, row) in retained_rows.into_iter().enumerate() {
            replacement.push_str(&line_content(text, row));
            if index + 1 < retained_count {
                replacement.push_str(line_ending);
            }
        }
        blocks.push(DeduplicateBlock {
            selection_index,
            edit: (range_start..range_end, replacement),
        });
    }

    blocks.sort_by_key(|block| (block.edit.0.start, block.edit.0.end));
    if blocks
        .windows(2)
        .any(|pair| pair[0].edit.0.end > pair[1].edit.0.start)
    {
        return None;
    }
    let edits = blocks
        .iter()
        .map(|block| block.edit.clone())
        .collect::<Vec<_>>();

    let selections_after = if expand_to_document {
        let block = blocks.first()?;
        let map = |offset| {
            if offset < block.edit.0.start || offset > block.edit.0.end {
                return transform_offset_through_edits(offset, &edits);
            }
            let mut relative = offset
                .saturating_sub(block.edit.0.start)
                .min(block.edit.1.len());
            while relative > 0 && !block.edit.1.is_char_boundary(relative) {
                relative -= 1;
            }
            block.edit.0.start + relative
        };
        let selection = selections[0];
        vec![EditorSelection::from_anchor_and_head(
            map(selection.anchor()),
            map(selection.head()),
        )]
    } else {
        let mut selections_after = vec![None; selections.len()];
        let mut delta = 0isize;
        for block in &blocks {
            let post_edit_start = block.edit.0.start.saturating_add_signed(delta);
            selections_after[block.selection_index] = Some(EditorSelection::from_anchor_and_head(
                post_edit_start,
                post_edit_start + block.edit.1.len(),
            ));
            delta += block.edit.1.len() as isize - block.edit.0.len() as isize;
        }
        selections_after.into_iter().collect::<Option<Vec<_>>>()?
    };

    Some(LineOperationPlan {
        edits,
        selections_after,
    })
}

fn plan_join_lines(text: &Rope, selections: &[EditorSelection]) -> Option<LineOperationPlan> {
    if selections.is_empty() || text.lines_len() == 0 {
        return None;
    }

    let mut row_blocks = selections
        .iter()
        .copied()
        .map(|selection| join_rows_for_selection(text, selection))
        .collect::<Option<Vec<_>>>()?;
    row_blocks.sort_by_key(|rows| (*rows.start(), *rows.end()));
    let mut merged = Vec::<RangeInclusive<usize>>::new();
    for rows in row_blocks {
        if let Some(previous) = merged.last_mut()
            && *rows.start() <= *previous.end()
        {
            let start = *previous.start();
            let end = (*previous.end()).max(*rows.end());
            *previous = start..=end;
        } else {
            merged.push(rows);
        }
    }
    let joined_line_count = merged.iter().try_fold(0usize, |count, rows| {
        count.checked_add(rows.clone().count())
    })?;
    if joined_line_count > MAX_JOIN_LINES {
        return None;
    }

    let mut rewrites = merged
        .into_iter()
        .map(|rows| build_join_rewrite(text, rows))
        .collect::<Vec<_>>();
    let mut delta = 0isize;
    for rewrite in &mut rewrites {
        rewrite.post_edit_start = rewrite.edit.0.start.saturating_add_signed(delta);
        delta += rewrite.edit.1.len() as isize - rewrite.edit.0.len() as isize;
    }
    let edits = rewrites
        .iter()
        .map(|rewrite| rewrite.edit.clone())
        .collect::<Vec<_>>();
    let selections_after = selections
        .iter()
        .copied()
        .map(|selection| {
            let rows = join_rows_for_selection(text, selection)?;
            let rewrite = rewrites.iter().find(|rewrite| {
                rewrite.rows.contains(rows.start()) && rewrite.rows.contains(rows.end())
            })?;
            if selection.is_empty() {
                let row = text.offset_to_point(selection.head()).row;
                let relative = rewrite
                    .boundary_after_row
                    .get(&row)
                    .copied()
                    .unwrap_or(rewrite.edit.1.len());
                return Some(EditorSelection::caret(rewrite.post_edit_start + relative));
            }
            let anchor = map_join_offset(text, selection.anchor(), rewrite)?;
            let head = map_join_offset(text, selection.head(), rewrite)?;
            Some(EditorSelection::from_anchor_and_head(anchor, head))
        })
        .collect::<Option<Vec<_>>>()?;

    Some(LineOperationPlan {
        edits,
        selections_after,
    })
}

fn join_rows_for_selection(
    text: &Rope,
    selection: EditorSelection,
) -> Option<RangeInclusive<usize>> {
    if selection.range.start > selection.range.end
        || selection.range.end > text.len()
        || !text.is_char_boundary(selection.range.start)
        || !text.is_char_boundary(selection.range.end)
    {
        return None;
    }
    let start_row = text.offset_to_point(selection.range.start).row;
    let selection_end_row = text.offset_to_point(selection.range.end).row;
    let end_row = if selection.is_empty() || start_row == selection_end_row {
        start_row
            .saturating_add(1)
            .min(text.lines_len().saturating_sub(1))
    } else {
        selection_end_row
    };
    Some(start_row..=end_row)
}

fn build_join_rewrite(text: &Rope, rows: RangeInclusive<usize>) -> JoinRewrite {
    let start_row = *rows.start();
    let end_row = *rows.end();
    let range_start = text.line_start_offset(start_row);
    let range_end = text.line_start_offset(end_row) + line_content(text, end_row).len();
    let first_line = line_content(text, start_row);
    let mut replacement = first_line.clone();
    let mut row_maps = BTreeMap::new();
    row_maps.insert(
        start_row,
        JoinRowMap {
            replacement_start: 0,
            indentation_len: 0,
            mapped_content_len: first_line.len(),
        },
    );
    let mut boundary_after_row = BTreeMap::new();
    let mut last_content_row = start_row;

    for row in start_row.saturating_add(1)..=end_row {
        let content = line_content(text, row);
        let Some(indentation_len) = first_join_content_offset(&content) else {
            boundary_after_row.insert(row - 1, replacement.len());
            row_maps.insert(
                row,
                JoinRowMap {
                    replacement_start: replacement.len(),
                    indentation_len: content.len(),
                    mapped_content_len: 0,
                },
            );
            continue;
        };

        let boundary = if replacement.is_empty() {
            0
        } else if replacement.ends_with(' ') || replacement.ends_with('\t') {
            trim_join_trailing_whitespace(&mut replacement);
            replacement.push(' ');
            if let Some(previous_map) = row_maps.get_mut(&last_content_row) {
                previous_map.mapped_content_len = replacement
                    .len()
                    .saturating_sub(previous_map.replacement_start);
            }
            replacement.len()
        } else {
            let boundary = replacement.len();
            replacement.push(' ');
            boundary
        };
        boundary_after_row.insert(row - 1, boundary);

        let replacement_start = replacement.len();
        let content_without_indentation = &content[indentation_len..];
        replacement.push_str(content_without_indentation);
        row_maps.insert(
            row,
            JoinRowMap {
                replacement_start,
                indentation_len,
                mapped_content_len: content_without_indentation.len(),
            },
        );
        last_content_row = row;
    }
    boundary_after_row.insert(end_row, replacement.len());

    JoinRewrite {
        rows,
        edit: (range_start..range_end, replacement),
        row_maps,
        boundary_after_row,
        post_edit_start: 0,
    }
}

fn first_join_content_offset(content: &str) -> Option<usize> {
    content
        .char_indices()
        .find_map(|(offset, character)| (!is_join_whitespace(character)).then_some(offset))
}

fn trim_join_trailing_whitespace(text: &mut String) {
    while let Some(character) = text.chars().next_back() {
        if !is_join_whitespace(character) {
            break;
        }
        text.truncate(text.len() - character.len_utf8());
    }
}

fn is_join_whitespace(character: char) -> bool {
    character.is_whitespace() || matches!(character, '\u{feff}' | '\u{00a0}')
}

fn map_join_offset(text: &Rope, offset: usize, rewrite: &JoinRewrite) -> Option<usize> {
    let point = text.offset_to_point(offset);
    let row_map = rewrite.row_maps.get(&point.row)?;
    let content_len = line_content(text, point.row).len();
    let column = point.column.min(content_len);
    let mapped_column = column
        .saturating_sub(row_map.indentation_len)
        .min(row_map.mapped_content_len);
    Some(rewrite.post_edit_start + row_map.replacement_start + mapped_column)
}

fn plan_insert_lines(
    text: &Rope,
    selections: &[EditorSelection],
    direction: InsertLineDirection,
) -> Option<LineOperationPlan> {
    if selections.is_empty() || text.lines_len() == 0 {
        return None;
    }
    let line_ending = document_line_ending(text);
    let mut row_insertions = BTreeMap::<usize, (usize, String, usize)>::new();
    for selection in selections {
        let row = text.offset_to_point(selection.head()).row;
        let indent = leading_indentation(text, row);
        let (offset, replacement, caret_within) = match direction {
            InsertLineDirection::Above => {
                let offset = text.line_start_offset(row);
                (offset, format!("{indent}{line_ending}"), indent.len())
            }
            InsertLineDirection::Below if row + 1 < text.lines_len() => {
                let offset = text.line_start_offset(row + 1);
                (offset, format!("{indent}{line_ending}"), indent.len())
            }
            InsertLineDirection::Below => {
                let offset = text.len();
                (
                    offset,
                    format!("{line_ending}{indent}"),
                    line_ending.len() + indent.len(),
                )
            }
        };
        row_insertions
            .entry(row)
            .or_insert((offset, replacement, caret_within));
    }

    let mut ordered = row_insertions
        .iter()
        .map(|(row, (offset, replacement, caret_within))| {
            (*row, *offset, replacement.clone(), *caret_within)
        })
        .collect::<Vec<_>>();
    ordered.sort_by_key(|(_, offset, _, _)| *offset);
    let mut caret_by_row = BTreeMap::new();
    let mut delta = 0usize;
    let mut edits = Vec::with_capacity(ordered.len());
    for (row, offset, replacement, caret_within) in ordered {
        caret_by_row.insert(row, offset + delta + caret_within);
        delta += replacement.len();
        edits.push((offset..offset, replacement));
    }
    let selections_after = selections
        .iter()
        .filter_map(|selection| {
            let row = text.offset_to_point(selection.head()).row;
            caret_by_row.get(&row).copied().map(EditorSelection::caret)
        })
        .collect::<Vec<_>>();
    Some(LineOperationPlan {
        edits,
        selections_after,
    })
}

fn preceding_line_ending_start(text: &Rope, line_start: usize) -> usize {
    if line_start >= 2 && text.slice(line_start - 2..line_start).to_string() == "\r\n" {
        line_start - 2
    } else {
        line_start.saturating_sub(1)
    }
}

fn document_line_ending(text: &Rope) -> &'static str {
    for row in 0..text.lines_len().saturating_sub(1) {
        if text.slice_line(row).to_string().ends_with('\r') {
            return "\r\n";
        }
        return "\n";
    }
    "\n"
}

fn line_content(text: &Rope, row: usize) -> String {
    let mut content = text.slice_line(row).to_string();
    if content.ends_with('\r') {
        content.pop();
    }
    content
}

fn leading_indentation(text: &Rope, row: usize) -> String {
    text.slice_line(row)
        .chars()
        .take_while(|character| matches!(character, ' ' | '\t'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collator(locale: &str) -> Collator {
        let locale: Locale = locale.parse().expect("test locale must be valid");
        Collator::try_new(&locale.into(), CollatorOptions::new())
            .expect("test locale must have compiled collation data")
    }

    fn apply_plan(source: &str, plan: &LineOperationPlan) -> String {
        let mut result = source.to_string();
        for (range, replacement) in plan.edits.iter().rev() {
            result.replace_range(range.clone(), replacement);
        }
        result
    }

    #[test]
    fn delete_lines_merges_adjacent_multi_selections_and_preserves_crlf() {
        let source = "one\r\ntwo\r\nthree\r\nfour";
        let two = source.find("two").unwrap();
        let three = source.find("three").unwrap();
        let selections = vec![EditorSelection::caret(two), EditorSelection::caret(three)];
        let plan = plan_delete_lines(&Rope::from(source), &selections).unwrap();

        assert_eq!(apply_plan(source, &plan), "one\r\nfour");
        assert_eq!(plan.edits.len(), 1);
        assert_eq!(plan.selections_after[0], EditorSelection::caret(two));
        assert_eq!(plan.selections_after[1], EditorSelection::caret(two));
    }

    #[test]
    fn deleting_the_final_line_removes_its_preceding_line_ending_only() {
        let source = "one\r\ntwo";
        let two = source.find("two").unwrap();
        let plan = plan_delete_lines(&Rope::from(source), &[EditorSelection::caret(two)]).unwrap();

        assert_eq!(apply_plan(source, &plan), "one");
        assert_eq!(plan.selections_after, vec![EditorSelection::caret(3)]);
    }

    #[test]
    fn insert_above_and_below_deduplicate_lines_and_keep_indentation() {
        let source = "fn main() {\r\n    value();\r\n}";
        let value = source.find("value").unwrap();
        let selections = vec![
            EditorSelection::caret(value),
            EditorSelection::caret(value + 2),
        ];
        let above = plan_insert_lines(&Rope::from(source), &selections, InsertLineDirection::Above)
            .unwrap();
        assert_eq!(
            apply_plan(source, &above),
            "fn main() {\r\n    \r\n    value();\r\n}"
        );
        assert_eq!(above.edits.len(), 1);
        assert_eq!(above.selections_after[0], above.selections_after[1]);

        let below = plan_insert_lines(
            &Rope::from(source),
            &[EditorSelection::caret(value)],
            InsertLineDirection::Below,
        )
        .unwrap();
        assert_eq!(
            apply_plan(source, &below),
            "fn main() {\r\n    value();\r\n    \r\n}"
        );
    }

    #[test]
    fn insert_below_a_final_line_without_newline_adds_one_document_eol() {
        let source = "    value";
        let plan = plan_insert_lines(
            &Rope::from(source),
            &[EditorSelection::caret(source.len())],
            InsertLineDirection::Below,
        )
        .unwrap();

        assert_eq!(apply_plan(source, &plan), "    value\n    ");
        assert_eq!(
            plan.selections_after,
            vec![EditorSelection::caret(source.len() + 5)]
        );
    }

    #[test]
    fn move_line_up_and_down_preserves_crlf_content_columns_and_final_eol_state() {
        let source = "one\r\n  two\r\nthree";
        let two = source.find("two").unwrap();
        let selection = EditorSelection::caret(two);

        let up = plan_move_lines(&Rope::from(source), &[selection], LineDirection::Up).unwrap();
        assert_eq!(apply_plan(source, &up), "  two\r\none\r\nthree");
        assert_eq!(up.selections_after, vec![EditorSelection::caret(2)]);

        let down = plan_move_lines(&Rope::from(source), &[selection], LineDirection::Down).unwrap();
        assert_eq!(apply_plan(source, &down), "one\r\nthree\r\n  two");
        assert_eq!(
            down.selections_after,
            vec![EditorSelection::caret("one\r\nthree\r\n  ".len())]
        );
    }

    #[test]
    fn move_preserves_a_full_line_selection_ending_at_the_next_line_start() {
        let source = "alpha\nbeta\ngamma\n";
        let beta = source.find("beta").unwrap();
        let gamma = source.find("gamma").unwrap();
        let selection = EditorSelection::from_anchor_and_head(beta, gamma);
        let plan = plan_move_lines(&Rope::from(source), &[selection], LineDirection::Up).unwrap();

        assert_eq!(apply_plan(source, &plan), "beta\nalpha\ngamma\n");
        assert_eq!(
            plan.selections_after,
            vec![EditorSelection::from_anchor_and_head(0, "beta\n".len())]
        );
    }

    #[test]
    fn copy_line_down_handles_multiple_blocks_and_a_final_line_without_eol() {
        let source = "a\nb\nc";
        let selections = vec![
            EditorSelection::caret(0),
            EditorSelection::caret(source.find('c').unwrap()),
        ];
        let plan = plan_copy_lines(&Rope::from(source), &selections, LineDirection::Down).unwrap();

        assert_eq!(apply_plan(source, &plan), "a\na\nb\nc\nc");
        assert_eq!(
            plan.selections_after,
            vec![EditorSelection::caret(2), EditorSelection::caret(8)]
        );
    }

    #[test]
    fn copy_line_up_duplicates_a_multiline_selection_once() {
        let source = "zero\none\ntwo\nthree";
        let one = source.find("one").unwrap();
        let three = source.find("three").unwrap();
        let selection = EditorSelection::from_anchor_and_head(one, three);
        let plan = plan_copy_lines(&Rope::from(source), &[selection], LineDirection::Up).unwrap();

        assert_eq!(apply_plan(source, &plan), "zero\none\ntwo\none\ntwo\nthree");
        assert_eq!(
            plan.selections_after,
            vec![EditorSelection::from_anchor_and_head(
                one,
                one + "one\ntwo\n".len()
            )]
        );
    }

    #[test]
    fn copy_adjacent_caret_lines_independently() {
        let source = "a\nb";
        let selections = vec![
            EditorSelection::caret(0),
            EditorSelection::caret(source.find('b').unwrap()),
        ];
        let plan = plan_copy_lines(&Rope::from(source), &selections, LineDirection::Down).unwrap();

        assert_eq!(apply_plan(source, &plan), "a\na\nb\nb");
        assert_eq!(
            plan.selections_after,
            vec![EditorSelection::caret(2), EditorSelection::caret(6)]
        );
    }

    #[test]
    fn duplicate_non_empty_selection_selects_the_new_copy_and_preserves_direction() {
        let source = "hello";
        let selection = EditorSelection::from_anchor_and_head(4, 1);
        let plan = plan_duplicate_selections(&Rope::from(source), &[selection]).unwrap();

        assert_eq!(apply_plan(source, &plan), "hellello");
        assert_eq!(
            plan.selections_after,
            vec![EditorSelection::from_anchor_and_head(7, 4)]
        );
    }

    #[test]
    fn duplicate_carets_copy_lines_independently_and_preserve_crlf_columns() {
        let source = "a\r\n  beta";
        let selections = vec![
            EditorSelection::caret(0),
            EditorSelection::caret(source.find("beta").unwrap() + 2),
        ];
        let plan = plan_duplicate_selections(&Rope::from(source), &selections).unwrap();

        assert_eq!(apply_plan(source, &plan), "a\r\na\r\n  beta\r\n  beta");
        assert_eq!(
            plan.selections_after,
            vec![EditorSelection::caret(3), EditorSelection::caret(18)]
        );
    }

    #[test]
    fn reverse_single_line_selection_applies_to_document_and_excludes_trailing_empty_line() {
        let source = "alice\r\nbob\r\ncharlie\r\n";
        let charlie = source.find("charlie").unwrap();
        let selection = EditorSelection::caret(charlie + 2);
        let plan = plan_reverse_lines(&Rope::from(source), &[selection]).unwrap();

        assert_eq!(apply_plan(source, &plan), "charlie\r\nbob\r\nalice\r\n");
        assert_eq!(plan.selections_after, vec![EditorSelection::caret(2)]);
    }

    #[test]
    fn reverse_multiple_partial_selections_preserves_columns_and_direction() {
        let source = "one\ntwo\nthree\nfour\nfive\nsix";
        let first = EditorSelection::from_anchor_and_head(1, source.find("three").unwrap() + 2);
        let four = source.find("four").unwrap();
        let six = source.find("six").unwrap();
        let second = EditorSelection::from_anchor_and_head(six + 2, four + 1);
        let plan = plan_reverse_lines(&Rope::from(source), &[first, second]).unwrap();

        assert_eq!(
            apply_plan(source, &plan),
            "three\ntwo\none\nsix\nfive\nfour"
        );
        assert_eq!(
            plan.selections_after,
            vec![
                EditorSelection::from_anchor_and_head("three\ntwo\n".len() + 1, 2,),
                EditorSelection::from_anchor_and_head(
                    "three\ntwo\none\n".len() + 2,
                    "three\ntwo\none\nsix\nfive\n".len() + 1,
                ),
            ]
        );
    }

    #[test]
    fn sort_lines_uses_locale_collation_and_preserves_crlf() {
        let source = "pollo\r\npolvo\r\ncasa";
        let selection = EditorSelection::caret(source.find("pollo").unwrap() + 2);
        let plan = plan_sort_lines(
            &Rope::from(source),
            &[selection],
            &collator("es-u-co-trad"),
            false,
        )
        .unwrap();

        // Traditional Spanish orders "ll" after "lv"; a bytewise sort
        // would incorrectly leave pollo before polvo.
        assert_eq!(apply_plan(source, &plan), "casa\r\npolvo\r\npollo");
        assert_eq!(plan.selections_after, vec![selection]);
    }

    #[test]
    fn sort_lines_tracks_selection_offsets_like_monaco() {
        let source = "first\nsecond line\nthird line\nfourth line\nfifth";
        let third = source.find("third line").unwrap();
        let fourth = source.find("fourth line").unwrap();
        let selection = EditorSelection::from_anchor_and_head(third + 2, fourth + 1);
        let plan =
            plan_sort_lines(&Rope::from(source), &[selection], &collator("en-US"), false).unwrap();

        assert_eq!(
            apply_plan(source, &plan),
            "first\nsecond line\nfourth line\nthird line\nfifth"
        );
        assert_eq!(plan.selections_after, vec![selection]);
    }

    #[test]
    fn sort_lines_handles_disjoint_selections_and_excludes_a_next_line_start() {
        let source = "b\na\nmiddle\nd\nc";
        let middle = source.find("middle").unwrap();
        let d = source.find("\nd\n").unwrap() + 1;
        let selections = vec![
            EditorSelection::from_anchor_and_head(0, middle),
            EditorSelection::from_anchor_and_head(d, source.len()),
        ];
        let plan =
            plan_sort_lines(&Rope::from(source), &selections, &collator("en-US"), false).unwrap();

        assert_eq!(apply_plan(source, &plan), "a\nb\nmiddle\nc\nd");
        assert_eq!(plan.selections_after, selections);

        let mut selection_with_single_line = vec![EditorSelection::caret(middle + 1)];
        selection_with_single_line.extend(plan.selections_after);
        assert!(
            plan_sort_lines(
                &Rope::from(source),
                &selection_with_single_line,
                &collator("en-US"),
                false,
            )
            .is_none(),
            "one unsortable multi-cursor selection must cancel the whole action"
        );
    }

    #[test]
    fn sort_lines_descending_reverses_the_collated_order() {
        let source = "alpha\nbeta\ngamma";
        let selection = EditorSelection::caret(0);
        let plan =
            plan_sort_lines(&Rope::from(source), &[selection], &collator("en-US"), true).unwrap();

        assert_eq!(apply_plan(source, &plan), "gamma\nbeta\nalpha");
        assert_eq!(plan.selections_after, vec![selection]);
    }

    #[test]
    fn delete_duplicate_lines_keeps_first_occurrences_in_an_explicit_selection() {
        let source = "alpha\nbeta\nbeta\nbeta\nalpha\nomicron";
        let omicron = source.find("omicron").unwrap();
        let selection = EditorSelection::from_anchor_and_head(2, omicron + 3);
        let plan = plan_delete_duplicate_lines(&Rope::from(source), &[selection]).unwrap();
        let expected = "alpha\nbeta\nomicron";

        assert_eq!(apply_plan(source, &plan), expected);
        assert_eq!(
            plan.selections_after,
            vec![EditorSelection::from_anchor_and_head(0, expected.len())]
        );
    }

    #[test]
    fn delete_duplicate_lines_applies_to_the_document_for_one_single_line_selection() {
        let source = "alpha\nbeta\nalpha\nomicron";
        let beta = source.find("beta").unwrap();
        let selection = EditorSelection::from_anchor_and_head(beta, beta + 1);
        let plan = plan_delete_duplicate_lines(&Rope::from(source), &[selection]).unwrap();

        assert_eq!(apply_plan(source, &plan), "alpha\nbeta\nomicron");
        assert_eq!(plan.selections_after, vec![selection]);
    }

    #[test]
    fn delete_duplicate_lines_updates_disjoint_multi_selections_after_prior_deletions() {
        let source = "alpha\nbeta\nbeta\nomicron\n\nalpha\nalpha\nbeta";
        let omicron = source.find("omicron").unwrap();
        let second_alpha = source.match_indices("alpha").nth(1).unwrap().0;
        let final_beta = source.rfind("beta").unwrap();
        let selections = vec![
            EditorSelection::from_anchor_and_head(1, omicron + 2),
            EditorSelection::from_anchor_and_head(second_alpha + 1, final_beta + 2),
        ];
        let plan = plan_delete_duplicate_lines(&Rope::from(source), &selections).unwrap();
        let expected = "alpha\nbeta\nomicron\n\nalpha\nbeta";
        let expected_second_alpha = expected.match_indices("alpha").nth(1).unwrap().0;

        assert_eq!(apply_plan(source, &plan), expected);
        assert_eq!(
            plan.selections_after,
            vec![
                EditorSelection::from_anchor_and_head(0, "alpha\nbeta\nomicron".len()),
                EditorSelection::from_anchor_and_head(
                    expected_second_alpha,
                    expected_second_alpha + "alpha\nbeta".len(),
                ),
            ]
        );
    }

    #[test]
    fn delete_duplicate_lines_preserves_crlf_case_and_a_final_line_ending() {
        let source = "b\r\na\r\nA\r\nb\r\n";
        let selection = EditorSelection::caret(0);
        let plan = plan_delete_duplicate_lines(&Rope::from(source), &[selection]).unwrap();

        assert_eq!(apply_plan(source, &plan), "b\r\na\r\nA\r\n");
        assert_eq!(plan.selections_after, vec![selection]);
    }

    #[test]
    fn line_rewrite_planners_reject_out_of_bounds_selections_before_mapping_them() {
        let source = "a\nb";
        let invalid = EditorSelection::caret(source.len() + 1);

        assert!(plan_reverse_lines(&Rope::from(source), &[invalid]).is_none());
        assert!(
            plan_sort_lines(&Rope::from(source), &[invalid], &collator("en-US"), false,).is_none()
        );
        assert!(plan_delete_duplicate_lines(&Rope::from(source), &[invalid]).is_none());
    }

    #[test]
    fn line_rewrite_planners_bound_explicit_ui_thread_work() {
        let source = "x\n".repeat(MAX_REORDER_LINES + 1);
        let text = Rope::from(source.as_str());
        let selection = EditorSelection::caret(0);

        assert!(plan_reverse_lines(&text, &[selection]).is_none());
        assert!(plan_sort_lines(&text, &[selection], &collator("en-US"), false).is_none());
        assert!(plan_delete_duplicate_lines(&text, &[selection]).is_none());
    }

    #[test]
    fn join_caret_line_with_next_removes_indentation_and_places_caret_at_boundary() {
        let source = "let a = 1;\n    let b = 2;";
        let plan = plan_join_lines(
            &Rope::from(source),
            &[EditorSelection::caret(source.find('a').unwrap())],
        )
        .unwrap();

        assert_eq!(apply_plan(source, &plan), "let a = 1; let b = 2;");
        assert_eq!(
            plan.selections_after,
            vec![EditorSelection::caret("let a = 1;".len())]
        );
    }

    #[test]
    fn join_multiline_selection_normalizes_trailing_space_and_preserves_crlf_input() {
        let source = "alpha  \r\n\tbeta\r\n\r\ngamma";
        let selection = EditorSelection::from_anchor_and_head(1, source.len() - 2);
        let plan = plan_join_lines(&Rope::from(source), &[selection]).unwrap();

        assert_eq!(apply_plan(source, &plan), "alpha beta gamma");
        assert_eq!(plan.edits.len(), 1);
        assert_eq!(plan.selections_after[0].anchor(), 1);
        assert_eq!(plan.selections_after[0].head(), "alpha beta gam".len());
    }

    #[test]
    fn join_non_overlapping_carets_applies_one_undo_plan_and_keeps_both_cursors() {
        let source = "a\n b\nc\n d";
        let selections = vec![
            EditorSelection::caret(0),
            EditorSelection::caret(source.find('c').unwrap()),
        ];
        let plan = plan_join_lines(&Rope::from(source), &selections).unwrap();

        assert_eq!(apply_plan(source, &plan), "a b\nc d");
        assert_eq!(plan.edits.len(), 2);
        assert_eq!(
            plan.selections_after,
            vec![EditorSelection::caret(1), EditorSelection::caret(5)]
        );
    }

    #[test]
    fn join_overlapping_caret_ranges_merge_before_editing() {
        let source = "a\nb\nc";
        let selections = vec![
            EditorSelection::caret(0),
            EditorSelection::caret(source.find('b').unwrap()),
        ];
        let plan = plan_join_lines(&Rope::from(source), &selections).unwrap();

        assert_eq!(apply_plan(source, &plan), "a b c");
        assert_eq!(plan.edits.len(), 1);
        assert_eq!(
            plan.selections_after,
            vec![EditorSelection::caret(1), EditorSelection::caret(3)]
        );
    }

    #[test]
    fn join_on_the_final_line_moves_an_empty_caret_to_line_end_without_text_change() {
        let source = "alpha\nomega";
        let omega = source.find("omega").unwrap();
        let plan =
            plan_join_lines(&Rope::from(source), &[EditorSelection::caret(omega + 1)]).unwrap();

        assert_eq!(apply_plan(source, &plan), source);
        assert_eq!(
            plan.selections_after,
            vec![EditorSelection::caret(source.len())]
        );
    }
}
