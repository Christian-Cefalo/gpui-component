use std::ops::Range as ByteRange;

use anyhow::Result;
use gpui::{App, Context, Task, Window};
use instant::Duration;
use lsp_types::{LinkedEditingRanges, Position};
use regex::Regex;
use ropey::Rope;

use crate::input::{InputState, Lsp, RopeExt};

const LINKED_EDITING_DEBOUNCE: Duration = Duration::from_millis(75);
const MAX_LINKED_EDITING_RANGES: usize = 256;
const MAX_LINKED_EDITING_RANGE_BYTES: usize = 64 * 1024;
const MAX_LINKED_EDITING_PATTERN_BYTES: usize = 4 * 1024;
const DEFAULT_LINKED_EDITING_PATTERN: &str = r"^[\p{L}\p{M}\p{N}_$:\.\-]*$";

/// Supplies ranges whose contents should be edited together.
pub trait LinkedEditingRangeProvider {
    fn linked_editing_ranges(
        &self,
        text: &Rope,
        position: Position,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Option<LinkedEditingRanges>>>;
}

/// Serializable editor state for diagnostics and automation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LinkedEditingSnapshot {
    pub ranges: Vec<lsp_types::Range>,
    pub word_pattern: Option<String>,
    pub pending: bool,
}

#[derive(Debug)]
struct ValidatedLinkedEditingRanges {
    ranges: Vec<ByteRange<usize>>,
    word_pattern: Option<String>,
    regex: Regex,
}

fn valid_scalar_offset(text: &Rope, position: Position) -> Option<usize> {
    let offset = text.position_to_offset(&position);
    (offset <= text.len() && text.offset_to_position(offset) == position).then_some(offset)
}

fn compile_word_pattern(source: Option<&str>) -> (Option<String>, Regex) {
    let source = source.filter(|source| source.len() <= MAX_LINKED_EDITING_PATTERN_BYTES);
    if let Some(source) = source
        && let Ok(regex) = Regex::new(source)
    {
        return (Some(source.to_string()), regex);
    }
    (
        None,
        Regex::new(DEFAULT_LINKED_EDITING_PATTERN)
            .expect("the built-in linked-editing pattern must compile"),
    )
}

fn regex_matches_entire_value(regex: &Regex, value: &str) -> bool {
    regex
        .find(value)
        .is_some_and(|matched| matched.start() == 0 && matched.end() == value.len())
}

fn validate_linked_editing_ranges(
    text: &Rope,
    cursor: usize,
    response: LinkedEditingRanges,
) -> Option<ValidatedLinkedEditingRanges> {
    if !(2..=MAX_LINKED_EDITING_RANGES).contains(&response.ranges.len()) {
        return None;
    }

    let mut ranges = response
        .ranges
        .into_iter()
        .map(|range| {
            let start = valid_scalar_offset(text, range.start)?;
            let end = valid_scalar_offset(text, range.end)?;
            (start < end && end.saturating_sub(start) <= MAX_LINKED_EDITING_RANGE_BYTES)
                .then_some(start..end)
        })
        .collect::<Option<Vec<_>>>()?;

    let mut ordered = ranges.clone();
    ordered.sort_by_key(|range| (range.start, range.end));
    if ordered.windows(2).any(|pair| pair[0].end > pair[1].start) {
        return None;
    }

    let reference_value = text.slice(ordered[0].clone()).to_string();
    if ordered
        .iter()
        .skip(1)
        .any(|range| text.slice(range.clone()).to_string() != reference_value)
    {
        return None;
    }

    let primary_index = ranges
        .iter()
        .position(|range| range.start <= cursor && cursor <= range.end)?;
    ranges.swap(0, primary_index);

    let (word_pattern, regex) = compile_word_pattern(response.word_pattern.as_deref());
    regex_matches_entire_value(&regex, &reference_value).then_some(ValidatedLinkedEditingRanges {
        ranges,
        word_pattern,
        regex,
    })
}

fn track_ranges_for_edit(
    ranges: &mut [ByteRange<usize>],
    edited_index: usize,
    edit: &ByteRange<usize>,
    inserted_len: usize,
) -> bool {
    let removed_len = edit.end.saturating_sub(edit.start);
    let delta = inserted_len as isize - removed_len as isize;
    for (index, range) in ranges.iter_mut().enumerate() {
        if index == edited_index {
            if edit.start < range.start || edit.end > range.end {
                return false;
            }
            range.end = range.end.saturating_add_signed(delta).max(range.start);
        } else if range.end <= edit.start {
            // Entirely before the edit.
        } else if range.start >= edit.end {
            range.start = range.start.saturating_add_signed(delta);
            range.end = range.end.saturating_add_signed(delta);
        } else {
            return false;
        }
    }
    true
}

