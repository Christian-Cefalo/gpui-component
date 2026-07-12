use anyhow::Result;
use gpui::{App, Context, Task, Window};
use instant::Duration;
use lsp_types::FoldingRange;
use ropey::Rope;

use crate::input::{InputState, RopeExt, display_map::FoldRange};

const FOLDING_RANGE_DEBOUNCE: Duration = Duration::from_millis(100);
const MAX_FOLDING_RANGES: usize = 20_000;

pub trait FoldingRangeProvider {
    fn folding_ranges(
        &self,
        text: &Rope,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Vec<FoldingRange>>>;
}

fn normalize_folding_ranges(text: &Rope, ranges: Vec<FoldingRange>) -> Vec<FoldRange> {
    let last_line = text.lines_len().saturating_sub(1);
    let mut normalized = ranges
        .into_iter()
        .take(MAX_FOLDING_RANGES)
        .filter_map(|range| {
            let start = usize::try_from(range.start_line).ok()?;
            let end = usize::try_from(range.end_line).ok()?;
            (start < end && end <= last_line).then(|| FoldRange::new(start, end))
        })
        .collect::<Vec<_>>();
    normalized.sort_by_key(|range| (range.start_line, range.end_line));
    normalized.dedup();
    normalized
}

impl super::Lsp {
    pub(crate) fn update_folding_ranges(
        &mut self,
        text: &Rope,
        window: &mut Window,
        cx: &mut Context<InputState>,
    ) {
        let Some(provider) = self.folding_range_provider.as_ref().cloned() else {
            self.folding_ranges.clear();
            return;
        };
        let text = text.clone();
        let input = cx.entity();
        self._folding_range_task = cx.spawn_in(window, async move |_, cx| {
            cx.background_executor().timer(FOLDING_RANGE_DEBOUNCE).await;
            let task = cx
                .update(|window, cx| provider.folding_ranges(&text, window, cx))
                .ok();
            let Some(task) = task else {
                return;
            };
            let Ok(ranges) = task.await else {
                return;
            };
            let normalized = normalize_folding_ranges(&text, ranges);
            let _ = input.update(cx, |input, cx| {
                if input.text != text {
                    return;
                }
                input.lsp.folding_ranges = normalized;
                input.update_fold_candidates();
                cx.notify();
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(start: u32, end: u32) -> FoldingRange {
        FoldingRange {
            start_line: start,
            start_character: None,
            end_line: end,
            end_character: None,
            kind: None,
            collapsed_text: None,
        }
    }

    #[test]
    fn folding_ranges_are_bounded_validated_and_sorted() {
        let text = Rope::from_str("a\nb\nc\nd\n");
        let normalized = normalize_folding_ranges(
            &text,
            vec![range(2, 3), range(0, 2), range(4, 9), range(1, 1)],
        );
        assert_eq!(normalized, vec![FoldRange::new(0, 2), FoldRange::new(2, 3)]);
    }
}
