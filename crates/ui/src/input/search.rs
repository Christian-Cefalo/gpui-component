use aho_corasick::AhoCorasick;
use rust_i18n::t;
use std::{ops::Range, rc::Rc};

use gpui::{
    App, AppContext as _, Context, Empty, Entity, FocusHandle, Focusable, Half,
    InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Render, Styled, Subscription,
    Window, actions, div, prelude::FluentBuilder as _,
};
use regex::{Regex, RegexBuilder};
use ropey::Rope;

use crate::{
    ActiveTheme, Disableable, ElementExt, IconName, Selectable, Sizable,
    button::{Button, ButtonVariants},
    h_flex,
    input::{
        Enter, Escape, IndentInline, Input, InputEvent, InputState, Replace, RopeExt as _, Search,
        ToggleSearchCase, ToggleSearchPreserveCase, ToggleSearchRegex, ToggleSearchWholeWord,
        movement::MoveDirection,
    },
    label::Label,
    v_flex,
};

const CONTEXT: &'static str = "SearchPanel";
const MAX_SEARCH_SELECTION_SEED_BYTES: usize = 16 * 1024;
const MAX_REGEX_QUERY_BYTES: usize = 16 * 1024;
const MAX_REGEX_COMPILED_BYTES: usize = 2 * 1024 * 1024;

fn bounded_single_line_search_seed(selected_text: String) -> String {
    (!selected_text.is_empty()
        && selected_text.len() <= MAX_SEARCH_SELECTION_SEED_BYTES
        && !selected_text.contains('\n')
        && !selected_text.contains('\r'))
    .then_some(selected_text)
    .unwrap_or_default()
}

fn is_search_word_character(character: char) -> bool {
    character == '_' || character.is_alphanumeric()
}

fn is_whole_word_match(text: &str, range: &Range<usize>) -> bool {
    let matched = &text[range.clone()];
    let Some(first) = matched.chars().next() else {
        return false;
    };
    let Some(last) = matched.chars().next_back() else {
        return false;
    };

    if is_search_word_character(first)
        && text[..range.start]
            .chars()
            .next_back()
            .is_some_and(is_search_word_character)
    {
        return false;
    }
    if is_search_word_character(last)
        && text[range.end..]
            .chars()
            .next()
            .is_some_and(is_search_word_character)
    {
        return false;
    }
    true
}

fn replacement_uses_capture(reference: &str) -> bool {
    let mut characters = reference.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '$' {
            continue;
        }
        match characters.peek().copied() {
            Some('$') => {
                characters.next();
            }
            Some('&' | '{' | '0'..='9' | 'A'..='Z' | '_' | 'a'..='z') => return true,
            _ => {}
        }
    }
    false
}

fn normalize_regex_replacement(reference: &str) -> String {
    let mut normalized = String::with_capacity(reference.len());
    let mut characters = reference.chars().peekable();
    while let Some(character) = characters.next() {
        match (character, characters.peek().copied()) {
            ('$', Some('&')) => {
                characters.next();
                normalized.push_str("${0}");
            }
            ('\\', Some('n')) => {
                characters.next();
                normalized.push('\n');
            }
            ('\\', Some('t')) => {
                characters.next();
                normalized.push('\t');
            }
            ('\\', Some('\\')) => {
                characters.next();
                normalized.push('\\');
            }
            _ => normalized.push(character),
        }
    }
    normalized
}

fn replace_first_character_case(value: &str, uppercase: bool) -> String {
    let Some(first) = value.chars().next() else {
        return String::new();
    };
    let mut result = if uppercase {
        first.to_uppercase().collect::<String>()
    } else {
        first.to_lowercase().collect::<String>()
    };
    result.push_str(&value[first.len_utf8()..]);
    result
}

fn separator_case_pattern(matched: &str, replacement: &str, separator: char) -> Option<String> {
    if !matched.contains(separator) || !replacement.contains(separator) {
        return None;
    }
    let matched_parts = matched.split(separator).collect::<Vec<_>>();
    let replacement_parts = replacement.split(separator).collect::<Vec<_>>();
    if matched_parts.len() != replacement_parts.len() {
        return None;
    }
    Some(
        matched_parts
            .into_iter()
            .zip(replacement_parts)
            .map(|(matched, replacement)| preserve_replacement_case(matched, replacement))
            .collect::<Vec<_>>()
            .join(&separator.to_string()),
    )
}

