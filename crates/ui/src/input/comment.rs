use std::{collections::BTreeMap, ops::Range};

use gpui::{Context, Window};

use crate::input::{
    EditorSelection, InputState, Rope, RopeExt as _, ToggleBlockComment, ToggleLineComment,
};

/// The code editor currently documents support for files up to 50K lines. Keep
/// comment planning bounded above that limit so one action cannot monopolize
/// the UI thread if a host supplies a much larger buffer.
const MAX_LINE_COMMENT_LINES: usize = 100_000;

#[derive(Debug, Clone)]
struct SelectedLine {
    line_start: usize,
    indentation: String,
    first_non_whitespace: Option<usize>,
    starts_with_token: bool,
    token_has_following_space: bool,
}

#[derive(Debug, Clone)]
struct IndexedBufferEdit {
    id: usize,
    range: Range<usize>,
    replacement: String,
}

#[derive(Debug, Clone, Copy)]
enum SelectionMarker {
    BeforeEdit(usize),
    AfterEdit(usize),
    WithinEdit(usize, usize),
    OriginalAfter(usize),
}

#[derive(Debug, Clone, Copy)]
struct PendingSelection {
    start: SelectionMarker,
    end: SelectionMarker,
    reversed: bool,
}

#[derive(Debug, Clone)]
struct BlockCommentPlan {
    edits: Vec<(Range<usize>, String)>,
    selections_after: Vec<EditorSelection>,
}

impl InputState {
    /// Toggle the configured line comment for every editor selection as one
    /// undoable mutation. Returns false when the language has no line-comment
    /// token or the action produces no valid edit.
    pub fn toggle_line_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.mode.is_code_editor() {
            return false;
        }
        let Some(token) = self.language_configuration.line_comment() else {
            return false;
        };
        let edits = plan_toggle_line_comment_edits(
            &self.text,
            &self.selections(),
            token,
            self.mode.tab_size().tab_size,
        );
        self.apply_buffer_edits(edits, window, cx)
    }

    pub(super) fn on_action_toggle_line_comment(
        &mut self,
        _: &ToggleLineComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_line_comment(window, cx);
    }

    /// Toggle the configured block comment for every selection as one
    /// undoable mutation, retaining the selected content or placing an empty
    /// caret between the new delimiters.
    pub fn toggle_block_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.mode.is_code_editor() {
            return false;
        }
        let Some(pair) = self.language_configuration.block_comment() else {
            return false;
        };
        let open = pair.open.clone();
        let close = pair.close.clone();
        let Some(plan) =
            plan_toggle_block_comment_edits(&self.text, &self.selections(), &open, &close)
        else {
            return false;
        };
        self.apply_buffer_edits_with_selections(plan.edits, plan.selections_after, window, cx)
    }

    pub(super) fn on_action_toggle_block_comment(
        &mut self,
        _: &ToggleBlockComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_block_comment(window, cx);
    }
}

