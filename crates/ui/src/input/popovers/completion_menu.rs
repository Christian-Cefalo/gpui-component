use std::{ops::Range, rc::Rc};

use futures::{FutureExt as _, future::Shared};
use gpui::{
    Action, AnyElement, App, AppContext, Context, DismissEvent, Empty, Entity, EventEmitter,
    Half as _, HighlightStyle, InteractiveElement as _, IntoElement, ParentElement, Pixels, Point,
    Render, RenderOnce, SharedString, Styled, StyledText, Subscription, Task, Window, deferred,
    div, prelude::FluentBuilder, px, relative,
};
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionTextEdit, InsertTextFormat, InsertTextMode,
};
use ropey::{LineType, Rope};

type SharedCompletionResolution = Shared<futures::future::LocalBoxFuture<'static, CompletionItem>>;

const MAX_MENU_WIDTH: Pixels = px(320.);
const MAX_MENU_HEIGHT: Pixels = px(240.);
const POPOVER_GAP: Pixels = px(4.);
const MAX_COMPLETION_ITEMS: usize = 5_000;

#[derive(Default)]
struct CompletionResolutionTracker {
    generation: u64,
    resolving_index: Option<usize>,
}

impl CompletionResolutionTracker {
    fn start(&mut self, index: usize) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.resolving_index = Some(index);
        self.generation
    }

    fn cancel(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.resolving_index = None;
    }

    fn is_current(&self, generation: u64, index: usize) -> bool {
        self.generation == generation && self.resolving_index == Some(index)
    }

    fn finish(&mut self, generation: u64, index: usize) -> bool {
        if !self.is_current(generation, index) {
            return false;
        }
        self.resolving_index = None;
        true
    }
}

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
        self, CompletionInsertMode, InputState, RopeExt,
        popovers::{editor_popover, render_markdown},
        snippet::{adjust_text_indentation, parse_snippet_with_variables},
        snippet_variables::SnippetVariables,
    },
    label::Label,
    list::{List, ListDelegate, ListEvent, ListState},
};

struct ContextMenuDelegate {
    query: SharedString,
    menu: Entity<CompletionMenu>,
    items: Vec<Rc<CompletionItem>>,
    resolved: Vec<bool>,
    selected_ix: usize,
}

fn primary_completion_edit(
    text: &Rope,
    trigger_range: std::ops::Range<usize>,
    item: &CompletionItem,
    insert_mode: CompletionInsertMode,
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
                let selected_range = match insert_mode {
                    CompletionInsertMode::Insert => &edit.insert,
                    CompletionInsertMode::Replace => &edit.replace,
                };
                range.start = text.position_to_offset(&selected_range.start);
                range.end = text.position_to_offset(&selected_range.end);
            }
        }
    }
    (range, new_text)
}

fn completion_line_indentation(text: &Rope, cursor: usize) -> String {
    let cursor = cursor.min(text.len());
    let line = text.byte_to_line_idx(cursor, LineType::LF);
    let line_start = text.line_to_byte_idx(line, LineType::LF);
    text.try_slice(line_start..cursor)
        .ok()
        .map(|prefix| {
            prefix
                .chars()
                .take_while(|character| matches!(character, ' ' | '\t'))
                .collect()
        })
        .unwrap_or_default()
}

fn completion_document_line_ending(text: &Rope) -> &'static str {
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\r' if characters.peek() == Some(&'\n') => return "\r\n",
            '\r' => return "\r",
            '\n' => return "\n",
            _ => {}
        }
    }
    "\n"
}

fn completion_adjusts_indentation(mode: Option<InsertTextMode>) -> bool {
    mode != Some(InsertTextMode::AS_IS)
}

fn completion_trigger_range(
    trigger_start_offset: Option<usize>,
    response_offset: usize,
    current_cursor: usize,
) -> Option<Range<usize>> {
    let start = trigger_start_offset.unwrap_or(response_offset);
    (current_cursor >= start).then_some(start..current_cursor)
}