fn preserve_replacement_case(matched: &str, replacement: &str) -> String {
    if matched.is_empty() || replacement.is_empty() {
        return replacement.to_string();
    }
    let hyphenated = separator_case_pattern(matched, replacement, '-');
    let underscored = separator_case_pattern(matched, replacement, '_');
    match (hyphenated, underscored) {
        (Some(value), None) | (None, Some(value)) => return value,
        _ => {}
    }

    if matched.to_uppercase() == matched {
        return replacement.to_uppercase();
    }
    if matched.to_lowercase() == matched {
        return replacement.to_lowercase();
    }

    let first = matched.chars().next().expect("matched text is nonempty");
    let first_text = first.to_string();
    let first_uppercase = first.to_uppercase().collect::<String>();
    let first_lowercase = first.to_lowercase().collect::<String>();
    if first_uppercase == first_text && first_lowercase != first_text {
        replace_first_character_case(replacement, true)
    } else if first_lowercase == first_text && first_uppercase != first_text {
        replace_first_character_case(replacement, false)
    } else {
        replacement.to_string()
    }
}

actions!(input, [Tab]);

#[derive(Debug, Clone)]
pub struct SearchMatcher {
    text: Rope,
    pub query: Option<AhoCorasick>,
    regex_query: Option<Regex>,
    whole_word: bool,
    query_error: bool,

    pub(super) matched_ranges: Rc<Vec<Range<usize>>>,
    pub(super) current_match_ix: usize,
    /// Is in replacing mode, if true, the next update will update the current match index based on matched ranges.
    replacing: bool,
}

impl SearchMatcher {
    pub fn new() -> Self {
        Self {
            text: "".into(),
            query: None,
            regex_query: None,
            whole_word: false,
            query_error: false,
            matched_ranges: Rc::new(Vec::new()),
            current_match_ix: 0,
            replacing: false,
        }
    }

    /// Update source text and re-match
    pub(crate) fn update(&mut self, text: &Rope) {
        if self.text.eq(text) {
            return;
        }

        self.text = text.clone();
        self.update_matches();
    }

    fn update_matches(&mut self) {
        let mut new_ranges = Vec::new();
        if self.query.is_some() || self.regex_query.is_some() {
            let text = self.text.to_string();
            if let Some(query) = &self.regex_query {
                for query_match in query.find_iter(&text) {
                    let range = query_match.range();
                    if !self.whole_word || is_whole_word_match(&text, &range) {
                        new_ranges.push(range);
                    }
                }
            } else if let Some(query) = &self.query {
                // FIXME: Use stream find without materializing the Rope.
                let matches = query.stream_find_iter(text.as_bytes());
                for query_match in matches {
                    let range = query_match
                        .expect("query match for select all action")
                        .range();
                    if !self.whole_word || is_whole_word_match(&text, &range) {
                        new_ranges.push(range);
                    }
                }
            }
        }
        self.matched_ranges = Rc::new(new_ranges);
        if !self.replacing {
            self.current_match_ix = 0;
        } else if self.matched_ranges.is_empty() {
            self.current_match_ix = 0;
        } else {
            self.current_match_ix = self.current_match_ix.min(self.matched_ranges.len() - 1);
        }
        self.replacing = false;
    }

    /// Update the search query and reset the current match index.
    #[cfg(test)]
    pub fn update_query(&mut self, query: &str, case_insensitive: bool) {
        self.update_query_with_options(query, case_insensitive, false, false);
    }

    pub fn update_query_with_options(
        &mut self,
        query: &str,
        case_insensitive: bool,
        whole_word: bool,
        regex_mode: bool,
    ) {
        self.query = None;
        self.regex_query = None;
        self.whole_word = whole_word;
        self.query_error = false;

        if query.len() > 0 {
            if regex_mode {
                if query.len() > MAX_REGEX_QUERY_BYTES {
                    self.query_error = true;
                } else {
                    let mut builder = RegexBuilder::new(query);
                    builder
                        .case_insensitive(case_insensitive)
                        .multi_line(true)
                        .size_limit(MAX_REGEX_COMPILED_BYTES)
                        .dfa_size_limit(MAX_REGEX_COMPILED_BYTES);
                    match builder.build() {
                        Ok(regex) => self.regex_query = Some(regex),
                        Err(_) => self.query_error = true,
                    }
                }
            } else {
                self.query = Some(
                    AhoCorasick::builder()
                        .ascii_case_insensitive(case_insensitive)
                        .build(&[query.to_string()])
                        .expect("failed to build AhoCorasick query in SearchMatcher"),
                );
            }
        }
        self.update_matches();
    }

    fn has_query_error(&self) -> bool {
        self.query_error
    }