/// Plan VS Code-style Toggle Line Comment edits in UTF-8 byte ranges.
///
/// Each selection decides independently whether to add or remove comments.
/// When selections share a line, the earlier editor selection owns it so the
/// token is never inserted or removed twice.
fn plan_toggle_line_comment_edits(
    text: &Rope,
    selections: &[EditorSelection],
    token: &str,
    indent_size: usize,
) -> Vec<(Range<usize>, String)> {
    if token.is_empty() || selections.is_empty() || text.lines_len() == 0 {
        return Vec::new();
    }

    let mut line_owners = BTreeMap::<usize, usize>::new();
    for (selection_index, selection) in selections.iter().enumerate() {
        let Some(lines) = selection_line_range(text, *selection) else {
            continue;
        };
        for row in lines {
            line_owners.entry(row).or_insert(selection_index);
            if line_owners.len() > MAX_LINE_COMMENT_LINES {
                return Vec::new();
            }
        }
    }

    let mut owned_lines = vec![Vec::new(); selections.len()];
    for (row, selection_index) in line_owners {
        owned_lines[selection_index].push(row);
    }

    let indent_size = indent_size.max(1);
    let mut edits = Vec::new();
    for rows in owned_lines {
        if rows.is_empty() {
            continue;
        }

        let lines = rows
            .into_iter()
            .map(|row| selected_line(text, row, token))
            .collect::<Vec<_>>();
        let only_whitespace = lines.iter().all(|line| line.first_non_whitespace.is_none());
        let should_remove = !only_whitespace
            && lines
                .iter()
                .all(|line| line.first_non_whitespace.is_none() || line.starts_with_token);

        if should_remove {
            for line in &lines {
                let Some(offset) = line.first_non_whitespace else {
                    continue;
                };
                let remove_end = offset + token.len() + usize::from(line.token_has_following_space);
                edits.push((
                    line.line_start + offset..line.line_start + remove_end,
                    String::new(),
                ));
            }
            continue;
        }

        let relevant = lines
            .iter()
            .filter(|line| only_whitespace || line.first_non_whitespace.is_some())
            .collect::<Vec<_>>();
        let minimum_visible_column = relevant
            .iter()
            .map(|line| visible_column(&line.indentation, indent_size))
            .min()
            .unwrap_or(0);
        let comment = format!("{token} ");

        for line in relevant {
            let insertion_offset = byte_offset_at_visible_column(
                &line.indentation,
                minimum_visible_column,
                indent_size,
            );
            let offset = line.line_start + insertion_offset;
            edits.push((offset..offset, comment.clone()));
        }
    }

    edits
}

pub(super) fn selection_line_range(
    text: &Rope,
    selection: EditorSelection,
) -> Option<std::ops::RangeInclusive<usize>> {
    if selection.range.start > text.len() || selection.range.end > text.len() {
        return None;
    }
    let start_row = text.offset_to_point(selection.range.start).row;
    let mut end_row = text.offset_to_point(selection.range.end).row;
    if !selection.is_empty()
        && end_row > start_row
        && selection.range.end == text.line_start_offset(end_row)
    {
        end_row -= 1;
    }
    Some(start_row..=end_row)
}

fn selected_line(text: &Rope, row: usize, token: &str) -> SelectedLine {
    let content = line_content(text, row);
    let first_non_whitespace = content
        .bytes()
        .position(|byte| byte != b' ' && byte != b'\t');
    let indentation_end = first_non_whitespace.unwrap_or(content.len());
    let starts_with_token =
        first_non_whitespace.is_some_and(|offset| content[offset..].starts_with(token));
    let token_has_following_space =
        starts_with_token && content.as_bytes().get(indentation_end + token.len()) == Some(&b' ');
    SelectedLine {
        line_start: text.line_start_offset(row),
        indentation: content[..indentation_end].to_owned(),
        first_non_whitespace,
        starts_with_token,
        token_has_following_space,
    }
}

fn line_content(text: &Rope, row: usize) -> String {
    let mut content = text.slice_line(row).to_string();
    if content.ends_with('\r') {
        content.pop();
    }
    content
}

fn visible_column(indentation: &str, indent_size: usize) -> usize {
    indentation.bytes().fold(0, |column, byte| {
        if byte == b'\t' {
            column + (indent_size - column % indent_size)
        } else {
            column + 1
        }
    })
}

fn byte_offset_at_visible_column(
    indentation: &str,
    target_column: usize,
    indent_size: usize,
) -> usize {
    let mut column = 0;
    for (offset, byte) in indentation.bytes().enumerate() {
        if column >= target_column {
            return offset;
        }
        let next = if byte == b'\t' {
            column + (indent_size - column % indent_size)
        } else {
            column + 1
        };
        if next > target_column {
            return offset;
        }
        column = next;
    }
    indentation.len()
}