fn completion_item_display_width(item: &CompletionItem) -> usize {
    item.label.len()
        + item
            .label_details
            .as_ref()
            .and_then(|details| details.detail.as_ref())
            .map_or(0, String::len)
        + item
            .label_details
            .as_ref()
            .and_then(|details| details.description.as_ref())
            .map_or(0, String::len)
        + item.detail.as_ref().map_or(0, String::len)
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
    fn set_items(&mut self, items: Vec<CompletionItem>, resolved: bool) {
        self.resolved = vec![resolved; items.len()];
        self.items = items.into_iter().map(Rc::new).collect();
        self.selected_ix = 0;
    }

    fn selected_item(&self) -> Option<&Rc<CompletionItem>> {
        self.items.get(self.selected_ix)
    }

    fn selected_item_is_resolved(&self) -> bool {
        self.resolved
            .get(self.selected_ix)
            .copied()
            .unwrap_or(false)
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
        let label_detail = item
            .label_details
            .as_ref()
            .and_then(|details| details.detail.clone());
        let label_description = item
            .label_details
            .as_ref()
            .and_then(|details| details.description.clone());
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
            .child(
                h_flex()
                    .gap_0()
                    .child(
                        div()
                            .child(StyledText::new(item.label.clone()).with_highlights(highlights)),
                    )
                    .when_some(label_detail, |this, detail| {
                        this.child(Label::new(detail).text_color(cx.theme().muted_foreground))
                    }),
            )
            .when_some(label_description, |this, description| {
                this.child(
                    Label::new(description)
                        .text_color(cx.theme().muted_foreground)
                        .italic(),
                )
            })
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
        window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) {
        self.selected_ix = ix.map(|i| i.row).unwrap_or(0);
        let menu = self.menu.clone();
        let selected_ix = ix.map(|ix| ix.row);
        window.defer(cx, move |window, cx| {
            _ = menu.update(cx, |menu, cx| {
                menu.resolve_selected_item(selected_ix, window, cx);
            });
        });
        cx.notify();
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<ListState<Self>>) {
        let Some(item) = self.selected_item().cloned() else {
            return;
        };
        let resolved = self.selected_item_is_resolved();

        self.menu.update(cx, |this, cx| {
            this.select_item(&item, resolved, None, window, cx);
        });
    }
}

/// A context menu for code completions and code actions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum CompletionMenuMode {
    #[default]
    Completion,
    SnippetChoice,
}

pub struct CompletionMenu {
    offset: usize,
    editor: Entity<InputState>,
    list: Entity<ListState<ContextMenuDelegate>>,
    open: bool,
    incomplete: bool,
    mode: CompletionMenuMode,

