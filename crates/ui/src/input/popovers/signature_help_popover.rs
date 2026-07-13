use std::ops::Range;

use gpui::{
    App, AppContext as _, Entity, HighlightStyle, IntoElement, ParentElement as _, Render,
    Styled as _, StyledText, Window, div, prelude::FluentBuilder as _,
};
use lsp_types::{Documentation, ParameterLabel, SignatureHelp, SignatureInformation};

use crate::{
    ActiveTheme as _, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{
        InputState,
        popovers::{Popover, render_markdown},
    },
    v_flex,
};

const MAX_DOCUMENTATION_CHARS: usize = 16_384;

pub(crate) struct SignatureHelpPopover {
    editor: Entity<InputState>,
    anchor_offset: usize,
    help: SignatureHelp,
}

impl SignatureHelpPopover {
    pub(crate) fn new(
        editor: Entity<InputState>,
        anchor_offset: usize,
        help: SignatureHelp,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|_| Self {
            editor,
            anchor_offset,
            help,
        })
    }

    pub(crate) fn help(&self) -> &SignatureHelp {
        &self.help
    }

    pub(crate) fn set(
        &mut self,
        anchor_offset: usize,
        help: SignatureHelp,
        cx: &mut gpui::Context<Self>,
    ) {
        self.anchor_offset = anchor_offset;
        self.help = help;
        cx.notify();
    }

    pub(crate) fn previous(&mut self, cx: &mut gpui::Context<Self>) -> bool {
        let active = self.active_signature_index();
        if self.help.signatures.len() < 2 || active == 0 {
            return false;
        }
        self.help.active_signature = Some((active - 1) as u32);
        cx.notify();
        true
    }

    pub(crate) fn next(&mut self, cx: &mut gpui::Context<Self>) -> bool {
        let active = self.active_signature_index();
        if self.help.signatures.len() < 2 || active + 1 >= self.help.signatures.len() {
            return false;
        }
        self.help.active_signature = Some((active + 1) as u32);
        cx.notify();
        true
    }

    fn active_signature_index(&self) -> usize {
        (self.help.active_signature.unwrap_or(0) as usize)
            .min(self.help.signatures.len().saturating_sub(1))
    }
}

fn utf16_offset_to_byte(text: &str, target: u32) -> Option<usize> {
    let mut utf16 = 0_u32;
    for (byte, character) in text.char_indices() {
        if utf16 == target {
            return Some(byte);
        }
        utf16 = utf16.checked_add(character.len_utf16() as u32)?;
        if utf16 > target {
            return None;
        }
    }
    (utf16 == target).then_some(text.len())
}

