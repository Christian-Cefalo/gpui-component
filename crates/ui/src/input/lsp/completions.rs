use anyhow::Result;
use gpui::{Context, Entity, EntityInputHandler, Task, Window};
use lsp_types::{
    CompletionContext, CompletionItem, CompletionResponse, InlineCompletionContext,
    InlineCompletionItem, InlineCompletionResponse, InlineCompletionTriggerKind,
    request::Completion,
};
use ropey::Rope;
use std::{cell::RefCell, rc::Rc, time::Duration};
use sum_tree::Bias;

use crate::input::{
    InputState, RopeExt as _, TriggerCompletion,
    popovers::{CompletionMenu, ContextMenu},
};

/// Default debounce duration for inline completions.
const DEFAULT_INLINE_COMPLETION_DEBOUNCE: Duration = Duration::from_millis(300);

/// A trait for providing code completions based on the current input state and context.
pub trait CompletionProvider {
    /// Fetches completions based on the given byte offset.
    ///
    /// - The `offset` is in bytes of current cursor.
    ///
    /// textDocument/completion
    ///
    /// https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocument_completion
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        trigger: CompletionContext,
        window: &mut Window,
        cx: &mut Context<InputState>,
    ) -> Task<Result<CompletionResponse>>;

    /// Fetches an inline completion suggestion for the given position.
    ///
    /// This is called after a debounce period when the user stops typing.
    /// The provider can analyze the text and cursor position to determine
    /// what inline completion suggestion to show.
    ///
    ///
    /// # Arguments
    /// * `rope` - The current text content
    /// * `offset` - The cursor position in bytes
    ///
    /// textDocument/inlineCompletion
    ///
    /// https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_inlineCompletion
    fn inline_completion(
        &self,
        _rope: &Rope,
        _offset: usize,
        _trigger: InlineCompletionContext,
        _window: &mut Window,
        _cx: &mut Context<InputState>,
    ) -> Task<Result<InlineCompletionResponse>> {
        Task::ready(Ok(InlineCompletionResponse::Array(vec![])))
    }

    /// Returns the debounce duration for inline completions.
    ///
    /// Default: 300ms
    #[inline]
    fn inline_completion_debounce(&self) -> Duration {
        DEFAULT_INLINE_COMPLETION_DEBOUNCE
    }

    #[deprecated(note = "use resolve_completion for the accepted CompletionItem")]
    fn resolve_completions(
        &self,
        _completion_indices: Vec<usize>,
        _completions: Rc<RefCell<Box<[Completion]>>>,
        _: &mut Context<InputState>,
    ) -> Task<Result<bool>> {
        Task::ready(Ok(false))
    }

    /// Resolves a completion when it becomes the focused suggestion.
    ///
    /// Successful results are cached by the completion menu and reused on
    /// acceptance. A request can be dropped when focus moves or the menu is
    /// dismissed, so providers should make cancellation safe. Acceptance
    /// reuses an in-flight focused-item request; if no request exists, the menu
    /// starts one final resolve before insertion. Providers should return the
    /// original item when no resolution is required.
    fn resolve_completion(
        &self,
        item: CompletionItem,
        _window: &mut Window,
        _cx: &mut Context<InputState>,
    ) -> Task<Result<CompletionItem>> {
        Task::ready(Ok(item))
    }

    /// Runs after the resolved completion has been inserted successfully.
    ///
    /// This lets clients perform protocol-owned follow-up work such as an LSP
    /// completion command without coupling the input component to a specific
    /// language-server runtime.
    fn completion_accepted(
        &self,
        _item: CompletionItem,
        _window: &mut Window,
        _cx: &mut Context<InputState>,
    ) -> Task<Result<()>> {
        Task::ready(Ok(()))
    }

    /// Determines if the completion should be triggered based on the given byte offset.
    ///
    /// This is called on the main thread.
    fn is_completion_trigger(
        &self,
        offset: usize,
        new_text: &str,
        cx: &mut Context<InputState>,
    ) -> bool;

    /// Returns the protocol context for an automatic completion request.
    ///
    /// Providers with multi-character triggers should override this method
    /// and inspect `text` at `offset`. The default preserves compatibility
    /// with providers that only implement single-character triggering.
    fn completion_context(
        &self,
        _text: &Rope,
        offset: usize,
        new_text: &str,
        cx: &mut Context<InputState>,
    ) -> Option<CompletionContext> {
        self.is_completion_trigger(offset, new_text, cx)
            .then(|| CompletionContext {
                trigger_kind: lsp_types::CompletionTriggerKind::TRIGGER_CHARACTER,
                trigger_character: Some(new_text.to_string()),
            })
    }
}

pub(crate) struct InlineCompletion {
    /// Completion item to display as an inline completion suggestion
    pub(crate) item: Option<InlineCompletionItem>,
    /// Task for debouncing inline completion requests
    pub(crate) task: Task<Result<InlineCompletionResponse>>,
}

impl Default for InlineCompletion {
    fn default() -> Self {
        Self {
            item: None,
            task: Task::ready(Ok(InlineCompletionResponse::Array(vec![]))),
        }
    }
}

