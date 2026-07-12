use std::{ops::Range, rc::Rc};

use gpui::{
    Action, AnyElement, App, AppContext, Context, DismissEvent, Empty, Entity, EventEmitter,
    Half as _, HighlightStyle, InteractiveElement as _, IntoElement, ParentElement, Pixels, Point,
    Render, RenderOnce, SharedString, Styled, StyledText, Subscription, Window, deferred, div,
    prelude::FluentBuilder, px, relative,
};
use lsp_types::{CompletionItem, CompletionTextEdit, InsertTextFormat};
use ropey::Rope;

const MAX_MENU_WIDTH: Pixels = px(320.);
const MAX_MENU_HEIGHT: Pixels = px(240.);
const POPOVER_GAP: Pixels = px(4.);
const MAX_COMPLETION_ITEMS: usize = 5_000;

fn completion_query_fragment(query: &str) -> &str {
    let start = query
        .char_indices()
        .rev()
        .find(|(_, character)| !(character.is_alphanumeric() || *character == '_'))
        .map(|(index, character)| index + character.len_utf8())
        .unwrap_or(0);
    &query[start..]
}

fn completion_characters_equal(left: char, right: char) -> bool {
    left == right || (left.is_ascii() && right.is_ascii() && left.eq_ignore_ascii_case(&right))
}

fn completion_match(query: &str, candidate: &str) -> Option<(i64, Vec<Range<usize>>)> {
    if query.is_empty() {
        return Some((0, Vec::new()));
    }
    let candidate_characters = candidate.char_indices().collect::<Vec<_>>();
    let mut ranges: Vec<Range<usize>> = Vec::new();
    let mut score = 0i64;
    let mut search_from = 0usize;
    let mut previous_match = None;
    for (query_index, query_character) in query.chars().enumerate() {
        let (candidate_index, &(byte_start, candidate_character)) = candidate_characters
            .iter()
            .enumerate()
            .skip(search_from)
            .find(|(_, (_, candidate_character))| {
                completion_characters_equal(query_character, *candidate_character)
            })?;
        let byte_end = byte_start + candidate_character.len_utf8();
        score += 100;
        if query_character == candidate_character {
            score += 5;
        }
        if candidate_index == 0 {
            score += 60;
        } else {
            let previous_character = candidate_characters[candidate_index - 1].1;
            if !previous_character.is_alphanumeric()
                || (previous_character.is_lowercase() && candidate_character.is_uppercase())
            {
                score += 35;
            }
        }
        if previous_match == Some(candidate_index.saturating_sub(1)) {
            score += 45;
        } else if let Some(previous_match) = previous_match {
            score -= candidate_index.saturating_sub(previous_match + 1) as i64;
        } else if query_index == 0 {
            score -= candidate_index as i64;
        }
        if let Some(last_range) = ranges.last_mut() {
            if last_range.end == byte_start {
                last_range.end = byte_end;
            } else {
                ranges.push(byte_start..byte_end);
            }
        } else {
            ranges.push(byte_start..byte_end);
        }
        previous_match = Some(candidate_index);
        search_from = candidate_index + 1;
    }
    score -= candidate_characters
        .len()
        .saturating_sub(query.chars().count()) as i64;
    if candidate.len() == query.len()
        && candidate
            .chars()
            .zip(query.chars())
            .all(|(left, right)| completion_characters_equal(left, right))
    {
        score += 300;
    }
    Some((score, ranges))
}

fn rank_completion_items(
    items: Vec<CompletionItem>,
    raw_query: &str,
) -> (Vec<CompletionItem>, usize) {
    let query = completion_query_fragment(raw_query);
    let mut ranked = items
        .into_iter()
        .take(MAX_COMPLETION_ITEMS)
        .enumerate()
        .filter_map(|(original_index, item)| {
            let candidate = item.filter_text.as_deref().unwrap_or(&item.label);
            completion_match(query, candidate).map(|(score, _)| {
                let sort_key = item
                    .sort_text
                    .as_deref()
                    .unwrap_or(&item.label)
                    .to_lowercase();
                let label_key = item.label.to_lowercase();
                (item, score, sort_key, label_key, original_index)
            })
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        right
            .1
            .cmp(&left.1)
            .then_with(|| left.2.cmp(&right.2))
            .then_with(|| left.3.cmp(&right.3))
            .then_with(|| left.4.cmp(&right.4))
    });
    let selected_index = ranked
        .iter()
        .position(|(item, _, _, _, _)| item.preselect.unwrap_or(false))
        .unwrap_or(0);
    (
        ranked.into_iter().map(|(item, _, _, _, _)| item).collect(),
        selected_index,
    )
}

