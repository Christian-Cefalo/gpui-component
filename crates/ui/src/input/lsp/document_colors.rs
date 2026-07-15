use anyhow::Result;
use gpui::{App, Context, Entity, Hsla, Task, Window};
use instant::Duration;
use lsp_types::{Color, ColorInformation, Position};
use ropey::Rope;
use std::ops::Range;

use crate::input::{InputState, Lsp, OpenDocumentColorPicker, RopeExt};

const MAX_DOCUMENT_COLORS: usize = 2_000;

pub trait DocumentColorProvider {
    /// Fetches document colors for the specified range.
    ///
    /// textDocument/documentColor
    ///
    /// https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocument_documentColor
    fn document_colors(
        &self,
        _text: &Rope,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Vec<ColorInformation>>>;

    /// Opens the host's color-editing UI for a cached document color.
    ///
    /// Ranges use the editor's scalar-position convention. Integrations that
    /// communicate with an LSP server must convert to UTF-16 at that boundary.
    fn activate_document_color(
        &self,
        _state: Entity<InputState>,
        _color: ColorInformation,
        _window: &mut Window,
        _cx: &mut Context<InputState>,
    ) -> bool {
        false
    }
}

fn exact_scalar_offset(text: &Rope, position: Position) -> Option<usize> {
    ((position.line as usize) < text.lines_len())
        .then(|| text.position_to_offset(&position))
        .filter(|offset| text.offset_to_position(*offset) == position)
}

fn valid_color(color: &Color) -> bool {
    [color.red, color.green, color.blue, color.alpha]
        .into_iter()
        .all(|component| component.is_finite() && (0.0..=1.0).contains(&component))
}

fn normalize_document_colors(
    text: &Rope,
    mut colors: Vec<ColorInformation>,
) -> Vec<ColorInformation> {
    colors.retain(|info| {
        let Some(start) = exact_scalar_offset(text, info.range.start) else {
            return false;
        };
        let Some(end) = exact_scalar_offset(text, info.range.end) else {
            return false;
        };
        start < end && valid_color(&info.color)
    });
    colors.sort_by_key(|info| (info.range.start, info.range.end));
    colors.truncate(MAX_DOCUMENT_COLORS);

    let mut normalized = Vec::with_capacity(colors.len());
    for color in colors {
        if normalized
            .last()
            .is_some_and(|previous: &ColorInformation| previous.range.end > color.range.start)
        {
            continue;
        }
        normalized.push(color);
    }
    normalized
}

impl Lsp {
    fn document_color_at_offset(&self, text: &Rope, offset: usize) -> Option<ColorInformation> {
        self.document_colors
            .iter()
            .find(|(range, _)| {
                let start = text.position_to_offset(&range.start);
                let end = text.position_to_offset(&range.end);
                (start..end).contains(&offset)
            })
            .map(|(range, color)| {
                let rgba = color.to_rgb();
                ColorInformation {
                    range: *range,
                    color: Color {
                        red: rgba.r,
                        green: rgba.g,
                        blue: rgba.b,
                        alpha: rgba.a,
                    },
                }
            })
    }

    /// Get document colors that intersect with the visible range (0-based row).
    ///
    /// Returns byte ranges and colors.
    pub(crate) fn document_colors_for_range(
        &self,
        text: &Rope,
        visible_range: &Range<usize>,
    ) -> Vec<(Range<usize>, Hsla)> {
        self.document_colors
            .iter()
            .filter_map(|(range, color)| {
                if (range.start.line as usize) > visible_range.end
                    || (range.end.line as usize) < visible_range.start
                {
                    return None;
                }

                let start = text.position_to_offset(&range.start);
                let end = text.position_to_offset(&range.end);

                Some((start..end, *color))
            })
            .collect()
    }

    pub(crate) fn update_document_colors(
        &mut self,
        text: &Rope,
        window: &mut Window,
        cx: &mut Context<InputState>,
    ) {
        let Some(provider) = self.document_color_provider.as_ref() else {
            self.document_colors.clear();
            return;
        };

        // Cached ranges belong to the previous buffer generation. Keeping
        // them visible while a replacement request is in flight can paint or
        // activate a color at the wrong text after an edit.
        self.document_colors.clear();

        let provider = provider.clone();
        let text = text.clone();
        let input_state = cx.entity();

        // debounce timer 100ms
        self._document_color_task = cx.spawn_in(window, async move |_, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;

            let task_result = cx
                .update(|window, cx| provider.document_colors(&text, window, cx))
                .ok();

            if let Some(task) = task_result {
                if let Ok(colors) = task.await {
                    let _ = input_state.update(cx, |input_state, cx| {
                        let document_colors: Vec<(lsp_types::Range, Hsla)> =
                            normalize_document_colors(&text, colors)
                                .into_iter()
                                .map(|info| {
                                    let color = gpui::Rgba {
                                        r: info.color.red,
                                        g: info.color.green,
                                        b: info.color.blue,
                                        a: info.color.alpha,
                                    }
                                    .into();

                                    (info.range, color)
                                })
                                .collect();

                        if document_colors != input_state.lsp.document_colors {
                            input_state.lsp.document_colors = document_colors;
                            cx.notify();
                        }
                    });
                }
            }
        });
    }
}