fn ranges_do_not_overlap(ranges: &[ByteRange<usize>]) -> bool {
    let mut ordered = ranges.to_vec();
    ordered.sort_by_key(|range| (range.start, range.end));
    ordered.windows(2).all(|pair| pair[0].end <= pair[1].start)
}

fn minimal_replacement(old: &str, new: &str) -> (ByteRange<usize>, String) {
    let mut prefix = 0;
    for (old_character, new_character) in old.chars().zip(new.chars()) {
        if old_character != new_character {
            break;
        }
        prefix += old_character.len_utf8();
    }

    let old_tail = &old[prefix..];
    let new_tail = &new[prefix..];
    let mut suffix = 0;
    for (old_character, new_character) in old_tail.chars().rev().zip(new_tail.chars().rev()) {
        if old_character != new_character {
            break;
        }
        suffix += old_character.len_utf8();
    }

    let old_end = old.len().saturating_sub(suffix);
    let new_end = new.len().saturating_sub(suffix);
    (prefix..old_end, new[prefix..new_end].to_string())
}

impl Lsp {
    pub(super) fn invalidate_linked_editing_request(&mut self) {
        if !self.linked_editing_pending {
            return;
        }
        self.linked_editing_generation = self.linked_editing_generation.wrapping_add(1);
        self.linked_editing_pending = false;
        self._linked_editing_task = Task::ready(());
    }

    pub(super) fn clear_linked_editing(&mut self) {
        self.linked_editing_generation = self.linked_editing_generation.wrapping_add(1);
        self.linked_editing_ranges.clear();
        self.linked_editing_word_pattern = None;
        self.linked_editing_regex = None;
        self.linked_editing_pending = false;
    }

    fn begin_linked_editing_request(&mut self) -> u64 {
        self.linked_editing_generation = self.linked_editing_generation.wrapping_add(1);
        self.linked_editing_pending = true;
        self.linked_editing_generation
    }

    fn prepare_linked_editing_edit(
        &mut self,
        edit: &ByteRange<usize>,
        inserted_len: usize,
    ) -> bool {
        let Some(primary) = self.linked_editing_ranges.first() else {
            return false;
        };
        if edit.start < primary.start || edit.end > primary.end {
            self.clear_linked_editing();
            return false;
        }

        let mut updated = self.linked_editing_ranges.clone();
        if !track_ranges_for_edit(&mut updated, 0, edit, inserted_len)
            || !ranges_do_not_overlap(&updated)
        {
            self.clear_linked_editing();
            return false;
        }
        self.linked_editing_ranges = updated;
        true
    }

    fn track_linked_editing_edit(
        &mut self,
        edited_index: usize,
        edit: &ByteRange<usize>,
        inserted_len: usize,
    ) -> bool {
        let mut updated = self.linked_editing_ranges.clone();
        if !track_ranges_for_edit(&mut updated, edited_index, edit, inserted_len)
            || !ranges_do_not_overlap(&updated)
        {
            self.clear_linked_editing();
            return false;
        }
        self.linked_editing_ranges = updated;
        true
    }
}

