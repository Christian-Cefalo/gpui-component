use gpui::{Context, Window};

use crate::input::InputState;

impl InputState {
    /// Refetch semantic tokens for the current document without requiring a
    /// text edit. LSP servers call `workspace/semanticTokens/refresh` when
    /// project-wide state changes can alter highlighting in otherwise
    /// unchanged open documents.
    pub fn refresh_semantic_tokens(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.text.clone();
        self.lsp.update_semantic_tokens(&text, window, cx);
    }
}