fn is_identifier_character(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

fn simple_parameter_range(label: &str, parameter: &str) -> Option<Range<usize>> {
    if parameter.is_empty() {
        return None;
    }
    label.match_indices(parameter).find_map(|(start, matched)| {
        let end = start + matched.len();
        let left_ok = label[..start]
            .chars()
            .next_back()
            .is_none_or(|character| !is_identifier_character(character));
        let right_ok = label[end..]
            .chars()
            .next()
            .is_none_or(|character| !is_identifier_character(character));
        (left_ok && right_ok).then_some(start..end)
    })
}

fn active_parameter_range(
    signature: &SignatureInformation,
    active_parameter: usize,
) -> Option<Range<usize>> {
    let parameter = signature.parameters.as_ref()?.get(active_parameter)?;
    match &parameter.label {
        ParameterLabel::Simple(label) => simple_parameter_range(&signature.label, label),
        ParameterLabel::LabelOffsets([start, end]) => {
            let start = utf16_offset_to_byte(&signature.label, *start)?;
            let end = utf16_offset_to_byte(&signature.label, *end)?;
            (start < end && end <= signature.label.len()).then_some(start..end)
        }
    }
}

fn documentation_text(documentation: &Documentation) -> Option<String> {
    let value = match documentation {
        Documentation::String(value) => value,
        Documentation::MarkupContent(content) => &content.value,
    };
    let value = value.trim();
    (!value.is_empty()).then(|| value.chars().take(MAX_DOCUMENTATION_CHARS).collect())
}

fn signature_documentation(
    signature: &SignatureInformation,
    active_parameter: usize,
) -> Option<String> {
    let parameter_documentation = signature
        .parameters
        .as_ref()
        .and_then(|parameters| parameters.get(active_parameter))
        .and_then(|parameter| parameter.documentation.as_ref())
        .and_then(documentation_text);
    let signature_documentation = signature
        .documentation
        .as_ref()
        .and_then(documentation_text);
    match (parameter_documentation, signature_documentation) {
        (Some(parameter), Some(signature)) => Some(format!("{parameter}\n\n{signature}")),
        (Some(documentation), None) | (None, Some(documentation)) => Some(documentation),
        (None, None) => None,
    }
}

impl Render for SignatureHelpPopover {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        let active_signature_index = self.active_signature_index();
        let signature = self.help.signatures[active_signature_index].clone();
        let parameter_count = signature.parameters.as_ref().map_or(0, Vec::len);
        let active_parameter = signature
            .active_parameter
            .or(self.help.active_parameter)
            .unwrap_or(0) as usize;
        let active_parameter = active_parameter.min(parameter_count.saturating_sub(1));
        let highlight_range = active_parameter_range(&signature, active_parameter);
        let documentation = signature_documentation(&signature, active_parameter);
        let signature_count = self.help.signatures.len();
        let editor = self.editor.clone();
        let previous_editor = editor.clone();
        let next_editor = editor.clone();
        let anchor = self.anchor_offset;

        Popover::new(
            ("signature-help-popover", editor.entity_id()),
            editor,
            anchor..anchor,
            move |window, cx| {
                let highlights = highlight_range
                    .clone()
                    .map(|range| {
                        vec![(
                            range,
                            HighlightStyle {
                                color: Some(cx.theme().blue),
                                ..Default::default()
                            },
                        )]
                    })
                    .unwrap_or_default();
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .items_start()
                            .gap_2()
                            .when(signature_count > 1, |row| {
                                row.child(
                                    v_flex()
                                        .items_center()
                                        .child(
                                            Button::new("signature-help-previous")
                                                .xsmall()
                                                .ghost()
                                                .icon(IconName::ChevronUp)
                                                .on_click({
                                                    let editor = previous_editor.clone();
                                                    move |_, _, cx| {
                                                        let _ = editor.update(cx, |state, cx| {
                                                            state.previous_parameter_hint(cx);
                                                        });
                                                    }
                                                }),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(format!(
                                                    "{}/{}",
                                                    active_signature_index + 1,
                                                    signature_count
                                                )),
                                        )
                                        .child(
                                            Button::new("signature-help-next")
                                                .xsmall()
                                                .ghost()
                                                .icon(IconName::ChevronDown)
                                                .on_click({
                                                    let editor = next_editor.clone();
                                                    move |_, _, cx| {
                                                        let _ = editor.update(cx, |state, cx| {
                                                            state.next_parameter_hint(cx);
                                                        });
                                                    }
                                                }),
                                        ),
                                )
                            })
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .font_family(cx.theme().mono_font_family.clone())
                                    .child(
                                        StyledText::new(signature.label.clone())
                                            .with_highlights(highlights),
                                    ),
                            ),
                    )
                    .when_some(documentation.clone(), |panel, documentation| {
                        panel.child(
                            div()
                                .pt_1()
                                .border_t_1()
                                .border_color(cx.theme().border)
                                .child(render_markdown(
                                    "signature-help-documentation",
                                    documentation,
                                    window,
                                    cx,
                                )),
                        )
                    })
            },
        )
        .content_id(("signature-help-popover-content", self.editor.entity_id()))
        .dismiss_signature_help()
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{ParameterInformation, ParameterLabel};

    #[test]
    fn parameter_offsets_are_utf16_safe_and_reject_split_surrogates() {
        let signature = SignatureInformation {
            label: "call(😀value)".into(),
            documentation: None,
            parameters: Some(vec![ParameterInformation {
                label: ParameterLabel::LabelOffsets([7, 12]),
                documentation: None,
            }]),
            active_parameter: None,
        };
        assert_eq!(
            active_parameter_range(&signature, 0),
            Some("call(😀".len().."call(😀value".len())
        );
        let malformed = SignatureInformation {
            parameters: Some(vec![ParameterInformation {
                label: ParameterLabel::LabelOffsets([6, 12]),
                documentation: None,
            }]),
            ..signature
        };
        assert_eq!(active_parameter_range(&malformed, 0), None);
    }

    #[test]
    fn simple_parameter_labels_require_identifier_boundaries() {
        assert_eq!(
            simple_parameter_range("call(value, value2)", "value"),
            Some(5..10)
        );
        assert_eq!(simple_parameter_range("call(myvalue)", "value"), None);
    }

    #[test]
    fn active_parameter_documentation_precedes_signature_documentation() {
        let signature = SignatureInformation {
            label: "call(value)".into(),
            documentation: Some(Documentation::String("Signature docs".into())),
            parameters: Some(vec![ParameterInformation {
                label: ParameterLabel::Simple("value".into()),
                documentation: Some(Documentation::String("Parameter docs".into())),
            }]),
            active_parameter: None,
        };
        assert_eq!(
            signature_documentation(&signature, 0).as_deref(),
            Some("Parameter docs\n\nSignature docs")
        );
    }
}
