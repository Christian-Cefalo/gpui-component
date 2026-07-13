use anyhow::Result;
use gpui::{App, Context, Hsla, MouseMoveEvent, SharedString, Task, Window};
use ropey::Rope;
use std::ops::Range as ByteRange;
use std::rc::Rc;

use crate::input::{
    InputState, RopeExt, Selection,
    popovers::{CodeActionItem, ContextMenu},
};

mod code_actions;
mod code_lens;
mod completions;
mod definitions;
mod document_colors;
mod document_highlights;
mod document_links;
mod folding_ranges;
mod hover;
mod inlay_hints;
mod refresh;
mod selection_ranges;
mod semantic_tokens;
mod signature_help;

pub use code_actions::*;
pub use code_lens::*;
pub use completions::*;
pub use definitions::*;
pub use document_colors::*;
pub use document_highlights::*;
pub use document_links::*;
pub use folding_ranges::*;
pub use hover::*;
pub use inlay_hints::*;
pub use selection_ranges::*;
pub use semantic_tokens::*;
pub use signature_help::*;

/// LSP ServerCapabilities
///
/// https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#serverCapabilities
pub struct Lsp {
    /// The completion provider.
    pub completion_provider: Option<Rc<dyn CompletionProvider>>,
    /// Whether `InsertReplaceEdit` completions preserve or replace text to the
    /// right of the cursor.
    pub completion_insert_mode: CompletionInsertMode,
    /// The code action providers.
    pub code_action_providers: Vec<Rc<dyn CodeActionProvider>>,
    /// The document CodeLens provider.
    pub code_lens_provider: Option<Rc<dyn CodeLensProvider>>,
    /// The hover provider.
    pub hover_provider: Option<Rc<dyn HoverProvider>>,
    /// The definition provider.
    pub definition_provider: Option<Rc<dyn DefinitionProvider>>,
    /// The document color provider.
    pub document_color_provider: Option<Rc<dyn DocumentColorProvider>>,
    /// The document highlight provider.
    pub document_highlight_provider: Option<Rc<dyn DocumentHighlightProvider>>,
    /// The document link provider.
    pub document_link_provider: Option<Rc<dyn DocumentLinkProvider>>,
    /// The document folding-range provider.
    pub folding_range_provider: Option<Rc<dyn FoldingRangeProvider>>,
    /// The viewport inlay-hint provider.
    pub inlay_hint_provider: Option<Rc<dyn InlayHintProvider>>,
    /// The smart expand-selection provider.
    pub selection_range_provider: Option<Rc<dyn SelectionRangeProvider>>,
    /// The range semantic tokens provider.
    pub semantic_tokens_provider: Option<Rc<dyn DocumentRangeSemanticTokensProvider>>,
    /// The parameter-hint provider.
    pub signature_help_provider: Option<Rc<dyn SignatureHelpProvider>>,

    document_colors: Vec<(lsp_types::Range, Hsla)>,
    document_highlights: Vec<lsp_types::DocumentHighlight>,
    document_links: Vec<lsp_types::DocumentLink>,
    active_document_link: Option<lsp_types::DocumentLink>,
    document_link_generation: u64,
    document_link_requested_generation: Option<u64>,
    code_lenses: Vec<lsp_types::CodeLens>,
    code_lens_generation: u64,
    code_lens_requested_generation: Option<u64>,
    code_lens_resolve_attempted: Vec<bool>,
    pub(super) folding_ranges: Vec<crate::input::display_map::FoldRange>,
    inlay_hints: Vec<lsp_types::InlayHint>,
    inlay_hint_range: Option<lsp_types::Range>,
    selection_range_history: Vec<Selection>,
    selection_range_last: Option<Selection>,
    /// Cached semantic tokens as absolute position ranges + theme token-type
    /// names. Color is resolved from the name at paint time so theme switches
    /// take effect without a refetch.
    semantic_tokens: Vec<(lsp_types::Range, SharedString)>,
    pub(super) signature_help_generation: u64,
    automatic_code_actions: Vec<CodeActionItem>,
    automatic_code_action_range: Option<ByteRange<usize>>,
    automatic_code_action_requested_range: Option<ByteRange<usize>>,
    code_action_generation: u64,
    _hover_task: Task<Result<()>>,
    _document_color_task: Task<()>,
    _document_highlight_task: Task<()>,
    _document_link_task: Task<()>,
    _document_link_resolve_task: Task<Result<()>>,
    _code_lens_task: Task<()>,
    _code_lens_resolve_task: Task<()>,
    _code_lens_command_task: Task<Result<()>>,
    _folding_range_task: Task<()>,
    _inlay_hint_task: Task<()>,
    _selection_range_task: Task<()>,
    _semantic_tokens_task: Task<()>,
    pub(super) _code_action_task: Task<()>,
    pub(super) _signature_help_task: Task<()>,
}

