use std::{collections::BTreeMap, ops::Range};

use gpui::{
    Bounds, Context, Hsla, Path, PathBuilder, Pixels, SharedString, TextRun, TextStyle, Window,
    point, px,
};
use ropey::RopeSlice;

use crate::{
    RopeExt,
    input::{
        EditorSelection, Indent, IndentInline, InputState, LastLayout, Outdent, OutdentInline,
        Rope, Selection, comment::selection_line_range, element::TextElement, mode::InputMode,
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct IndentPlan {
    edits: Vec<(Range<usize>, String)>,
    selections_after: Vec<EditorSelection>,
}

#[derive(Debug, Copy, Clone)]
pub struct TabSize {
    /// Default is 2
    pub tab_size: usize,
    /// Set true to use `\t` as tab indent, default is false
    pub hard_tabs: bool,
}

impl Default for TabSize {
    fn default() -> Self {
        Self {
            tab_size: 2,
            hard_tabs: false,
        }
    }
}

impl TabSize {
    pub(super) fn to_string(&self) -> SharedString {
        if self.hard_tabs {
            "\t".into()
        } else {
            " ".repeat(self.tab_size).into()
        }
    }

    /// Count the indent size of the line in spaces.
    pub fn indent_count(&self, line: &RopeSlice) -> usize {
        let mut count = 0;
        for ch in line.chars() {
            match ch {
                '\t' => count += self.tab_size,
                ' ' => count += 1,
                _ => break,
            }
        }

        count
    }
}

impl InputMode {
    #[inline]
    pub(super) fn is_indentable(&self) -> bool {
        match self {
            InputMode::PlainText { multi_line, .. } | InputMode::CodeEditor { multi_line, .. } => {
                *multi_line
            }
            _ => false,
        }
    }

    #[inline]
    pub(super) fn has_indent_guides(&self) -> bool {
        match self {
            InputMode::CodeEditor {
                indent_guides,
                multi_line,
                ..
            } => *indent_guides && *multi_line,
            _ => false,
        }
    }

    #[inline]
    pub(super) fn tab_size(&self) -> TabSize {
        match self {
            InputMode::PlainText { tab, .. } => *tab,
            InputMode::CodeEditor { tab, .. } => *tab,
            _ => TabSize::default(),
        }
    }
}

impl TextElement {
    /// Measure the indent width in pixels for given column count.
    fn measure_indent_width(&self, style: &TextStyle, column: usize, window: &Window) -> Pixels {
        let font_size = style.font_size.to_pixels(window.rem_size());
        let layout = window.text_system().shape_line(
            SharedString::from(" ".repeat(column)),
            font_size,
            &[TextRun {
                len: column,
                font: style.font(),
                color: Hsla::default(),
                background_color: None,
                strikethrough: None,
                underline: None,
            }],
            None,
        );

        layout.width
    }

    pub(super) fn layout_indent_guides(
        &self,
        state: &InputState,
        bounds: &Bounds<Pixels>,
        last_layout: &LastLayout,
        text_style: &TextStyle,
        window: &mut Window,
    ) -> Option<Path<Pixels>> {
        if !state.mode.has_indent_guides() {
            return None;
        }

        let indent_width =
            self.measure_indent_width(text_style, state.mode.tab_size().tab_size, window);

        let tab_size = state.mode.tab_size();
        let line_height = last_layout.line_height;
        let mut builder = PathBuilder::stroke(px(1.));
        let mut offset_y = last_layout.visible_top;
        let mut last_indents = vec![];

        for (&buffer_line, line_layout) in last_layout
            .visible_buffer_lines
            .iter()
            .zip(last_layout.lines.iter())
        {
            offset_y += last_layout.code_lens_height_before(buffer_line);
            let line = state.text.slice_line(buffer_line);
            let mut current_indents = vec![];
            if line.len() > 0 {
                let indent_count = tab_size.indent_count(&line);
                for offset in (0..indent_count).step_by(tab_size.tab_size) {
                    let x = if indent_count > 0 {
                        indent_width * offset as f32 / tab_size.tab_size as f32
                    } else {
                        px(0.)
                    };

                    let pos = point(x + last_layout.line_number_width, offset_y);

                    builder.move_to(pos);
                    builder.line_to(point(pos.x, pos.y + line_height));
                    current_indents.push(pos.x);
                }
            } else if last_indents.len() > 0 {
                for x in &last_indents {
                    let pos = point(*x, offset_y);
                    builder.move_to(pos);
                    builder.line_to(point(pos.x, pos.y + line_height));
                }
                current_indents = last_indents.clone();
            }

            offset_y += line_layout.wrapped_lines.len() * line_height;
            last_indents = current_indents;
        }

        builder.translate(bounds.origin);
        let path = builder.build().unwrap();
        Some(path)
    }
}

impl InputState {
    /// Set whether to show indent guides in code editor mode, default is true.
    ///
    /// Only for [`InputMode::CodeEditor`] mode.
    pub fn indent_guides(mut self, indent_guides: bool) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            indent_guides: l, ..
        } = &mut self.mode
        {
            *l = indent_guides;
        }
        self
    }

    /// Set indent guides in code editor mode.
    ///
    /// Only for [`InputMode::CodeEditor`] mode.
    pub fn set_indent_guides(
        &mut self,
        indent_guides: bool,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        debug_assert!(self.mode.is_code_editor());
        if let InputMode::CodeEditor {
            indent_guides: l, ..
        } = &mut self.mode
        {
            *l = indent_guides;
        }
        cx.notify();
    }

    /// Set the tab size for the input.
    ///
    /// Only for [`InputMode::PlainText`] and [`InputMode::CodeEditor`] mode with multi_line.
    pub fn tab_size(mut self, tab: TabSize) -> Self {
        debug_assert!(self.mode.is_multi_line() || self.mode.is_code_editor());
        update_tab_size(&mut self.mode, tab);
        self
    }

    /// Update the tab size after the input has been created.
    ///
    /// Only for [`InputMode::PlainText`] and [`InputMode::CodeEditor`] modes
    /// with multiple lines. This keeps indentation commands and rendering on
    /// the same live setting when an editor changes document preferences.
    pub fn set_tab_size(&mut self, tab: TabSize, _: &mut Window, cx: &mut Context<Self>) {
        debug_assert!(self.mode.is_multi_line() || self.mode.is_code_editor());
        if !update_tab_size(&mut self.mode, tab) {
            return;
        }
        cx.notify();
    }

    pub(super) fn indent_inline(
        &mut self,
        action: &IndentInline,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.handle_action_for_context_menu(Box::new(action.clone()), window, cx) {
            return;
        }
        if self.move_to_next_snippet_tabstop(window, cx) {
            return;
        }
        // First, try to accept inline completion if present
        if self.accept_inline_completion(window, cx) {
            return;
        }
        self.indent(false, window, cx);
    }

    pub(super) fn indent_block(&mut self, _: &Indent, window: &mut Window, cx: &mut Context<Self>) {
        self.indent(true, window, cx);
    }

    pub(super) fn outdent_inline(
        &mut self,
        _: &OutdentInline,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.move_to_previous_snippet_tabstop(window, cx) {
            return;
        }
        self.outdent(false, window, cx);
    }

    pub(super) fn outdent_block(
        &mut self,
        _: &Outdent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.outdent(true, window, cx);
    }

    pub(super) fn indent(&mut self, block: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.mode.is_indentable() {
            cx.propagate();
            return;
        }
        let Some(plan) = plan_indent(&self.text, &self.selections(), self.mode.tab_size(), block)
        else {
            return;
        };
        self.apply_buffer_edits_with_selections(plan.edits, plan.selections_after, window, cx);
    }

    pub(super) fn outdent(&mut self, block: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.mode.is_indentable() {
            cx.propagate();
            return;
        }
        let Some(plan) = plan_outdent(&self.text, &self.selections(), self.mode.tab_size(), block)
        else {
            return;
        };
        self.apply_buffer_edits_with_selections(plan.edits, plan.selections_after, window, cx);
    }
}

