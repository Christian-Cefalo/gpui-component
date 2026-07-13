use std::ops::Range;

use gpui::{Context, Window};

use super::{
    EditorSelection, InputEvent, InputState, popovers::ContextMenu, snippet::ParsedSnippet,
};
use crate::input::multi_cursor::MAX_EDITOR_SELECTIONS;
use crate::input::snippet::SnippetSession;

impl InputState {
    pub fn has_active_snippet_session(&self) -> bool {
        self.snippet_session.is_some()
    }

    pub fn active_snippet_tabstop_index(&self) -> Option<u32> {
        self.snippet_session
            .as_ref()
            .and_then(SnippetSession::active_tabstop_index)
    }

    pub fn active_snippet_instance_count(&self) -> usize {
        self.snippet_session
            .as_ref()
            .map_or(0, SnippetSession::instance_count)
    }

    pub fn active_snippet_tabstop_ranges(&self) -> Vec<Range<usize>> {
        self.snippet_session
            .as_ref()
            .map_or_else(Vec::new, SnippetSession::active_ranges)
    }

    pub(super) fn cancel_snippet_session_if_selection_outside(&mut self, cx: &mut Context<Self>) {
        let selections = self
            .selections()
            .into_iter()
            .map(|selection| selection.range.start..selection.range.end)
            .collect::<Vec<_>>();
        let should_cancel = self
            .snippet_session
            .as_ref()
            .is_some_and(|session| !session.contains_selections(&selections));
        if should_cancel {
            self.snippet_session = None;
            self.hide_snippet_choice_menu(cx);
        }
    }

    pub(super) fn start_snippet_session(
        &mut self,
        parsed: &ParsedSnippet,
        insertion_start: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_snippet_sessions(parsed, &[insertion_start], window, cx);
    }

    pub(super) fn start_snippet_sessions(
        &mut self,
        parsed: &ParsedSnippet,
        insertion_starts: &[usize],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut unique_starts = insertion_starts.to_vec();
        unique_starts.sort_unstable();
        unique_starts.dedup();
        if insertion_starts.is_empty()
            || insertion_starts.len() > MAX_EDITOR_SELECTIONS
            || unique_starts.len() != insertion_starts.len()
        {
            self.snippet_session = None;
            self.hide_snippet_choice_menu(cx);
            return;
        }
        let Some(session) = SnippetSession::new_many(parsed, insertion_starts.iter().copied())
        else {
            self.snippet_session = None;
            self.hide_snippet_choice_menu(cx);
            return;
        };
        let ranges = session.active_ranges();
        if ranges.len() != insertion_starts.len() {
            self.snippet_session = None;
            self.hide_snippet_choice_menu(cx);
            return;
        }
        let is_final = session.active_is_final();
        self.snippet_session = (!is_final).then_some(session);
        self.select_snippet_ranges(ranges, cx);
        self.refresh_active_snippet_choice_menu(window, cx);
    }

    pub fn move_to_next_snippet_tabstop(
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
        if session.move_next().is_none() {
            self.hide_snippet_choice_menu(cx);
            return true;
        }
        let ranges = session.active_ranges();
        let is_final = session.active_is_final();
        self.snippet_session = (!is_final).then_some(session);
        self.select_snippet_ranges(ranges, cx);
        self.refresh_active_snippet_choice_menu(window, cx);
        true
    }

    pub fn move_to_previous_snippet_tabstop(
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
        let moved = session.move_previous().is_some();
        let ranges = session.active_ranges();
        self.snippet_session = Some(session);
        if moved {
            self.select_snippet_ranges(ranges, cx);
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
        let Some((ranges, choices)) = self.snippet_session.as_ref().and_then(|session| {
            Some((session.active_ranges(), session.active_choices()?.to_vec()))
        }) else {
            return false;
        };
        if ranges.is_empty()
            || ranges.iter().any(|range| range.end > self.text.len())
            || !choices.iter().any(|option| option == choice)
        {
            return false;
        }

        if ranges
            .iter()
            .any(|range| self.text.slice(range.clone()).to_string() != choice)
        {
            self.replace_selection_ranges(
                ranges.clone(),
                vec![choice.to_string(); ranges.len()],
                window,
                cx,
            );
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
        let primaries = session.active_ranges();
        if primaries.is_empty() {
            self.snippet_session = None;
            self.hide_snippet_choice_menu(cx);
            return false;
        }
        if primaries.iter().any(|range| range.end > self.text.len()) {
            self.snippet_session = None;
            self.hide_snippet_choice_menu(cx);
            return false;
        }
        let values = primaries
            .into_iter()
            .map(|range| self.text.slice(range).to_string())
            .collect::<Vec<_>>();
        let mut mirrors = session.active_mirror_replacements_for_values(&values);
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

    fn select_snippet_ranges(&mut self, ranges: Vec<Range<usize>>, cx: &mut Context<Self>) {
        if ranges.is_empty() || ranges.iter().any(|range| range.end > self.text.len()) {
            self.snippet_session = None;
            return;
        }
        self.set_editor_selections_internal(
            ranges
                .into_iter()
                .map(|range| EditorSelection::from_anchor_and_head(range.start, range.end)),
        );
        self.multi_cursor_occurrence_session = None;
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