impl Default for Lsp {
    fn default() -> Self {
        Self {
            completion_provider: None,
            completion_insert_mode: CompletionInsertMode::default(),
            code_action_providers: vec![],
            code_lens_provider: None,
            hover_provider: None,
            definition_provider: None,
            document_color_provider: None,
            document_highlight_provider: None,
            document_link_provider: None,
            folding_range_provider: None,
            inlay_hint_provider: None,
            selection_range_provider: None,
            semantic_tokens_provider: None,
            signature_help_provider: None,
            document_colors: vec![],
            document_highlights: vec![],
            document_links: vec![],
            active_document_link: None,
            document_link_generation: 0,
            document_link_requested_generation: None,
            code_lenses: vec![],
            code_lens_generation: 0,
            code_lens_requested_generation: None,
            code_lens_resolve_attempted: vec![],
            folding_ranges: Vec::new(),
            inlay_hints: vec![],
            inlay_hint_range: None,
            selection_range_history: Vec::new(),
            selection_range_last: None,
            semantic_tokens: vec![],
            signature_help_generation: 0,
            automatic_code_actions: Vec::new(),
            automatic_code_action_range: None,
            automatic_code_action_requested_range: None,
            code_action_generation: 0,
            _hover_task: Task::ready(Ok(())),
            _document_color_task: Task::ready(()),
            _document_highlight_task: Task::ready(()),
            _document_link_task: Task::ready(()),
            _document_link_resolve_task: Task::ready(Ok(())),
            _code_lens_task: Task::ready(()),
            _code_lens_resolve_task: Task::ready(()),
            _code_lens_command_task: Task::ready(Ok(())),
            _folding_range_task: Task::ready(()),
            _inlay_hint_task: Task::ready(()),
            _selection_range_task: Task::ready(()),
            _semantic_tokens_task: Task::ready(()),
            _code_action_task: Task::ready(()),
            _signature_help_task: Task::ready(()),
        }
    }
}

impl Lsp {
    /// Update the LSP when the text changes.
    pub(crate) fn update(
        &mut self,
        text: &Rope,
        window: &mut Window,
        cx: &mut Context<InputState>,
    ) {
        self.inlay_hint_range = None;
        self.inlay_hints.clear();
        self.invalidate_code_lenses();
        self.invalidate_document_links();
        self.selection_range_history.clear();
        self.selection_range_last = None;
        self.folding_ranges.clear();
        self.cancel_signature_help_request();
        self.invalidate_automatic_code_actions();
        self.update_folding_ranges(text, window, cx);
        self.update_document_colors(text, window, cx);
        self.update_semantic_tokens(text, window, cx);
    }