fn plan_indent(
    text: &Rope,
    selections: &[EditorSelection],
    tab: TabSize,
    block: bool,
) -> Option<IndentPlan> {
    if selections.is_empty() || text.lines_len() == 0 {
        return None;
    }
    let mut line_edits = BTreeMap::<usize, bool>::new();
    let mut inline_offsets = Vec::new();
    for selection in selections {
        if block || !selection.is_empty() {
            let rows = selection_line_range(text, *selection)?;
            let single_line = rows.start() == rows.end();
            for row in rows {
                line_edits
                    .entry(row)
                    .and_modify(|indent_empty| *indent_empty |= single_line)
                    .or_insert(single_line);
            }
        } else {
            inline_offsets.push(selection.head());
        }
    }

    let mut edits = BTreeMap::<(usize, usize), String>::new();
    let indent = tab.to_string().to_string();
    for (row, indent_empty) in line_edits {
        let content = line_content(text, row);
        if content.is_empty() && !indent_empty {
            continue;
        }
        let offset = text.line_start_offset(row);
        edits.insert((offset, offset), indent.clone());
    }
    for offset in inline_offsets {
        if offset > text.len() || !text.is_char_boundary(offset) {
            return None;
        }
        let insertion = inline_tab_text(text, offset, tab);
        edits.entry((offset, offset)).or_insert(insertion);
    }
    finish_indent_plan(selections, edits)
}

