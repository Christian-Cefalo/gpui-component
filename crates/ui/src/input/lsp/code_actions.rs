use anyhow::Result;
use gpui::{
    AnyElement, App, Context, Entity, InteractiveElement, IntoElement, MouseButton, ParentElement,
    SharedString, StatefulInteractiveElement, Styled, Task, Window, deferred, div, px,
};
use lsp_types::CodeAction;
use std::ops::Range;
use std::time::Duration;

use crate::ActiveTheme;
use crate::input::{
    InputState, ToggleCodeActions,
    popovers::{CodeActionItem, CodeActionMenu, ContextMenu},
};

const AUTOMATIC_CODE_ACTION_DELAY: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CodeActionTrigger {
    #[default]
    Invoked,
    Automatic,
}

pub trait CodeActionProvider {
    /// The id for this CodeAction.
    fn id(&self) -> SharedString;

    /// Fetches code actions for the specified range.
    ///
    /// textDocument/codeAction
    ///
    /// https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocument_codeAction
    fn code_actions(
        &self,
        state: Entity<InputState>,
        range: Range<usize>,
        trigger: CodeActionTrigger,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Vec<CodeAction>>>;

    /// Performs the specified code action.
    fn perform_code_action(
        &self,
        state: Entity<InputState>,
        action: CodeAction,
        push_to_history: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>>;
}

impl InputState {
    pub(crate) fn on_action_toggle_code_actions(
        &mut self,
        _: &ToggleCodeActions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.handle_code_action_trigger(window, cx)
    }

    /// Show code actions for the cursor.
    pub(crate) fn handle_code_action_trigger(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.lsp.invalidate_automatic_code_actions();
        let providers = self.lsp.code_action_providers.clone();
        let menu = match self.context_menu_content.as_ref() {
            Some(ContextMenu::CodeAction(menu)) => Some(menu),
            _ => None,
        };

        let menu = match menu {
            Some(menu) => menu.clone(),
            None => {
                let menu = CodeActionMenu::new(cx.entity(), window, cx);
                self.context_menu_content = Some(ContextMenu::CodeAction(menu.clone()));
                menu
            }
        };

        let range = self.selected_range.start..self.selected_range.end;

        let state = cx.entity();
        self._context_menu_task = cx.spawn_in(window, async move |editor, cx| {
            let mut provider_responses = vec![];
            _ = cx.update(|window, cx| {
                for provider in providers {
                    let task = provider.code_actions(
                        state.clone(),
                        range.clone(),
                        CodeActionTrigger::Invoked,
                        window,
                        cx,
                    );
                    provider_responses.push((provider.id(), task));
                }
            });

            let mut code_actions: Vec<CodeActionItem> = vec![];
            for (provider_id, provider_responses) in provider_responses {
                if let Some(responses) = provider_responses.await.ok() {
                    code_actions.extend(responses.into_iter().map(|action| CodeActionItem {
                        provider_id: provider_id.clone(),
                        action,
                    }))
                }
            }

            if code_actions.is_empty() {
                _ = menu.update(cx, |menu, cx| {
                    menu.hide(cx);
                    cx.notify();
                });

                return Ok(());
            }
            editor
                .update_in(cx, |editor, window, cx| {
                    if !editor.focus_handle.is_focused(window) {
                        return;
                    }

                    _ = menu.update(cx, |menu, cx| {
                        menu.show(editor.cursor(), code_actions, window, cx);
                    });

                    cx.notify();
                })
                .ok();

            Ok(())
        });
    }

    pub(crate) fn refresh_automatic_code_actions(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.disabled
            || !self.mode.is_code_editor()
            || self.lsp.code_action_providers.is_empty()
            || !self.focus_handle.is_focused(window)
        {
            self.lsp.invalidate_automatic_code_actions();
            return;
        }

        let range = self.selected_range.start..self.selected_range.end;
        if self.lsp.automatic_code_action_requested_range.as_ref() == Some(&range) {
            return;
        }

        self.lsp.invalidate_automatic_code_actions();
        self.lsp.automatic_code_action_requested_range = Some(range.clone());
        let generation = self.lsp.code_action_generation;
        let providers = self.lsp.code_action_providers.clone();
        let state = cx.entity();
        self.lsp._code_action_task = cx.spawn_in(window, async move |editor, cx| {
            cx.background_executor()
                .timer(AUTOMATIC_CODE_ACTION_DELAY)
                .await;
            let current = editor
                .update_in(cx, |editor, window, _| {
                    editor.lsp.code_action_generation == generation
                        && (editor.selected_range.start..editor.selected_range.end) == range
                        && editor.focus_handle.is_focused(window)
                })
                .unwrap_or(false);
            if !current {
                return;
            }

            let mut provider_responses = Vec::new();
            _ = cx.update(|window, cx| {
                for provider in providers {
                    provider_responses.push((
                        provider.id(),
                        provider.code_actions(
                            state.clone(),
                            range.clone(),
                            CodeActionTrigger::Automatic,
                            window,
                            cx,
                        ),
                    ));
                }
            });

            let mut actions = Vec::new();
            for (provider_id, response) in provider_responses {
                if let Some(response) = response.await.ok() {
                    actions.extend(
                        response
                            .into_iter()
                            .filter(|action| action.disabled.is_none())
                            .map(|action| CodeActionItem {
                                provider_id: provider_id.clone(),
                                action,
                            }),
                    );
                }
            }

            _ = editor.update_in(cx, |editor, _, cx| {
                if editor.lsp.code_action_generation != generation
                    || (editor.selected_range.start..editor.selected_range.end) != range
                {
                    return;
                }
                editor.lsp.automatic_code_actions = actions;
                editor.lsp.automatic_code_action_range = Some(range.clone());
                cx.notify();
            });
        });
    }

    pub fn invalidate_code_actions(&mut self, cx: &mut Context<Self>) {
        self.lsp.invalidate_automatic_code_actions();
        cx.notify();
    }

    pub fn automatic_code_actions(&self) -> Vec<CodeAction> {
        self.lsp
            .automatic_code_actions
            .iter()
            .map(|item| item.action.clone())
            .collect()
    }

    pub fn automatic_code_action_range(&self) -> Option<Range<usize>> {
        self.lsp.automatic_code_action_range.clone()
    }

    fn show_automatic_code_actions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.lsp.automatic_code_actions.is_empty() {
            self.handle_code_action_trigger(window, cx);
            return;
        }
        let menu = match self.context_menu_content.as_ref() {
            Some(ContextMenu::CodeAction(menu)) => menu.clone(),
            _ => {
                let menu = CodeActionMenu::new(cx.entity(), window, cx);
                self.context_menu_content = Some(ContextMenu::CodeAction(menu.clone()));
                menu
            }
        };
        let offset = self
            .lsp
            .automatic_code_action_range
            .as_ref()
            .map(|range| range.end)
            .unwrap_or_else(|| self.cursor());
        let actions = self.lsp.automatic_code_actions.clone();
        menu.update(cx, |menu, cx| menu.show(offset, actions, window, cx));
    }

    pub fn open_automatic_code_actions(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.lsp.automatic_code_actions.is_empty() {
            return false;
        }
        self.show_automatic_code_actions(window, cx);
        true
    }

    pub(crate) fn render_automatic_code_action_indicator(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.lsp.automatic_code_actions.is_empty()
            || self.lsp.automatic_code_action_range.as_ref()
                != Some(&(self.selected_range.start..self.selected_range.end))
        {
            return None;
        }
        let layout = self.last_layout.as_ref()?;
        let cursor_origin = layout.cursor_bounds?.origin;
        let scroll_origin = self.scroll_handle.offset();
        let origin = scroll_origin + cursor_origin - self.input_bounds.origin;
        let left = (origin.x - px(24.)).max(px(2.));
        let warning = cx.theme().warning;
        Some(
            deferred(
                div()
                    .id("automatic-code-action-indicator")
                    .absolute()
                    .left(left)
                    .top(origin.y)
                    .size(px(20.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.))
                    .border_1()
                    .border_color(warning)
                    .bg(cx.theme().background)
                    .text_color(warning)
                    .text_sm()
                    .cursor_pointer()
                    .occlude()
                    .child("💡")
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(|editor, _, window, cx| {
                        cx.stop_propagation();
                        editor.open_automatic_code_actions(window, cx);
                    })),
            )
            .into_any_element(),
        )
    }

    pub(crate) fn perform_code_action(
        &mut self,
        item: &CodeActionItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let providers = self.lsp.code_action_providers.clone();
        let Some(provider) = providers
            .iter()
            .find(|provider| provider.id() == item.provider_id)
        else {
            return;
        };

        let state = cx.entity();
        let task = provider.perform_code_action(state, item.action.clone(), true, window, cx);

        cx.spawn_in(window, async move |_, _| {
            let _ = task.await;
        })
        .detach();
    }
}

impl super::Lsp {
    pub(crate) fn invalidate_automatic_code_actions(&mut self) {
        self.code_action_generation = self.code_action_generation.wrapping_add(1);
        self.automatic_code_actions.clear();
        self.automatic_code_action_range = None;
        self.automatic_code_action_requested_range = None;
        self._code_action_task = Task::ready(());
    }
}
