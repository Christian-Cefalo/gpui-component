use std::ops::Range;

const DEFAULT_AUTO_CLOSE_BEFORE_QUOTES: &str = ";:.,=}])> \n\t";
const DEFAULT_AUTO_CLOSE_BEFORE_BRACKETS: &str = "'\"`;:.,=}])> \n\t";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditorTokenContext {
    Code,
    String,
    Comment,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditorCharacterPair {
    pub open: String,
    pub close: String,
}

impl EditorCharacterPair {
    pub fn new(open: impl Into<String>, close: impl Into<String>) -> Self {
        Self {
            open: open.into(),
            close: close.into(),
        }
    }
}

impl<S, T> From<(S, T)> for EditorCharacterPair
where
    S: Into<String>,
    T: Into<String>,
{
    fn from((open, close): (S, T)) -> Self {
        Self::new(open, close)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditorAutoClosingPair {
    pub open: String,
    pub close: String,
    pub not_in: Vec<EditorTokenContext>,
}

impl EditorAutoClosingPair {
    pub fn new(open: impl Into<String>, close: impl Into<String>) -> Self {
        Self {
            open: open.into(),
            close: close.into(),
            not_in: Vec::new(),
        }
    }

    pub fn not_in(mut self, contexts: impl IntoIterator<Item = EditorTokenContext>) -> Self {
        self.not_in = contexts.into_iter().collect();
        self
    }
}

impl<S, T> From<(S, T)> for EditorAutoClosingPair
where
    S: Into<String>,
    T: Into<String>,
{
    fn from((open, close): (S, T)) -> Self {
        Self::new(open, close)
    }
}

/// Data supplied by the host application for one editor language.
///
/// The shape intentionally follows VS Code's language configuration rather
/// than baking language names into the editor. Presets and user-defined
/// languages therefore use the same bracket, auto-close, surround, comment,
/// and string rules.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EditorLanguageConfiguration {
    brackets: Vec<EditorCharacterPair>,
    auto_closing_pairs: Vec<EditorAutoClosingPair>,
    surrounding_pairs: Vec<EditorCharacterPair>,
    line_comment: Option<String>,
    block_comment: Option<EditorCharacterPair>,
    string_delimiters: Vec<String>,
    auto_close_before: Option<String>,
}

impl EditorLanguageConfiguration {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_brackets<I, P>(mut self, pairs: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<EditorCharacterPair>,
    {
        self.brackets = pairs.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_auto_closing_pairs<I, P>(mut self, pairs: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<EditorAutoClosingPair>,
    {
        self.auto_closing_pairs = pairs.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_surrounding_pairs<I, P>(mut self, pairs: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<EditorCharacterPair>,
    {
        self.surrounding_pairs = pairs.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_line_comment(mut self, token: impl Into<String>) -> Self {
        self.line_comment = Some(token.into());
        self
    }

    pub fn with_block_comment(mut self, open: impl Into<String>, close: impl Into<String>) -> Self {
        self.block_comment = Some(EditorCharacterPair::new(open, close));
        self
    }

    pub fn with_string_delimiters<I, S>(mut self, delimiters: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.string_delimiters = delimiters.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_auto_close_before(mut self, characters: impl Into<String>) -> Self {
        self.auto_close_before = Some(characters.into());
        self
    }

    pub fn brackets(&self) -> &[EditorCharacterPair] {
        &self.brackets
    }

    pub fn auto_closing_pairs(&self) -> &[EditorAutoClosingPair] {
        &self.auto_closing_pairs
    }

    pub fn surrounding_pairs(&self) -> &[EditorCharacterPair] {
        &self.surrounding_pairs
    }

    /// The language's line-comment token, such as `//` or `#`.
    pub fn line_comment(&self) -> Option<&str> {
        self.line_comment.as_deref()
    }

    /// The language's block-comment delimiters, such as `/*` and `*/`.
    pub fn block_comment(&self) -> Option<&EditorCharacterPair> {
        self.block_comment.as_ref()
    }

    pub fn is_empty(&self) -> bool {
        self.brackets.is_empty()
            && self.auto_closing_pairs.is_empty()
            && self.surrounding_pairs.is_empty()
    }

    pub(crate) fn normalized(mut self) -> Self {
        const MAX_PAIRS: usize = 128;
        const MAX_TOKEN_BYTES: usize = 32;

        if self.auto_closing_pairs.is_empty() {
            self.auto_closing_pairs = self
                .brackets
                .iter()
                .map(|pair| EditorAutoClosingPair::new(&pair.open, &pair.close))
                .collect();
        }
        if self.surrounding_pairs.is_empty() {
            self.surrounding_pairs = self
                .auto_closing_pairs
                .iter()
                .map(|pair| EditorCharacterPair::new(&pair.open, &pair.close))
                .collect();
        }

        self.brackets.truncate(MAX_PAIRS);
        self.auto_closing_pairs.truncate(MAX_PAIRS);
        self.surrounding_pairs.truncate(MAX_PAIRS);
        self.string_delimiters.truncate(MAX_PAIRS);

        self.brackets.retain(|pair| {
            valid_token(&pair.open, MAX_TOKEN_BYTES) && valid_token(&pair.close, MAX_TOKEN_BYTES)
        });
        self.auto_closing_pairs.retain(|pair| {
            valid_token(&pair.open, MAX_TOKEN_BYTES) && valid_token(&pair.close, MAX_TOKEN_BYTES)
        });
        self.surrounding_pairs.retain(|pair| {
            valid_token(&pair.open, MAX_TOKEN_BYTES) && valid_token(&pair.close, MAX_TOKEN_BYTES)
        });
        self.string_delimiters
            .retain(|token| valid_token(token, MAX_TOKEN_BYTES));
        self.line_comment = self
            .line_comment
            .filter(|token| valid_token(token, MAX_TOKEN_BYTES));
        self.block_comment = self.block_comment.filter(|pair| {
            valid_token(&pair.open, MAX_TOKEN_BYTES) && valid_token(&pair.close, MAX_TOKEN_BYTES)
        });
        self
    }

    fn auto_close_before(&self, quote: bool) -> &str {
        self.auto_close_before.as_deref().unwrap_or(if quote {
            DEFAULT_AUTO_CLOSE_BEFORE_QUOTES
        } else {
            DEFAULT_AUTO_CLOSE_BEFORE_BRACKETS
        })
    }

    fn lexical_string_delimiters(&self) -> Vec<&str> {
        let mut delimiters = self
            .string_delimiters
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        for pair in &self.auto_closing_pairs {
            if pair.open == pair.close && is_quote(&pair.open) {
                delimiters.push(&pair.open);
            }
        }
        delimiters.sort_unstable_by_key(|token| std::cmp::Reverse(token.len()));
        delimiters.dedup();
        delimiters
    }
}

fn valid_token(token: &str, max_bytes: usize) -> bool {
    !token.is_empty() && token.len() <= max_bytes
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TrackedAutoClosingPair {
    pub(crate) open: Range<usize>,
    pub(crate) close: Range<usize>,
    pub(crate) open_text: String,
    pub(crate) close_text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PairTypingPlan {
    pub(crate) replacement: String,
    pub(crate) selection_after: Range<usize>,
    pub(crate) tracked_pair: Option<TrackedAutoClosingPair>,
}

pub(crate) fn pair_typing_plan(
    text: &str,
    selection: Range<usize>,
    typed: &str,
    config: &EditorLanguageConfiguration,
) -> Option<PairTypingPlan> {
    if typed.chars().count() != 1
        || selection.start > selection.end
        || selection.end > text.len()
        || !text.is_char_boundary(selection.start)
        || !text.is_char_boundary(selection.end)
    {
        return None;
    }

    if !selection.is_empty() {
        return surround_selection_plan(text, selection, typed, config);
    }

    let pair = config
        .auto_closing_pairs
        .iter()
        .filter(|pair| pair.open.ends_with(typed))
        .filter(|pair| {
            let prefix = &pair.open[..pair.open.len() - typed.len()];
            selection.start >= prefix.len()
                && &text[selection.start - prefix.len()..selection.start] == prefix
        })
        .max_by_key(|pair| pair.open.len())?;

    let quote = is_quote(&pair.open);
    if quote && escaped_at(text, selection.start) {
        return None;
    }

    let context = lexical_context_at(text, selection.start, config);
    if pair.not_in.contains(&context) {
        return None;
    }

    if quote {
        let before_open = selection
            .start
            .saturating_sub(pair.open.len().saturating_sub(typed.len()));
        if before_open > 0
            && text[..before_open]
                .chars()
                .next_back()
                .is_some_and(is_word_character)
        {
            return None;
        }
    }

    if let Some(after) = text[selection.start..].chars().next() {
        let before_allowed = config.auto_close_before(quote).contains(after);
        let before_closing_pair = config
            .auto_closing_pairs
            .iter()
            .any(|candidate| text[selection.start..].starts_with(&candidate.close));
        if !before_allowed && !before_closing_pair {
            return None;
        }
    }

    let cursor = selection.start + typed.len();
    let close_end = cursor + pair.close.len();
    Some(PairTypingPlan {
        replacement: format!("{typed}{}", pair.close),
        selection_after: cursor..cursor,
        tracked_pair: Some(TrackedAutoClosingPair {
            // Only the fragment inserted by this keystroke is editor-owned.
            // For a multi-character opener such as `${`, Backspace must
            // remove the typed `{` and generated `}`, not the existing `$`.
            open: selection.start..cursor,
            close: cursor..close_end,
            open_text: typed.to_string(),
            close_text: pair.close.clone(),
        }),
    })
}

fn surround_selection_plan(
    text: &str,
    selection: Range<usize>,
    typed: &str,
    config: &EditorLanguageConfiguration,
) -> Option<PairTypingPlan> {
    let pair = config
        .surrounding_pairs
        .iter()
        .find(|pair| pair.open == typed)?;
    let selected = &text[selection.clone()];
    if selected
        .chars()
        .all(|character| matches!(character, ' ' | '\t'))
    {
        return None;
    }
    if is_quote(&pair.open)
        && selected.chars().count() == 1
        && selected
            .chars()
            .next()
            .is_some_and(|character| matches!(character, '\'' | '"' | '`'))
    {
        return None;
    }

    let selected_start = selection.start + pair.open.len();
    let selected_end = selected_start + selected.len();
    Some(PairTypingPlan {
        replacement: format!("{}{selected}{}", pair.open, pair.close),
        selection_after: selected_start..selected_end,
        tracked_pair: None,
    })
}

pub(crate) fn overtype_target(
    text: &str,
    cursor: usize,
    typed: &str,
    tracked: &[TrackedAutoClosingPair],
) -> Option<(usize, usize)> {
    tracked.iter().enumerate().find_map(|(index, pair)| {
        (pair.close.start == cursor
            && pair.close_text == typed
            && pair.close.end <= text.len()
            && text.get(pair.close.clone()) == Some(pair.close_text.as_str()))
        .then_some((index, pair.close.end))
    })
}

pub(crate) fn paired_backspace_range(
    text: &str,
    cursor: usize,
    tracked: &[TrackedAutoClosingPair],
) -> Option<Range<usize>> {
    tracked.iter().find_map(|pair| {
        (pair.open.end == cursor
            && pair.close.start == cursor
            && pair.open.end <= text.len()
            && pair.close.end <= text.len()
            && text.get(pair.open.clone()) == Some(pair.open_text.as_str())
            && text.get(pair.close.clone()) == Some(pair.close_text.as_str()))
        .then_some(pair.open.start..pair.close.end)
    })
}

pub(crate) fn adjust_tracked_pairs_for_edit(
    tracked: &mut Vec<TrackedAutoClosingPair>,
    edit: &Range<usize>,
    inserted_len: usize,
) {
    let delta = inserted_len as isize - edit.len() as isize;
    tracked.retain_mut(|pair| {
        if edit.is_empty() {
            if edit.start <= pair.open.start {
                shift_pair(pair, delta, true);
                return true;
            }
            if edit.start >= pair.open.end && edit.start <= pair.close.start {
                shift_range(&mut pair.close, delta);
                return true;
            }
            return edit.start >= pair.close.end;
        }

        if edit.end <= pair.open.start {
            shift_pair(pair, delta, true);
            true
        } else if edit.start >= pair.close.end {
            true
        } else if edit.start >= pair.open.end && edit.end <= pair.close.start {
            shift_range(&mut pair.close, delta);
            true
        } else {
            false
        }
    });
}

fn shift_pair(pair: &mut TrackedAutoClosingPair, delta: isize, shift_open: bool) {
    if shift_open {
        shift_range(&mut pair.open, delta);
    }
    shift_range(&mut pair.close, delta);
}

fn shift_range(range: &mut Range<usize>, delta: isize) {
    range.start = range.start.saturating_add_signed(delta);
    range.end = range.end.saturating_add_signed(delta);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BracketMatch {
    pub(crate) open: Range<usize>,
    pub(crate) close: Range<usize>,
}

pub(crate) fn bracket_match_near(
    text: &str,
    cursor: usize,
    config: &EditorLanguageConfiguration,
) -> Option<BracketMatch> {
    let pairs = scan_bracket_pairs(text, config);
    pairs
        .into_iter()
        .filter(|pair| {
            range_is_near_cursor(&pair.open, cursor) || range_is_near_cursor(&pair.close, cursor)
        })
        .min_by_key(|pair| {
            distance_to_range(&pair.open, cursor).min(distance_to_range(&pair.close, cursor))
        })
}

pub(crate) fn bracket_navigation_target(
    text: &str,
    cursor: usize,
    config: &EditorLanguageConfiguration,
) -> Option<usize> {
    let pairs = scan_bracket_pairs(text, config);
    if let Some(pair) = pairs
        .iter()
        .find(|pair| range_is_near_cursor(&pair.open, cursor))
    {
        return Some(pair.close.start);
    }
    if let Some(pair) = pairs
        .iter()
        .find(|pair| range_is_near_cursor(&pair.close, cursor))
    {
        return Some(pair.open.start);
    }
    if let Some(pair) = pairs
        .iter()
        .filter(|pair| pair.open.end <= cursor && cursor <= pair.close.start)
        .max_by_key(|pair| pair.open.start)
    {
        return Some(pair.close.start);
    }
    pairs
        .iter()
        .flat_map(|pair| [pair.open.start, pair.close.start])
        .filter(|offset| *offset > cursor)
        .min()
}

fn scan_bracket_pairs(text: &str, config: &EditorLanguageConfiguration) -> Vec<BracketMatch> {
    #[derive(Clone, Copy)]
    enum BracketToken {
        Open(usize),
        Close(usize),
    }

    let mut matches = Vec::new();
    let mut stack: Vec<(usize, Range<usize>)> = Vec::new();
    let mut state = LexicalState::Code;
    let delimiters = config.lexical_string_delimiters();
    let mut offset = 0;
    while offset < text.len() {
        match state {
            LexicalState::LineComment => {
                let length = next_char_len(text, offset);
                if text[offset..].starts_with('\n') {
                    state = LexicalState::Code;
                }
                offset += length;
            }
            LexicalState::BlockComment(close) => {
                if text[offset..].starts_with(close) {
                    offset += close.len();
                    state = LexicalState::Code;
                } else {
                    offset += next_char_len(text, offset);
                }
            }
            LexicalState::String(delimiter) => {
                if text[offset..].starts_with(delimiter) && !escaped_at(text, offset) {
                    offset += delimiter.len();
                    state = LexicalState::Code;
                } else {
                    offset += next_char_len(text, offset);
                }
            }
            LexicalState::Code => {
                if let Some(line_comment) = config.line_comment.as_deref()
                    && text[offset..].starts_with(line_comment)
                {
                    offset += line_comment.len();
                    state = LexicalState::LineComment;
                    continue;
                }
                if let Some(block_comment) = config.block_comment.as_ref()
                    && text[offset..].starts_with(&block_comment.open)
                {
                    offset += block_comment.open.len();
                    state = LexicalState::BlockComment(&block_comment.close);
                    continue;
                }
                if let Some(delimiter) = delimiters
                    .iter()
                    .find(|delimiter| text[offset..].starts_with(**delimiter))
                {
                    offset += delimiter.len();
                    state = LexicalState::String(delimiter);
                    continue;
                }

                let token = config
                    .brackets
                    .iter()
                    .enumerate()
                    .flat_map(|(index, pair)| {
                        [
                            (pair.open.as_str(), BracketToken::Open(index)),
                            (pair.close.as_str(), BracketToken::Close(index)),
                        ]
                    })
                    .filter(|(token, _)| text[offset..].starts_with(*token))
                    .max_by_key(|(token, _)| token.len());
                if let Some((token, BracketToken::Open(index))) = token {
                    let range = offset..offset + token.len();
                    stack.push((index, range));
                    offset += token.len();
                } else if let Some((token, BracketToken::Close(index))) = token {
                    let close = offset..offset + token.len();
                    if stack
                        .last()
                        .is_some_and(|(open_index, _)| *open_index == index)
                    {
                        let (_, open) = stack.pop().expect("checked bracket stack");
                        matches.push(BracketMatch { open, close });
                    }
                    offset += token.len();
                } else {
                    offset += next_char_len(text, offset);
                }
            }
        }
    }
    matches.sort_by_key(|pair| pair.open.start);
    matches
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LexicalState<'a> {
    Code,
    LineComment,
    BlockComment(&'a str),
    String(&'a str),
}

fn lexical_context_at(
    text: &str,
    target: usize,
    config: &EditorLanguageConfiguration,
) -> EditorTokenContext {
    let mut state = LexicalState::Code;
    let delimiters = config.lexical_string_delimiters();
    let mut offset = 0;
    while offset < target.min(text.len()) {
        match state {
            LexicalState::LineComment => {
                if text[offset..].starts_with('\n') {
                    state = LexicalState::Code;
                }
                offset += next_char_len(text, offset);
            }
            LexicalState::BlockComment(close) => {
                if text[offset..].starts_with(close) {
                    offset += close.len();
                    state = LexicalState::Code;
                } else {
                    offset += next_char_len(text, offset);
                }
            }
            LexicalState::String(delimiter) => {
                if text[offset..].starts_with(delimiter) && !escaped_at(text, offset) {
                    offset += delimiter.len();
                    state = LexicalState::Code;
                } else {
                    offset += next_char_len(text, offset);
                }
            }
            LexicalState::Code => {
                if let Some(line_comment) = config.line_comment.as_deref()
                    && text[offset..].starts_with(line_comment)
                {
                    offset += line_comment.len();
                    state = LexicalState::LineComment;
                } else if let Some(block_comment) = config.block_comment.as_ref()
                    && text[offset..].starts_with(&block_comment.open)
                {
                    offset += block_comment.open.len();
                    state = LexicalState::BlockComment(&block_comment.close);
                } else if let Some(delimiter) = delimiters
                    .iter()
                    .find(|delimiter| text[offset..].starts_with(**delimiter))
                {
                    offset += delimiter.len();
                    state = LexicalState::String(delimiter);
                } else {
                    offset += next_char_len(text, offset);
                }
            }
        }
    }
    match state {
        LexicalState::String(_) => EditorTokenContext::String,
        LexicalState::LineComment | LexicalState::BlockComment(_) => EditorTokenContext::Comment,
        LexicalState::Code => EditorTokenContext::Code,
    }
}

fn is_quote(token: &str) -> bool {
    token.ends_with('\'') || token.ends_with('"') || token.ends_with('`')
}

fn is_word_character(character: char) -> bool {
    character == '_' || character.is_alphanumeric()
}

fn escaped_at(text: &str, offset: usize) -> bool {
    let slash_count = text[..offset]
        .chars()
        .rev()
        .take_while(|character| *character == '\\')
        .count();
    slash_count % 2 == 1
}

fn next_char_len(text: &str, offset: usize) -> usize {
    text[offset..]
        .chars()
        .next()
        .map(char::len_utf8)
        .unwrap_or(1)
}

fn range_is_near_cursor(range: &Range<usize>, cursor: usize) -> bool {
    range.start <= cursor && cursor <= range.end
}

fn distance_to_range(range: &Range<usize>, cursor: usize) -> usize {
    if cursor < range.start {
        range.start - cursor
    } else if cursor > range.end {
        cursor - range.end
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rust_configuration() -> EditorLanguageConfiguration {
        EditorLanguageConfiguration::new()
            .with_brackets([("{", "}"), ("[", "]"), ("(", ")")])
            .with_auto_closing_pairs([
                EditorAutoClosingPair::new("{", "}"),
                EditorAutoClosingPair::new("[", "]"),
                EditorAutoClosingPair::new("(", ")"),
                EditorAutoClosingPair::new("\"", "\"").not_in([EditorTokenContext::String]),
            ])
            .with_surrounding_pairs([("{", "}"), ("[", "]"), ("(", ")"), ("\"", "\"")])
            .with_line_comment("//")
            .with_block_comment("/*", "*/")
            .with_string_delimiters(["\""])
    }

    #[test]
    fn auto_close_and_surround_are_configuration_driven() {
        let config = rust_configuration();
        let plan = pair_typing_plan("call", 4..4, "(", &config).expect("auto close");
        assert_eq!(plan.replacement, "()");
        assert_eq!(plan.selection_after, 5..5);
        assert_eq!(plan.tracked_pair.unwrap().close, 5..6);

        let plan = pair_typing_plan("value", 0..5, "(", &config).expect("surround");
        assert_eq!(plan.replacement, "(value)");
        assert_eq!(plan.selection_after, 1..6);
        assert!(plan.tracked_pair.is_none());
    }

    #[test]
    fn multi_character_openers_only_own_the_typed_fragment() {
        let config = EditorLanguageConfiguration::new()
            .with_auto_closing_pairs([EditorAutoClosingPair::new("${", "}")]);
        let plan = pair_typing_plan("$", 1..1, "{", &config).expect("template close");
        assert_eq!(plan.replacement, "{}");
        let tracked = plan.tracked_pair.expect("tracked close");
        assert_eq!(tracked.open, 1..2);
        assert_eq!(tracked.open_text, "{");
        assert_eq!(paired_backspace_range("${}", 2, &[tracked]), Some(1..3));
    }

    #[test]
    fn quotes_respect_word_escape_context_and_following_character_rules() {
        let config = rust_configuration();
        assert!(pair_typing_plan("word", 4..4, "\"", &config).is_none());
        assert!(pair_typing_plan("\\", 1..1, "\"", &config).is_none());
        assert!(pair_typing_plan("\"text", 5..5, "\"", &config).is_none());
        assert!(pair_typing_plan("name", 0..0, "(", &config).is_none());
        assert!(pair_typing_plan(";", 0..0, "(", &config).is_some());
    }

    #[test]
    fn tracked_pairs_shift_interior_closes_and_reject_manual_adjacency() {
        let mut tracked = vec![TrackedAutoClosingPair {
            open: 0..1,
            close: 1..2,
            open_text: "(".into(),
            close_text: ")".into(),
        }];
        adjust_tracked_pairs_for_edit(&mut tracked, &(1..1), "é".len());
        assert_eq!(tracked[0].open, 0..1);
        assert_eq!(tracked[0].close, 3..4);
        assert_eq!(paired_backspace_range("(é)", 3, &tracked), None);

        adjust_tracked_pairs_for_edit(&mut tracked, &(1..3), 0);
        assert_eq!(paired_backspace_range("()", 1, &tracked), Some(0..2));
        assert_eq!(paired_backspace_range("()", 1, &[]), None);
    }

    #[test]
    fn bracket_matching_ignores_strings_and_comments_and_navigates_nested_pairs() {
        let config = rust_configuration();
        let text = "fn main() { let value = \"}\"; /* ] */ call([1]); }";
        let open = text.find('{').unwrap();
        let close = text.rfind('}').unwrap();
        let matched = bracket_match_near(text, open + 1, &config).expect("outer match");
        assert_eq!(matched.open, open..open + 1);
        assert_eq!(matched.close, close..close + 1);

        let call_cursor = text.find("call").unwrap() + 4;
        assert_eq!(
            bracket_navigation_target(text, call_cursor, &config),
            text.find("]);").map(|offset| offset + 1)
        );
    }

    #[test]
    fn invalid_and_oversized_tokens_are_removed_from_host_configuration() {
        let config = EditorLanguageConfiguration::new()
            .with_brackets([
                EditorCharacterPair::new("", ")"),
                EditorCharacterPair::new("x".repeat(33), ")"),
                EditorCharacterPair::new("(", ")"),
            ])
            .normalized();
        assert_eq!(config.brackets(), &[EditorCharacterPair::new("(", ")")]);
    }
}