fn plan_toggle_block_comment_edits(
    text: &Rope,
    selections: &[EditorSelection],
    open: &str,
    close: &str,
) -> Option<BlockCommentPlan> {
    if open.is_empty() || close.is_empty() || selections.is_empty() {
        return None;
    }

    let mut edits = Vec::<IndexedBufferEdit>::with_capacity(selections.len() * 2);
    let mut pending_selections = Vec::with_capacity(selections.len());
    for selection in selections {
        if selection.range.start > selection.range.end
            || selection.range.end > text.len()
            || !text.is_char_boundary(selection.range.start)
            || !text.is_char_boundary(selection.range.end)
        {
            return None;
        }

        if let Some(removal_ranges) = enclosing_block_comment_ranges(
            text,
            selection.range.start..selection.range.end,
            open,
            close,
        ) {
            for range in removal_ranges {
                push_indexed_edit(&mut edits, range, String::new());
            }
            pending_selections.push(PendingSelection {
                start: SelectionMarker::OriginalAfter(selection.range.start),
                end: SelectionMarker::OriginalAfter(selection.range.end),
                reversed: selection.reversed,
            });
        } else if selection.is_empty() {
            let replacement = format!("{open}  {close}");
            let edit_id = push_indexed_edit(
                &mut edits,
                selection.range.start..selection.range.end,
                replacement,
            );
            let caret = SelectionMarker::WithinEdit(edit_id, open.len() + 1);
            pending_selections.push(PendingSelection {
                start: caret,
                end: caret,
                reversed: false,
            });
        } else {
            let open_id = push_indexed_edit(
                &mut edits,
                selection.range.start..selection.range.start,
                format!("{open} "),
            );
            let close_id = push_indexed_edit(
                &mut edits,
                selection.range.end..selection.range.end,
                format!(" {close}"),
            );
            pending_selections.push(PendingSelection {
                start: SelectionMarker::AfterEdit(open_id),
                end: SelectionMarker::BeforeEdit(close_id),
                reversed: selection.reversed,
            });
        }
    }

    edits.sort_by_key(|edit| (edit.range.start, edit.range.end));
    if edits
        .windows(2)
        .any(|pair| pair[0].range.end > pair[1].range.start)
    {
        return None;
    }

    let resolved_spans = resolve_edit_spans(&edits);
    let selections_after = pending_selections
        .into_iter()
        .map(|selection| {
            let start = resolve_selection_marker(selection.start, &edits, &resolved_spans);
            let end = resolve_selection_marker(selection.end, &edits, &resolved_spans);
            EditorSelection {
                range: crate::input::Selection::new(start.min(end), start.max(end)),
                reversed: selection.reversed,
            }
        })
        .collect::<Vec<_>>();
    Some(BlockCommentPlan {
        edits: edits
            .into_iter()
            .map(|edit| (edit.range, edit.replacement))
            .collect(),
        selections_after,
    })
}

fn push_indexed_edit(
    edits: &mut Vec<IndexedBufferEdit>,
    range: Range<usize>,
    replacement: String,
) -> usize {
    let id = edits.len();
    edits.push(IndexedBufferEdit {
        id,
        range,
        replacement,
    });
    id
}

