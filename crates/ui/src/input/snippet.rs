use std::{collections::BTreeMap, ops::Range};

use super::TabSize;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SnippetTabstop {
    pub(crate) index: u32,
    pub(crate) ranges: Vec<Range<usize>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ParsedSnippet {
    pub(crate) text: String,
    pub(crate) tabstops: Vec<SnippetTabstop>,
}

impl ParsedSnippet {
    pub(crate) fn adjust_indentation(
        &mut self,
        base_indentation: &str,
        tab_size: TabSize,
        line_ending: &str,
    ) {
        let (text, offset_map) =
            rewrite_indentation(&self.text, base_indentation, tab_size, line_ending);
        for tabstop in &mut self.tabstops {
            for range in &mut tabstop.ranges {
                range.start = offset_map[range.start.min(offset_map.len() - 1)];
                range.end = offset_map[range.end.min(offset_map.len() - 1)];
            }
        }
        self.text = text;
    }
}

pub(crate) fn adjust_text_indentation(
    text: &str,
    base_indentation: &str,
    tab_size: TabSize,
    line_ending: &str,
) -> String {
    rewrite_indentation(text, base_indentation, tab_size, line_ending).0
}

fn rewrite_indentation(
    text: &str,
    base_indentation: &str,
    tab_size: TabSize,
    line_ending: &str,
) -> (String, Vec<usize>) {
    let line_ending = if line_ending.is_empty() {
        "\n"
    } else {
        line_ending
    };
    let mut rewritten = String::with_capacity(text.len() + base_indentation.len());
    let mut offset_map = vec![0; text.len() + 1];
    if text.is_empty() {
        return (rewritten, offset_map);
    }

    let bytes = text.as_bytes();
    let mut offset = 0;
    let mut line_index = 0;
    let mut ended_with_line_break = false;
    while offset < bytes.len() {
        let mut line_end = offset;
        while line_end < bytes.len() && !matches!(bytes[line_end], b'\r' | b'\n') {
            line_end += 1;
        }
        let mut line_break_end = line_end;
        if line_break_end < bytes.len() {
            line_break_end += 1;
            if bytes[line_end] == b'\r'
                && line_break_end < bytes.len()
                && bytes[line_break_end] == b'\n'
            {
                line_break_end += 1;
            }
        }

        let mut whitespace_end = offset;
        while whitespace_end < line_end && matches!(bytes[whitespace_end], b' ' | b'\t') {
            whitespace_end += 1;
        }
        let indentation = if line_index == 0 {
            normalize_indentation(&text[offset..whitespace_end], tab_size)
        } else {
            let mut combined = String::with_capacity(
                base_indentation.len() + whitespace_end.saturating_sub(offset),
            );
            combined.push_str(base_indentation);
            combined.push_str(&text[offset..whitespace_end]);
            normalize_indentation(&combined, tab_size)
        };
        append_replacement(
            &mut rewritten,
            &mut offset_map,
            offset..whitespace_end,
            &indentation,
        );
        append_copy(
            &mut rewritten,
            &mut offset_map,
            text,
            whitespace_end..line_end,
        );
        if line_break_end > line_end {
            append_replacement(
                &mut rewritten,
                &mut offset_map,
                line_end..line_break_end,
                line_ending,
            );
        }

        ended_with_line_break = line_break_end > line_end && line_break_end == bytes.len();
        offset = line_break_end;
        line_index += 1;
    }

    if ended_with_line_break {
        let indentation = normalize_indentation(base_indentation, tab_size);
        append_replacement(
            &mut rewritten,
            &mut offset_map,
            text.len()..text.len(),
            &indentation,
        );
    }

    (rewritten, offset_map)
}

fn normalize_indentation(indentation: &str, tab_size: TabSize) -> String {
    let tab_width = tab_size.tab_size.max(1);
    let columns = indentation
        .bytes()
        .fold(0usize, |columns, byte| match byte {
            b' ' => columns + 1,
            b'\t' => columns + tab_width - (columns % tab_width),
            _ => columns,
        });
    if tab_size.hard_tabs {
        format!(
            "{}{}",
            "\t".repeat(columns / tab_width),
            " ".repeat(columns % tab_width)
        )
    } else {
        " ".repeat(columns)
    }
}

fn append_replacement(
    rewritten: &mut String,
    offset_map: &mut [usize],
    old_range: Range<usize>,
    replacement: &str,
) {
    let new_start = rewritten.len();
    for old_offset in old_range.clone() {
        offset_map[old_offset] = new_start + (old_offset - old_range.start).min(replacement.len());
    }
    rewritten.push_str(replacement);
    offset_map[old_range.end] = rewritten.len();
}

fn append_copy(
    rewritten: &mut String,
    offset_map: &mut [usize],
    source: &str,
    old_range: Range<usize>,
) {
    let new_start = rewritten.len();
    rewritten.push_str(&source[old_range.clone()]);
    for old_offset in old_range.clone() {
        offset_map[old_offset] = new_start + old_offset - old_range.start;
    }
    offset_map[old_range.end] = rewritten.len();
}

#[derive(Clone, Debug)]
pub(crate) struct SnippetSession {
    tabstops: Vec<SnippetTabstop>,
    active: usize,
}

impl SnippetSession {
    pub(crate) fn new(parsed: &ParsedSnippet, insertion_start: usize) -> Option<Self> {
        let tabstops = parsed
            .tabstops
            .iter()
            .map(|tabstop| SnippetTabstop {
                index: tabstop.index,
                ranges: tabstop
                    .ranges
                    .iter()
                    .map(|range| insertion_start + range.start..insertion_start + range.end)
                    .collect(),
            })
            .collect::<Vec<_>>();
        (!tabstops.is_empty()).then_some(Self {
            tabstops,
            active: 0,
        })
    }

    pub(crate) fn active_range(&self) -> Option<Range<usize>> {
        self.tabstops
            .get(self.active)
            .and_then(|tabstop| tabstop.ranges.first())
            .cloned()
    }

    pub(crate) fn active_mirrors(&self) -> &[Range<usize>] {
        self.tabstops
            .get(self.active)
            .and_then(|tabstop| tabstop.ranges.get(1..))
            .unwrap_or_default()
    }

    pub(crate) fn active_is_final(&self) -> bool {
        self.tabstops
            .get(self.active)
            .is_some_and(|tabstop| tabstop.index == 0)
    }

    pub(crate) fn move_next(&mut self) -> Option<Range<usize>> {
        if self.active + 1 >= self.tabstops.len() {
            return None;
        }
        self.active += 1;
        self.active_range()
    }

    pub(crate) fn move_previous(&mut self) -> Option<Range<usize>> {
        if self.active == 0 {
            return self.active_range();
        }
        self.active -= 1;
        self.active_range()
    }

    /// Track a user edit to the active placeholder. Editing anywhere else
    /// invalidates the session so stale tab stops can never mutate the buffer.
    pub(crate) fn track_user_edit(&mut self, edit: Range<usize>, inserted_len: usize) -> bool {
        let Some(active) = self.active_range() else {
            return false;
        };
        if edit.start < active.start || edit.end > active.end {
            return false;
        }
        let removed_len = edit.end.saturating_sub(edit.start);
        let delta = inserted_len as isize - removed_len as isize;
        for (tabstop_index, tabstop) in self.tabstops.iter_mut().enumerate() {
            for (range_index, range) in tabstop.ranges.iter_mut().enumerate() {
                if tabstop_index == self.active && range_index == 0 {
                    range.end = range.end.saturating_add_signed(delta).max(range.start);
                } else {
                    track_range(range, &edit, inserted_len);
                }
            }
        }
        true
    }

    pub(crate) fn track_edit(&mut self, edit: Range<usize>, inserted_len: usize) {
        for tabstop in &mut self.tabstops {
            for range in &mut tabstop.ranges {
                track_range(range, &edit, inserted_len);
            }
        }
    }
}

fn track_range(range: &mut Range<usize>, edit: &Range<usize>, inserted_len: usize) {
    let removed_len = edit.end.saturating_sub(edit.start);
    let delta = inserted_len as isize - removed_len as isize;
    if range.start == edit.start && range.end == edit.end {
        range.end = range.start + inserted_len;
    } else if range.end <= edit.start {
        // Entirely before the edit.
    } else if range.start >= edit.end {
        range.start = range.start.saturating_add_signed(delta);
        range.end = range.end.saturating_add_signed(delta);
    } else {
        // Nested placeholders can overlap their parent. Preserve that overlap
        // while shifting the affected trailing boundary.
        range.start = range.start.min(edit.start);
        range.end = range.end.saturating_add_signed(delta).max(range.start);
    }
}

pub(crate) fn parse_snippet(source: &str) -> ParsedSnippet {
    let mut parser = Parser {
        source,
        offset: 0,
        text: String::with_capacity(source.len()),
        ranges: BTreeMap::new(),
        defaults: BTreeMap::new(),
    };
    parser.parse_until(None);
    let mut tabstops = parser
        .ranges
        .into_iter()
        .map(|(index, ranges)| SnippetTabstop { index, ranges })
        .collect::<Vec<_>>();
    // LSP/VS Code semantics always visit $0 last.
    tabstops.sort_by_key(|tabstop| (tabstop.index == 0, tabstop.index));
    ParsedSnippet {
        text: parser.text,
        tabstops,
    }
}

struct Parser<'a> {
    source: &'a str,
    offset: usize,
    text: String,
    ranges: BTreeMap<u32, Vec<Range<usize>>>,
    defaults: BTreeMap<u32, String>,
}

