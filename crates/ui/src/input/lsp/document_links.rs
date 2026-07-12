use std::ops::Range as ByteRange;

use anyhow::Result;
use gpui::{
    App, Context, HighlightStyle, Hitbox, MouseDownEvent, Task, UnderlineStyle, Window, px,
};
use instant::Duration;
use lsp_types::DocumentLink;
use ropey::Rope;

use crate::{
    highlighter::HighlightTheme,
    input::{InputState, Lsp, OpenDocumentLink, RopeExt as _, element::TextElement},
};

const DOCUMENT_LINK_DEBOUNCE: Duration = Duration::from_millis(1_000);
const MAX_DOCUMENT_LINKS: usize = 2_000;

/// Supplies, resolves, and activates LSP document links.
///
/// Ranges use the editor's scalar-position convention. Integrations that talk
/// to an LSP server must map UTF-16 protocol positions at the boundary.
pub trait DocumentLinkProvider {
    fn can_resolve_document_links(&self) -> bool {
        false
    }

    fn document_links(
        &self,
        text: &Rope,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Vec<DocumentLink>>>;

    fn resolve_document_link(
        &self,
        _text: &Rope,
        link: DocumentLink,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Task<Result<DocumentLink>> {
        Task::ready(Ok(link))
    }

    /// Activate a resolved link. Return true when the integration handled it.
    fn activate_document_link(
        &self,
        _link: &DocumentLink,
        _window: &mut Window,
        _cx: &mut Context<InputState>,
    ) -> bool {
        false
    }
}

fn valid_document_link(text: &Rope, link: &DocumentLink) -> bool {
    let range = link.range;
    range.start.line == range.end.line
        && range.start <= range.end
        && range.start != range.end
        && (range.start.line as usize) < text.lines_len()
        && (range.end.line as usize) < text.lines_len()
}

fn normalize_document_links(text: &Rope, mut links: Vec<DocumentLink>) -> Vec<DocumentLink> {
    links.retain(|link| valid_document_link(text, link));
    links.sort_by_key(|link| (link.range.start, link.range.end));
    links.truncate(MAX_DOCUMENT_LINKS);

    let mut normalized = Vec::<DocumentLink>::with_capacity(links.len());
    for link in links {
        if normalized.last().is_some_and(|previous| {
            previous.range.end.line == link.range.start.line
                && previous.range.end.character >= link.range.start.character
        }) {
            continue;
        }
        normalized.push(link);
    }
    normalized
}

fn byte_range(text: &Rope, link: &DocumentLink) -> ByteRange<usize> {
    text.position_to_offset(&link.range.start)..text.position_to_offset(&link.range.end)
}

impl Lsp {
    pub(crate) fn invalidate_document_links(&mut self) {
        self.document_link_generation = self.document_link_generation.wrapping_add(1);
        self.document_link_requested_generation = None;
        self.document_links.clear();
        self.active_document_link = None;
        self._document_link_task = Task::ready(());
        self._document_link_resolve_task = Task::ready(Ok(()));
    }

    pub fn document_links(&self) -> &[DocumentLink] {
        &self.document_links
    }

    pub(crate) fn document_link_at_offset(
        &self,
        text: &Rope,
        offset: usize,
    ) -> Option<&DocumentLink> {
        self.document_links
            .iter()
            .find(|link| byte_range(text, link).contains(&offset))
    }

    pub(crate) fn set_active_document_link_at(&mut self, text: &Rope, offset: usize) -> bool {
        let next = self.document_link_at_offset(text, offset).cloned();
        let changed = self.active_document_link != next;
        self.active_document_link = next;
        changed
    }

    pub(crate) fn clear_active_document_link(&mut self) -> bool {
        self.active_document_link.take().is_some()
    }

    pub(crate) fn document_link_styles_for_range(
        &self,
        text: &Rope,
        visible_range: &ByteRange<usize>,
        theme: &HighlightTheme,
    ) -> Vec<(ByteRange<usize>, HighlightStyle)> {
        self.document_links
            .iter()
            .filter_map(|link| {
                let range = byte_range(text, link);
                if range.end <= visible_range.start || range.start >= visible_range.end {
                    return None;
                }
                let active = self.active_document_link.as_ref() == Some(link);
                let mut style = if active {
                    theme.link_text.map(Into::into).unwrap_or_default()
                } else {
                    HighlightStyle::default()
                };
                style.underline = Some(UnderlineStyle {
                    thickness: px(1.),
                    ..UnderlineStyle::default()
                });
                Some((range, style))
            })
            .collect()
    }

    fn request_document_links(
        &mut self,
        text: &Rope,
        window: &mut Window,
        cx: &mut Context<InputState>,
    ) {
        let Some(provider) = self.document_link_provider.as_ref().cloned() else {
            self.invalidate_document_links();
            return;
        };
        let generation = self.document_link_generation;
        if self.document_link_requested_generation == Some(generation) {
            return;
        }
        self.document_link_requested_generation = Some(generation);

        let text = text.clone();
        let editor = cx.entity();
        self._document_link_task = cx.spawn_in(window, async move |_, cx| {
            cx.background_executor().timer(DOCUMENT_LINK_DEBOUNCE).await;
            let task = cx
                .update(|window, cx| provider.document_links(&text, window, cx))
                .ok();
            let Some(task) = task else {
                return;
            };
            let Ok(links) = task.await else {
                return;
            };
            let links = normalize_document_links(&text, links);
            let _ = editor.update(cx, |editor, cx| {
                if editor.lsp.document_link_generation != generation
                    || editor.lsp.document_link_requested_generation != Some(generation)
                {
                    return;
                }
                editor.lsp.document_links = links;
                editor.lsp.active_document_link = None;
                cx.notify();
            });
        });
    }
}

impl InputState {
    pub fn refresh_document_links(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.text.clone();
        self.lsp.request_document_links(&text, window, cx);
    }

    pub fn document_links(&self) -> &[DocumentLink] {
        self.lsp.document_links()
    }

    pub fn has_document_link_at_cursor(&self) -> bool {
        self.lsp
            .document_link_at_offset(&self.text, self.cursor())
            .is_some()
    }

    pub(crate) fn handle_hover_document_link(&mut self, offset: usize) -> bool {
        let text = self.text.clone();
        self.lsp.set_active_document_link_at(&text, offset);
        self.lsp.active_document_link.is_some()
    }

    pub(crate) fn clear_active_document_link(&mut self) -> bool {
        self.lsp.clear_active_document_link()
    }

    fn activate_resolved_document_link(
        &mut self,
        provider: &std::rc::Rc<dyn DocumentLinkProvider>,
        link: &DocumentLink,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if provider.activate_document_link(link, window, cx) {
            return;
        }
        let Some(target) = link.target.as_ref() else {
            return;
        };
        if matches!(
            target.scheme().map(|scheme| scheme.as_str()),
            Some("http" | "https")
        ) {
            cx.open_url(&target.to_string());
        }
    }

    fn activate_document_link(
        &mut self,
        link: DocumentLink,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(provider) = self.lsp.document_link_provider.as_ref().cloned() else {
            return;
        };
        if link.target.is_some() {
            self.activate_resolved_document_link(&provider, &link, window, cx);
            return;
        }
        if !provider.can_resolve_document_links() {
            return;
        }

        let generation = self.lsp.document_link_generation;
        let text = self.text.clone();
        let original = link.clone();
        let task = provider.resolve_document_link(&text, link, window, cx);
        let editor = cx.entity();
        self.lsp._document_link_resolve_task = cx.spawn_in(window, async move |_, cx| {
            let resolved = task.await?;
            let _ = editor.update_in(cx, |editor, window, cx| {
                if editor.lsp.document_link_generation != generation {
                    return;
                }
                let Some(index) = editor
                    .lsp
                    .document_links
                    .iter()
                    .position(|candidate| candidate == &original)
                else {
                    return;
                };
                if resolved.range != original.range || !valid_document_link(&editor.text, &resolved)
                {
                    return;
                }
                editor.lsp.document_links[index] = resolved.clone();
                editor.activate_resolved_document_link(&provider, &resolved, window, cx);
                cx.notify();
            });
            Ok(())
        });
    }

    pub fn open_document_link_at_cursor(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(link) = self
            .lsp
            .document_link_at_offset(&self.text, self.cursor())
            .cloned()
        else {
            return false;
        };
        self.activate_document_link(link, window, cx);
        true
    }

    pub(crate) fn on_action_open_document_link(
        &mut self,
        _: &OpenDocumentLink,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_document_link_at_cursor(window, cx);
    }

    pub(crate) fn handle_click_document_link(
        &mut self,
        event: &MouseDownEvent,
        offset: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !event.modifiers.secondary() {
            return false;
        }
        let Some(link) = self
            .lsp
            .document_link_at_offset(&self.text, offset)
            .cloned()
        else {
            return false;
        };
        self.activate_document_link(link, window, cx);
        true
    }
}

impl TextElement {
    pub(crate) fn layout_document_link_hitbox(
        &self,
        editor: &InputState,
        window: &mut Window,
    ) -> Option<Hitbox> {
        let link = editor.lsp.active_document_link.as_ref()?;
        let bounds = editor.range_to_bounds(&byte_range(&editor.text, link))?;
        Some(window.insert_hitbox(bounds, gpui::HitboxBehavior::Normal))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{Position, Range};

    fn link(start: u32, end: u32, target: Option<&str>) -> DocumentLink {
        DocumentLink {
            range: Range::new(Position::new(0, start), Position::new(0, end)),
            target: target.map(|target| target.parse().unwrap()),
            tooltip: None,
            data: None,
        }
    }

    #[test]
    fn links_are_bounded_sorted_and_overlaps_are_removed() {
        let text = Rope::from_str("alpha beta gamma\n");
        let links = normalize_document_links(
            &text,
            vec![
                link(11, 16, None),
                link(0, 5, Some("https://example.com")),
                link(4, 9, None),
            ],
        );

        assert_eq!(links.len(), 2);
        assert_eq!(
            links[0].range,
            Range::new(Position::new(0, 0), Position::new(0, 5))
        );
        assert_eq!(
            links[1].range,
            Range::new(Position::new(0, 11), Position::new(0, 16))
        );
    }

    #[test]
    fn malformed_empty_multiline_and_out_of_document_links_are_rejected() {
        let text = Rope::from_str("alpha\nbeta\n");
        let mut candidates = vec![
            link(1, 1, None),
            DocumentLink {
                range: Range::new(Position::new(0, 1), Position::new(1, 1)),
                target: None,
                tooltip: None,
                data: None,
            },
            DocumentLink {
                range: Range::new(Position::new(8, 0), Position::new(8, 1)),
                target: None,
                tooltip: None,
                data: None,
            },
        ];
        candidates.push(link(0, 5, None));

        let normalized = normalize_document_links(&text, candidates);
        assert_eq!(normalized, vec![link(0, 5, None)]);
    }
}