fn plan_outdent(
    text: &Rope,
    selections: &[EditorSelection],
    tab: TabSize,
    _block: bool,
) -> Option<IndentPlan> {
    if selections.is_empty() || text.lines_len() == 0 {
        return None;
    }
    let mut rows = BTreeMap::<usize, ()>::new();
    for selection in selections {
        for row in selection_line_range(text, *selection)? {
            rows.insert(row, ());
        }
    }
    let mut edits = BTreeMap::<(usize, usize), String>::new();
    for row in rows.into_keys() {
        let content = line_content(text, row);
        let remove = outdent_prefix_len(&content, tab.tab_size.max(1));
        if remove == 0 {
            continue;
        }
        let start = text.line_start_offset(row);
        edits.insert((start, start + remove), String::new());
    }
    finish_indent_plan(selections, edits)
}

fn finish_indent_plan(
    selections: &[EditorSelection],
    edits: BTreeMap<(usize, usize), String>,
) -> Option<IndentPlan> {
    if edits.is_empty() {
        return None;
    }
    let edits = edits
        .into_iter()
        .map(|((start, end), replacement)| (start..end, replacement))
        .collect::<Vec<_>>();
    let selections_after = selections
        .iter()
        .copied()
        .map(|selection| transform_selection_for_indent(selection, &edits))
        .collect();
    Some(IndentPlan {
        edits,
        selections_after,
    })
}

fn transform_selection_for_indent(
    selection: EditorSelection,
    edits: &[(Range<usize>, String)],
) -> EditorSelection {
    let start = transform_indent_offset(selection.range.start, selection.is_empty(), edits);
    let end = transform_indent_offset(selection.range.end, selection.is_empty(), edits);
    EditorSelection {
        range: Selection::new(start.min(end), start.max(end)),
        reversed: selection.reversed,
    }
}