fn completion_word_start(text: &Rope, cursor: usize) -> usize {
    let cursor = text.clip_offset(cursor.min(text.len()), Bias::Left);
    let mut start = cursor;
    while start > 0 {
        let previous = text.clip_offset(start.saturating_sub(1), Bias::Left);
        let Some(character) = text.char_at(previous) else {
            break;
        };
        if !(character.is_alphanumeric() || character == '_') {
            break;
        }
        start = previous;
    }
    start
}

fn completion_context_for_open_menu(
    context: Option<CompletionContext>,
    menu_open: bool,
    menu_incomplete: bool,
    new_text: &str,
) -> Option<CompletionContext> {
    if let Some(mut context) = context {
        if menu_incomplete && context.trigger_kind == lsp_types::CompletionTriggerKind::INVOKED {
            context.trigger_kind =
                lsp_types::CompletionTriggerKind::TRIGGER_FOR_INCOMPLETE_COMPLETIONS;
            context.trigger_character = None;
        }
        return Some(context);
    }
    (menu_open && new_text.is_empty()).then_some(CompletionContext {
        trigger_kind: if menu_incomplete {
            lsp_types::CompletionTriggerKind::TRIGGER_FOR_INCOMPLETE_COMPLETIONS
        } else {
            lsp_types::CompletionTriggerKind::INVOKED
        },
        trigger_character: None,
    })
}

impl InputState {
    pub(crate) fn handle_completion_trigger(
        &mut self,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.completion_inserting {
            return;
        }

        let Some(provider) = self.lsp.completion_provider.clone() else {
            return;
        };

        // Always schedule inline completion (debounced).
        // It will check if menu is open before showing the suggestion.
        self.schedule_inline_completion(window, cx);

        let new_offset = self.cursor();
        let existing_menu = match self.context_menu_content.as_ref() {
            Some(ContextMenu::Completion(menu)) if menu.read(cx).is_open() => Some(menu.clone()),
            _ => None,
        };
        let menu_incomplete = existing_menu
            .as_ref()
            .is_some_and(|menu| menu.read(cx).is_incomplete());
        let completion_context = completion_context_for_open_menu(
            provider.completion_context(&self.text, new_offset, new_text, cx),
            existing_menu.is_some(),
            menu_incomplete,
            new_text,
        );
        let Some(completion_context) = completion_context else {
            if let Some(menu) = existing_menu {
                _ = menu.update(cx, |menu, cx| menu.hide(cx));
            }
            return;
        };

        let menu = self.completion_menu(window, cx);
        let replacement_start = completion_word_start(&self.text, new_offset);
        let start_offset = menu
            .read(cx)
            .trigger_start_offset
            .unwrap_or(replacement_start);
        if new_offset < start_offset {
            _ = menu.update(cx, |menu, cx| menu.hide(cx));
            return;
        }

        let query = self
            .text_for_range(
                self.range_to_utf16(&(start_offset..new_offset)),
                &mut None,
                window,
                cx,
            )
            .unwrap_or_default();
        _ = menu.update(cx, |menu, _| {
            menu.update_query(start_offset, query.clone());
        });

        self.request_completion_menu(provider, menu, new_offset, completion_context, window, cx);
    }

    pub(crate) fn trigger_completion(
        &mut self,
        _: &TriggerCompletion,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.disabled || self.completion_inserting {
            return;
        }
        let Some(provider) = self.lsp.completion_provider.clone() else {
            return;
        };
        self.clear_inline_completion(cx);
        let cursor = self.cursor();
        let start_offset = completion_word_start(&self.text, cursor);
        let query = self.text.slice(start_offset..cursor).to_string();
        let menu = self.completion_menu(window, cx);
        _ = menu.update(cx, |menu, _| {
            menu.begin_query(start_offset, query);
        });
        self.request_completion_menu(
            provider,
            menu,
            cursor,
            CompletionContext {
                trigger_kind: lsp_types::CompletionTriggerKind::INVOKED,
                trigger_character: None,
            },
            window,
            cx,
        );
    }