    fn replacement_texts(
        &self,
        ranges: &[Range<usize>],
        replacement: &str,
        preserve_case: bool,
    ) -> Vec<String> {
        let text = self.text.to_string();
        let capture_replacement =
            self.regex_query.is_some() && replacement_uses_capture(replacement);
        let regex_replacement = self
            .regex_query
            .as_ref()
            .map(|_| normalize_regex_replacement(replacement));
        ranges
            .iter()
            .map(|range| {
                let expanded = self
                    .regex_query
                    .as_ref()
                    .and_then(|regex| regex.captures_at(&text, range.start))
                    .filter(|captures| {
                        captures
                            .get(0)
                            .is_some_and(|whole_match| whole_match.range() == *range)
                    })
                    .map(|captures| {
                        let mut expanded = String::new();
                        captures.expand(
                            regex_replacement.as_deref().unwrap_or(replacement),
                            &mut expanded,
                        );
                        expanded
                    })
                    .unwrap_or_else(|| replacement.to_string());
                if preserve_case && !capture_replacement {
                    preserve_replacement_case(&text[range.clone()], &expanded)
                } else {
                    expanded
                }
            })
            .collect()
    }

    /// Returns the number of matches found.
    #[allow(unused)]
    #[inline]
    fn len(&self) -> usize {
        self.matched_ranges.len()
    }

    fn peek(&self) -> Option<Range<usize>> {
        let next_match_ix = self.next_ix()?;
        self.matched_ranges.get(next_match_ix).cloned()
    }

    fn next_ix(&self) -> Option<usize> {
        if self.matched_ranges.is_empty() {
            None
        } else if self.has_next_match_without_wrap() {
            Some(self.current_match_ix + 1)
        } else {
            Some(0)
        }
    }

    fn has_next_match_without_wrap(&self) -> bool {
        self.current_match_ix < self.matched_ranges.len().saturating_sub(1)
    }

    fn label(&self) -> String {
        if self.query_error {
            return "Invalid regex".to_string();
        }
        if self.len() == 0 {
            return "0/0".to_string();
        }
        format!("{}/{}", self.current_match_ix + 1, self.len())
    }

    /// Update the current match index based on the given offset.
    fn update_cursor_by_offset(&mut self, offset: usize) {
        for (ix, range) in self.matched_ranges.iter().enumerate() {
            self.current_match_ix = ix;
            if range.contains(&offset) || range.end >= offset {
                return;
            }
        }
    }
}

impl Iterator for SearchMatcher {
    type Item = Range<usize>;

    fn next(&mut self) -> Option<Self::Item> {
        let next_match_ix = self.next_ix()?;
        self.current_match_ix = next_match_ix;
        self.matched_ranges.get(next_match_ix).cloned()
    }
}

impl DoubleEndedIterator for SearchMatcher {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.matched_ranges.is_empty() {
            return None;
        }

        if self.current_match_ix == 0 {
            self.current_match_ix = self.matched_ranges.len();
        }

        self.current_match_ix -= 1;
        let item = self.matched_ranges[self.current_match_ix].clone();

        Some(item)
    }
}

pub(super) struct SearchPanel {
    editor: Entity<InputState>,
    search_input: Entity<InputState>,
    replace_input: Entity<InputState>,
    case_insensitive: bool,
    whole_word: bool,
    regex_mode: bool,
    preserve_case: bool,
    replace_mode: bool,
    matcher: SearchMatcher,
    input_width: Pixels,

    open: bool,
    _subscriptions: Vec<Subscription>,
}

impl InputState {
    /// Update the search matcher when text changes.
    pub(super) fn update_search(&mut self, cx: &mut App) {
        let Some(search_panel) = self.search_panel.as_ref() else {
            return;
        };

        let text = self.text.clone();
        search_panel.update(cx, |this, _| {
            this.matcher.update(&text);
        });
    }

    pub(super) fn on_action_search(
        &mut self,
        _: &Search,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_search_panel(false, window, cx);
    }

    pub(super) fn on_action_replace(
        &mut self,
        _: &Replace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.replaceable {
            return;
        }
        self.open_search_panel(true, window, cx);
    }