impl Parser<'_> {
    fn parse_until(&mut self, terminator: Option<u8>) {
        while self.offset < self.source.len() {
            let byte = self.source.as_bytes()[self.offset];
            if Some(byte) == terminator {
                self.offset += 1;
                return;
            }
            match byte {
                b'\\' => self.parse_escape(),
                b'$' => {
                    if !self.parse_dollar() {
                        self.push_source_char();
                    }
                }
                _ => self.push_source_char(),
            }
        }
    }

    fn parse_escape(&mut self) {
        self.offset += 1;
        if self.offset >= self.source.len() {
            self.text.push('\\');
            return;
        }
        let escaped = self.source.as_bytes()[self.offset];
        if matches!(escaped, b'\\' | b'$' | b'}' | b',' | b'|') {
            self.text.push(escaped as char);
            self.offset += 1;
        } else {
            self.text.push('\\');
        }
    }

    fn parse_dollar(&mut self) -> bool {
        let start = self.offset;
        self.offset += 1;
        if self.peek() == Some(b'{') {
            self.offset += 1;
            if let Some(index) = self.parse_number() {
                return self.parse_braced_tabstop(index, start);
            }
            if let Some(name) = self.parse_identifier() {
                return self.parse_braced_variable(&name, start);
            }
            self.offset = start;
            return false;
        }
        if let Some(index) = self.parse_number() {
            self.insert_tabstop(index, None);
            return true;
        }
        if let Some(name) = self.parse_identifier() {
            self.text.push_str(variable_value(&name).unwrap_or(&name));
            return true;
        }
        self.offset = start;
        false
    }

    fn parse_braced_tabstop(&mut self, index: u32, start: usize) -> bool {
        match self.peek() {
            Some(b'}') => {
                self.offset += 1;
                self.insert_tabstop(index, None);
                true
            }
            Some(b':') => {
                self.offset += 1;
                let output_start = self.text.len();
                self.parse_until(Some(b'}'));
                let output_end = self.text.len();
                self.defaults
                    .entry(index)
                    .or_insert_with(|| self.text[output_start..output_end].to_string());
                self.ranges
                    .entry(index)
                    .or_default()
                    .push(output_start..output_end);
                true
            }
            Some(b'|') => self.parse_choice(index, start),
            _ => {
                self.offset = start;
                false
            }
        }
    }

    fn parse_choice(&mut self, index: u32, start: usize) -> bool {
        self.offset += 1;
        let choice_start = self.offset;
        let mut escaped = false;
        while self.offset + 1 < self.source.len() {
            let byte = self.source.as_bytes()[self.offset];
            if escaped {
                escaped = false;
                self.offset += 1;
                continue;
            }
            if byte == b'\\' {
                escaped = true;
                self.offset += 1;
                continue;
            }
            if byte == b'|' && self.source.as_bytes()[self.offset + 1] == b'}' {
                let raw = &self.source[choice_start..self.offset];
                let first = first_choice(raw);
                self.offset += 2;
                let output_start = self.text.len();
                self.text.push_str(&first);
                let output_end = self.text.len();
                self.defaults.entry(index).or_insert(first);
                self.ranges
                    .entry(index)
                    .or_default()
                    .push(output_start..output_end);
                return true;
            }
            self.offset += 1;
        }
        self.offset = start;
        false
    }

    fn parse_braced_variable(&mut self, name: &str, start: usize) -> bool {
        match self.peek() {
            Some(b'}') => {
                self.offset += 1;
                self.text.push_str(variable_value(name).unwrap_or(name));
                true
            }
            Some(b':') => {
                self.offset += 1;
                if let Some(value) = variable_value(name) {
                    self.text.push_str(value);
                    self.skip_balanced_braces();
                } else {
                    self.parse_until(Some(b'}'));
                }
                true
            }
            _ => {
                self.offset = start;
                false
            }
        }
    }

    fn skip_balanced_braces(&mut self) {
        let mut depth = 0usize;
        while self.offset < self.source.len() {
            match self.source.as_bytes()[self.offset] {
                b'\\' => self.offset = (self.offset + 2).min(self.source.len()),
                b'{' => {
                    depth += 1;
                    self.offset += 1;
                }
                b'}' if depth == 0 => {
                    self.offset += 1;
                    return;
                }
                b'}' => {
                    depth -= 1;
                    self.offset += 1;
                }
                _ => self.offset += 1,
            }
        }
    }

    fn insert_tabstop(&mut self, index: u32, value: Option<&str>) {
        let output_start = self.text.len();
        if let Some(value) = value.or_else(|| self.defaults.get(&index).map(String::as_str)) {
            self.text.push_str(value);
        }
        let output_end = self.text.len();
        self.ranges
            .entry(index)
            .or_default()
            .push(output_start..output_end);
    }

    fn parse_number(&mut self) -> Option<u32> {
        let start = self.offset;
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.offset += 1;
        }
        (self.offset > start)
            .then(|| self.source[start..self.offset].parse().ok())
            .flatten()
    }

    fn parse_identifier(&mut self) -> Option<String> {
        let start = self.offset;
        while self
            .peek()
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            self.offset += 1;
        }
        (self.offset > start).then(|| self.source[start..self.offset].to_string())
    }

    fn push_source_char(&mut self) {
        let character = self.source[self.offset..].chars().next().unwrap();
        self.text.push(character);
        self.offset += character.len_utf8();
    }

    fn peek(&self) -> Option<u8> {
        self.source.as_bytes().get(self.offset).copied()
    }
}

