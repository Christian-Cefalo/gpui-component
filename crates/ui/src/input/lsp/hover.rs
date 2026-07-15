use anyhow::Result;
use gpui::{App, Context, Task, Window};
use instant::Duration;
use ropey::Rope;
use std::ops::Range;

use crate::input::{InputState, RopeExt, popovers::HoverPopover};

/// Hover provider
///
/// https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocument_hover
pub trait HoverProvider {
    /// textDocument/hover
    ///
    /// https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocument_hover
    fn hover(
        &self,
        _text: &Rope,
        _offset: usize,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Task<Result<Option<lsp_types::Hover>>>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct HoverPopoverSnapshot {
    pub symbol_range: Range<usize>,
    pub hover: lsp_types::Hover,
}

impl InputState {
    /// Handle hover trigger LSP request.
    pub(super) fn handle_hover_popover(
        &mut self,
        offset: usize,
        window: &mut Window,
        cx: &mut Context<InputState>,
    ) {
        self.request_hover_popover(offset, true, window, cx);
    }

    /// Show hover information for the symbol at the primary cursor.
    ///
    /// Unlike pointer hover, this keyboard/programmatic path requests the
    /// result immediately. It returns `false` when the input is not a code
    /// editor or no document-link/hover provider can serve the cursor.
    pub fn show_hover_at_cursor(
        &mut self,
        window: &mut Window,
        cx: &mut Context<InputState>,
    ) -> bool {
        if !self.mode.is_code_editor() || self.selecting {
            return false;
        }

        let offset = self.cursor();
        if self.handle_hover_document_link(offset, false, cx) {
            return true;
        }
        if self.lsp.hover_provider.is_none() {
            return false;
        }
        self.request_hover_popover(offset, false, window, cx);
        true
    }

    /// Dismiss the active hover and cancel any pending hover provider request.
    pub fn hide_hover_popover(&mut self, cx: &mut Context<InputState>) -> bool {
        let was_visible = self.hover_popover.is_some();
        self.clear_hover_state(cx);
        was_visible
    }

    /// Return the currently rendered hover range and protocol payload.
    pub fn hover_popover_snapshot(&self, cx: &App) -> Option<HoverPopoverSnapshot> {
        let popover = self.hover_popover.as_ref()?.read(cx);
        Some(HoverPopoverSnapshot {
            symbol_range: popover.symbol_range.clone(),
            hover: popover.hover.as_ref().clone(),
        })
    }

    fn request_hover_popover(
        &mut self,
        offset: usize,
        delay_initial_request: bool,
        window: &mut Window,
        cx: &mut Context<InputState>,
    ) {
        if self.selecting {
            return;
        }

        let Some(provider) = self.lsp.hover_provider.clone() else {
            return;
        };

        if let Some(hover_popover) = self.hover_popover.as_ref() {
            if hover_popover.read(cx).is_same(offset) {
                return;
            }
        }

        // Currently not implemented.
        let task = provider.hover(&self.text, offset, window, cx);
        let mut symbol_range = self.text.word_range(offset).unwrap_or(offset..offset);
        let editor = cx.entity();
        let should_delay = delay_initial_request && self.hover_popover.is_none();
        self.lsp._hover_task = cx.spawn_in(window, async move |_, cx| {
            if should_delay {
                cx.background_executor()
                    .timer(Duration::from_millis(150))
                    .await;
            }

            let result = task.await?;

            _ = editor.update(cx, |editor, cx| match result {
                Some(hover) => {
                    if let Some(range) = hover.range {
                        let start = editor.text.position_to_offset(&range.start);
                        let end = editor.text.position_to_offset(&range.end);
                        symbol_range = start..end;
                    }
                    let hover_popover = HoverPopover::new(cx.entity(), symbol_range, &hover, cx);
                    editor.hover_popover = Some(hover_popover);
                    cx.notify();
                }
                None => {
                    editor.hover_popover = None;
                    cx.notify();
                }
            });

            Ok(())
        });
    }
}