use crate::{
    ActiveTheme, IndexPath, Selectable, actions, h_flex,
    input::{
        self, InputState, RopeExt,
        popovers::{editor_popover, render_markdown},
        snippet::parse_snippet,
    },
    label::Label,
    list::{List, ListDelegate, ListEvent, ListState},
};

struct ContextMenuDelegate {
    query: SharedString,
    menu: Entity<CompletionMenu>,
    items: Vec<Rc<CompletionItem>>,
    selected_ix: usize,
}

fn primary_completion_edit(
    text: &Rope,
    trigger_range: std::ops::Range<usize>,
    item: &CompletionItem,
) -> (std::ops::Range<usize>, String) {
    let mut range = trigger_range;
    let mut new_text = item
        .insert_text
        .clone()
        .unwrap_or_else(|| item.label.clone());
    if let Some(text_edit) = item.text_edit.as_ref() {
        match text_edit {
            CompletionTextEdit::Edit(edit) => {
                new_text = edit.new_text.clone();
                range.start = text.position_to_offset(&edit.range.start);
                range.end = text.position_to_offset(&edit.range.end);
            }
            CompletionTextEdit::InsertAndReplace(edit) => {
                new_text = edit.new_text.clone();
                range.start = text.position_to_offset(&edit.replace.start);
                range.end = text.position_to_offset(&edit.replace.end);
            }
        }
    }
    (range, new_text)
}

fn should_insert_commit_character(
    text: &Rope,
    cursor: usize,
    completion_text: &str,
    character: &str,
) -> bool {
    let mut characters = character.chars();
    let Some(character) = characters.next() else {
        return false;
    };
    characters.next().is_none()
        && !completion_text.ends_with(character)
        && text.char_at(cursor) != Some(character)
}

impl ContextMenuDelegate {
    fn set_items(&mut self, items: Vec<CompletionItem>) {
        self.items = items.into_iter().map(Rc::new).collect();
        self.selected_ix = 0;
    }

    fn selected_item(&self) -> Option<&Rc<CompletionItem>> {
        self.items.get(self.selected_ix)
    }
}

#[derive(IntoElement)]
struct CompletionMenuItem {
    ix: usize,
    item: Rc<CompletionItem>,
    children: Vec<AnyElement>,
    selected: bool,
    highlight_prefix: SharedString,
}

impl CompletionMenuItem {
    fn new(ix: usize, item: Rc<CompletionItem>) -> Self {
        Self {
            ix,
            item,
            children: vec![],
            selected: false,
            highlight_prefix: "".into(),
        }
    }

    fn highlight_prefix(mut self, s: impl Into<SharedString>) -> Self {
        self.highlight_prefix = s.into();
        self
    }
}
impl Selectable for CompletionMenuItem {
    fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    fn is_selected(&self) -> bool {
        self.selected
    }
}

impl ParentElement for CompletionMenuItem {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}
impl RenderOnce for CompletionMenuItem {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let item = self.item;