impl InputState {
    /// Request linked ranges at the current caret, debounced for normal cursor movement.
    pub fn refresh_linked_editing_ranges(
        &mut self,
        force: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(provider) = self.lsp.linked_editing_range_provider.as_ref().cloned() else {
            self.cancel_linked_editing(cx);
            return;
        };
        if !self.mode.is_code_editor() || self.snippet_session.is_some() {
            self.cancel_linked_editing(cx);
            return;
        }

        let cursor = self.cursor();
        if !force
            && self
                .lsp
                .linked_editing_ranges
                .first()
                .is_some_and(|range| range.start <= cursor && cursor <= range.end)
        {
            return;
        }
        if self
            .lsp
            .linked_editing_ranges
            .first()
            .is_none_or(|range| cursor < range.start || cursor > range.end)
        {
            self.lsp.clear_linked_editing();
        }

        let text = self.text.clone();
        let position = text.offset_to_position(cursor);
        let generation = self.lsp.begin_linked_editing_request();
        let input = cx.entity();
        let immediate_request =
            force.then(|| provider.linked_editing_ranges(&text, position, window, cx));
        self.lsp._linked_editing_task = cx.spawn_in(window, async move |_, cx| {
            let request = if let Some(request) = immediate_request {
                Some(request)
            } else {
                cx.background_executor()
                    .timer(LINKED_EDITING_DEBOUNCE)
                    .await;
                cx.update(|window, cx| provider.linked_editing_ranges(&text, position, window, cx))
                    .ok()
            };
            let response = match request {
                Some(request) => request.await.ok().flatten(),
                None => None,
            };
            let _ = input.update(cx, |input, cx| {
                if input.lsp.linked_editing_generation != generation
                    || input.text != text
                    || input.cursor() != cursor
                {
                    return;
                }
                input.lsp.linked_editing_pending = false;
                let Some(validated) = response
                    .and_then(|response| validate_linked_editing_ranges(&text, cursor, response))
                else {
                    input.lsp.linked_editing_ranges.clear();
                    input.lsp.linked_editing_word_pattern = None;
                    input.lsp.linked_editing_regex = None;
                    cx.notify();
                    return;
                };
                input.lsp.linked_editing_ranges = validated.ranges;
                input.lsp.linked_editing_word_pattern = validated.word_pattern;
                input.lsp.linked_editing_regex = Some(validated.regex);
                cx.notify();
            });
        });
    }