    fn completion_menu(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<CompletionMenu> {
        let menu = match self.context_menu_content.as_ref() {
            Some(ContextMenu::Completion(menu)) => Some(menu),
            _ => None,
        };

        match menu {
            Some(menu) => menu.clone(),
            None => {
                let menu = CompletionMenu::new(cx.entity(), window, cx);
                self.context_menu_content = Some(ContextMenu::Completion(menu.clone()));
                menu
            }
        }
    }

    fn request_completion_menu(
        &mut self,
        provider: Rc<dyn CompletionProvider>,
        menu: Entity<CompletionMenu>,
        offset: usize,
        completion_context: CompletionContext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let provider_responses =
            provider.completions(&self.text, offset, completion_context, window, cx);
        self._context_menu_task = cx.spawn_in(window, async move |editor, cx| {
            let mut completions: Vec<CompletionItem> = vec![];
            let mut is_incomplete = false;
            if let Some(provider_responses) = provider_responses.await.ok() {
                match provider_responses {
                    CompletionResponse::Array(items) => completions.extend(items),
                    CompletionResponse::List(list) => {
                        is_incomplete = list.is_incomplete;
                        completions.extend(list.items);
                    }
                }
            }

            if completions.is_empty() {
                _ = menu.update(cx, |menu, cx| {
                    menu.hide(cx);
                    cx.notify();
                });

                return Ok(());
            }

            editor
                .update_in(cx, |editor, window, cx| {
                    if !editor.focus_handle.is_focused(window) || editor.cursor() != offset {
                        return;
                    }

                    _ = menu.update(cx, |menu, cx| {
                        menu.show(offset, completions, is_incomplete, window, cx);
                    });

                    cx.notify();
                })
                .ok();

            Ok(())
        });
    }

    /// Schedule an inline completion request after debouncing.
    pub(crate) fn schedule_inline_completion(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Clear any existing inline completion on text change
        self.clear_inline_completion(cx);

        let Some(provider) = self.lsp.completion_provider.clone() else {
            return;
        };

        let offset = self.cursor();
        let text = self.text.clone();
        let debounce = provider.inline_completion_debounce();
        let background_executor = cx.background_executor().clone();

        self.inline_completion.task = cx.spawn_in(window, async move |editor, cx| {
            // Debounce: wait before fetching to avoid unnecessary requests while typing
            background_executor.timer(debounce).await;

            // Now fetch the inline completion after the debounce period
            let task = editor.update_in(cx, |editor, window, cx| {
                // Check if cursor has moved during debounce
                if editor.cursor() != offset {
                    return None;
                }

                // Don't fetch if completion menu is open
                if editor.is_context_menu_open(cx) {
                    return None;
                }

                let trigger = InlineCompletionContext {
                    trigger_kind: InlineCompletionTriggerKind::Automatic,
                    selected_completion_info: None,
                };

                Some(provider.inline_completion(&text, offset, trigger, window, cx))
            })?;

            let Some(task) = task else {
                return Ok(InlineCompletionResponse::Array(vec![]));
            };

            let response = task.await?;

            editor.update_in(cx, |editor, _window, cx| {
                // Only apply if cursor still hasn't moved
                if editor.cursor() != offset {
                    return;
                }

                // Don't show if completion menu opened while we were fetching
                if editor.is_context_menu_open(cx) {
                    return;
                }

                if let Some(item) = match response.clone() {
                    InlineCompletionResponse::Array(items) => items.into_iter().next(),
                    InlineCompletionResponse::List(comp_list) => comp_list.items.into_iter().next(),
                } {
                    editor.inline_completion.item = Some(item);
                    cx.notify();
                }
            })?;

            Ok(response)
        });
    }

    /// Check if an inline completion suggestion is currently displayed.
    #[inline]
    pub(crate) fn has_inline_completion(&self) -> bool {
        self.inline_completion.item.is_some()
    }

    /// Clear the inline completion suggestion.
    pub(crate) fn clear_inline_completion(&mut self, cx: &mut Context<Self>) {
        self.inline_completion = InlineCompletion::default();
        cx.notify();
    }

    /// Accept the inline completion, inserting it at the cursor position.
    /// Returns true if a completion was accepted, false if there was none.
    pub(crate) fn accept_inline_completion(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(completion_item) = self.inline_completion.item.take() else {
            return false;
        };

        let cursor = self.cursor();
        let range_utf16 = self.range_to_utf16(&(cursor..cursor));
        let completion_text = completion_item.insert_text;
        self.replace_text_in_range_silent(Some(range_utf16), &completion_text, window, cx);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_completion_replaces_only_the_identifier_before_the_cursor() {
        let text = Rope::from("value.Δelta_name");
        assert_eq!(completion_word_start(&text, text.len()), "value.".len());

        let punctuation = Rope::from("Type::");
        assert_eq!(
            completion_word_start(&punctuation, punctuation.len()),
            punctuation.len()
        );
    }

    #[test]
    fn incomplete_completion_lists_retrigger_without_losing_trigger_characters() {
        let invoked = CompletionContext {
            trigger_kind: lsp_types::CompletionTriggerKind::INVOKED,
            trigger_character: None,
        };
        assert_eq!(
            completion_context_for_open_menu(Some(invoked), true, true, "m")
                .unwrap()
                .trigger_kind,
            lsp_types::CompletionTriggerKind::TRIGGER_FOR_INCOMPLETE_COMPLETIONS
        );

        let trigger = CompletionContext {
            trigger_kind: lsp_types::CompletionTriggerKind::TRIGGER_CHARACTER,
            trigger_character: Some(".".into()),
        };
        assert_eq!(
            completion_context_for_open_menu(Some(trigger.clone()), true, true, "."),
            Some(trigger)
        );
        assert_eq!(
            completion_context_for_open_menu(None, true, true, "")
                .unwrap()
                .trigger_kind,
            lsp_types::CompletionTriggerKind::TRIGGER_FOR_INCOMPLETE_COMPLETIONS
        );
    }
}