    fn open_search_panel(
        &mut self,
        reveal_replace: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.searchable {
            return;
        }

        let search_panel = match self.search_panel.as_ref() {
            Some(panel) => panel.clone(),
            None => SearchPanel::new(cx.entity(), window, cx),
        };

        let text = self.text.clone();
        let editor = cx.entity();
        let selected_text = bounded_single_line_search_seed(self.selected_text().to_string());
        let find_input_focused = search_panel
            .read(cx)
            .search_input
            .read(cx)
            .focus_handle
            .is_focused(window);
        let focus_replace = reveal_replace && (find_input_focused || !selected_text.is_empty());
        let selected_text = Rope::from(selected_text);
        search_panel.update(cx, |this, cx| {
            this.editor = editor;
            this.matcher.update(&text);
            this.show(&selected_text, reveal_replace, focus_replace, window, cx);
        });
        self.search_panel = Some(search_panel);
        cx.notify();
    }
}

impl SearchPanel {
    fn next_scroll_direction(
        previous_match_ix: usize,
        current_match_ix: usize,
    ) -> Option<MoveDirection> {
        if current_match_ix <= previous_match_ix {
            None
        } else {
            Some(MoveDirection::Down)
        }
    }

    fn prev_scroll_direction(
        previous_match_ix: usize,
        current_match_ix: usize,
    ) -> Option<MoveDirection> {
        if current_match_ix >= previous_match_ix {
            None
        } else {
            Some(MoveDirection::Up)
        }
    }

    pub fn new(editor: Entity<InputState>, window: &mut Window, cx: &mut App) -> Entity<Self> {
        let search_input = cx.new(|cx| InputState::new(window, cx));
        let replace_input = cx.new(|cx| InputState::new(window, cx));

        cx.new(|cx| {
            let _subscriptions =
                vec![
                    cx.subscribe(&search_input, |this: &mut Self, _, ev: &InputEvent, cx| {
                        // Handle search input changes
                        match ev {
                            InputEvent::Change => {
                                this.update_search_query(cx);
                            }
                            _ => {}
                        }
                    }),
                ];

            Self {
                editor,
                search_input,
                replace_input,
                case_insensitive: true,
                whole_word: false,
                regex_mode: false,
                preserve_case: false,
                replace_mode: false,
                matcher: SearchMatcher::new(),
                open: true,
                input_width: Pixels::ZERO,
                _subscriptions,
            }
        })
    }

    pub(super) fn show(
        &mut self,
        selected_text: &Rope,
        reveal_replace: bool,
        focus_replace: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open = true;
        if reveal_replace {
            self.replace_mode = true;
        }

        self.search_input.update(cx, |this, cx| {
            if selected_text.len() > 0 {
                // Set value will emit to update_search_query
                this.set_value(selected_text.to_string(), window, cx);
            }
            this.select_all(&super::SelectAll, window, cx);
        });
        if focus_replace {
            self.replace_input.update(cx, |this, cx| {
                this.select_all(&super::SelectAll, window, cx);
            });
            self.replace_input
                .read(cx)
                .focus_handle
                .clone()
                .focus(window, cx);
        } else {
            self.search_input
                .read(cx)
                .focus_handle
                .clone()
                .focus(window, cx);
        }
    }

    fn update_search_query(&mut self, cx: &mut Context<Self>) {
        let query = self.search_input.read(cx).value();
        let visible_range_offset = self
            .editor
            .read(cx)
            .last_layout
            .as_ref()
            .map(|l| l.visible_range_offset.clone());

        self.matcher.update_query_with_options(
            query.as_str(),
            self.case_insensitive,
            self.whole_word,
            self.regex_mode,
        );

        if let Some(visible_range_offset) = visible_range_offset {
            self.matcher
                .update_cursor_by_offset(visible_range_offset.start);
        }
        cx.notify();
    }

    fn replaceable(&self, cx: &App) -> bool {
        let editor = self.editor.read(cx);
        editor.replaceable
    }

    pub(super) fn hide(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open = false;
        self.editor.read(cx).focus_handle.clone().focus(window, cx);
        cx.notify();
    }

    fn on_action_enter(&mut self, action: &Enter, window: &mut Window, cx: &mut Context<Self>) {
        if action.shift {
            self.prev(window, cx);
        } else {
            self.next(window, cx);
        }
    }

    fn on_action_escape(&mut self, _: &Escape, window: &mut Window, cx: &mut Context<Self>) {
        self.hide(window, cx);
    }

    fn on_action_tab(&mut self, _: &IndentInline, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.focus_handle(cx).focus(window, cx);
    }

    fn on_action_replace(&mut self, _: &Replace, window: &mut Window, cx: &mut Context<Self>) {
        if !self.replaceable(cx) {
            return;
        }
        self.replace_mode = true;
        self.replace_input.update(cx, |this, cx| {
            this.select_all(&super::SelectAll, window, cx);
        });
        self.replace_input
            .read(cx)
            .focus_handle
            .clone()
            .focus(window, cx);
        cx.notify();
    }