fn enclosing_block_comment_ranges(
    text: &Rope,
    selection: Range<usize>,
    open: &str,
    close: &str,
) -> Option<Vec<Range<usize>>> {
    let start_row = text.offset_to_point(selection.start).row;
    let end_row = text.offset_to_point(selection.end).row;
    let start_line_start = text.line_start_offset(start_row);
    let end_line_start = text.line_start_offset(end_row);
    let start_content = line_content(text, start_row);
    let end_content = if start_row == end_row {
        start_content.clone()
    } else {
        line_content(text, end_row)
    };
    let start_local = selection
        .start
        .saturating_sub(start_line_start)
        .min(start_content.len());
    let end_local = selection
        .end
        .saturating_sub(end_line_start)
        .min(end_content.len());
    let open_limit = start_local.saturating_add(open.len());
    let open_index = start_content
        .match_indices(open)
        .map(|(index, _)| index)
        .take_while(|index| *index <= open_limit)
        .last()?;
    let close_minimum = end_local.saturating_sub(close.len());
    let close_index = end_content
        .match_indices(close)
        .map(|(index, _)| index)
        .find(|index| *index >= close_minimum)?;
    let open_end = open_index + open.len();

    if start_row == end_row {
        if open_end > close_index || start_content[open_end..close_index].contains(close) {
            return None;
        }
    } else if start_content[open_end..].contains(close)
        || end_content[..close_index].contains(close)
    {
        return None;
    }

    let open_remove_end =
        open_end + usize::from(start_content.as_bytes().get(open_end) == Some(&b' '));
    let close_remove_start = close_index
        - usize::from(
            close_index > 0 && end_content.as_bytes().get(close_index - 1) == Some(&b' '),
        );
    let open_range = start_line_start + open_index..start_line_start + open_remove_end;
    let close_range =
        end_line_start + close_remove_start..end_line_start + close_index + close.len();
    if open_range.end > close_range.start {
        Some(vec![open_range.start..close_range.end])
    } else {
        Some(vec![open_range, close_range])
    }
}

fn resolve_edit_spans(edits: &[IndexedBufferEdit]) -> Vec<(usize, usize)> {
    let mut spans = vec![(0, 0); edits.len()];
    let mut delta = 0isize;
    for edit in edits {
        let start = edit.range.start.saturating_add_signed(delta);
        let end = start + edit.replacement.len();
        spans[edit.id] = (start, end);
        delta += edit.replacement.len() as isize - edit.range.len() as isize;
    }
    spans
}