fn first_choice(raw: &str) -> String {
    let mut result = String::new();
    let mut escaped = false;
    for character in raw.chars() {
        if escaped {
            result.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == ',' {
            break;
        } else {
            result.push(character);
        }
    }
    if escaped {
        result.push('\\');
    }
    result
}

fn variable_value(name: &str) -> Option<&'static str> {
    match name {
        "TM_SELECTED_TEXT" | "CLIPBOARD" => Some(""),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ordered_placeholders_mirrors_choices_and_final_cursor() {
        let parsed = parse_snippet("fn ${1:name}(${2|value,other|}) { $1($2); }$0");
        assert_eq!(parsed.text, "fn name(value) { name(value); }");
        assert_eq!(
            parsed.tabstops,
            vec![
                SnippetTabstop {
                    index: 1,
                    ranges: vec![3..7, 17..21],
                },
                SnippetTabstop {
                    index: 2,
                    ranges: vec![8..13, 22..27],
                },
                SnippetTabstop {
                    index: 0,
                    ranges: vec![31..31],
                },
            ]
        );
    }

    #[test]
    fn parses_nested_placeholders_and_escaped_metacharacters() {
        let parsed = parse_snippet(r"${1:outer ${2:inner}} \$ \\ \} ${3|a\,b,c|}");
        assert_eq!(parsed.text, "outer inner $ \\ } a,b");
        assert_eq!(parsed.tabstops[0].ranges, vec![0..11]);
        assert_eq!(parsed.tabstops[1].ranges, vec![6..11]);
        assert_eq!(parsed.tabstops[2].ranges, vec![18..21]);
    }

    #[test]
    fn snippet_session_tracks_active_edits_and_rejects_external_mutation() {
        let parsed = parse_snippet("${1:name} = $1;$0");
        let mut session = SnippetSession::new(&parsed, 10).unwrap();
        assert_eq!(session.active_range(), Some(10..14));
        assert!(session.track_user_edit(10..14, 2));
        assert_eq!(session.active_range(), Some(10..12));
        assert_eq!(session.active_mirrors(), &[15..19]);
        assert!(!session.track_user_edit(0..0, 1));
    }

    #[test]
    fn zero_width_active_tabstop_grows_when_the_user_types() {
        let parsed = parse_snippet("before $1 after$0");
        let mut session = SnippetSession::new(&parsed, 0).unwrap();
        assert_eq!(session.active_range(), Some(7..7));
        assert!(session.track_user_edit(7..7, 3));
        assert_eq!(session.active_range(), Some(7..10));
        assert_eq!(session.move_next(), Some(16..16));
        assert!(session.active_is_final());
    }

    #[test]
    fn adjusted_multiline_snippet_preserves_placeholder_ranges() {
        let mut parsed = parse_snippet("call(\r\n\t${1:value}\r\n)$0");
        parsed.adjust_indentation(
            "    ",
            TabSize {
                tab_size: 4,
                hard_tabs: false,
            },
            "\n",
        );

        assert_eq!(parsed.text, "call(\n        value\n    )");
        assert_eq!(parsed.tabstops[0].ranges, vec![14..19]);
        assert_eq!(parsed.tabstops[1].ranges, vec![25..25]);
    }

    #[test]
    fn adjusted_plain_text_uses_document_eol_and_hard_tabs() {
        let adjusted = adjust_text_indentation(
            "foo\n  bar\n",
            "\t",
            TabSize {
                tab_size: 4,
                hard_tabs: true,
            },
            "\r\n",
        );

        assert_eq!(adjusted, "foo\r\n\t  bar\r\n\t");
    }
}
