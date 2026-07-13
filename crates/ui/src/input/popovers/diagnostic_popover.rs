use std::rc::Rc;

use gpui::{
    App, AppContext as _, Bounds, Context, Empty, Entity, IntoElement, ParentElement, Pixels,
    Point, Render, Styled, Window, div, prelude::FluentBuilder as _, px,
};

use crate::{
    StyledExt,
    highlighter::DiagnosticEntry,
    input::{
        InputState,
        popovers::{Popover, render_markdown},
    },
};

pub struct DiagnosticPopover {
    state: Entity<InputState>,
    pub(crate) diagnostic: Rc<DiagnosticEntry>,
    bounds: Bounds<Pixels>,
    open: bool,
}

impl DiagnosticPopover {
    pub fn new(
        diagnostic: &DiagnosticEntry,
        state: Entity<InputState>,
        cx: &mut App,
    ) -> Entity<Self> {
        let diagnostic = Rc::new(diagnostic.clone());

        cx.new(|_| Self {
            diagnostic,
            state,
            bounds: Bounds::default(),
            open: true,
        })
    }

    pub(crate) fn show(&mut self, cx: &mut Context<Self>) {
        self.open = true;
        cx.notify();
    }

    pub(crate) fn hide(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        cx.notify();
    }

    pub(crate) fn check_to_hide(&mut self, mouse_position: Point<Pixels>, cx: &mut Context<Self>) {
        if !self.open {
            return;
        }

        let padding = px(5.);
        let bounds = Bounds {
            origin: self.bounds.origin.map(|v| v - padding),
            size: self.bounds.size.map(|v| v + padding * 2.),
        };

        if !bounds.contains(&mouse_position) {
            self.hide(cx);
        }
    }
}

impl Render for DiagnosticPopover {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return Empty.into_any_element();
        }

        let message = self.diagnostic.message.clone();
        let metadata = diagnostic_metadata_label(&self.diagnostic);
        let tags = diagnostic_tag_labels(&self.diagnostic);
        let documentation_url = self
            .diagnostic
            .code_description
            .as_ref()
            .map(|description| description.href.as_str().to_string());
        let related = self
            .diagnostic
            .related_information
            .clone()
            .unwrap_or_default();
        let hidden_related_count = related.len().saturating_sub(5);

        let (border, bg, fg) = (
            self.diagnostic.severity.border(cx),
            self.diagnostic.severity.bg(cx),
            self.diagnostic.severity.fg(cx),
        );

        Popover::new(
            "diagnostic-popover",
            self.state.clone(),
            self.diagnostic.range.clone(),
            move |window, cx| {
                div()
                    .v_flex()
                    .gap_1()
                    .max_w(px(560.))
                    .when_some(metadata.clone(), |this, metadata| {
                        this.child(div().text_xs().child(metadata))
                    })
                    .when(!tags.is_empty(), |this| {
                        this.child(div().text_xs().child(tags.join(" · ")))
                    })
                    .child(render_markdown("message", message.clone(), window, cx))
                    .when_some(documentation_url.clone(), |this, url| {
                        this.child(div().text_xs().child(format!("Documentation: {url}")))
                    })
                    .when(!related.is_empty(), |this| {
                        this.child(div().text_xs().child("Related information"))
                            .children(related.iter().take(5).map(|information| {
                                div()
                                    .text_xs()
                                    .child(diagnostic_related_information_label(information))
                            }))
                    })
                    .when(hidden_related_count > 0, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .child(format!("+{hidden_related_count} more related locations")),
                        )
                    })
            },
        )
        .when(!self.open, |this| this.invisible())
        .px_1()
        .py_0p5()
        .bg(bg)
        .text_color(fg)
        .border_1()
        .border_color(border)
        .into_any_element()
    }
}

fn diagnostic_metadata_label(diagnostic: &DiagnosticEntry) -> Option<String> {
    match (diagnostic.source.as_deref(), diagnostic.code.as_deref()) {
        (Some(source), Some(code)) => Some(format!("{source}({code})")),
        (Some(source), None) => Some(source.to_string()),
        (None, Some(code)) => Some(code.to_string()),
        (None, None) => None,
    }
}

fn diagnostic_tag_labels(diagnostic: &DiagnosticEntry) -> Vec<&'static str> {
    let Some(tags) = diagnostic.tags.as_ref() else {
        return Vec::new();
    };
    let mut labels = Vec::new();
    if tags.contains(&lsp_types::DiagnosticTag::UNNECESSARY) {
        labels.push("Unnecessary");
    }
    if tags.contains(&lsp_types::DiagnosticTag::DEPRECATED) {
        labels.push("Deprecated");
    }
    labels
}

fn diagnostic_related_information_label(
    information: &lsp_types::DiagnosticRelatedInformation,
) -> String {
    let uri = information.location.uri.as_str();
    let target = uri
        .rsplit('/')
        .next()
        .filter(|value| !value.is_empty())
        .unwrap_or(uri);
    format!(
        "{target}:{}:{} — {}",
        information.location.range.start.line + 1,
        information.location.range.start.character + 1,
        information.message
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::highlighter::Diagnostic;

    #[test]
    fn diagnostic_popover_formats_rich_protocol_metadata() {
        let protocol = lsp_types::Diagnostic {
            range: lsp_types::Range::new(
                lsp_types::Position::new(1, 2),
                lsp_types::Position::new(1, 5),
            ),
            code: Some(lsp_types::NumberOrString::Number(17)),
            source: Some("test-server".to_string()),
            message: "deprecated value".to_string(),
            tags: Some(vec![
                lsp_types::DiagnosticTag::UNNECESSARY,
                lsp_types::DiagnosticTag::DEPRECATED,
            ]),
            ..Default::default()
        };
        let entry = DiagnosticEntry {
            range: 0..3,
            diagnostic: Diagnostic::from(protocol),
        };

        assert_eq!(
            diagnostic_metadata_label(&entry).as_deref(),
            Some("test-server(17)")
        );
        assert_eq!(
            diagnostic_tag_labels(&entry),
            vec!["Unnecessary", "Deprecated"]
        );

        let information = lsp_types::DiagnosticRelatedInformation {
            location: lsp_types::Location {
                uri: "file:///workspace/src/related.rs".parse().unwrap(),
                range: lsp_types::Range::new(
                    lsp_types::Position::new(3, 1),
                    lsp_types::Position::new(3, 4),
                ),
            },
            message: "declared here".to_string(),
        };
        assert_eq!(
            diagnostic_related_information_label(&information),
            "related.rs:4:2 — declared here"
        );
    }
}