        let deprecated = item.deprecated.unwrap_or(false);
        let highlight_query = completion_query_fragment(&self.highlight_prefix);
        let highlights = completion_match(highlight_query, &item.label)
            .map(|(_, ranges)| {
                ranges
                    .into_iter()
                    .map(|range| {
                        (
                            range,
                            HighlightStyle {
                                color: Some(cx.theme().blue),
                                ..Default::default()
                            },
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        h_flex()
            .id(self.ix)
            .gap_2()
            .p_1()
            .text_xs()
            .line_height(relative(1.))
            .rounded(cx.theme().radius.half())
            .when(item.deprecated.unwrap_or(false), |this| this.line_through())
            .hover(|this| this.bg(cx.theme().accent.opacity(0.8)))
            .when(self.selected, |this| {
                this.bg(cx.theme().tokens.accent)
                    .text_color(cx.theme().accent_foreground)
            })
            .child(div().child(StyledText::new(item.label.clone()).with_highlights(highlights)))
            .when(item.detail.is_some(), |this| {
                this.child(
                    Label::new(item.detail.as_deref().unwrap_or("").to_string())
                        .text_color(cx.theme().muted_foreground)
                        .when(deprecated, |this| this.line_through())
                        .italic(),
                )
            })
            .children(self.children)
    }
}

impl EventEmitter<DismissEvent> for ContextMenuDelegate {}

impl ListDelegate for ContextMenuDelegate {
    type Item = CompletionMenuItem;

    fn items_count(&self, _: usize, _: &gpui::App) -> usize {
        self.items.len()
    }

    fn render_item(
        &mut self,
        ix: crate::IndexPath,
        _: &mut Window,
        _: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let item = self.items.get(ix.row)?;
        Some(CompletionMenuItem::new(ix.row, item.clone()).highlight_prefix(self.query.clone()))
    }

    fn set_selected_index(
        &mut self,
        ix: Option<crate::IndexPath>,
        _: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) {
        self.selected_ix = ix.map(|i| i.row).unwrap_or(0);
        cx.notify();
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<ListState<Self>>) {
        let Some(item) = self.selected_item() else {
            return;
        };

        self.menu.update(cx, |this, cx| {
            this.select_item(&item, None, window, cx);
        });
    }
}

/// A context menu for code completions and code actions.
pub struct CompletionMenu {
    offset: usize,
    editor: Entity<InputState>,
    list: Entity<ListState<ContextMenuDelegate>>,
    open: bool,

    /// The offset of the first character that triggered the completion.
    pub(crate) trigger_start_offset: Option<usize>,
    query: SharedString,
    _subscriptions: Vec<Subscription>,
}

impl CompletionMenu {
    /// Creates a new `CompletionMenu` with the given offset and completion items.
    ///
    /// NOTE: This element should not call from InputState::new, unless that will stack overflow.
    pub(crate) fn new(
        editor: Entity<InputState>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let view = cx.entity();
            let menu = ContextMenuDelegate {
                query: SharedString::default(),
                menu: view,
                items: vec![],
                selected_ix: 0,
            };

            let list = cx.new(|cx| ListState::new(menu, window, cx));

            let _subscriptions =
                vec![
                    cx.subscribe(&list, |this: &mut Self, _, ev: &ListEvent, cx| {
                        match ev {
                            ListEvent::Confirm(_) => {
                                this.hide(cx);
                            }
                            _ => {}
                        }
                        cx.notify();
                    }),
                ];

            Self {
                offset: 0,
                editor,
                list,
                open: false,
                trigger_start_offset: None,
                query: SharedString::default(),
                _subscriptions,
            }
        })
    }

    fn select_item(
        &mut self,
        item: &CompletionItem,
        commit_character: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let item = item.clone();
        let trigger_range = self.trigger_start_offset.unwrap_or(self.offset)..self.offset;

        let editor = self.editor.clone();

        cx.spawn_in(window, async move |_, cx| {
            let resolved = editor
                .update_in(cx, |editor, window, cx| {
                    editor
                        .lsp
                        .completion_provider
                        .clone()
                        .map(|provider| provider.resolve_completion(item.clone(), window, cx))
                })
                .ok()
                .flatten();
            let item = match resolved {
                Some(task) => task.await.unwrap_or(item),
                None => item,
            };
            let accepted = item.clone();
            editor.update_in(cx, |editor, window, cx| {
                editor.completion_inserting = true;

                let (range, new_text) = primary_completion_edit(&editor.text, trigger_range, &item);
                let parsed_snippet = (item.insert_text_format == Some(InsertTextFormat::SNIPPET))
                    .then(|| parse_snippet(&new_text));
                let new_text = parsed_snippet
                    .as_ref()
                    .map(|snippet| snippet.text.clone())
                    .unwrap_or(new_text);

                let mut replacements = item
                    .additional_text_edits
                    .as_ref()
                    .into_iter()
                    .flatten()
                    .map(|edit| {
                        (
                            editor.text.position_to_offset(&edit.range.start)
                                ..editor.text.position_to_offset(&edit.range.end),
                            edit.new_text.clone(),
                            false,
                        )
                    })
                    .collect::<Vec<_>>();
                replacements.push((range.clone(), new_text.clone(), true));
                replacements.sort_by_key(|(range, _, _)| (range.start, range.end));
                let valid = replacements.iter().all(|(range, _, _)| {
                    range.start <= range.end && range.end <= editor.text.len()
                }) && replacements
                    .windows(2)
                    .all(|pair| pair[0].0.end <= pair[1].0.start);
                if valid {
                    let shift_before_primary = replacements
                        .iter()
                        .filter(|(edit_range, _, primary)| {
                            !primary && edit_range.end <= range.start
                        })
                        .map(|(edit_range, text, _)| {
                            text.len() as isize - (edit_range.end - edit_range.start) as isize
                        })
                        .sum::<isize>();
                    let snippet_start = range.start.saturating_add_signed(shift_before_primary);
                    let cursor = snippet_start + new_text.len();
                    for (edit_range, text, _) in replacements.into_iter().rev() {
                        editor.replace_text_in_range_silent(
                            Some(editor.range_to_utf16(&edit_range)),
                            &text,
                            window,
                            cx,
                        );
                    }
                    let cursor = editor
                        .text
                        .offset_to_position(cursor.min(editor.text.len()));
                    editor.set_cursor_position(cursor, window, cx);
                    if let Some(commit_character) = commit_character.as_deref() {
                        let cursor = editor.cursor();
                        if should_insert_commit_character(
                            &editor.text,
                            cursor,
                            &new_text,
                            commit_character,
                        ) {
                            editor.replace_text_in_range_silent(
                                Some(editor.range_to_utf16(&(cursor..cursor))),
                                commit_character,
                                window,
                                cx,
                            );
                        }
                    }
                    if let Some(parsed_snippet) = parsed_snippet.as_ref() {
                        editor.start_snippet_session(parsed_snippet, snippet_start, cx);
                    }
                } else {
                    editor.replace_text_in_range_silent(
                        Some(editor.range_to_utf16(&range)),
                        &new_text,
                        window,
                        cx,
                    );
                    if let Some(parsed_snippet) = parsed_snippet.as_ref() {
                        editor.start_snippet_session(parsed_snippet, range.start, cx);
                    }
                }
                editor.completion_inserting = false;
                // FIXME: Input not get the focus
                editor.focus(window, cx);
            })?;

            let accepted_task = editor
                .update_in(cx, |editor, window, cx| {
                    editor
                        .lsp
                        .completion_provider
                        .clone()
                        .map(|provider| provider.completion_accepted(accepted, window, cx))
                })
                .ok()
                .flatten();
            if let Some(task) = accepted_task {
                task.await?;
            }
            Ok::<(), anyhow::Error>(())
        })
        .detach();

        self.hide(cx);
    }

    pub(crate) fn handle_action(
        &mut self,
        action: Box<dyn Action>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.open {
            return false;
        }

        cx.propagate();
        if input::Enter::is_primary(&*action) {
            self.on_action_enter(window, cx);
        } else if action.partial_eq(&input::IndentInline) {
            self.on_action_tab(window, cx);
        } else if action.partial_eq(&input::Escape) {
            self.on_action_escape(window, cx);
        } else if action.partial_eq(&input::MoveUp) {
            self.on_action_up(window, cx);
        } else if action.partial_eq(&input::MoveDown) {
            self.on_action_down(window, cx);
        } else {
            return false;
        }

        true
    }

    fn on_action_enter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.list.read(cx).delegate().selected_item().cloned() else {
            return;
        };
        self.select_item(&item, None, window, cx);
    }

    fn on_action_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.list.read(cx).delegate().selected_item().cloned() else {
            return;
        };
        self.select_item(&item, None, window, cx);
    }