    fn on_action_toggle_case(
        &mut self,
        _: &ToggleSearchCase,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.case_insensitive = !self.case_insensitive;
        self.update_search_query(cx);
        cx.notify();
    }

    fn on_action_toggle_whole_word(
        &mut self,
        _: &ToggleSearchWholeWord,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.whole_word = !self.whole_word;
        self.update_search_query(cx);
        cx.notify();
    }

    fn on_action_toggle_regex(
        &mut self,
        _: &ToggleSearchRegex,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.regex_mode = !self.regex_mode;
        self.update_search_query(cx);
        cx.notify();
    }

    fn on_action_toggle_preserve_case(
        &mut self,
        _: &ToggleSearchPreserveCase,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.preserve_case = !self.preserve_case;
        cx.notify();
    }

    fn prev(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let previous_match_ix = self.matcher.current_match_ix;
        if let Some(range) = self.matcher.next_back() {
            let direction =
                Self::prev_scroll_direction(previous_match_ix, self.matcher.current_match_ix);
            self.editor.update(cx, |state, cx| {
                state.scroll_to(range.start, direction, cx);
            });
        }
    }

    fn next(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let previous_match_ix = self.matcher.current_match_ix;
        if let Some(range) = self.matcher.next() {
            let direction =
                Self::next_scroll_direction(previous_match_ix, self.matcher.current_match_ix);
            self.editor.update(cx, |state, cx| {
                state.scroll_to(range.end, direction, cx);
            });
        }
    }

    pub(super) fn matcher(&self) -> Option<&SearchMatcher> {
        if !self.open {
            return None;
        }

        Some(&self.matcher)
    }

    fn replace_next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.replaceable(cx) {
            self.replace_mode = false;
            cx.notify();
            return;
        }

        let new_text = self.replace_input.read(cx).value();
        self.matcher.replacing = true;
        let previous_match_ix = self.matcher.current_match_ix;
        if let Some(range) = self
            .matcher
            .matched_ranges
            .get(self.matcher.current_match_ix)
            .cloned()
        {
            let replacement = self
                .matcher
                .replacement_texts(std::slice::from_ref(&range), &new_text, self.preserve_case)
                .pop()
                .unwrap_or_else(|| new_text.to_string());
            let text_state = self.editor.clone();
            let next_match_ix = self.matcher.next_ix().unwrap_or(previous_match_ix);
            let next_range = self.matcher.peek().unwrap_or(range.clone());
            self.matcher.current_match_ix = next_match_ix;
            let direction = Self::next_scroll_direction(previous_match_ix, next_match_ix);
            cx.spawn_in(window, async move |_, cx| {
                cx.update(|window, cx| {
                    text_state.update(cx, |state, cx| {
                        let range_utf16 = state.range_to_utf16(&range);
                        state.scroll_to(next_range.end, direction, cx);
                        state.replace_text_in_range_silent(
                            Some(range_utf16),
                            replacement.as_str(),
                            window,
                            cx,
                        );
                    });
                })
            })
            .detach();
        }
    }

    fn replace_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.replaceable(cx) {
            self.replace_mode = false;
            cx.notify();
            return;
        }

        let new_text = self.replace_input.read(cx).value();
        self.matcher.replacing = true;
        let ranges = self.matcher.matched_ranges.clone();
        if ranges.is_empty() {
            return;
        }
        let replacements = self
            .matcher
            .replacement_texts(&ranges, &new_text, self.preserve_case);

        let editor = self.editor.clone();
        cx.spawn_in(window, async move |_, cx| {
            cx.update(|window, cx| {
                editor.update(cx, |state, cx| {
                    // Replace from the end to avoid messing up the ranges.
                    let mut rope = state.text.clone();
                    for (range, replacement) in ranges.iter().zip(replacements.iter()).rev() {
                        rope.replace(range.clone(), replacement);
                    }
                    state.replace_text_in_range_silent(
                        Some(0..state.text.len()),
                        &rope.to_string(),
                        window,
                        cx,
                    );
                    state.scroll_to(0, Some(MoveDirection::Down), cx);
                });
            })
        })
        .detach();
    }
}

impl Focusable for SearchPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.search_input.read(cx).focus_handle.clone()
    }
}

