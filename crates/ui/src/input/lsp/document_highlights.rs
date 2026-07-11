use anyhow::Result;
use gpui::{App, Context, Task, Window};
use instant::Duration;
use lsp_types::{DocumentHighlight, Position};
use ropey::Rope;
use std::ops::Range;

use crate::input::{InputState, Lsp, RopeExt};

pub trait DocumentHighlightProvider {
    /// Fetches read/write occurrences for the symbol at `position`.
    fn document_highlights(
        &self,
        text: &Rope,
        position: Position,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Vec<DocumentHighlight>>>;
}

impl Lsp {
    pub(crate) fn document_highlights_for_range(
        &self,
        text: &Rope,
        visible_range: &Range<usize>,
    ) -> Vec<(Range<usize>, Option<lsp_types::DocumentHighlightKind>)> {
        self.document_highlights
            .iter()
            .filter_map(|highlight| {
                if (highlight.range.start.line as usize) > visible_range.end
                    || (highlight.range.end.line as usize) < visible_range.start
                {
                    return None;
                }
                let start = text.position_to_offset(&highlight.range.start);
                let end = text.position_to_offset(&highlight.range.end);
                (start < end).then_some((start..end, highlight.kind))
            })
            .collect()
    }

    pub(crate) fn update_document_highlights(
        &mut self,
        text: &Rope,
        position: Position,
        window: &mut Window,
        cx: &mut Context<InputState>,
    ) {
        let Some(provider) = self.document_highlight_provider.as_ref().cloned() else {
            self.document_highlights.clear();
            return;
        };
        let text = text.clone();
        let input_state = cx.entity();

        self._document_highlight_task = cx.spawn_in(window, async move |_, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(75))
                .await;
            let task = cx
                .update(|window, cx| provider.document_highlights(&text, position, window, cx))
                .ok();
            if let Some(task) = task
                && let Ok(mut highlights) = task.await
            {
                highlights.sort_by_key(|highlight| highlight.range.start);
                let _ = input_state.update(cx, |input_state, cx| {
                    if highlights != input_state.lsp.document_highlights {
                        input_state.lsp.document_highlights = highlights;
                        cx.notify();
                    }
                });
            }
        });
    }
}

impl InputState {
    /// Refresh symbol occurrences after a caret or selection move.
    pub fn refresh_document_highlights(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.text.clone();
        let position = text.offset_to_position(self.cursor());
        self.lsp
            .update_document_highlights(&text, position, window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_highlights_convert_utf16_ranges_and_filter_the_viewport() {
        let text = Rope::from_str("let crab = \"🦀\";\ncrab\n");
        let mut lsp = Lsp::default();
        lsp.document_highlights = vec![
            DocumentHighlight {
                range: lsp_types::Range::new(Position::new(0, 4), Position::new(0, 8)),
                kind: Some(lsp_types::DocumentHighlightKind::WRITE),
            },
            DocumentHighlight {
                range: lsp_types::Range::new(Position::new(1, 0), Position::new(1, 4)),
                kind: Some(lsp_types::DocumentHighlightKind::READ),
            },
        ];

        let first_line = lsp.document_highlights_for_range(&text, &(0..0));
        assert_eq!(first_line.len(), 1);
        let first_range = first_line[0].0.clone();
        assert_eq!(text.slice(first_range).to_string(), "crab");
        assert_eq!(
            first_line[0].1,
            Some(lsp_types::DocumentHighlightKind::WRITE)
        );

        let second_line = lsp.document_highlights_for_range(&text, &(1..1));
        assert_eq!(second_line.len(), 1);
        let second_range = second_line[0].0.clone();
        assert_eq!(text.slice(second_range).to_string(), "crab");
    }
}
