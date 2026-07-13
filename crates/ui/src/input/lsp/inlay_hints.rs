use anyhow::Result;
use gpui::{App, Context, MouseDownEvent, MouseMoveEvent, Task, Window};
use instant::Duration;
use lsp_types::{
    Hover, HoverContents, InlayHint, InlayHintKind, InlayHintLabel, InlayHintLabelPartTooltip,
    InlayHintTooltip, MarkedString, MarkupContent, Position,
};
use ropey::Rope;

use crate::input::{InputState, Lsp, RopeExt, popovers::HoverPopover};

const INLAY_HINT_DEBOUNCE: Duration = Duration::from_millis(75);
const INLAY_HINT_VIEWPORT_MARGIN_LINES: usize = 20;
const MAX_INLAY_HINTS: usize = 1_000;
const MAX_INLAY_HINT_LABEL_CHARS: usize = 80;
const MAX_INLAY_HINT_TEXT_EDITS: usize = 128;
const MAX_INLAY_HINT_EDIT_BYTES: usize = 1_048_576;
const MAX_INLAY_HINT_TOOLTIP_CHARS: usize = 16_384;

pub trait InlayHintProvider {
    fn inlay_hints(
        &self,
        text: &Rope,
        range: lsp_types::Range,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Vec<InlayHint>>>;

    fn can_resolve_inlay_hints(&self) -> bool {
        false
    }

    fn resolve_inlay_hint(
        &self,
        _text: &Rope,
        hint: InlayHint,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Task<Result<InlayHint>> {
        Task::ready(Ok(hint))
    }

    fn activate_inlay_hint_location(
        &self,
        _location: &lsp_types::Location,
        _window: &mut Window,
        _cx: &mut Context<InputState>,
    ) -> bool {
        false
    }

    fn execute_inlay_hint_command(
        &self,
        _command: &lsp_types::Command,
        _window: &mut Window,
        _cx: &mut Context<InputState>,
    ) -> bool {
        false
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InlayHintIdentity {
    pub hint_index: usize,
    pub part_index: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PendingInlayHintAction {
    Hover(InlayHintIdentity),
    Activate(InlayHintIdentity),
    Apply(usize),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DisplayInlayHintPart {
    pub(crate) part_index: usize,
    pub(crate) label: String,
    pub(crate) interactive: bool,
    pub(crate) active: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DisplayInlayHint {
    pub(crate) hint_index: usize,
    pub(crate) buffer_offset: usize,
    pub(crate) parts: Vec<DisplayInlayHintPart>,
    pub(crate) kind: Option<InlayHintKind>,
    pub(crate) padding_left: bool,
    pub(crate) padding_right: bool,
}

fn bounded_label_parts(
    label: &InlayHintLabel,
    hint_index: usize,
    active: Option<InlayHintIdentity>,
) -> Vec<DisplayInlayHintPart> {
    let parts = match label {
        InlayHintLabel::String(label) => vec![(0, label.as_str(), false)],
        InlayHintLabel::LabelParts(parts) => parts
            .iter()
            .enumerate()
            .map(|(index, part)| {
                (
                    index,
                    part.value.as_str(),
                    part.location.is_some() || part.command.is_some(),
                )
            })
            .collect(),
    };
    let mut remaining = MAX_INLAY_HINT_LABEL_CHARS;
    let mut result = Vec::new();
    for (part_index, label, interactive) in parts {
        if remaining == 0 {
            break;
        }
        let char_count = label.chars().count();
        let take = char_count.min(remaining);
        let mut bounded = label.chars().take(take).collect::<String>();
        remaining -= take;
        if take < char_count {
            bounded.push('…');
            remaining = 0;
        }
        if bounded.is_empty() {
            continue;
        }
        result.push(DisplayInlayHintPart {
            part_index,
            label: bounded,
            interactive,
            active: interactive
                && active
                    == Some(InlayHintIdentity {
                        hint_index,
                        part_index,
                    }),
        });
    }
    result
}

fn label_has_text(label: &InlayHintLabel) -> bool {
    match label {
        InlayHintLabel::String(label) => !label.is_empty(),
        InlayHintLabel::LabelParts(parts) => {
            !parts.is_empty() && parts.iter().all(|part| !part.value.is_empty())
        }
    }
}

fn exact_scalar_offset(text: &Rope, position: Position) -> Option<usize> {
    ((position.line as usize) < text.lines_len())
        .then(|| text.position_to_offset(&position))
        .filter(|offset| text.offset_to_position(*offset) == position)
}

fn valid_inlay_hint(text: &Rope, hint: &InlayHint) -> bool {
    label_has_text(&hint.label) && exact_scalar_offset(text, hint.position).is_some()
}

fn bounded_tooltip(value: &str) -> String {
    let mut chars = value.chars();
    let mut bounded = chars
        .by_ref()
        .take(MAX_INLAY_HINT_TOOLTIP_CHARS)
        .collect::<String>();
    if chars.next().is_some() {
        bounded.push('…');
    }
    bounded
}

fn bounded_markup(mut markup: MarkupContent) -> MarkupContent {
    markup.value = bounded_tooltip(&markup.value);
    markup
}

fn part_tooltip(hint: &InlayHint, part_index: usize) -> Option<HoverContents> {
    let InlayHintLabel::LabelParts(parts) = &hint.label else {
        return None;
    };
    match parts.get(part_index)?.tooltip.as_ref()? {
        InlayHintLabelPartTooltip::String(value) => Some(HoverContents::Scalar(
            MarkedString::String(bounded_tooltip(value)),
        )),
        InlayHintLabelPartTooltip::MarkupContent(markup) => {
            Some(HoverContents::Markup(bounded_markup(markup.clone())))
        }
    }
}

fn hint_tooltip(hint: &InlayHint) -> Option<HoverContents> {
    match hint.tooltip.as_ref()? {
        InlayHintTooltip::String(value) => Some(HoverContents::Scalar(MarkedString::String(
            bounded_tooltip(value),
        ))),
        InlayHintTooltip::MarkupContent(markup) => {
            Some(HoverContents::Markup(bounded_markup(markup.clone())))
        }
    }
}

fn merge_resolved_inlay_hint(mut original: InlayHint, resolved: InlayHint) -> Option<InlayHint> {
    if original.position != resolved.position || !label_has_text(&resolved.label) {
        return None;
    }
    original.label = resolved.label;
    if resolved.tooltip.is_some() {
        original.tooltip = resolved.tooltip;
    }
    if resolved.text_edits.is_some() {
        original.text_edits = resolved.text_edits;
    }
    Some(original)
}

fn normalized_inlay_hint_text_edits(
    text: &Rope,
    edits: &[lsp_types::TextEdit],
) -> Option<Vec<(std::ops::Range<usize>, String)>> {
    if edits.is_empty() || edits.len() > MAX_INLAY_HINT_TEXT_EDITS {
        return None;
    }
    let mut total_bytes = 0usize;
    let mut normalized = Vec::with_capacity(edits.len());
    for edit in edits {
        let start = exact_scalar_offset(text, edit.range.start)?;
        let end = exact_scalar_offset(text, edit.range.end)?;
        if start > end {
            return None;
        }
        total_bytes = total_bytes.checked_add(edit.new_text.len())?;
        if total_bytes > MAX_INLAY_HINT_EDIT_BYTES {
            return None;
        }
        normalized.push((start..end, edit.new_text.clone()));
    }
    normalized.sort_by_key(|(range, _)| (range.start, range.end));
    for pair in normalized.windows(2) {
        let previous = &pair[0].0;
        let next = &pair[1].0;
        if previous.end > next.start
            || (previous.is_empty() && next.is_empty() && previous.start == next.start)
        {
            return None;
        }
    }
    normalized.reverse();
    Some(normalized)
}

impl Lsp {
    pub(crate) fn inlay_hints_for_line(
        &self,
        text: &Rope,
        buffer_line: usize,
    ) -> Vec<DisplayInlayHint> {
        let line_start = text.line_start_offset(buffer_line);
        let line_end = text.line_end_offset(buffer_line);
        self.inlay_hints
            .iter()
            .enumerate()
            .filter(|(_, hint)| hint.position.line as usize == buffer_line)
            .filter_map(|(hint_index, hint)| {
                let offset = text.position_to_offset(&hint.position);
                let parts = bounded_label_parts(&hint.label, hint_index, self.active_inlay_hint);
                (offset >= line_start && offset <= line_end && !parts.is_empty()).then(|| {
                    DisplayInlayHint {
                        hint_index,
                        buffer_offset: offset - line_start,
                        parts,
                        kind: hint.kind,
                        padding_left: hint.padding_left.unwrap_or(false),
                        padding_right: hint.padding_right.unwrap_or(false),
                    }
                })
            })
            .collect()
    }

    pub(crate) fn inlay_hints(&self) -> &[InlayHint] {
        &self.inlay_hints
    }

    pub(crate) fn invalidate_inlay_hints(&mut self) {
        self.inlay_hint_generation = self.inlay_hint_generation.wrapping_add(1);
        self.inlay_hint_range = None;
        self.inlay_hints.clear();
        self.inlay_hint_resolve_attempted.clear();
        self.inlay_hint_resolved.clear();
        self.inlay_hint_resolve_actions.clear();
        self.active_inlay_hint = None;
        self._inlay_hint_task = Task::ready(());
    }

    pub(crate) fn update_inlay_hints(
        &mut self,
        text: &Rope,
        visible_rows: std::ops::Range<usize>,
        window: &mut Window,
        cx: &mut Context<InputState>,
    ) {
        let Some(provider) = self.inlay_hint_provider.as_ref().cloned() else {
            self.invalidate_inlay_hints();
            return;
        };
        let last_line = text.lines_len().saturating_sub(1);
        let start_line = visible_rows
            .start
            .saturating_sub(INLAY_HINT_VIEWPORT_MARGIN_LINES)
            .min(last_line);
        let end_line = visible_rows
            .end
            .saturating_add(INLAY_HINT_VIEWPORT_MARGIN_LINES)
            .min(last_line);
        let end_offset = text.line_end_offset(end_line);
        let range = lsp_types::Range::new(
            Position::new(start_line as u32, 0),
            text.offset_to_position(end_offset),
        );
        if self.inlay_hint_range == Some(range) {
            return;
        }
        self.inlay_hint_range = Some(range);

        let text = text.clone();
        let input_state = cx.entity();
        self._inlay_hint_task = cx.spawn_in(window, async move |_, cx| {
            cx.background_executor().timer(INLAY_HINT_DEBOUNCE).await;
            let task = cx
                .update(|window, cx| provider.inlay_hints(&text, range, window, cx))
                .ok();
            if let Some(task) = task
                && let Ok(mut hints) = task.await
            {
                hints.retain(|hint| valid_inlay_hint(&text, hint));
                hints.truncate(MAX_INLAY_HINTS);
                hints.sort_by_key(|hint| hint.position);
                let _ = input_state.update(cx, |input_state, cx| {
                    if input_state.lsp.inlay_hint_range == Some(range) {
                        input_state.lsp.inlay_hint_generation =
                            input_state.lsp.inlay_hint_generation.wrapping_add(1);
                        input_state.lsp.inlay_hint_resolve_attempted = vec![false; hints.len()];
                        input_state.lsp.inlay_hint_resolved = vec![false; hints.len()];
                        input_state.lsp.inlay_hint_resolve_actions = vec![None; hints.len()];
                        input_state.lsp.inlay_hints = hints;
                        input_state.lsp.active_inlay_hint = None;
                        cx.notify();
                    }
                });
            }
        });
    }
}

impl InputState {
    pub fn refresh_inlay_hints(
        &mut self,
        visible_rows: std::ops::Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = self.text.clone();
        self.lsp.update_inlay_hints(&text, visible_rows, window, cx);
    }

    pub fn invalidate_inlay_hints(&mut self, cx: &mut Context<Self>) {
        self.lsp.invalidate_inlay_hints();
        cx.notify();
    }

    pub fn inlay_hints(&self) -> &[InlayHint] {
        self.lsp.inlay_hints()
    }

    pub fn active_inlay_hint(&self) -> Option<InlayHintIdentity> {
        self.lsp.active_inlay_hint
    }

    pub(crate) fn inlay_hint_for_mouse_position(
        &self,
        position: gpui::Point<gpui::Pixels>,
    ) -> Option<InlayHintIdentity> {
        let bounds = self.last_bounds.as_ref()?;
        let last_layout = self.last_layout.as_ref()?;
        let inner_position =
            position - bounds.origin - gpui::point(last_layout.line_number_width, gpui::px(0.));
        let mut y_offset = last_layout.visible_top;
        for (index, line_layout) in last_layout.lines.iter().enumerate() {
            let buffer_line = *last_layout.visible_buffer_lines.get(index)?;
            y_offset += last_layout.code_lens_height_before(buffer_line);
            let position = inner_position - gpui::point(gpui::px(0.), y_offset);
            if let Some((hint_index, part_index)) =
                line_layout.inlay_hint_for_position(position, last_layout)
            {
                return Some(InlayHintIdentity {
                    hint_index,
                    part_index,
                });
            }
            y_offset += line_layout.size(last_layout.line_height).height;
        }
        None
    }

    fn inlay_hint_anchor_range(&self, hint: &InlayHint) -> Option<std::ops::Range<usize>> {
        let offset = exact_scalar_offset(&self.text, hint.position)?;
        let next = self.next_boundary(offset);
        if next > offset {
            Some(offset..next)
        } else if offset > 0 {
            Some(self.previous_boundary(offset)..offset)
        } else {
            Some(0..0)
        }
    }

    fn show_inlay_hint_tooltip(&mut self, identity: InlayHintIdentity, cx: &mut Context<Self>) {
        let Some(hint) = self.lsp.inlay_hints.get(identity.hint_index).cloned() else {
            return;
        };
        let contents = part_tooltip(&hint, identity.part_index).or_else(|| hint_tooltip(&hint));
        let Some(contents) = contents else {
            self.hover_popover = None;
            return;
        };
        let Some(range) = self.inlay_hint_anchor_range(&hint) else {
            return;
        };
        let hover = Hover {
            contents,
            range: None,
        };
        self.hover_popover = Some(HoverPopover::new(cx.entity(), range, &hover, cx));
    }

    fn apply_inlay_hint_text_edits(
        &mut self,
        hint_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(edits) = self
            .lsp
            .inlay_hints
            .get(hint_index)
            .and_then(|hint| hint.text_edits.as_deref())
        else {
            return false;
        };
        let Some(edits) = normalized_inlay_hint_text_edits(&self.text, edits) else {
            return false;
        };
        self.start_undo_transaction();
        for (range, replacement) in edits {
            let range_utf16 = self.range_to_utf16(&range);
            self.replace_text_in_range_silent(Some(range_utf16), &replacement, window, cx);
        }
        self.end_undo_transaction();
        true
    }

    fn perform_inlay_hint_action(
        &mut self,
        action: PendingInlayHintAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match action {
            PendingInlayHintAction::Hover(identity) => {
                self.show_inlay_hint_tooltip(identity, cx);
                true
            }
            PendingInlayHintAction::Activate(identity) => {
                let Some(provider) = self.lsp.inlay_hint_provider.as_ref().cloned() else {
                    return false;
                };
                let Some(part) = self
                    .lsp
                    .inlay_hints
                    .get(identity.hint_index)
                    .and_then(|hint| match &hint.label {
                        InlayHintLabel::String(_) => None,
                        InlayHintLabel::LabelParts(parts) => parts.get(identity.part_index),
                    })
                    .cloned()
                else {
                    return false;
                };
                if let Some(location) = part.location.as_ref()
                    && provider.activate_inlay_hint_location(location, window, cx)
                {
                    return true;
                }
                part.command
                    .as_ref()
                    .is_some_and(|command| provider.execute_inlay_hint_command(command, window, cx))
            }
            PendingInlayHintAction::Apply(hint_index) => {
                self.apply_inlay_hint_text_edits(hint_index, window, cx)
            }
        }
    }

    fn resolve_or_perform_inlay_hint_action(
        &mut self,
        hint_index: usize,
        action: PendingInlayHintAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(provider) = self.lsp.inlay_hint_provider.as_ref().cloned() else {
            return false;
        };
        let Some(hint) = self.lsp.inlay_hints.get(hint_index).cloned() else {
            return false;
        };
        if !provider.can_resolve_inlay_hints()
            || self
                .lsp
                .inlay_hint_resolved
                .get(hint_index)
                .copied()
                .unwrap_or(false)
        {
            return self.perform_inlay_hint_action(action, window, cx);
        }
        let Some(attempted) = self.lsp.inlay_hint_resolve_attempted.get_mut(hint_index) else {
            return false;
        };
        if *attempted {
            if let Some(pending) = self.lsp.inlay_hint_resolve_actions.get_mut(hint_index) {
                *pending = Some(action);
            }
            return true;
        }
        *attempted = true;
        self.lsp.inlay_hint_resolve_actions[hint_index] = Some(action);
        let generation = self.lsp.inlay_hint_generation;
        let original_position = hint.position;
        let text = self.text.clone();
        let task = provider.resolve_inlay_hint(&text, hint, window, cx);
        let editor = cx.entity();
        cx.spawn_in(window, async move |_, cx| {
            let result = task.await;
            let _ = editor.update_in(cx, |editor, window, cx| {
                if editor.lsp.inlay_hint_generation != generation {
                    return;
                }
                let Some(original) = editor.lsp.inlay_hints.get(hint_index).cloned() else {
                    return;
                };
                if original.position != original_position {
                    return;
                }
                match result {
                    Ok(resolved) => {
                        let Some(merged) = merge_resolved_inlay_hint(original, resolved) else {
                            editor.lsp.inlay_hint_resolve_attempted[hint_index] = false;
                            editor.lsp.inlay_hint_resolve_actions[hint_index] = None;
                            return;
                        };
                        if !valid_inlay_hint(&editor.text, &merged) {
                            editor.lsp.inlay_hint_resolve_attempted[hint_index] = false;
                            editor.lsp.inlay_hint_resolve_actions[hint_index] = None;
                            return;
                        }
                        editor.lsp.inlay_hints[hint_index] = merged;
                        editor.lsp.inlay_hint_resolved[hint_index] = true;
                        let action = editor.lsp.inlay_hint_resolve_actions[hint_index].take();
                        if let Some(action) = action {
                            editor.perform_inlay_hint_action(action, window, cx);
                        }
                        cx.notify();
                    }
                    Err(_) => {
                        editor.lsp.inlay_hint_resolve_attempted[hint_index] = false;
                        editor.lsp.inlay_hint_resolve_actions[hint_index] = None;
                    }
                }
            });
        })
        .detach();
        true
    }

    pub fn activate_inlay_hint_at(
        &mut self,
        hint_index: usize,
        part_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let identity = InlayHintIdentity {
            hint_index,
            part_index,
        };
        self.resolve_or_perform_inlay_hint_action(
            hint_index,
            PendingInlayHintAction::Activate(identity),
            window,
            cx,
        )
    }

    pub fn apply_inlay_hint_at(
        &mut self,
        hint_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.resolve_or_perform_inlay_hint_action(
            hint_index,
            PendingInlayHintAction::Apply(hint_index),
            window,
            cx,
        )
    }

    pub(crate) fn handle_inlay_hint_mouse_move(
        &mut self,
        identity: InlayHintIdentity,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let interactive = self
            .lsp
            .inlay_hints
            .get(identity.hint_index)
            .and_then(|hint| match &hint.label {
                InlayHintLabel::String(_) => None,
                InlayHintLabel::LabelParts(parts) => parts.get(identity.part_index),
            })
            .is_some_and(|part| part.location.is_some() || part.command.is_some());
        let active = (interactive && event.modifiers.secondary()).then_some(identity);
        let changed = self.lsp.active_inlay_hint != active;
        self.lsp.active_inlay_hint = active;
        self.resolve_or_perform_inlay_hint_action(
            identity.hint_index,
            PendingInlayHintAction::Hover(identity),
            window,
            cx,
        );
        if changed {
            cx.notify();
        }
        true
    }

    pub(crate) fn clear_active_inlay_hint(&mut self) -> bool {
        self.lsp.active_inlay_hint.take().is_some()
    }

    pub(crate) fn handle_click_inlay_hint(
        &mut self,
        event: &MouseDownEvent,
        identity: InlayHintIdentity,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if event.button != gpui::MouseButton::Left {
            return false;
        }
        if event.click_count >= 2 {
            return self.apply_inlay_hint_at(identity.hint_index, window, cx);
        }
        if event.modifiers.secondary() {
            return self.activate_inlay_hint_at(
                identity.hint_index,
                identity.part_index,
                window,
                cx,
            );
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inlay_hints_keep_buffer_offsets_and_bound_labels() {
        let text = Rope::from_str("let crab = 1;\n");
        let mut lsp = Lsp::default();
        lsp.inlay_hints = vec![InlayHint {
            position: Position::new(0, 8),
            label: InlayHintLabel::String("x".repeat(100)),
            kind: Some(InlayHintKind::TYPE),
            text_edits: None,
            tooltip: None,
            padding_left: Some(true),
            padding_right: Some(false),
            data: None,
        }];

        let hints = lsp.inlay_hints_for_line(&text, 0);
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].buffer_offset, 8);
        assert!(hints[0].parts[0].label.ends_with('…'));
        assert!(hints[0].parts[0].label.chars().count() <= MAX_INLAY_HINT_LABEL_CHARS + 1);
    }

    #[test]
    fn inlay_hint_label_parts_keep_semantic_identity_and_active_state() {
        let text = Rope::from_str("let crab = 1;\n");
        let mut lsp = Lsp::default();
        lsp.inlay_hints = vec![InlayHint {
            position: Position::new(0, 8),
            label: InlayHintLabel::LabelParts(vec![
                lsp_types::InlayHintLabelPart {
                    value: ": ".to_string(),
                    ..Default::default()
                },
                lsp_types::InlayHintLabelPart {
                    value: "i32".to_string(),
                    command: Some(lsp_types::Command {
                        title: "Inspect".to_string(),
                        command: "inspect.type".to_string(),
                        arguments: None,
                    }),
                    ..Default::default()
                },
            ]),
            kind: Some(InlayHintKind::TYPE),
            text_edits: None,
            tooltip: None,
            padding_left: None,
            padding_right: None,
            data: None,
        }];
        lsp.active_inlay_hint = Some(InlayHintIdentity {
            hint_index: 0,
            part_index: 1,
        });

        let hints = lsp.inlay_hints_for_line(&text, 0);
        assert_eq!(hints[0].parts.len(), 2);
        assert_eq!(hints[0].parts[1].part_index, 1);
        assert!(hints[0].parts[1].interactive);
        assert!(hints[0].parts[1].active);
        assert!(!hints[0].parts[0].active);
    }

    #[test]
    fn resolved_inlay_hints_merge_lazy_fields_without_moving_the_anchor() {
        let original = InlayHint {
            position: Position::new(0, 4),
            label: InlayHintLabel::String(": ?".to_string()),
            kind: Some(InlayHintKind::TYPE),
            text_edits: None,
            tooltip: None,
            padding_left: Some(true),
            padding_right: Some(false),
            data: Some(serde_json::json!({ "id": 7 })),
        };
        let resolved = InlayHint {
            position: original.position,
            label: InlayHintLabel::String(": i32".to_string()),
            kind: None,
            text_edits: Some(vec![lsp_types::TextEdit {
                range: lsp_types::Range::new(Position::new(0, 4), Position::new(0, 4)),
                new_text: ": i32".to_string(),
            }]),
            tooltip: Some(InlayHintTooltip::String("type hint".to_string())),
            padding_left: None,
            padding_right: None,
            data: None,
        };

        let merged = merge_resolved_inlay_hint(original.clone(), resolved).unwrap();
        assert!(matches!(merged.label, InlayHintLabel::String(ref label) if label == ": i32"));
        assert!(merged.text_edits.is_some());
        assert!(merged.tooltip.is_some());
        assert_eq!(merged.kind, original.kind);
        assert_eq!(merged.padding_left, original.padding_left);
        assert_eq!(merged.data, original.data);

        let moved = InlayHint {
            position: Position::new(0, 5),
            ..original.clone()
        };
        assert!(merge_resolved_inlay_hint(original, moved).is_none());
    }

    #[test]
    fn inlay_hint_text_edits_are_bounded_non_overlapping_and_reverse_ordered() {
        let text = Rope::from_str("a💡bcdef\n");
        let edits = vec![
            lsp_types::TextEdit {
                range: lsp_types::Range::new(Position::new(0, 1), Position::new(0, 2)),
                new_text: "lamp".to_string(),
            },
            lsp_types::TextEdit {
                range: lsp_types::Range::new(Position::new(0, 4), Position::new(0, 6)),
                new_text: "XY".to_string(),
            },
        ];
        let normalized = normalized_inlay_hint_text_edits(&text, &edits).unwrap();
        assert_eq!(normalized[0].0, "a💡bc".len().."a💡bcde".len());
        assert_eq!(normalized[1].0, 1.."a💡".len());

        let overlapping = vec![
            lsp_types::TextEdit {
                range: lsp_types::Range::new(Position::new(0, 1), Position::new(0, 4)),
                new_text: "x".to_string(),
            },
            lsp_types::TextEdit {
                range: lsp_types::Range::new(Position::new(0, 3), Position::new(0, 5)),
                new_text: "y".to_string(),
            },
        ];
        assert!(normalized_inlay_hint_text_edits(&text, &overlapping).is_none());
    }
}
