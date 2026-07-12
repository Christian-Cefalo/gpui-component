use std::ops::Range;

use gpui::{Context, Window};

use super::{InputEvent, InputState, Selection, popovers::ContextMenu, snippet::ParsedSnippet};
use crate::input::snippet::SnippetSession;

impl InputState {
    pub(super) fn start_snippet_session(
        &mut self,
        parsed: &ParsedSnippet,
        insertion_start: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = SnippetSession::new(parsed, insertion_start) else {
            self.snippet_session = None;
            self.hide_snippet_choice_menu(cx);
            return;
        };
        let Some(range) = session.active_range() else {
            self.snippet_session = None;
            self.hide_snippet_choice_menu(cx);
            return;
        };
        let is_final = session.active_is_final();
        self.snippet_session = (!is_final).then_some(session);
        self.select_snippet_range(range, cx);
        self.refresh_active_snippet_choice_menu(window, cx);
    }

    pub(super) fn move_to_next_snippet_tabstop(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.snippet_session.is_none() {
            return false;
        }
        if !self.synchronize_active_snippet_mirrors(window, cx) {
            return true;
        }
        let Some(mut session) = self.snippet_session.take() else {
            return true;
        };
        let Some(range) = session.move_next() else {
            self.hide_snippet_choice_menu(cx);
            return true;
        };
        let is_final = session.active_is_final();
        self.snippet_session = (!is_final).then_some(session);
        self.select_snippet_range(range, cx);
        self.refresh_active_snippet_choice_menu(window, cx);
        true
    }

    pub(super) fn move_to_previous_snippet_tabstop(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.snippet_session.is_none() {
            return false;
        }
        if !self.synchronize_active_snippet_mirrors(window, cx) {
            return true;
        }
        let Some(mut session) = self.snippet_session.take() else {
            return true;
        };
        let range = session.move_previous();
        self.snippet_session = Some(session);
        if let Some(range) = range {
            self.select_snippet_range(range, cx);
        }
        self.refresh_active_snippet_choice_menu(window, cx);
        true
    }

    pub(super) fn accept_active_snippet_choice(
        &mut self,
        choice: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((range, choices)) = self.snippet_session.as_ref().and_then(|session| {
            Some((session.active_range()?, session.active_choices()?.to_vec()))
        }) else {
            return false;
        };
        if range.end > self.text.len() || !choices.iter().any(|option| option == choice) {
            return false;
        }

        if self.text.slice(range.clone()).to_string() != choice {
            self.start_undo_transaction();
            let range_utf16 = self.range_to_utf16(&range);
            self.replace_text_in_range_silent(Some(range_utf16), choice, window, cx);
            self.end_undo_transaction();
        }
        _ = self.move_to_next_snippet_tabstop(window, cx);
        true
    }

    pub(super) fn refresh_active_snippet_choice_menu(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let active = self.snippet_session.as_ref().and_then(|session| {
            Some((session.active_range()?, session.active_choices()?.to_vec()))
        });
        let Some((range, choices)) = active else {
            self.hide_snippet_choice_menu(cx);
            return false;
        };
        if range.end > self.text.len() {
            self.snippet_session = None;
            self.hide_snippet_choice_menu(cx);
            return false;
        }

        let current_value = self.text.slice(range.clone()).to_string();
        let menu = self.completion_menu(window, cx);
        _ = menu.update(cx, |menu, cx| {
            menu.show_snippet_choices(range, &current_value, &choices, window, cx);
        });
        true
    }

    pub(super) fn synchronize_active_snippet_mirrors(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(session) = self.snippet_session.as_ref() else {
            return false;
        };
        let Some(primary) = session.active_range() else {
            self.snippet_session = None;
            self.hide_snippet_choice_menu(cx);
            return false;
        };
        if primary.end > self.text.len() {
            self.snippet_session = None;
            self.hide_snippet_choice_menu(cx);
            return false;
        }
        let value = self.text.slice(primary).to_string();
        let mut mirrors = session.active_mirror_replacements(&value);
        mirrors.sort_by_key(|(range, _)| (range.start, range.end));
        self.snippet_tracking_suspended = true;
        for (mirror, replacement) in mirrors.into_iter().rev() {
            if mirror.end > self.text.len() {
                self.snippet_tracking_suspended = false;
                self.snippet_session = None;
                self.hide_snippet_choice_menu(cx);
                return false;
            }
            if self.text.slice(mirror.clone()).to_string() == replacement {
                continue;
            }
            let range_utf16 = self.range_to_utf16(&mirror);
            self.replace_text_in_range_silent(Some(range_utf16), &replacement, window, cx);
            if let Some(session) = self.snippet_session.as_mut() {
                session.track_edit(mirror, replacement.len());
            }
        }
        self.snippet_tracking_suspended = false;
        true
    }

    fn select_snippet_range(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        if range.end > self.text.len() {
            self.snippet_session = None;
            return;
        }
        self.selected_range = Selection::from(range);
        self.selection_reversed = false;
        self.update_preferred_column();
        cx.emit(InputEvent::SelectionChange);
        cx.notify();
    }

    fn hide_snippet_choice_menu(&mut self, cx: &mut Context<Self>) {
        let Some(ContextMenu::Completion(menu)) = self.context_menu_content.as_ref() else {
            return;
        };
        let menu = menu.clone();
        if menu.read(cx).is_snippet_choice() {
            _ = menu.update(cx, |menu, cx| menu.hide(cx));
        }
    }
}