impl InputState {
    pub fn document_colors(&self) -> Vec<ColorInformation> {
        self.lsp
            .document_colors
            .iter()
            .map(|(range, color)| {
                let rgba = color.to_rgb();
                ColorInformation {
                    range: *range,
                    color: Color {
                        red: rgba.r,
                        green: rgba.g,
                        blue: rgba.b,
                        alpha: rgba.a,
                    },
                }
            })
            .collect()
    }

    pub fn has_document_color_at_cursor(&self) -> bool {
        self.lsp
            .document_color_at_offset(&self.text, self.cursor())
            .is_some()
    }

    pub fn open_document_color_at_cursor(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(color) = self.lsp.document_color_at_offset(&self.text, self.cursor()) else {
            return false;
        };
        let Some(provider) = self.lsp.document_color_provider.as_ref().cloned() else {
            return false;
        };
        provider.activate_document_color(cx.entity(), color, window, cx)
    }

    /// Opens a cached document color by stable document-order index. This is
    /// the debug-automation equivalent of the editor's Edit Color command.
    pub fn open_document_color_at(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((range, color)) = self.lsp.document_colors.get(index).copied() else {
            return false;
        };
        let Some(provider) = self.lsp.document_color_provider.as_ref().cloned() else {
            return false;
        };
        let rgba = color.to_rgb();
        provider.activate_document_color(
            cx.entity(),
            ColorInformation {
                range,
                color: Color {
                    red: rgba.r,
                    green: rgba.g,
                    blue: rgba.b,
                    alpha: rgba.a,
                },
            },
            window,
            cx,
        )
    }

    pub(crate) fn on_action_open_document_color_picker(
        &mut self,
        _: &OpenDocumentColorPicker,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_document_color_at_cursor(window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::Range as LspRange;

    fn color(start: u32, end: u32, red: f32) -> ColorInformation {
        ColorInformation {
            range: LspRange::new(Position::new(0, start), Position::new(0, end)),
            color: Color {
                red,
                green: 0.5,
                blue: 0.25,
                alpha: 1.0,
            },
        }
    }

    #[test]
    fn colors_are_sorted_bounded_and_overlaps_are_removed() {
        let text = Rope::from_str("#111 #222 #333\n");
        let colors = normalize_document_colors(
            &text,
            vec![color(10, 14, 0.3), color(0, 4, 0.1), color(3, 9, 0.2)],
        );

        assert_eq!(colors.len(), 2);
        assert_eq!(
            colors[0].range,
            LspRange::new(Position::new(0, 0), Position::new(0, 4))
        );
        assert_eq!(
            colors[1].range,
            LspRange::new(Position::new(0, 10), Position::new(0, 14))
        );
    }

    #[test]
    fn invalid_positions_empty_ranges_and_non_finite_colors_are_rejected() {
        let text = Rope::from_str("#111\n");
        let colors = normalize_document_colors(
            &text,
            vec![
                color(0, 4, 0.1),
                color(1, 1, 0.2),
                color(0, 99, 0.3),
                color(0, 4, f32::NAN),
            ],
        );

        assert_eq!(colors, vec![color(0, 4, 0.1)]);
    }

    #[test]
    fn cached_color_lookup_uses_scalar_columns_after_non_bmp_text() {
        let text = Rope::from_str("😀#111\n");
        let mut lsp = Lsp::default();
        lsp.document_colors = vec![(
            LspRange::new(Position::new(0, 1), Position::new(0, 5)),
            gpui::Rgba {
                r: 0.1,
                g: 0.2,
                b: 0.3,
                a: 1.0,
            }
            .into(),
        )];

        assert!(lsp.document_color_at_offset(&text, 4).is_some());
        assert!(lsp.document_color_at_offset(&text, 7).is_some());
        assert!(lsp.document_color_at_offset(&text, 8).is_none());
        assert!(lsp.document_color_at_offset(&text, 3).is_none());
    }
}