    pub(crate) fn accept_commit_character(
        &mut self,
        character: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.open || character.chars().count() != 1 {
            return false;
        }
        let Some(item) = self.list.read(cx).delegate().selected_item().cloned() else {
            return false;
        };
        if !item
            .commit_characters
            .as_ref()
            .is_some_and(|characters| characters.iter().any(|value| value == character))
        {
            return false;
        }
        self.select_item(&item, Some(character.to_string()), window, cx);
        true
    }

    fn on_action_escape(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.hide(cx);
    }

    fn on_action_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.list.update(cx, |this, cx| {
            this.on_action_select_prev(&actions::SelectUp, window, cx)
        });
    }

    fn on_action_down(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.list.update(cx, |this, cx| {
            this.on_action_select_next(&actions::SelectDown, window, cx)
        });
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    /// Hide the completion menu and reset the trigger start offset.
    pub(crate) fn hide(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        self.trigger_start_offset = None;
        cx.notify();
    }

    /// Sets the trigger start offset if it is not already set.
    pub(crate) fn update_query(&mut self, start_offset: usize, query: impl Into<SharedString>) {
        if self.trigger_start_offset.is_none() {
            self.trigger_start_offset = Some(start_offset);
        }
        self.query = query.into();
    }

    pub(crate) fn begin_query(&mut self, start_offset: usize, query: impl Into<SharedString>) {
        self.trigger_start_offset = Some(start_offset);
        self.query = query.into();
    }

    pub(crate) fn show(
        &mut self,
        offset: usize,
        items: impl Into<Vec<CompletionItem>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (items, selected_index) = rank_completion_items(items.into(), &self.query);
        if items.is_empty() {
            self.hide(cx);
            return;
        }
        self.offset = offset;
        self.open = true;
        self.list.update(cx, |this, cx| {
            let longest_ix = items
                .iter()
                .enumerate()
                .max_by_key(|(_, item)| {
                    item.label.len() + item.detail.as_ref().map(|d| d.len()).unwrap_or(0)
                })
                .map(|(ix, _)| ix)
                .unwrap_or(0);

            this.delegate_mut().query = self.query.clone();
            this.delegate_mut().set_items(items);
            this.set_selected_index(Some(IndexPath::new(selected_index)), window, cx);
            this.set_item_to_measure_index(IndexPath::new(longest_ix), window, cx);
        });

        cx.notify();
    }

    fn origin(&self, cx: &App) -> Option<Point<Pixels>> {
        let editor = self.editor.read(cx);
        let Some(last_layout) = editor.last_layout.as_ref() else {
            return None;
        };
        let Some(cursor_origin) = last_layout.cursor_bounds.map(|b| b.origin) else {
            return None;
        };

        let scroll_origin = self.editor.read(cx).scroll_handle.offset();

        Some(
            scroll_origin + cursor_origin - editor.input_bounds.origin
                + Point::new(-px(4.), last_layout.line_height + px(4.)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_text_replaces_the_typed_completion_prefix() {
        let text = Rope::from_str("value.for");
        let item = CompletionItem {
            label: "format".to_string(),
            insert_text: Some("format!".to_string()),
            ..CompletionItem::default()
        };

        assert_eq!(
            primary_completion_edit(&text, 6..9, &item),
            (6..9, "format!".to_string())
        );
    }

    #[test]
    fn explicit_text_edit_overrides_the_trigger_range() {
        let text = Rope::from_str("value.for");
        let item = CompletionItem {
            label: "format".to_string(),
            text_edit: Some(CompletionTextEdit::Edit(lsp_types::TextEdit {
                range: lsp_types::Range::new(
                    lsp_types::Position::new(0, 0),
                    lsp_types::Position::new(0, 9),
                ),
                new_text: "format!(value)".to_string(),
            })),
            ..CompletionItem::default()
        };

        assert_eq!(
            primary_completion_edit(&text, 6..9, &item),
            (0..9, "format!(value)".to_string())
        );
    }

    #[test]
    fn commit_character_is_inserted_once_and_overtypes_an_existing_character() {
        let text = Rope::from_str("format value");
        assert!(should_insert_commit_character(&text, 6, "format", "("));
        assert!(!should_insert_commit_character(&text, 6, "format(", "("));
        let existing = Rope::from_str("format(value)");
        assert!(!should_insert_commit_character(&existing, 6, "format", "("));
        assert!(!should_insert_commit_character(&text, 6, "format", "::"));
    }

    #[test]
    fn completion_ranking_filters_fuzzily_and_honors_filter_and_sort_text() {
        let (items, selected) = rank_completion_items(
            vec![
                CompletionItem {
                    label: "display_name".into(),
                    filter_text: Some("dr".into()),
                    sort_text: Some("b".into()),
                    ..CompletionItem::default()
                },
                CompletionItem {
                    label: "DebugRepresentation".into(),
                    filter_text: Some("dr".into()),
                    sort_text: Some("a".into()),
                    preselect: Some(true),
                    ..CompletionItem::default()
                },
                CompletionItem {
                    label: "unrelated".into(),
                    ..CompletionItem::default()
                },
            ],
            ".dr",
        );
        assert_eq!(
            items
                .iter()
                .map(|item| item.label.as_str())
                .collect::<Vec<_>>(),
            ["DebugRepresentation", "display_name"]
        );
        assert_eq!(selected, 0);
    }

    #[test]
    fn completion_ranking_uses_sort_text_for_equal_empty_query_scores() {
        let (items, selected) = rank_completion_items(
            vec![
                CompletionItem {
                    label: "zebra".into(),
                    sort_text: Some("002".into()),
                    ..CompletionItem::default()
                },
                CompletionItem {
                    label: "alpha".into(),
                    sort_text: Some("001".into()),
                    preselect: Some(true),
                    ..CompletionItem::default()
                },
            ],
            "::",
        );
        assert_eq!(items[0].label, "alpha");
        assert_eq!(items[1].label, "zebra");
        assert_eq!(selected, 0);
    }

    #[test]
    fn completion_match_returns_utf8_safe_highlight_ranges() {
        let (_, ranges) = completion_match("éx", "éclair_x").unwrap();
        assert_eq!(ranges, vec![0..2, 8..9]);
        for range in ranges {
            assert!("éclair_x".is_char_boundary(range.start));
            assert!("éclair_x".is_char_boundary(range.end));
        }
    }
}

impl Render for CompletionMenu {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return Empty.into_any_element();
        }

        if self.list.read(cx).delegate().items.is_empty() {
            self.open = false;
            return Empty.into_any_element();
        }

        let Some(pos) = self.origin(cx) else {
            return Empty.into_any_element();
        };

        let selected_documentation = self
            .list
            .read(cx)
            .delegate()
            .selected_item()
            .and_then(|item| item.documentation.clone());

        let max_width = MAX_MENU_WIDTH.min(window.bounds().size.width - pos.x);
        let abs_pos = self.editor.read(cx).input_bounds.origin + pos;
        let vertical_layout =
            abs_pos.x + MAX_MENU_WIDTH + POPOVER_GAP + MAX_MENU_WIDTH + POPOVER_GAP
                > window.bounds().size.width;

        deferred(
            div()
                .absolute()
                .left(pos.x)
                .top(pos.y)
                .flex()
                .flex_row()
                .gap(POPOVER_GAP)
                .items_start()
                .when(vertical_layout, |this| this.flex_col())
                .child(
                    editor_popover("completion-menu", cx)
                        .max_w(max_width)
                        .min_w(px(120.))
                        .child(List::new(&self.list).max_h(MAX_MENU_HEIGHT)),
                )
                .when_some(selected_documentation, |this, documentation| {
                    let mut doc = match documentation {
                        lsp_types::Documentation::String(s) => s.clone(),
                        lsp_types::Documentation::MarkupContent(mc) => mc.value.clone(),
                    };
                    if vertical_layout {
                        doc = doc.split("\n").next().unwrap_or_default().to_string();
                    }

                    this.child(
                        div().child(
                            editor_popover("completion-menu", cx)
                                .w(MAX_MENU_WIDTH)
                                .px_2()
                                .child(render_markdown("doc", doc, window, cx)),
                        ),
                    )
                })
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.hide(cx);
                })),
        )
        .into_any_element()
    }
}