    /// The offset of the first character that triggered the completion.
    pub(crate) trigger_start_offset: Option<usize>,
    query: SharedString,
    resolution: CompletionResolutionTracker,
    pending_resolution: Option<SharedCompletionResolution>,
    _resolve_task: Task<anyhow::Result<()>>,
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
                resolved: vec![],
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
                incomplete: false,
                mode: CompletionMenuMode::default(),
                trigger_start_offset: None,
                query: SharedString::default(),
                resolution: CompletionResolutionTracker::default(),
                pending_resolution: None,
                _resolve_task: Task::ready(Ok(())),
                _subscriptions,
            }
        })
    }

    fn cancel_completion_resolution(&mut self) {
        self.resolution.cancel();
        self._resolve_task = Task::ready(Ok(()));
        self.pending_resolution = None;
    }

    fn resolve_selected_item(
        &mut self,
        requested_index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.mode == CompletionMenuMode::SnippetChoice {
            self.cancel_completion_resolution();
            return;
        }
        if !self.open
            || self.list.read(cx).selected_index().map(|index| index.row) != requested_index
        {
            return;
        }
        let Some(index) = requested_index else {
            self.cancel_completion_resolution();
            return;
        };
        let (item, resolved) = {
            let list = self.list.read(cx);
            let delegate = list.delegate();
            let Some(item) = delegate.items.get(index).cloned() else {
                return;
            };
            let resolved = delegate.resolved.get(index).copied().unwrap_or(false);
            (item, resolved)
        };
        if resolved {
            self.cancel_completion_resolution();
            return;
        }
        if self.resolution.resolving_index == Some(index) {
            return;
        }

        self.cancel_completion_resolution();
        let generation = self.resolution.start(index);
        let editor = self.editor.clone();
        let original = (*item).clone();
        let resolution = editor.update(cx, |editor, cx| {
            editor
                .lsp
                .completion_provider
                .clone()
                .map(|provider| provider.resolve_completion(original.clone(), window, cx))
        });
        let pending_resolution = async move {
            match resolution {
                Some(task) => task.await.unwrap_or(original),
                None => original,
            }
        }
        .boxed_local()
        .shared();
        self.pending_resolution = Some(pending_resolution.clone());
        self._resolve_task = cx.spawn_in(window, async move |menu, cx| {
            let resolved_item = pending_resolution.await;

            menu.update_in(cx, |menu, _window, cx| {
                if !menu.open
                    || !menu.resolution.is_current(generation, index)
                    || menu
                        .list
                        .read(cx)
                        .selected_index()
                        .map(|selected| selected.row)
                        != Some(index)
                {
                    return;
                }
                menu.list.update(cx, |list, cx| {
                    let delegate = list.delegate_mut();
                    if let (Some(item), Some(resolved)) = (
                        delegate.items.get_mut(index),
                        delegate.resolved.get_mut(index),
                    ) {
                        *item = Rc::new(resolved_item);
                        *resolved = true;
                    }
                    cx.notify();
                });
                menu.resolution.finish(generation, index);
                menu.pending_resolution = None;
                cx.notify();
            })?;
            Ok(())
        });
    }

    fn select_item(
        &mut self,
        item: &CompletionItem,
        already_resolved: bool,
        commit_character: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.mode == CompletionMenuMode::SnippetChoice {
            self.select_snippet_choice(item, window, cx);
            return;
        }
        let item = item.clone();
        let editor = self.editor.clone();
        let selected_index = self.list.read(cx).selected_index().map(|index| index.row);
        let pending_resolution = (!already_resolved
            && self.resolution.resolving_index == selected_index)
            .then(|| self.pending_resolution.clone())
            .flatten();
        let (expected_text, expected_cursor) = {
            let editor = editor.read(cx);
            (editor.text.clone(), editor.cursor())
        };
        let Some(trigger_range) =
            completion_trigger_range(self.trigger_start_offset, self.offset, expected_cursor)
        else {
            self.hide(cx);
            return;
        };

        cx.spawn_in(window, async move |_, cx| {
            let item = if already_resolved {
                item
            } else if let Some(pending_resolution) = pending_resolution {
                pending_resolution.await
            } else {
                let resolved =
                    editor
                        .update_in(cx, |editor, window, cx| {
                            editor.lsp.completion_provider.clone().map(|provider| {
                                provider.resolve_completion(item.clone(), window, cx)
                            })
                        })
                        .ok()
                        .flatten();
                match resolved {
                    Some(task) => task.await.unwrap_or(item),
                    None => item,
                }
            };
            let accepted = item.clone();
            let inserted = editor.update_in(cx, |editor, window, cx| {
                if editor.cursor() != expected_cursor || editor.text != expected_text {
                    return false;
                }
                editor.start_undo_transaction();
                editor.completion_inserting = true;

                let (range, new_text) = primary_completion_edit(
                    &editor.text,
                    trigger_range,
                    &item,
                    editor.lsp.completion_insert_mode,
                );
                let adjust_indentation = completion_adjusts_indentation(item.insert_text_mode);
                let base_indentation = completion_line_indentation(&editor.text, expected_cursor);
                let line_ending = completion_document_line_ending(&editor.text);
                let tab_size = editor.mode.tab_size();
                let mut parsed_snippet =
                    if item.insert_text_format == Some(InsertTextFormat::SNIPPET) {
                        let selection = editor.selected_text().to_string();
                        let clipboard = cx
                            .read_from_clipboard()
                            .and_then(|clipboard| clipboard.text());
                        let variables = SnippetVariables::for_completion(
                            &editor.snippet_variable_context,
                            &editor.text,
                            expected_cursor,
                            &selection,
                            clipboard.as_deref(),
                        );
                        Some(parse_snippet_with_variables(&new_text, &variables))
                    } else {
                        None
                    };
                if adjust_indentation {
                    if let Some(snippet) = parsed_snippet.as_mut() {
                        snippet.adjust_indentation(&base_indentation, tab_size, line_ending);
                    }
                }
                let new_text = match parsed_snippet.as_ref() {
                    Some(snippet) => snippet.text.clone(),
                    None if adjust_indentation => {
                        adjust_text_indentation(&new_text, &base_indentation, tab_size, line_ending)
                    }
                    None => new_text,
                };

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
                        editor.start_snippet_session(parsed_snippet, snippet_start, window, cx);
                    }
                } else {
                    editor.replace_text_in_range_silent(
                        Some(editor.range_to_utf16(&range)),
                        &new_text,
                        window,
                        cx,
                    );
                    if let Some(parsed_snippet) = parsed_snippet.as_ref() {
                        editor.start_snippet_session(parsed_snippet, range.start, window, cx);
                    }
                }
                editor.completion_inserting = false;
                editor.handle_signature_help_text_change(true, cx);
                editor.end_undo_transaction();
                // FIXME: Input not get the focus
                editor.focus(window, cx);
                true
            })?;
            if !inserted {
                return Ok::<(), anyhow::Error>(());
            }

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

    fn select_snippet_choice(
        &mut self,
        item: &CompletionItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let choice = item
            .insert_text
            .clone()
            .unwrap_or_else(|| item.label.clone());
        let editor = self.editor.clone();
        self.hide(cx);
        window.defer(cx, move |window, cx| {
            _ = editor.update(cx, |editor, cx| {
                editor.accept_active_snippet_choice(&choice, window, cx);
            });
        });
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
        let list = self.list.read(cx);
        let Some(item) = list.delegate().selected_item().cloned() else {
            return;
        };
        let resolved = list.delegate().selected_item_is_resolved();
        self.select_item(&item, resolved, None, window, cx);
    }

    fn on_action_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let list = self.list.read(cx);
        let Some(item) = list.delegate().selected_item().cloned() else {
            return;
        };
        let resolved = list.delegate().selected_item_is_resolved();
        self.select_item(&item, resolved, None, window, cx);
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
        let list = self.list.read(cx);
        let Some(item) = list.delegate().selected_item().cloned() else {
            return false;
        };
        let resolved = list.delegate().selected_item_is_resolved();
        if !item
            .commit_characters
            .as_ref()
            .is_some_and(|characters| characters.iter().any(|value| value == character))
        {
            return false;
        }
        self.select_item(&item, resolved, Some(character.to_string()), window, cx);
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

    pub(crate) fn is_incomplete(&self) -> bool {
        self.open && self.incomplete
    }

    pub(crate) fn is_snippet_choice(&self) -> bool {
        self.open && self.mode == CompletionMenuMode::SnippetChoice
    }

    /// Hide the completion menu and reset the trigger start offset.
    pub(crate) fn hide(&mut self, cx: &mut Context<Self>) {
        self.cancel_completion_resolution();
        self.open = false;
        self.incomplete = false;
        self.mode = CompletionMenuMode::Completion;
        self.trigger_start_offset = None;
        cx.notify();
    }

    /// Sets the trigger start offset if it is not already set.
    pub(crate) fn update_query(&mut self, start_offset: usize, query: impl Into<SharedString>) {
        let query = query.into();
        if self.query != query {
            self.cancel_completion_resolution();
        }
        if self.trigger_start_offset.is_none() {
            self.trigger_start_offset = Some(start_offset);
        }
        self.query = query;
    }

    pub(crate) fn begin_query(&mut self, start_offset: usize, query: impl Into<SharedString>) {
        self.cancel_completion_resolution();
        self.mode = CompletionMenuMode::Completion;
        self.trigger_start_offset = Some(start_offset);
        self.query = query.into();
    }

    pub(crate) fn show(
        &mut self,
        offset: usize,
        items: impl Into<Vec<CompletionItem>>,
        incomplete: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_completion_resolution();
        self.mode = CompletionMenuMode::Completion;
        let (items, selected_index) = rank_completion_items(items.into(), &self.query);
        self.show_ranked(offset, items, selected_index, false, incomplete, window, cx);
    }

    pub(crate) fn show_snippet_choices(
        &mut self,
        range: Range<usize>,
        current_value: &str,
        choices: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_completion_resolution();
        self.mode = CompletionMenuMode::SnippetChoice;
        self.trigger_start_offset = Some(range.start);
        self.query = current_value.to_string().into();
        let current_is_choice = choices.iter().any(|choice| choice == current_value);
        let items = choices
            .iter()
            .enumerate()
            .map(|(index, choice)| CompletionItem {
                label: choice.clone(),
                kind: Some(CompletionItemKind::VALUE),
                insert_text: Some(choice.clone()),
                filter_text: current_is_choice.then(|| format!("{current_value}_{choice}")),
                sort_text: Some(format!("{index:08}")),
                preselect: Some(choice == current_value),
                ..CompletionItem::default()
            })
            .collect::<Vec<_>>();
        let (items, selected_index) = rank_completion_items(items, &self.query);
        self.show_ranked(range.end, items, selected_index, true, false, window, cx);
    }

    fn show_ranked(
        &mut self,
        offset: usize,
        items: Vec<CompletionItem>,
        selected_index: usize,
        resolved: bool,
        incomplete: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if items.is_empty() {
            self.hide(cx);
            return;
        }
        self.offset = offset;
        self.open = true;
        self.incomplete = incomplete;
        self.list.update(cx, |this, cx| {
            let longest_ix = items
                .iter()
                .enumerate()
                .max_by_key(|(_, item)| completion_item_display_width(item))
                .map(|(ix, _)| ix)
                .unwrap_or(0);

            this.delegate_mut().query = self.query.clone();
            this.delegate_mut().set_items(items, resolved);
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
    use std::{
        cell::{Cell, RefCell},
        collections::VecDeque,
    };

    use gpui::{TestAppContext, VisualTestContext};

    use crate::{
        Root,
        input::{CompletionProvider, SignatureHelpProvider},
    };

    enum TestCompletionResolution {
        Ready(CompletionItem),
        Pending(async_channel::Receiver<CompletionItem>),
    }

    struct TestCompletionProvider {
        calls: Rc<Cell<usize>>,
        resolutions: RefCell<VecDeque<TestCompletionResolution>>,
    }

    struct TestSignatureHelpProvider {
        contexts: Rc<RefCell<Vec<lsp_types::SignatureHelpContext>>>,
    }

    impl SignatureHelpProvider for TestSignatureHelpProvider {
        fn trigger_characters(&self, _: &App) -> Vec<String> {
            vec!["(".into()]
        }

        fn signature_help(
            &self,
            _text: &Rope,
            _offset: usize,
            context: lsp_types::SignatureHelpContext,
            _cx: &mut Context<InputState>,
        ) -> Task<anyhow::Result<Option<lsp_types::SignatureHelp>>> {
            self.contexts.borrow_mut().push(context);
            Task::ready(Ok(None))
        }
    }

    impl CompletionProvider for TestCompletionProvider {
        fn completions(
            &self,
            _text: &Rope,
            _offset: usize,
            _trigger: lsp_types::CompletionContext,
            _window: &mut Window,
            _cx: &mut Context<InputState>,
        ) -> Task<anyhow::Result<lsp_types::CompletionResponse>> {
            Task::ready(Ok(lsp_types::CompletionResponse::Array(Vec::new())))
        }

        fn resolve_completion(
            &self,
            _item: CompletionItem,
            window: &mut Window,
            cx: &mut Context<InputState>,
        ) -> Task<anyhow::Result<CompletionItem>> {
            self.calls.set(self.calls.get() + 1);
            match self.resolutions.borrow_mut().pop_front() {
                Some(TestCompletionResolution::Ready(item)) => Task::ready(Ok(item)),
                Some(TestCompletionResolution::Pending(receiver)) => {
                    window.spawn(cx, async move |_| Ok(receiver.recv().await?))
                }
                None => Task::ready(Err(anyhow::anyhow!(
                    "completion item was resolved more than once"
                ))),
            }
        }

        fn is_completion_trigger(
            &self,
            _offset: usize,
            _new_text: &str,
            _cx: &mut Context<InputState>,
        ) -> bool {
            false
        }
    }

    fn completion_test_view(
        cx: &mut TestAppContext,
        provider: Rc<dyn CompletionProvider>,
    ) -> (
        Entity<InputState>,
        Entity<CompletionMenu>,
        gpui::WindowHandle<Root>,
    ) {
        cx.update(crate::init);
        let mut input = None;
        let mut menu = None;
        let window = cx.update(|cx| {
            cx.open_window(Default::default(), |window, cx| {
                let input_entity = cx.new(|cx| {
                    let mut state = InputState::new(window, cx).code_editor("rust");
                    state.lsp.completion_provider = Some(provider);
                    state.set_value("fo", window, cx);
                    state.set_cursor_position(lsp_types::Position::new(0, 2), window, cx);
                    state
                });
                let menu_entity = CompletionMenu::new(input_entity.clone(), window, cx);
                input = Some(input_entity.clone());
                menu = Some(menu_entity);
                cx.new(|cx| Root::new(input_entity, window, cx))
            })
            .unwrap()
        });
        (input.unwrap(), menu.unwrap(), window)
    }

    fn resolved_completion(label: &str, detail: &str, documentation: &str) -> CompletionItem {
        CompletionItem {
            label: label.into(),
            insert_text: Some(label.into()),
            detail: Some(detail.into()),
            documentation: Some(lsp_types::Documentation::String(documentation.into())),
            ..CompletionItem::default()
        }
    }

    fn resolved_format_completion() -> CompletionItem {
        resolved_completion(
            "format",
            "fn format(value: &str)",
            "Formats a value without allocating.",
        )
    }

    #[test]
    fn completion_resolution_tracker_invalidates_stale_selections() {
        let mut tracker = CompletionResolutionTracker::default();
        let first = tracker.start(2);
        assert!(tracker.is_current(first, 2));

        let second = tracker.start(4);
        assert!(!tracker.is_current(first, 2));
        assert!(tracker.is_current(second, 4));
        assert!(!tracker.finish(first, 2));
        assert!(tracker.finish(second, 4));
        assert_eq!(tracker.resolving_index, None);

        let third = tracker.start(1);
        tracker.cancel();
        assert!(!tracker.is_current(third, 1));
    }

    #[gpui::test]
    fn focused_completion_resolves_once_and_refreshes_visible_details(cx: &mut TestAppContext) {
        let calls = Rc::new(Cell::new(0));
        let provider = Rc::new(TestCompletionProvider {
            calls: calls.clone(),
            resolutions: RefCell::new(VecDeque::from([TestCompletionResolution::Ready(
                resolved_format_completion(),
            )])),
        });
        let (input, menu, window) = completion_test_view(cx, provider);
        let mut cx = VisualTestContext::from_window(window.into(), cx);

        cx.update(|window, cx| {
            menu.update(cx, |menu, cx| {
                menu.begin_query(0, "fo");
                menu.show(
                    2,
                    vec![CompletionItem {
                        label: "format".into(),
                        ..CompletionItem::default()
                    }],
                    false,
                    window,
                    cx,
                );
            });
        });
        cx.run_until_parked();

        cx.update(|_window, cx| {
            let list = menu.read(cx).list.clone();
            let list = list.read(cx);
            let item = list.delegate().selected_item().unwrap();
            assert_eq!(item.detail.as_deref(), Some("fn format(value: &str)"));
            assert_eq!(
                item.documentation,
                Some(lsp_types::Documentation::String(
                    "Formats a value without allocating.".into()
                ))
            );
            assert!(list.delegate().selected_item_is_resolved());
            assert_eq!(calls.get(), 1);
        });

        cx.update(|window, cx| {
            menu.update(cx, |menu, cx| menu.on_action_enter(window, cx));
        });
        cx.run_until_parked();
        assert_eq!(calls.get(), 1);
        input.read_with(&cx, |input, _| assert_eq!(input.value(), "format"));
    }

    #[gpui::test]
    fn accepting_a_resolving_completion_reuses_the_in_flight_request(cx: &mut TestAppContext) {
        let calls = Rc::new(Cell::new(0));
        let (sender, receiver) = async_channel::bounded(1);
        let provider = Rc::new(TestCompletionProvider {
            calls: calls.clone(),
            resolutions: RefCell::new(VecDeque::from([TestCompletionResolution::Pending(
                receiver,
            )])),
        });
        let (input, menu, window) = completion_test_view(cx, provider);
        let mut cx = VisualTestContext::from_window(window.into(), cx);

        cx.update(|window, cx| {
            menu.update(cx, |menu, cx| {
                menu.begin_query(0, "fo");
                menu.show(
                    2,
                    vec![CompletionItem {
                        label: "format".into(),
                        ..CompletionItem::default()
                    }],
                    false,
                    window,
                    cx,
                );
            });
        });
        cx.run_until_parked();
        assert_eq!(calls.get(), 1);

        cx.update(|window, cx| {
            menu.update(cx, |menu, cx| menu.on_action_enter(window, cx));
        });
        sender.try_send(resolved_format_completion()).unwrap();
        cx.run_until_parked();

        assert_eq!(calls.get(), 1);
        input.read_with(&cx, |input, _| assert_eq!(input.value(), "format"));
    }

    #[gpui::test]
    fn accepted_completion_is_one_undo_step_separate_from_typed_prefix(cx: &mut TestAppContext) {
        let completion = CompletionItem {
            label: "format".into(),
            text_edit: Some(CompletionTextEdit::Edit(lsp_types::TextEdit {
                range: lsp_types::Range::new(
                    lsp_types::Position::new(0, 0),
                    lsp_types::Position::new(0, 2),
                ),
                new_text: "format".into(),
            })),
            additional_text_edits: Some(vec![lsp_types::TextEdit {
                range: lsp_types::Range::new(
                    lsp_types::Position::new(0, 0),
                    lsp_types::Position::new(0, 0),
                ),
                new_text: "use fmt;\n".into(),
            }]),
            ..CompletionItem::default()
        };
        let provider = Rc::new(TestCompletionProvider {
            calls: Rc::new(Cell::new(0)),
            resolutions: RefCell::new(VecDeque::from([TestCompletionResolution::Ready(
                completion.clone(),
            )])),
        });
        let (input, menu, window) = completion_test_view(cx, provider);
        let mut cx = VisualTestContext::from_window(window.into(), cx);

        cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                input.set_value("f", window, cx);
                input.set_cursor_position(lsp_types::Position::new(0, 1), window, cx);
                input.insert("o", window, cx);
            });
            menu.update(cx, |menu, cx| {
                menu.begin_query(0, "fo");
                menu.show(2, vec![completion], false, window, cx);
            });
        });
        cx.run_until_parked();

        cx.update(|window, cx| {
            menu.update(cx, |menu, cx| menu.on_action_enter(window, cx));
        });
        cx.run_until_parked();
        input.read_with(&cx, |input, _| {
            assert_eq!(input.value(), "use fmt;\nformat")
        });

        cx.update(|window, cx| {
            input.update(cx, |input, cx| input.undo(&crate::input::Undo, window, cx));
        });
        input.read_with(&cx, |input, _| assert_eq!(input.value(), "fo"));

        cx.update(|window, cx| {
            input.update(cx, |input, cx| input.undo(&crate::input::Undo, window, cx));
        });
        input.read_with(&cx, |input, _| assert_eq!(input.value(), "f"));
    }

    #[gpui::test]
    fn multiline_snippet_uses_adjusted_indentation_by_default(cx: &mut TestAppContext) {
        let completion = CompletionItem {
            label: "if".into(),
            text_edit: Some(CompletionTextEdit::Edit(lsp_types::TextEdit {
                range: lsp_types::Range::new(
                    lsp_types::Position::new(0, 4),
                    lsp_types::Position::new(0, 6),
                ),
                new_text: "if ${1:condition} {\n\t$0\n}".into(),
            })),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..CompletionItem::default()
        };
        let provider = Rc::new(TestCompletionProvider {
            calls: Rc::new(Cell::new(0)),
            resolutions: RefCell::new(VecDeque::from([TestCompletionResolution::Ready(
                completion.clone(),
            )])),
        });
        let (input, menu, window) = completion_test_view(cx, provider);
        let mut cx = VisualTestContext::from_window(window.into(), cx);

        cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                input.set_value("    if", window, cx);
                input.set_tab_size(
                    crate::input::TabSize {
                        tab_size: 4,
                        hard_tabs: false,
                    },
                    window,
                    cx,
                );
                input.set_cursor_position(lsp_types::Position::new(0, 6), window, cx);
            });
            menu.update(cx, |menu, cx| {
                menu.begin_query(4, "if");
                menu.show(6, vec![completion], false, window, cx);
            });
        });
        cx.run_until_parked();

        cx.update(|window, cx| {
            menu.update(cx, |menu, cx| menu.on_action_enter(window, cx));
        });
        cx.run_until_parked();

        input.read_with(&cx, |input, _| {
            assert_eq!(input.value(), "    if condition {\n        \n    }")
        });
    }

    #[gpui::test]
    fn accepted_snippet_resolves_document_and_dynamic_variables(cx: &mut TestAppContext) {
        let completion = CompletionItem {
            label: "if".into(),
            text_edit: Some(CompletionTextEdit::Edit(lsp_types::TextEdit {
                range: lsp_types::Range::new(
                    lsp_types::Position::new(0, 0),
                    lsp_types::Position::new(0, 2),
                ),
                new_text: "${TM_FILENAME_BASE/(.*)/${1:/upcase}/}:${TM_LINE_NUMBER}:${TM_SELECTED_TEXT:none}$0".into(),
            })),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..CompletionItem::default()
        };
        let provider = Rc::new(TestCompletionProvider {
            calls: Rc::new(Cell::new(0)),
            resolutions: RefCell::new(VecDeque::from([TestCompletionResolution::Ready(
                completion.clone(),
            )])),
        });
        let (input, menu, window) = completion_test_view(cx, provider);
        let mut cx = VisualTestContext::from_window(window.into(), cx);

        cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                input.set_value("if", window, cx);
                input.set_cursor_position(lsp_types::Position::new(0, 2), window, cx);
                input.set_snippet_variable_context(
                    crate::input::SnippetVariableContext::new("/workspace/src/main.rs")
                        .workspace_root("/workspace"),
                );
            });
            menu.update(cx, |menu, cx| {
                menu.begin_query(0, "if");
                menu.show(2, vec![completion], false, window, cx);
            });
        });
        cx.run_until_parked();

        cx.update(|window, cx| {
            menu.update(cx, |menu, cx| menu.on_action_enter(window, cx));
        });
        cx.run_until_parked();

        input.read_with(&cx, |input, _| {
            assert_eq!(input.value(), "MAIN:1:none");
        });
    }

    #[gpui::test]
    fn changing_completion_focus_rejects_the_stale_resolve_response(cx: &mut TestAppContext) {
        let calls = Rc::new(Cell::new(0));
        let (first_sender, first_receiver) = async_channel::bounded(1);
        let (second_sender, second_receiver) = async_channel::bounded(1);
        let provider = Rc::new(TestCompletionProvider {
            calls: calls.clone(),
            resolutions: RefCell::new(VecDeque::from([
                TestCompletionResolution::Pending(first_receiver),
                TestCompletionResolution::Pending(second_receiver),
            ])),
        });
        let (_input, menu, window) = completion_test_view(cx, provider);
        let mut cx = VisualTestContext::from_window(window.into(), cx);

        cx.update(|window, cx| {
            menu.update(cx, |menu, cx| {
                menu.begin_query(0, "");
                menu.show(
                    2,
                    vec![
                        CompletionItem {
                            label: "alpha".into(),
                            ..CompletionItem::default()
                        },
                        CompletionItem {
                            label: "beta".into(),
                            ..CompletionItem::default()
                        },
                    ],
                    false,
                    window,
                    cx,
                );
            });
        });
        cx.run_until_parked();
        assert_eq!(calls.get(), 1);

        cx.update(|window, cx| {
            let list = menu.read(cx).list.clone();
            list.update(cx, |list, cx| {
                list.set_selected_index(Some(IndexPath::new(1)), window, cx);
            });
        });
        cx.run_until_parked();
        assert_eq!(calls.get(), 2);

        _ = first_sender.try_send(resolved_completion(
            "alpha",
            "stale alpha detail",
            "stale alpha documentation",
        ));
        second_sender
            .try_send(resolved_completion(
                "beta",
                "current beta detail",
                "current beta documentation",
            ))
            .unwrap();
        cx.run_until_parked();

        cx.update(|_window, cx| {
            let list = menu.read(cx).list.clone();
            let list = list.read(cx);
            assert_eq!(list.delegate().selected_ix, 1);
            assert_eq!(list.delegate().items[0].detail, None);
            assert_eq!(
                list.delegate().items[1].detail.as_deref(),
                Some("current beta detail")
            );
            assert!(list.delegate().resolved[1]);
        });
    }

    #[test]
    fn insert_text_replaces_the_typed_completion_prefix() {
        let text = Rope::from_str("value.for");
        let item = CompletionItem {
            label: "format".to_string(),
            insert_text: Some("format!".to_string()),
            ..CompletionItem::default()
        };

        assert_eq!(
            primary_completion_edit(&text, 6..9, &item, CompletionInsertMode::Insert),
            (6..9, "format!".to_string())
        );
    }

    #[test]
    fn completion_indentation_mode_and_document_context_match_lsp_defaults() {
        assert!(completion_adjusts_indentation(None));
        assert!(completion_adjusts_indentation(Some(
            InsertTextMode::ADJUST_INDENTATION
        )));
        assert!(!completion_adjusts_indentation(Some(InsertTextMode::AS_IS)));

        let lf = Rope::from_str("    value\nnext");
        assert_eq!(completion_line_indentation(&lf, 9), "    ");
        assert_eq!(completion_document_line_ending(&lf), "\n");

        let crlf = Rope::from_str("\tvalue\r\nnext");
        assert_eq!(completion_line_indentation(&crlf, 6), "\t");
        assert_eq!(completion_document_line_ending(&crlf), "\r\n");
    }

    #[test]
    fn completion_acceptance_uses_the_current_query_end() {
        assert_eq!(completion_trigger_range(Some(6), 9, 12), Some(6..12));
        assert_eq!(completion_trigger_range(Some(6), 9, 5), None);
        assert_eq!(completion_trigger_range(None, 9, 12), Some(9..12));
    }

    #[test]
    fn completion_measurement_includes_label_details_and_description() {
        let item = CompletionItem {
            label: "map".into(),
            label_details: Some(lsp_types::CompletionItemLabelDetails {
                detail: Some("(callback)".into()),
                description: Some("Iterator".into()),
            }),
            detail: Some("fn map<B>".into()),
            ..CompletionItem::default()
        };
        assert_eq!(
            completion_item_display_width(&item),
            "map(callback)Iteratorfn map<B>".len()
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
            primary_completion_edit(&text, 6..9, &item, CompletionInsertMode::Insert),
            (0..9, "format!(value)".to_string())
        );
    }

    #[test]
    fn insert_replace_completion_respects_the_editor_mode() {
        let text = Rope::from_str("formatValue");
        let item = CompletionItem {
            label: "format".to_string(),
            text_edit: Some(CompletionTextEdit::InsertAndReplace(
                lsp_types::InsertReplaceEdit {
                    new_text: "format".to_string(),
                    insert: lsp_types::Range::new(
                        lsp_types::Position::new(0, 0),
                        lsp_types::Position::new(0, 3),
                    ),
                    replace: lsp_types::Range::new(
                        lsp_types::Position::new(0, 0),
                        lsp_types::Position::new(0, 11),
                    ),
                },
            )),
            ..CompletionItem::default()
        };

        assert_eq!(
            primary_completion_edit(&text, 0..3, &item, CompletionInsertMode::Insert),
            (0..3, "format".to_string())
        );
        assert_eq!(
            primary_completion_edit(&text, 0..3, &item, CompletionInsertMode::Replace),
            (0..11, "format".to_string())
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

    #[gpui::test]
    fn completion_commit_character_triggers_parameter_hints(cx: &mut TestAppContext) {
        let completion = CompletionItem {
            label: "format".into(),
            insert_text: Some("format".into()),
            commit_characters: Some(vec!["(".into()]),
            ..CompletionItem::default()
        };
        let provider = Rc::new(TestCompletionProvider {
            calls: Rc::new(Cell::new(0)),
            resolutions: RefCell::new(VecDeque::from([TestCompletionResolution::Ready(
                completion.clone(),
            )])),
        });
        let (input, menu, window) = completion_test_view(cx, provider);
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let contexts = Rc::new(RefCell::new(Vec::new()));

        cx.update(|window, cx| {
            input.update(cx, |input, _| {
                input.lsp.signature_help_provider = Some(Rc::new(TestSignatureHelpProvider {
                    contexts: contexts.clone(),
                }));
            });
            menu.update(cx, |menu, cx| {
                menu.begin_query(0, "fo");
                menu.show(2, vec![completion], false, window, cx);
            });
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            menu.update(cx, |menu, cx| {
                assert!(menu.accept_commit_character("(", window, cx));
            });
        });
        cx.run_until_parked();
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(121));
        cx.run_until_parked();

        input.read_with(&cx, |input, _| assert_eq!(input.value(), "format("));
        let contexts = contexts.borrow();
        assert_eq!(contexts.len(), 1);
        assert_eq!(
            contexts[0].trigger_kind,
            lsp_types::SignatureHelpTriggerKind::TRIGGER_CHARACTER
        );
        assert_eq!(contexts[0].trigger_character.as_deref(), Some("("));
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
