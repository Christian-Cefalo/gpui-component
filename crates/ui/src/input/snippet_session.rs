use std::ops::Range;

use gpui::{Context, Window};

use super::{InputEvent, InputState, Selection, snippet::ParsedSnippet};
use crate::input::snippet::SnippetSession;

impl InputState {
    pub(super) fn start_snippet_session(
        &mut self,
        parsed: &ParsedSnippet,
        insertion_start: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = SnippetSession::new(parsed, insertion_start) else {
            self.snippet_session = None;
            return;
        };
        let Some(range) = session.active_range() else {
            self.snippet_session = None;
            return;
        };
        let is_final = session.active_is_final();
        self.snippet_session = (!is_final).then_some(session);
        self.select_snippet_range(range, cx);
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
            return true;
        };
        let is_final = session.active_is_final();
        self.snippet_session = (!is_final).then_some(session);
        self.select_snippet_range(range, cx);
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
            return false;
        };
        if primary.end > self.text.len() {
            self.snippet_session = None;
            return false;
        }
        let value = self.text.slice(primary).to_string();
        let mut mirrors = session.active_mirrors().to_vec();
        mirrors.sort_by_key(|range| (range.start, range.end));
        self.snippet_tracking_suspended = true;
        for mirror in mirrors.into_iter().rev() {
            if mirror.end > self.text.len() {
                self.snippet_tracking_suspended = false;
                self.snippet_session = None;
                return false;
            }
            if self.text.slice(mirror.clone()).to_string() == value {
                continue;
            }
            let range_utf16 = self.range_to_utf16(&mirror);
            self.replace_text_in_range_silent(Some(range_utf16), &value, window, cx);
            if let Some(session) = self.snippet_session.as_mut() {
                session.track_edit(mirror, value.len());
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
}