    /// Reset all LSP states.
    pub(crate) fn reset(&mut self) {
        self.document_colors.clear();
        self.document_highlights.clear();
        self.document_links.clear();
        self.active_document_link = None;
        self.document_link_generation = self.document_link_generation.wrapping_add(1);
        self.document_link_requested_generation = None;
        self.code_lenses.clear();
        self.code_lens_generation = self.code_lens_generation.wrapping_add(1);
        self.code_lens_requested_generation = None;
        self.code_lens_resolve_attempted.clear();
        self.folding_ranges.clear();
        self.inlay_hints.clear();
        self.inlay_hint_range = None;
        self.selection_range_history.clear();
        self.selection_range_last = None;
        self.semantic_tokens.clear();
        self.cancel_signature_help_request();
        self.automatic_code_actions.clear();
        self.automatic_code_action_range = None;
        self.automatic_code_action_requested_range = None;
        self.code_action_generation = self.code_action_generation.wrapping_add(1);
        self._hover_task = Task::ready(Ok(()));
        self._document_color_task = Task::ready(());
        self._document_highlight_task = Task::ready(());
        self._document_link_task = Task::ready(());
        self._document_link_resolve_task = Task::ready(Ok(()));
        self._code_lens_task = Task::ready(());
        self._code_lens_resolve_task = Task::ready(());
        self._code_lens_command_task = Task::ready(Ok(()));
        self._folding_range_task = Task::ready(());
        self._inlay_hint_task = Task::ready(());
        self._selection_range_task = Task::ready(());
        self._semantic_tokens_task = Task::ready(());
        self._code_action_task = Task::ready(());
        self._signature_help_task = Task::ready(());
    }
}

impl InputState {
    pub(crate) fn hide_context_menu(&mut self, cx: &mut Context<Self>) {
        self.context_menu_content = None;
        self._context_menu_task = Task::ready(Ok(()));
        cx.notify();
    }

    pub(crate) fn is_context_menu_open(&self, cx: &App) -> bool {
        let Some(menu) = self.context_menu_content.as_ref() else {
            return false;
        };

        menu.is_open(cx)
    }

    /// Handles an action for the completion menu, if it exists.
    ///
    /// Return true if the action was handled, otherwise false.
    pub fn handle_action_for_context_menu(
        &mut self,
        action: Box<dyn gpui::Action>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(menu) = self.context_menu_content.as_ref() else {
            return false;
        };

        let mut handled = false;

        match menu {
            ContextMenu::Completion(menu) => {
                _ = menu.update(cx, |menu, cx| {
                    handled = menu.handle_action(action, window, cx)
                });
            }
            ContextMenu::CodeAction(menu) => {
                _ = menu.update(cx, |menu, cx| {
                    handled = menu.handle_action(action, window, cx)
                });
            }
        };

        handled
    }

    pub(crate) fn accept_completion_commit_character(
        &mut self,
        character: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(ContextMenu::Completion(menu)) = self.context_menu_content.as_ref() else {
            return false;
        };
        menu.update(cx, |menu, cx| {
            menu.accept_commit_character(character, window, cx)
        })
    }

    /// Apply a list of [`lsp_types::TextEdit`] to mutate the text.
    pub fn apply_lsp_edits(
        &mut self,
        text_edits: &Vec<lsp_types::TextEdit>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for edit in text_edits {
            let start = self.text.position_to_offset(&edit.range.start);
            let end = self.text.position_to_offset(&edit.range.end);

            let range_utf16 = self.range_to_utf16(&(start..end));
            self.replace_text_in_range_silent(Some(range_utf16), &edit.new_text, window, cx);
        }
    }

    pub(super) fn handle_mouse_move(
        &mut self,
        offset: usize,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<InputState>,
    ) {
        let had_definition = !self.hover_definition.is_empty();
        let previous_document_link = self.lsp.active_document_link.clone();
        let had_popover = self.hover_popover.is_some();

        if event.modifiers.secondary() {
            if self.handle_hover_document_link(offset) {
                self.hover_definition.clear();
            } else {
                self.handle_hover_definition(offset, window, cx);
            }
        } else {
            self.clear_active_document_link();
            self.hover_definition.clear();
            self.handle_hover_popover(offset, window, cx);
        }

        let changed = had_definition == self.hover_definition.is_empty()
            || previous_document_link != self.lsp.active_document_link
            || had_popover != self.hover_popover.is_some();
        if changed {
            cx.notify();
        }
    }

    pub(crate) fn clear_hover_state(&mut self, cx: &mut Context<InputState>) {
        self.hover_definition.clear();
        self.clear_active_document_link();
        self.hover_popover = None;
        self.lsp._hover_task = Task::ready(Ok(()));
        cx.notify();
    }
}
