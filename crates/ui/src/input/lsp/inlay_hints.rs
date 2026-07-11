use anyhow::Result;
use gpui::{App, Context, Task, Window};
use instant::Duration;
use lsp_types::{InlayHint, InlayHintKind, InlayHintLabel, Position};
use ropey::Rope;

use crate::input::{InputState, Lsp, RopeExt};

const INLAY_HINT_DEBOUNCE: Duration = Duration::from_millis(75);
const INLAY_HINT_VIEWPORT_MARGIN_LINES: usize = 20;
const MAX_INLAY_HINTS: usize = 1_000;
const MAX_INLAY_HINT_LABEL_CHARS: usize = 80;

pub trait InlayHintProvider {
    fn inlay_hints(
        &self,
        text: &Rope,
        range: lsp_types::Range,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Vec<InlayHint>>>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DisplayInlayHint {
    pub(crate) buffer_offset: usize,
    pub(crate) label: String,
    pub(crate) kind: Option<InlayHintKind>,
    pub(crate) padding_left: bool,
    pub(crate) padding_right: bool,
}

fn bounded_label(label: &InlayHintLabel) -> String {
    let label = match label {
        InlayHintLabel::String(label) => label.clone(),
        InlayHintLabel::LabelParts(parts) => parts
            .iter()
            .map(|part| part.value.as_str())
            .collect::<String>(),
    };
    let mut chars = label.chars();
    let mut bounded = chars
        .by_ref()
        .take(MAX_INLAY_HINT_LABEL_CHARS)
        .collect::<String>();
    if chars.next().is_some() {
        bounded.push('…');
    }
    bounded
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
            .filter(|hint| hint.position.line as usize == buffer_line)
            .filter_map(|hint| {
                let offset = text.position_to_offset(&hint.position);
                (offset >= line_start && offset <= line_end).then(|| DisplayInlayHint {
                    buffer_offset: offset - line_start,
                    label: bounded_label(&hint.label),
                    kind: hint.kind,
                    padding_left: hint.padding_left.unwrap_or(false),
                    padding_right: hint.padding_right.unwrap_or(false),
                })
            })
            .filter(|hint| !hint.label.is_empty())
            .collect()
    }

    pub(crate) fn invalidate_inlay_hints(&mut self) {
        self.inlay_hint_range = None;
        self.inlay_hints.clear();
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
                hints.truncate(MAX_INLAY_HINTS);
                hints.sort_by_key(|hint| hint.position);
                let _ = input_state.update(cx, |input_state, cx| {
                    if input_state.lsp.inlay_hint_range == Some(range) {
                        input_state.lsp.inlay_hints = hints;
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
        assert!(hints[0].label.ends_with('…'));
        assert!(hints[0].label.chars().count() <= MAX_INLAY_HINT_LABEL_CHARS + 1);
    }
}
