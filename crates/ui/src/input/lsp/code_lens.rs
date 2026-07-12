use anyhow::Result;
use gpui::{App, Context, Pixels, Task, Window, px};
use instant::Duration;
use lsp_types::CodeLens;
use ropey::Rope;

use crate::input::{InputState, Lsp, RopeExt as _};

const CODE_LENS_DEBOUNCE: Duration = Duration::from_millis(250);
const MAX_CODE_LENSES: usize = 1_000;
const MAX_CODE_LENS_TITLE_CHARS: usize = 120;

/// Supplies and executes document CodeLens items.
///
/// Ranges use the editor's scalar-position convention. An integration that
/// talks to a language server is responsible for converting UTF-16 LSP
/// positions before returning a lens, and converting them back before
/// `resolve_code_lens`.
pub trait CodeLensProvider {
    /// Whether the backing server advertises `codeLensProvider.resolveProvider`.
    fn can_resolve_code_lenses(&self) -> bool {
        false
    }

    /// Fetch all CodeLens items for the current document.
    fn code_lenses(
        &self,
        text: &Rope,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Vec<CodeLens>>>;

    /// Resolve a visible CodeLens item that did not include a command.
    fn resolve_code_lens(
        &self,
        _text: &Rope,
        lens: CodeLens,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Task<Result<CodeLens>> {
        Task::ready(Ok(lens))
    }

    /// Execute the command attached to a resolved CodeLens item.
    fn execute_code_lens(
        &self,
        text: &Rope,
        lens: CodeLens,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct DisplayCodeLens {
    /// Zero-based buffer line containing the lens.
    pub line: usize,
    /// Stable index among all lenses on the same line.
    pub index: usize,
    /// Bounded user-facing command title. `None` means resolution is pending.
    pub title: Option<String>,
    /// The underlying CodeLens value used for activation.
    pub lens: CodeLens,
}

fn bounded_title(title: &str) -> String {
    let mut chars = title.chars();
    let mut bounded = chars
        .by_ref()
        .take(MAX_CODE_LENS_TITLE_CHARS)
        .collect::<String>();
    if chars.next().is_some() {
        bounded.push('…');
    }
    bounded
}

fn valid_lens(text: &Rope, lens: &CodeLens) -> bool {
    let line = lens.range.start.line as usize;
    let end_line = lens.range.end.line as usize;
    line < text.lines_len() && end_line < text.lines_len() && lens.range.start <= lens.range.end
}

fn unique_lens_lines(lenses: &[CodeLens]) -> Vec<usize> {
    let mut lines = lenses
        .iter()
        .map(|lens| lens.range.start.line as usize)
        .collect::<Vec<_>>();
    lines.sort_unstable();
    lines.dedup();
    lines
}

fn code_lens_scroll_delta(
    old_lines: &[usize],
    new_lines: &[usize],
    anchor_line: usize,
    line_height: Pixels,
) -> Pixels {
    let old_count = old_lines.partition_point(|&line| line < anchor_line);
    let new_count = new_lines.partition_point(|&line| line < anchor_line);
    line_height * (new_count as f32 - old_count as f32)
}

impl Lsp {
    pub(crate) fn invalidate_code_lenses(&mut self) {
        self.code_lens_generation = self.code_lens_generation.wrapping_add(1);
        self.code_lens_requested_generation = None;
        self.code_lenses.clear();
        self.code_lens_resolve_attempted.clear();
        self._code_lens_task = Task::ready(());
        self._code_lens_resolve_task = Task::ready(());
    }

    pub(crate) fn code_lens_lines(&self) -> Vec<usize> {
        unique_lens_lines(&self.code_lenses)
    }

    pub(crate) fn has_code_lens_on_line(&self, line: usize) -> bool {
        self.code_lenses
            .iter()
            .any(|lens| lens.range.start.line as usize == line)
    }

    pub fn code_lenses_on_line(&self, line: usize) -> Vec<DisplayCodeLens> {
        self.code_lenses
            .iter()
            .filter(|lens| lens.range.start.line as usize == line)
            .enumerate()
            .map(|(index, lens)| DisplayCodeLens {
                line,
                index,
                title: lens
                    .command
                    .as_ref()
                    .map(|command| bounded_title(&command.title)),
                lens: lens.clone(),
            })
            .collect()
    }

    pub fn code_lenses(&self) -> Vec<DisplayCodeLens> {
        self.code_lens_lines()
            .into_iter()
            .flat_map(|line| self.code_lenses_on_line(line))
            .collect()
    }

    fn request_code_lenses(
        &mut self,
        text: &Rope,
        window: &mut Window,
        cx: &mut Context<InputState>,
    ) {
        let Some(provider) = self.code_lens_provider.as_ref().cloned() else {
            self.invalidate_code_lenses();
            return;
        };
        let generation = self.code_lens_generation;
        if self.code_lens_requested_generation == Some(generation) {
            return;
        }
        self.code_lens_requested_generation = Some(generation);

        let text = text.clone();
        let input_state = cx.entity();
        self._code_lens_task = cx.spawn_in(window, async move |_, cx| {
            cx.background_executor().timer(CODE_LENS_DEBOUNCE).await;
            let task = cx
                .update(|window, cx| provider.code_lenses(&text, window, cx))
                .ok();
            let Some(task) = task else {
                return;
            };
            let Ok(mut lenses) = task.await else {
                return;
            };
            lenses.retain(|lens| valid_lens(&text, lens));
            lenses.sort_by_key(|lens| (lens.range.start, lens.range.end));
            lenses.truncate(MAX_CODE_LENSES);

            let _ = input_state.update(cx, |input_state, cx| {
                if input_state.lsp.code_lens_generation != generation
                    || input_state.lsp.code_lens_requested_generation != Some(generation)
                {
                    return;
                }
                let can_resolve = input_state
                    .lsp
                    .code_lens_provider
                    .as_ref()
                    .is_some_and(|provider| provider.can_resolve_code_lenses());

                // Adding/removing view zones above a scrolled viewport must
                // not move the text the user is reading. At the document top
                // (`offset.y == 0`) the new lens is intentionally revealed.
                if input_state.scroll_handle.offset().y < px(0.)
                    && let Some(layout) = input_state.last_layout.as_ref()
                {
                    let old_lines = input_state
                        .lsp
                        .code_lens_lines()
                        .into_iter()
                        .filter(|&line| {
                            input_state
                                .display_map
                                .visible_wrap_row_count_for_buffer_line(line)
                                > 0
                        })
                        .collect::<Vec<_>>();
                    let new_lines = unique_lens_lines(&lenses)
                        .into_iter()
                        .filter(|&line| {
                            input_state
                                .display_map
                                .visible_wrap_row_count_for_buffer_line(line)
                                > 0
                        })
                        .collect::<Vec<_>>();
                    let delta = code_lens_scroll_delta(
                        &old_lines,
                        &new_lines,
                        layout.visible_range.start,
                        layout.code_lens_height,
                    );
                    if delta != px(0.) {
                        let mut offset = input_state.scroll_handle.offset();
                        offset.y -= delta;
                        input_state.scroll_handle.set_offset(offset);
                    }
                }

                input_state.lsp.code_lens_resolve_attempted = lenses
                    .iter()
                    .map(|lens| lens.command.is_some() || !can_resolve)
                    .collect();
                input_state.lsp.code_lenses = lenses;
                cx.notify();
            });
        });
    }

    fn resolve_visible_code_lens(
        &mut self,
        text: &Rope,
        visible_rows: std::ops::Range<usize>,
        window: &mut Window,
        cx: &mut Context<InputState>,
    ) {
        let Some(provider) = self.code_lens_provider.as_ref().cloned() else {
            return;
        };
        if !provider.can_resolve_code_lenses() {
            return;
        }
        let Some(index) = self
            .code_lenses
            .iter()
            .enumerate()
            .find(|(index, lens)| {
                let line = lens.range.start.line as usize;
                visible_rows.contains(&line)
                    && lens.command.is_none()
                    && !self
                        .code_lens_resolve_attempted
                        .get(*index)
                        .copied()
                        .unwrap_or(true)
            })
            .map(|(index, _)| index)
        else {
            return;
        };

        self.code_lens_resolve_attempted[index] = true;
        let generation = self.code_lens_generation;
        let original = self.code_lenses[index].clone();
        let text = text.clone();
        let input_state = cx.entity();
        self._code_lens_resolve_task = cx.spawn_in(window, async move |_, cx| {
            let task = cx
                .update(|window, cx| {
                    provider.resolve_code_lens(&text, original.clone(), window, cx)
                })
                .ok();
            let Some(task) = task else {
                return;
            };
            let Ok(mut resolved) = task.await else {
                return;
            };
            if !valid_lens(&text, &resolved) {
                return;
            }
            // The protocol requires resolve to preserve the identity/range of
            // the supplied item. Refuse a moved result so a stale server reply
            // cannot activate a command on another line.
            resolved.range = original.range;
            let _ = input_state.update(cx, |input_state, cx| {
                if input_state.lsp.code_lens_generation != generation
                    || input_state.lsp.code_lenses.get(index) != Some(&original)
                {
                    return;
                }
                input_state.lsp.code_lenses[index] = resolved;
                cx.notify();
            });
        });
    }
}

impl InputState {
    /// Fetch CodeLens data and lazily resolve only items in the viewport.
    pub fn refresh_code_lenses(
        &mut self,
        visible_rows: std::ops::Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = self.text.clone();
        self.lsp.request_code_lenses(&text, window, cx);
        self.lsp
            .resolve_visible_code_lens(&text, visible_rows, window, cx);
    }

    /// Drop cached CodeLens items and request a fresh document snapshot on
    /// the next editor paint.
    pub fn invalidate_code_lenses(&mut self, cx: &mut Context<Self>) {
        self.lsp.invalidate_code_lenses();
        cx.notify();
    }

    /// Ask the provider for a fresh snapshot while retaining the current
    /// rows until the response arrives. This is the flicker-free path for
    /// `workspace/codeLens/refresh`.
    pub fn refresh_code_lenses_from_provider(&mut self, cx: &mut Context<Self>) {
        self.lsp.code_lens_generation = self.lsp.code_lens_generation.wrapping_add(1);
        self.lsp.code_lens_requested_generation = None;
        self.lsp._code_lens_task = Task::ready(());
        self.lsp._code_lens_resolve_task = Task::ready(());
        cx.notify();
    }

    /// Discover CodeLens items attached to a buffer line. This is also the
    /// stable semantic surface used by debug automation.
    pub fn code_lenses_on_line(&self, line: usize) -> Vec<DisplayCodeLens> {
        self.lsp.code_lenses_on_line(line)
    }

    /// Discover every cached CodeLens in document order.
    pub fn code_lenses(&self) -> Vec<DisplayCodeLens> {
        self.lsp.code_lenses()
    }

    /// Execute a discovered CodeLens by line and per-line index.
    ///
    /// Returns false for a missing, unresolved, or provider-less item.
    pub fn execute_code_lens_at(
        &mut self,
        line: usize,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(provider) = self.lsp.code_lens_provider.as_ref().cloned() else {
            return false;
        };
        let Some(lens) = self
            .lsp
            .code_lenses
            .iter()
            .filter(|lens| lens.range.start.line as usize == line)
            .nth(index)
            .cloned()
        else {
            return false;
        };
        if lens.command.is_none() {
            return false;
        }

        let text = self.text.clone();
        self.lsp._code_lens_command_task = cx.spawn_in(window, async move |_, cx| {
            let task =
                cx.update(|window, cx| provider.execute_code_lens(&text, lens, window, cx))?;
            task.await
        });
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{Command, Position, Range};

    fn lens(line: u32, title: Option<&str>) -> CodeLens {
        CodeLens {
            range: Range::new(Position::new(line, 0), Position::new(line, 3)),
            command: title.map(|title| Command {
                title: title.to_string(),
                command: "nova.test".to_string(),
                arguments: None,
            }),
            data: None,
        }
    }

    #[test]
    fn code_lenses_are_grouped_by_start_line_and_titles_are_bounded() {
        let mut lsp = Lsp::default();
        lsp.code_lenses = vec![
            lens(2, Some(&"x".repeat(200))),
            lens(2, None),
            lens(7, None),
        ];

        assert_eq!(lsp.code_lens_lines(), vec![2, 7]);
        let displayed = lsp.code_lenses_on_line(2);
        assert_eq!(displayed.len(), 2);
        assert_eq!(displayed[0].index, 0);
        assert!(displayed[0].title.as_ref().unwrap().ends_with('…'));
        assert_eq!(displayed[1].title, None);
    }

    #[test]
    fn malformed_or_out_of_document_lenses_are_rejected() {
        let text = Rope::from_str("one\ntwo\n");
        assert!(valid_lens(&text, &lens(1, None)));
        assert!(!valid_lens(&text, &lens(4, None)));

        let mut reversed = lens(1, None);
        reversed.range.end = Position::new(0, 0);
        assert!(!valid_lens(&text, &reversed));
    }

    #[test]
    fn view_zone_scroll_delta_counts_unique_lenses_strictly_above_anchor() {
        let old = vec![1, 8];
        let new = vec![1, 4, 8, 12];

        assert_eq!(code_lens_scroll_delta(&old, &new, 8, px(16.)), px(16.));
        assert_eq!(code_lens_scroll_delta(&old, &new, 1, px(16.)), px(0.));
        assert_eq!(code_lens_scroll_delta(&new, &old, 8, px(16.)), px(-16.));
    }

    #[test]
    fn provider_refresh_retains_cached_rows_but_edit_invalidation_drops_them() {
        let mut lsp = Lsp::default();
        lsp.code_lenses = vec![lens(2, Some("Run"))];
        let generation = lsp.code_lens_generation;

        lsp.code_lens_generation = lsp.code_lens_generation.wrapping_add(1);
        lsp.code_lens_requested_generation = None;
        assert_eq!(lsp.code_lens_lines(), vec![2]);
        assert_ne!(lsp.code_lens_generation, generation);

        lsp.invalidate_code_lenses();
        assert!(lsp.code_lens_lines().is_empty());
    }
}