impl Render for SearchPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return Empty.into_any_element();
        }

        let has_matches = self.matcher.len() > 0;
        let allow_replace = self.replaceable(cx);
        if !allow_replace {
            self.replace_mode = false;
        }

        v_flex()
            .id("search-panel")
            .occlude()
            .track_focus(&self.focus_handle(cx))
            .key_context(CONTEXT)
            .on_action(cx.listener(Self::on_action_enter))
            .on_action(cx.listener(Self::on_action_escape))
            .on_action(cx.listener(Self::on_action_tab))
            .on_action(cx.listener(Self::on_action_replace))
            .on_action(cx.listener(Self::on_action_toggle_case))
            .on_action(cx.listener(Self::on_action_toggle_whole_word))
            .on_action(cx.listener(Self::on_action_toggle_regex))
            .on_action(cx.listener(Self::on_action_toggle_preserve_case))
            .font_family(cx.theme().font_family.clone())
            .items_center()
            .py_2()
            .px_3()
            .w_full()
            .gap_1()
            .bg(cx.theme().tokens.popover)
            .border_b_1()
            .rounded(cx.theme().radius.half())
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .gap_1()
                            .child(
                                Input::new(&self.search_input)
                                    .focus_bordered(false)
                                    .suffix(
                                        h_flex()
                                            .gap_0p5()
                                            .child(
                                                Button::new("match-case")
                                                    .selected(!self.case_insensitive)
                                                    .xsmall()
                                                    .compact()
                                                    .ghost()
                                                    .icon(IconName::CaseSensitive)
                                                    .tooltip("Match Case")
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.case_insensitive =
                                                            !this.case_insensitive;
                                                        this.update_search_query(cx);
                                                        cx.notify();
                                                    })),
                                            )
                                            .child(
                                                Button::new("whole-word")
                                                    .label("ab")
                                                    .selected(self.whole_word)
                                                    .xsmall()
                                                    .compact()
                                                    .ghost()
                                                    .tooltip("Match Whole Word")
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.whole_word = !this.whole_word;
                                                        this.update_search_query(cx);
                                                        cx.notify();
                                                    })),
                                            )
                                            .child(
                                                Button::new("regular-expression")
                                                    .label(".*")
                                                    .selected(self.regex_mode)
                                                    .xsmall()
                                                    .compact()
                                                    .ghost()
                                                    .tooltip("Use Regular Expression")
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.regex_mode = !this.regex_mode;
                                                        this.update_search_query(cx);
                                                        cx.notify();
                                                    })),
                                            ),
                                    )
                                    .small()
                                    .w_full()
                                    .shadow_none(),
                            )
                            .on_prepaint({
                                let view = cx.entity();
                                move |bounds, _, cx| {
                                    view.update(cx, |r, _| r.input_width = bounds.size.width)
                                }
                            }),
                    )
                    .when(allow_replace, |this| {
                        this.child(
                            Button::new("replace-mode")
                                .xsmall()
                                .ghost()
                                .icon(IconName::Replace)
                                .selected(self.replace_mode)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.replace_mode = !this.replace_mode;
                                    if this.replace_mode {
                                        this.replace_input
                                            .read(cx)
                                            .focus_handle
                                            .clone()
                                            .focus(window, cx);
                                    } else {
                                        this.search_input
                                            .read(cx)
                                            .focus_handle
                                            .clone()
                                            .focus(window, cx);
                                    }
                                    cx.notify();
                                })),
                        )
                    })
                    .child(
                        Button::new("prev")
                            .xsmall()
                            .ghost()
                            .icon(IconName::ChevronLeft)
                            .disabled(!has_matches)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.prev(window, cx);
                            })),
                    )
                    .child(
                        Button::new("next")
                            .xsmall()
                            .ghost()
                            .icon(IconName::ChevronRight)
                            .disabled(!has_matches)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.next(window, cx);
                            })),
                    )
                    .child(
                        Label::new(self.matcher.label())
                            .when(!has_matches, |this| {
                                this.text_color(cx.theme().muted_foreground)
                            })
                            .when(self.matcher.has_query_error(), |this| {
                                this.text_color(cx.theme().danger)
                            })
                            .text_left()
                            .min_w_16(),
                    )
                    .child(div().w_7())
                    .child(
                        Button::new("close")
                            .xsmall()
                            .ghost()
                            .icon(IconName::Close)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.on_action_escape(&Escape, window, cx);
                            })),
                    ),
            )
            .when(self.replace_mode && allow_replace, |this| {
                this.child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .child(
                            Input::new(&self.replace_input)
                                .focus_bordered(false)
                                .small()
                                .w(self.input_width)
                                .shadow_none(),
                        )
                        .child(
                            Button::new("preserve-case")
                                .label("AB")
                                .selected(self.preserve_case)
                                .xsmall()
                                .compact()
                                .ghost()
                                .tooltip("Preserve Case")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.preserve_case = !this.preserve_case;
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("replace-one")
                                .small()
                                .label(t!("Input.Replace"))
                                .disabled(!has_matches)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.replace_next(window, cx);
                                })),
                        )
                        .child(
                            Button::new("replace-all")
                                .small()
                                .label(t!("Input.Replace All"))
                                .disabled(!has_matches)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.replace_all(window, cx);
                                })),
                        ),
                )
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_search() {
        let mut matcher = SearchMatcher::new();
        matcher.update(&Rope::from("Hello 世界 this is a Is test string."));
        matcher.update_query("Is", true);

        assert_eq!(matcher.len(), 3);
        let mut matches = matcher.clone();
        assert_eq!(matches.current_match_ix, 0);
        assert_eq!(matches.next(), Some(18..20));
        assert_eq!(matches.next(), Some(23..25));
        assert_eq!(matches.current_match_ix, 2);
        assert_eq!(matches.next(), Some(15..17));
        assert_eq!(matches.current_match_ix, 0);
        assert_eq!(matches.next_back(), Some(23..25));
        assert_eq!(matches.current_match_ix, 2);
        assert_eq!(matches.next_back(), Some(18..20));
        assert_eq!(matches.current_match_ix, 1);
        assert_eq!(matches.next_back(), Some(15..17));
        assert_eq!(matches.current_match_ix, 0);
        assert_eq!(matches.next_back(), Some(23..25));

        matcher.update_query("IS", false);
        assert_eq!(matcher.len(), 0);
        assert_eq!(matcher.next(), None);
        assert_eq!(matcher.next_back(), None);
    }

    #[test]
    fn test_search_label() {
        let mut matcher = SearchMatcher::new();
        matcher.update(&Rope::from("Hello 世界 this is a Is test string."));
        matcher.update_query("Is", true);
        assert_eq!(matcher.label(), "1/3");
        matcher.next();
        assert_eq!(matcher.label(), "2/3");
        matcher.next();
        assert_eq!(matcher.label(), "3/3");
        matcher.next();
        assert_eq!(matcher.label(), "1/3");

        matcher.update_query("IS", false);
        assert_eq!(matcher.label(), "0/0");
    }

    #[test]
    fn search_seed_matches_single_line_editor_selection_behavior_and_is_bounded() {
        assert_eq!(
            bounded_single_line_search_seed("selected value".to_string()),
            "selected value"
        );
        assert!(bounded_single_line_search_seed("first\nsecond".to_string()).is_empty());
        assert!(bounded_single_line_search_seed("first\r\nsecond".to_string()).is_empty());
        assert!(
            bounded_single_line_search_seed("x".repeat(MAX_SEARCH_SELECTION_SEED_BYTES + 1))
                .is_empty()
        );
    }

    #[test]
    fn whole_word_search_rejects_identifier_substrings_and_handles_unicode() {
        let mut matcher = SearchMatcher::new();
        matcher.update(&Rope::from("foo foo_bar foo-bar $foo naïve naïveté"));
        matcher.update_query_with_options("foo", false, true, false);

        let text = matcher.text.to_string();
        let matches = matcher
            .matched_ranges
            .iter()
            .map(|range| text[range.clone()].to_string())
            .collect::<Vec<_>>();
        assert_eq!(matches, vec!["foo", "foo", "foo"]);

        matcher.update_query_with_options("naïve", false, true, false);
        assert_eq!(matcher.len(), 1);
        assert_eq!(&text[matcher.matched_ranges[0].clone()], "naïve");
    }

    #[test]
    fn regex_search_reports_invalid_patterns_and_expands_replacement_captures() {
        let mut matcher = SearchMatcher::new();
        matcher.update(&Rope::from("abc-123 def-456"));
        matcher.update_query_with_options(r"([a-z]+)-(\d+)", false, false, true);

        assert!(!matcher.has_query_error());
        assert_eq!(matcher.len(), 2);
        assert_eq!(
            matcher.replacement_texts(&matcher.matched_ranges, "$2:$1", true),
            vec!["123:abc", "456:def"]
        );
        assert_eq!(
            matcher.replacement_texts(&matcher.matched_ranges[..1], r"$&\n$2$$", false),
            vec!["abc-123\n123$"]
        );

        matcher.update_query_with_options("(", false, false, true);
        assert!(matcher.has_query_error());
        assert_eq!(matcher.len(), 0);
        assert_eq!(matcher.label(), "Invalid regex");

        matcher.update_query_with_options(
            &"x".repeat(MAX_REGEX_QUERY_BYTES + 1),
            false,
            false,
            true,
        );
        assert!(matcher.has_query_error());
        assert_eq!(matcher.len(), 0);
    }

    #[test]
    fn preserve_case_matches_common_editor_replacement_patterns() {
        assert_eq!(preserve_replacement_case("FOO", "bar"), "BAR");
        assert_eq!(preserve_replacement_case("foo", "BAR"), "bar");
        assert_eq!(preserve_replacement_case("Foo", "bar"), "Bar");
        assert_eq!(preserve_replacement_case("fOO", "Bar"), "bar");
        assert_eq!(
            preserve_replacement_case("HTTP_SERVER", "web_client"),
            "WEB_CLIENT"
        );
        assert_eq!(
            preserve_replacement_case("http-Server", "web-Client"),
            "web-Client"
        );
        assert_eq!(preserve_replacement_case("$foo", "Bar"), "bar");
    }

    #[test]
    fn preserve_case_does_not_override_explicit_regex_capture_replacements() {
        let mut matcher = SearchMatcher::new();
        matcher.update(&Rope::from("FOO-123"));
        matcher.update_query_with_options(r"([A-Z]+)-(\d+)", false, false, true);

        assert_eq!(
            matcher.replacement_texts(&matcher.matched_ranges, "$2:$1", true),
            vec!["123:FOO"]
        );
        assert_eq!(
            matcher.replacement_texts(&matcher.matched_ranges, "value", true),
            vec!["VALUE"]
        );
    }

    #[test]
    fn test_select_range_start() {
        let mut matcher = SearchMatcher::new();
        matcher.matched_ranges = Rc::new(vec![5..10, 15..20, 25..30]);
        matcher.update_cursor_by_offset(0);
        assert_eq!(matcher.current_match_ix, 0);

        matcher.update_cursor_by_offset(5);
        assert_eq!(matcher.current_match_ix, 0);

        matcher.update_cursor_by_offset(12);
        assert_eq!(matcher.current_match_ix, 1);

        matcher.update_cursor_by_offset(16);
        assert_eq!(matcher.current_match_ix, 1);

        matcher.update_cursor_by_offset(30);
        assert_eq!(matcher.current_match_ix, 2);

        matcher.update_cursor_by_offset(31);
        assert_eq!(matcher.current_match_ix, 2);
    }

    #[test]
    fn test_next_scroll_direction_returns_down_without_wrap() {
        assert!(matches!(
            SearchPanel::next_scroll_direction(0, 1),
            Some(MoveDirection::Down)
        ));
    }

    #[test]
    fn test_next_scroll_direction_returns_none_on_wrap() {
        assert!(SearchPanel::next_scroll_direction(2, 0).is_none());
    }

    #[test]
    fn test_next_scroll_direction_returns_none_for_single_match() {
        assert!(SearchPanel::next_scroll_direction(0, 0).is_none());
    }

    #[test]
    fn test_next_ix_wraps_to_start() {
        let mut matcher = SearchMatcher::new();
        matcher.matched_ranges = Rc::new(vec![5..10, 15..20, 25..30]);
        matcher.current_match_ix = 2;

        assert_eq!(matcher.next_ix(), Some(0));
    }

    #[test]
    fn test_prev_scroll_direction_returns_up_without_wrap() {
        assert!(matches!(
            SearchPanel::prev_scroll_direction(2, 1),
            Some(MoveDirection::Up)
        ));
    }

    #[test]
    fn test_prev_scroll_direction_returns_none_on_wrap() {
        assert!(SearchPanel::prev_scroll_direction(0, 2).is_none());
    }

    #[test]
    fn test_prev_scroll_direction_returns_none_for_single_match() {
        assert!(SearchPanel::prev_scroll_direction(0, 0).is_none());
    }

    #[test]
    fn test_update_matches_clamps_current_match_index_while_replacing() {
        let mut matcher = SearchMatcher::new();
        matcher.update(&Rope::from("foo foo foo"));
        matcher.update_query("foo", true);
        matcher.current_match_ix = 2;
        matcher.replacing = true;

        matcher.update(&Rope::from("foo xoo foo"));

        assert_eq!(matcher.len(), 2);
        assert_eq!(matcher.current_match_ix, 1);
        assert_eq!(matcher.label(), "2/2");
        assert!(!matcher.replacing);
    }
}