fn resolve_selection_marker(
    marker: SelectionMarker,
    edits: &[IndexedBufferEdit],
    spans: &[(usize, usize)],
) -> usize {
    match marker {
        SelectionMarker::BeforeEdit(id) => spans[id].0,
        SelectionMarker::AfterEdit(id) => spans[id].1,
        SelectionMarker::WithinEdit(id, offset) => {
            spans[id].0 + offset.min(spans[id].1.saturating_sub(spans[id].0))
        }
        SelectionMarker::OriginalAfter(offset) => {
            let mut delta = 0isize;
            for edit in edits {
                if edit.range.is_empty() {
                    if offset >= edit.range.start {
                        delta += edit.replacement.len() as isize;
                    }
                    continue;
                }
                if offset < edit.range.start {
                    break;
                }
                if offset < edit.range.end {
                    return spans[edit.id].1;
                }
                delta += edit.replacement.len() as isize - edit.range.len() as isize;
            }
            offset.saturating_add_signed(delta)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(source: &str, selections: &[EditorSelection], token: &str) -> String {
        let rope = Rope::from(source);
        let mut edits = plan_toggle_line_comment_edits(&rope, selections, token, 4);
        edits.sort_by_key(|(range, _)| (range.start, range.end));
        let mut result = source.to_owned();
        for (range, replacement) in edits.into_iter().rev() {
            result.replace_range(range, &replacement);
        }
        result
    }

    fn apply_block(
        source: &str,
        selections: &[EditorSelection],
        open: &str,
        close: &str,
    ) -> (String, Vec<EditorSelection>) {
        let rope = Rope::from(source);
        let plan = plan_toggle_block_comment_edits(&rope, selections, open, close)
            .expect("block-comment plan");
        let mut result = source.to_owned();
        for (range, replacement) in plan.edits.into_iter().rev() {
            result.replace_range(range, &replacement);
        }
        (result, plan.selections_after)
    }

    #[test]
    fn toggles_a_single_line_at_its_indentation() {
        let selection = EditorSelection::caret(5);
        let commented = apply("    value", &[selection], "//");
        assert_eq!(commented, "    // value");
        assert_eq!(
            apply(&commented, &[EditorSelection::caret(8)], "//"),
            "    value"
        );
    }

    #[test]
    fn excludes_a_selection_end_at_the_next_line_start() {
        let source = "first\nsecond\nthird";
        let selection = EditorSelection::from_anchor_and_head(0, "first\nsecond\n".len());
        assert_eq!(
            apply(source, &[selection], "//"),
            "// first\n// second\nthird"
        );
    }

    #[test]
    fn normalizes_mixed_indentation_and_ignores_blank_lines() {
        let source = "\tfirst\n\t   \n    second";
        let selection = EditorSelection::from_anchor_and_head(0, source.len());
        assert_eq!(
            apply(source, &[selection], "#"),
            "\t# first\n\t   \n    # second"
        );
    }

    #[test]
    fn comments_an_all_whitespace_selection() {
        let source = "\t    \n\t";
        let selection = EditorSelection::from_anchor_and_head(0, source.len());
        assert_eq!(apply(source, &[selection], "#"), "\t#     \n\t# ");
    }

    #[test]
    fn multiple_cursors_toggle_independently() {
        let source = "// first\nsecond";
        let selections = [EditorSelection::caret(3), EditorSelection::caret(12)];
        assert_eq!(apply(source, &selections, "//"), "first\n// second");
    }

    #[test]
    fn shared_lines_are_edited_once() {
        let source = "first\nsecond";
        let selections = [
            EditorSelection::from_anchor_and_head(0, source.len()),
            EditorSelection::caret(8),
        ];
        assert_eq!(apply(source, &selections, "//"), "// first\n// second");
    }

    #[test]
    fn preserves_crlf_line_endings_and_unicode_offsets() {
        let source = "  alpha\r\n  βeta";
        let selection = EditorSelection::from_anchor_and_head(0, source.len());
        assert_eq!(apply(source, &[selection], "//"), "  // alpha\r\n  // βeta");
    }

    #[test]
    fn empty_block_comment_keeps_the_caret_between_delimiters() {
        let (text, selections) = apply_block("first", &[EditorSelection::caret(2)], "/*", "*/");
        assert_eq!(text, "fi/*  */rst");
        assert_eq!(selections, vec![EditorSelection::caret(5)]);
    }

    #[test]
    fn block_comment_preserves_a_reversed_inner_selection() {
        let selected = EditorSelection::from_anchor_and_head(5, 2);
        let (commented, selections) = apply_block("first", &[selected], "/*", "*/");
        assert_eq!(commented, "fi/* rst */");
        assert_eq!(
            selections,
            vec![EditorSelection {
                range: crate::input::Selection::new(5, 8),
                reversed: true,
            }]
        );

        let (uncommented, restored) = apply_block(&commented, &selections, "/*", "*/");
        assert_eq!(uncommented, "first");
        assert_eq!(restored, vec![selected]);
    }

    #[test]
    fn block_comment_wraps_multiline_utf8_without_changing_line_endings() {
        let source = "α\r\nβ";
        let selection = EditorSelection::from_anchor_and_head(0, source.len());
        let (text, selections) = apply_block(source, &[selection], "/*", "*/");
        assert_eq!(text, "/* α\r\nβ */");
        assert_eq!(
            selections,
            vec![EditorSelection::from_anchor_and_head(3, source.len() + 3)]
        );
    }

    #[test]
    fn empty_block_comment_round_trips_a_shared_space_wrapper() {
        let (commented, selections) = apply_block("", &[EditorSelection::caret(0)], "/*", "*/");
        assert_eq!(commented, "/*  */");
        assert_eq!(selections, vec![EditorSelection::caret(3)]);

        let (uncommented, restored) = apply_block(&commented, &selections, "/*", "*/");
        assert_eq!(uncommented, "");
        assert_eq!(restored, vec![EditorSelection::caret(0)]);
    }

    #[test]
    fn multiple_block_comment_carets_resolve_post_edit_offsets() {
        let selections = [EditorSelection::caret(0), EditorSelection::caret(3)];
        let (text, selections) = apply_block("a b", &selections, "/*", "*/");
        assert_eq!(text, "/*  */a b/*  */");
        assert_eq!(
            selections,
            vec![EditorSelection::caret(3), EditorSelection::caret(12)]
        );
    }
}