fn transform_indent_offset(
    offset: usize,
    move_after_equal_insertion: bool,
    edits: &[(Range<usize>, String)],
) -> usize {
    let mut delta = 0isize;
    for (range, replacement) in edits {
        if offset < range.start {
            break;
        }
        if range.is_empty() {
            if offset > range.start || (offset == range.start && move_after_equal_insertion) {
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

fn line_content(text: &Rope, row: usize) -> String {
    let mut content = text.slice_line(row).to_string();
    if content.ends_with('\r') {
        content.pop();
    }
    content
}

fn inline_tab_text(text: &Rope, offset: usize, tab: TabSize) -> String {
    if tab.hard_tabs {
        return "\t".to_string();
    }
    let tab_size = tab.tab_size.max(1);
    let row = text.offset_to_point(offset).row;
    let line_start = text.line_start_offset(row);
    let prefix = text.slice(line_start..offset).to_string();
    let column = prefix.chars().fold(0usize, |column, character| {
        if character == '\t' {
            column + (tab_size - column % tab_size)
        } else {
            column + 1
        }
    });
    " ".repeat(tab_size - column % tab_size)
}

fn outdent_prefix_len(content: &str, tab_size: usize) -> usize {
    if content.starts_with('\t') {
        return 1;
    }
    content
        .as_bytes()
        .iter()
        .take(tab_size)
        .take_while(|byte| **byte == b' ')
        .count()
}

fn update_tab_size(mode: &mut InputMode, tab: TabSize) -> bool {
    match mode {
        InputMode::PlainText { tab: current, .. } | InputMode::CodeEditor { tab: current, .. } => {
            *current = tab;
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use ropey::RopeSlice;

    use super::{IndentPlan, TabSize, plan_indent, plan_outdent, update_tab_size};
    use crate::input::{EditorSelection, Rope, mode::InputMode};

    fn apply_plan(source: &str, plan: &IndentPlan) -> String {
        let mut result = source.to_string();
        for (range, replacement) in plan.edits.iter().rev() {
            result.replace_range(range.clone(), replacement);
        }
        result
    }

    #[test]
    fn test_tab_size() {
        let tab = TabSize {
            tab_size: 2,
            hard_tabs: false,
        };
        assert_eq!(tab.to_string(), "  ");
        let tab = TabSize {
            tab_size: 4,
            hard_tabs: false,
        };
        assert_eq!(tab.to_string(), "    ");

        let tab = TabSize {
            tab_size: 2,
            hard_tabs: true,
        };
        assert_eq!(tab.to_string(), "\t");
        let tab = TabSize {
            tab_size: 4,
            hard_tabs: true,
        };
        assert_eq!(tab.to_string(), "\t");
    }

    #[test]
    fn test_tab_size_indent_count() {
        let tab = TabSize {
            tab_size: 4,
            hard_tabs: false,
        };
        assert_eq!(tab.indent_count(&RopeSlice::from("abc")), 0);
        assert_eq!(tab.indent_count(&RopeSlice::from("  abc")), 2);
        assert_eq!(tab.indent_count(&RopeSlice::from("    abc")), 4);
        assert_eq!(tab.indent_count(&RopeSlice::from("\tabc")), 4);
        assert_eq!(tab.indent_count(&RopeSlice::from("  \tabc")), 6);
        assert_eq!(tab.indent_count(&RopeSlice::from(" \t abc  ")), 6);
        assert_eq!(tab.indent_count(&RopeSlice::from("abc")), 0);
    }

    #[test]
    fn runtime_tab_size_updates_the_live_editor_mode() {
        let mut mode = InputMode::code_editor("rust");
        assert!(update_tab_size(
            &mut mode,
            TabSize {
                tab_size: 4,
                hard_tabs: true,
            }
        ));
        assert_eq!(mode.tab_size().tab_size, 4);
        assert!(mode.tab_size().hard_tabs);
    }

    #[test]
    fn inline_tab_advances_every_unicode_cursor_to_the_next_visual_tab_stop() {
        let source = "α\tb\n12345\n";
        let first = "α".len();
        let second = source.find("12345").unwrap() + 5;
        let selections = vec![
            EditorSelection::caret(first),
            EditorSelection::caret(second),
        ];
        let plan = plan_indent(
            &Rope::from(source),
            &selections,
            TabSize {
                tab_size: 4,
                hard_tabs: false,
            },
            false,
        )
        .unwrap();

        assert_eq!(apply_plan(source, &plan), "α   \tb\n12345   \n");
        assert_eq!(
            plan.selections_after,
            vec![
                EditorSelection::caret(first + 3),
                EditorSelection::caret(second + 6),
            ]
        );
    }

    #[test]
    fn block_indent_deduplicates_lines_and_excludes_a_selection_end_at_next_line_start() {
        let source = "a\r\n\r\nb\r\n";
        let third_line = source.find('b').unwrap();
        let selections = vec![
            EditorSelection::from_anchor_and_head(0, third_line),
            EditorSelection::caret(third_line),
        ];
        let plan = plan_indent(
            &Rope::from(source),
            &selections,
            TabSize {
                tab_size: 2,
                hard_tabs: false,
            },
            true,
        )
        .unwrap();

        assert_eq!(apply_plan(source, &plan), "  a\r\n\r\n  b\r\n");
        assert_eq!(plan.selections_after[0].range.start, 0);
        assert_eq!(plan.selections_after[0].range.end, third_line + 2);
        assert_eq!(
            plan.selections_after[1],
            EditorSelection::caret(third_line + 4)
        );
    }

    #[test]
    fn outdent_handles_spaces_tabs_crlf_and_preserves_reversed_selections() {
        let source = "    one\r\n\ttwo\n  three";
        let second = source.find("two").unwrap();
        let third = source.find("three").unwrap();
        let selections = vec![
            EditorSelection::caret(source.find("one").unwrap()),
            EditorSelection::caret(second),
            EditorSelection::from_anchor_and_head(source.len(), third),
        ];
        let plan = plan_outdent(
            &Rope::from(source),
            &selections,
            TabSize {
                tab_size: 4,
                hard_tabs: false,
            },
            false,
        )
        .unwrap();

        assert_eq!(apply_plan(source, &plan), "one\r\ntwo\nthree");
        assert!(plan.selections_after[2].reversed);
        assert_eq!(plan.selections_after[0], EditorSelection::caret(0));
        assert_eq!(plan.selections_after[1], EditorSelection::caret(5));
    }
}