    /// Force an immediate linked-editing request, matching VS Code's Start Linked Editing action.
    pub fn start_linked_editing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_linked_editing_ranges(true, window, cx);
    }

    /// Cancel the active request and discard all linked ranges.
    pub fn cancel_linked_editing(&mut self, cx: &mut Context<Self>) -> bool {
        let was_active =
            self.lsp.linked_editing_pending || !self.lsp.linked_editing_ranges.is_empty();
        self.lsp.clear_linked_editing();
        self.lsp._linked_editing_task = Task::ready(());
        if was_active {
            cx.notify();
        }
        was_active
    }

    /// Current linked-editing state for debug surfaces and conformance tests.
    pub fn linked_editing_snapshot(&self) -> LinkedEditingSnapshot {
        LinkedEditingSnapshot {
            ranges: self
                .lsp
                .linked_editing_ranges
                .iter()
                .map(|range| {
                    lsp_types::Range::new(
                        self.text.offset_to_position(range.start),
                        self.text.offset_to_position(range.end),
                    )
                })
                .collect(),
            word_pattern: self.lsp.linked_editing_word_pattern.clone(),
            pending: self.lsp.linked_editing_pending,
        }
    }

    pub(crate) fn linked_editing_byte_ranges(&self) -> &[ByteRange<usize>] {
        &self.lsp.linked_editing_ranges
    }

    pub(crate) fn prepare_linked_editing_edit(
        &mut self,
        edit: &ByteRange<usize>,
        inserted_len: usize,
    ) -> bool {
        if self.linked_editing_tracking_suspended {
            return false;
        }
        self.lsp.prepare_linked_editing_edit(edit, inserted_len)
    }

    pub(crate) fn synchronize_linked_editing_mirrors(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(primary) = self.lsp.linked_editing_ranges.first().cloned() else {
            return false;
        };
        if primary.end > self.text.len() {
            self.cancel_linked_editing(cx);
            return false;
        }
        let value = self.text.slice(primary).to_string();
        if !self
            .lsp
            .linked_editing_regex
            .as_ref()
            .is_some_and(|regex| regex_matches_entire_value(regex, &value))
        {
            self.cancel_linked_editing(cx);
            return false;
        }

        let mut mirror_indices = (1..self.lsp.linked_editing_ranges.len()).collect::<Vec<_>>();
        mirror_indices
            .sort_by_key(|index| std::cmp::Reverse(self.lsp.linked_editing_ranges[*index].start));
        if mirror_indices
            .iter()
            .any(|index| self.lsp.linked_editing_ranges[*index].end > self.text.len())
        {
            self.cancel_linked_editing(cx);
            return false;
        }

        let selection = self.selected_range;
        let emit_events = self.emit_events;
        self.linked_editing_tracking_suspended = true;
        self.emit_events = false;
        let mut valid = true;
        for index in mirror_indices {
            let mirror = self.lsp.linked_editing_ranges[index].clone();
            let old_value = self.text.slice(mirror.clone()).to_string();
            if old_value == value {
                continue;
            }
            let (relative_edit, replacement) = minimal_replacement(&old_value, &value);
            let edit = mirror.start + relative_edit.start..mirror.start + relative_edit.end;
            let range_utf16 = self.range_to_utf16(&edit);
            self.replace_text_in_range_silent(Some(range_utf16), &replacement, window, cx);
            if !self
                .lsp
                .track_linked_editing_edit(index, &edit, replacement.len())
            {
                valid = false;
                break;
            }
        }
        self.emit_events = emit_events;
        self.linked_editing_tracking_suspended = false;
        self.selected_range = selection;
        self.update_preferred_column();
        if !valid {
            self.cancel_linked_editing(cx);
            return false;
        }
        true
    }

    pub(crate) fn on_action_start_linked_editing(
        &mut self,
        _: &crate::input::StartLinkedEditing,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_linked_editing(window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(ranges: &[(u32, u32)], word_pattern: Option<&str>) -> LinkedEditingRanges {
        LinkedEditingRanges {
            ranges: ranges
                .iter()
                .map(|(start, end)| {
                    lsp_types::Range::new(Position::new(0, *start), Position::new(0, *end))
                })
                .collect(),
            word_pattern: word_pattern.map(str::to_string),
        }
    }

    #[test]
    fn validates_identical_non_overlapping_ranges_and_selects_the_cursor_range() {
        let text = Rope::from_str("<name></name>");
        let validated = validate_linked_editing_ranges(
            &text,
            9,
            response(&[(1, 5), (8, 12)], Some(r"^[a-z]+$")),
        )
        .unwrap();
        assert_eq!(validated.ranges, vec![8..12, 1..5]);
        assert_eq!(validated.word_pattern.as_deref(), Some(r"^[a-z]+$"));
        assert!(regex_matches_entire_value(&validated.regex, "renamed"));
        assert!(!regex_matches_entire_value(&validated.regex, "not valid"));

        let first_range_at_end = validate_linked_editing_ranges(
            &text,
            5,
            response(&[(1, 5), (8, 12)], Some(r"^[a-z]+$")),
        )
        .expect("a caret at the end of a linked range remains inside that range");
        assert_eq!(first_range_at_end.ranges, vec![1..5, 8..12]);
    }

    #[test]
    fn rejects_malformed_overlapping_different_or_unmappable_ranges() {
        let text = Rope::from_str("<name></name>");
        assert!(validate_linked_editing_ranges(&text, 2, response(&[(1, 5)], None)).is_none());
        assert!(
            validate_linked_editing_ranges(&text, 2, response(&[(1, 5), (4, 8)], None)).is_none()
        );
        assert!(
            validate_linked_editing_ranges(&text, 2, response(&[(1, 5), (7, 11)], None)).is_none()
        );
        assert!(
            validate_linked_editing_ranges(&text, 2, response(&[(1, 5), (8, 99)], None)).is_none()
        );
    }

    #[test]
    fn validates_scalar_boundaries_after_utf16_is_mapped_by_the_host() {
        let text = Rope::from_str("<🦀></🦀>");
        let validated = validate_linked_editing_ranges(
            &text,
            1,
            LinkedEditingRanges {
                ranges: vec![
                    lsp_types::Range::new(Position::new(0, 1), Position::new(0, 2)),
                    lsp_types::Range::new(Position::new(0, 5), Position::new(0, 6)),
                ],
                word_pattern: Some(r"^.$".to_string()),
            },
        )
        .unwrap();
        assert_eq!(validated.ranges, vec![1..5, 8..12]);
    }

    #[test]
    fn minimal_replacement_preserves_unicode_boundaries() {
        assert_eq!(
            minimal_replacement("alpha🦀omega", "alpha🚀omega"),
            (5..9, "🚀".to_string())
        );
        assert_eq!(
            minimal_replacement("name", "named"),
            (4..4, "d".to_string())
        );
        assert_eq!(minimal_replacement("named", "name"), (4..5, "".to_string()));
    }

    #[test]
    fn range_tracking_shifts_mirrors_and_rejects_cross_range_edits() {
        let mut ranges = vec![1..5, 8..12];
        assert!(track_ranges_for_edit(&mut ranges, 0, &(3..3), 1));
        assert_eq!(ranges, vec![1..6, 9..13]);
        assert!(!track_ranges_for_edit(&mut [1..5, 8..12], 0, &(0..2), 1));
    }
}
