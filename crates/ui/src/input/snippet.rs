use std::{collections::BTreeMap, ops::Range};

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
}
