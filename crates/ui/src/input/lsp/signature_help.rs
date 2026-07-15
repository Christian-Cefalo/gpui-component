use std::time::Duration;

use anyhow::Result;
use gpui::{App, Context, Task};
use lsp_types::{
    Documentation, ParameterLabel, SignatureHelp, SignatureHelpContext, SignatureHelpTriggerKind,
    SignatureInformation,
};
use ropey::Rope;

use crate::input::{InputState, popovers::SignatureHelpPopover};

const SIGNATURE_HELP_DELAY: Duration = Duration::from_millis(120);
const MAX_SIGNATURES: usize = 100;
const MAX_SIGNATURE_CANDIDATES: usize = 400;
const MAX_PARAMETERS: usize = 256;
const MAX_SIGNATURE_LABEL_CHARS: usize = 8_192;
const MAX_PARAMETER_LABEL_CHARS: usize = 4_096;
const MAX_DOCUMENTATION_CHARS: usize = 16_384;
const MAX_RETAINED_SIGNATURE_HELP_CHARS: usize = 256 * 1_024;

/// Supplies LSP `textDocument/signatureHelp` results to an editor.
pub trait SignatureHelpProvider {
    /// Return the currently advertised trigger characters.
    ///
    /// This is queried at interaction time because a provider can be installed
    /// before its language server finishes initialization.
    fn trigger_characters(&self, _cx: &App) -> Vec<String> {
        Vec::new()
    }

    /// Return the currently advertised retrigger characters.
    fn retrigger_characters(&self, _cx: &App) -> Vec<String> {
        Vec::new()
    }

    /// Request signature help at the given UTF-8 byte offset.
    fn signature_help(
        &self,
        text: &Rope,
        offset: usize,
        context: SignatureHelpContext,
        cx: &mut Context<InputState>,
    ) -> Task<Result<Option<SignatureHelp>>>;
}

fn trigger_at(text: &Rope, offset: usize, triggers: &[String]) -> Option<String> {
    triggers
        .iter()
        .filter(|trigger| !trigger.is_empty() && trigger.len() <= offset)
        .filter(|trigger| {
            text.try_slice(offset - trigger.len()..offset)
                .is_ok_and(|suffix| suffix == trigger.as_str())
        })
        .max_by_key(|trigger| trigger.len())
        .cloned()
}

fn bounded_text(value: String, max_chars: usize) -> String {
    let mut characters = value.chars();
    let head = characters
        .by_ref()
        .take(max_chars.saturating_add(1))
        .collect::<Vec<_>>();
    if head.len() <= max_chars {
        return value;
    }
    if max_chars == 0 {
        return String::new();
    }
    head.into_iter()
        .take(max_chars - 1)
        .chain(std::iter::once('…'))
        .collect()
}

fn bound_documentation(
    documentation: Documentation,
    remaining_chars: &mut usize,
) -> Option<Documentation> {
    let max_chars = (*remaining_chars).min(MAX_DOCUMENTATION_CHARS);
    if max_chars == 0 {
        return None;
    }

    let documentation = match documentation {
        Documentation::String(value) => Documentation::String(bounded_text(value, max_chars)),
        Documentation::MarkupContent(mut content) => {
            content.value = bounded_text(content.value, max_chars);
            Documentation::MarkupContent(content)
        }
    };
    let retained_chars = match &documentation {
        Documentation::String(value) => value.chars().count(),
        Documentation::MarkupContent(content) => content.value.chars().count(),
    };
    *remaining_chars = (*remaining_chars).saturating_sub(retained_chars);
    Some(documentation)
}

fn normalize_signature(
    mut signature: SignatureInformation,
    remaining_chars: &mut usize,
) -> Option<SignatureInformation> {
    let label_chars = signature.label.chars().count();
    if label_chars > MAX_SIGNATURE_LABEL_CHARS || label_chars > *remaining_chars {
        return None;
    }
    *remaining_chars -= label_chars;

    if let Some(parameters) = signature.parameters.as_mut() {
        parameters.truncate(MAX_PARAMETERS);
        parameters.retain_mut(|parameter| match &mut parameter.label {
            ParameterLabel::Simple(label) => {
                if *remaining_chars == 0 {
                    return false;
                }
                let max_chars = (*remaining_chars).min(MAX_PARAMETER_LABEL_CHARS);
                *label = bounded_text(std::mem::take(label), max_chars);
                let retained_chars = label.chars().count();
                *remaining_chars = (*remaining_chars).saturating_sub(retained_chars);
                true
            }
            ParameterLabel::LabelOffsets(_) => true,
        });

        signature.documentation = signature
            .documentation
            .take()
            .and_then(|documentation| bound_documentation(documentation, remaining_chars));
        for parameter in parameters.iter_mut() {
            parameter.documentation = parameter
                .documentation
                .take()
                .and_then(|documentation| bound_documentation(documentation, remaining_chars));
        }

        signature.active_parameter = if parameters.is_empty() {
            None
        } else {
            Some(
                (signature.active_parameter.unwrap_or(0) as usize)
                    .min(parameters.len().saturating_sub(1)) as u32,
            )
        };
    } else {
        signature.documentation = signature
            .documentation
            .take()
            .and_then(|documentation| bound_documentation(documentation, remaining_chars));
        signature.active_parameter = None;
    }
    Some(signature)
}

fn normalize_signature_help(mut help: SignatureHelp) -> Option<SignatureHelp> {
    let requested_active_signature = help.active_signature.unwrap_or(0) as usize;
    let mut remaining_chars = MAX_RETAINED_SIGNATURE_HELP_CHARS;
    let retained = help
        .signatures
        .into_iter()
        .enumerate()
        .take(MAX_SIGNATURE_CANDIDATES)
        .filter_map(|(original_index, signature)| {
            normalize_signature(signature, &mut remaining_chars)
                .map(|signature| (original_index, signature))
        })
        .take(MAX_SIGNATURES)
        .collect::<Vec<_>>();
    if retained.is_empty() {
        return None;
    }

    let active_signature = retained
        .iter()
        .position(|(original_index, _)| *original_index == requested_active_signature)
        .or_else(|| {
            retained
                .iter()
                .rposition(|(original_index, _)| *original_index <= requested_active_signature)
        })
        .unwrap_or(0);
    help.signatures = retained
        .into_iter()
        .map(|(_, signature)| signature)
        .collect();
    help.active_signature = Some(active_signature as u32);
    let parameter_count = help.signatures[active_signature]
        .parameters
        .as_ref()
        .map_or(0, Vec::len);
    help.active_parameter = if parameter_count == 0 {
        None
    } else {
        Some(
            (help.active_parameter.unwrap_or(0) as usize).min(parameter_count.saturating_sub(1))
                as u32,
        )
    };
    Some(help)
}

impl InputState {
    fn active_signature_help(&self, cx: &App) -> Option<SignatureHelp> {
        self.signature_help_popover
            .as_ref()
            .map(|popover| popover.read(cx).help().clone())
    }

    fn request_signature_help(
        &mut self,
        context: SignatureHelpContext,
        delay: Duration,
        cx: &mut Context<Self>,
    ) {
        let Some(provider) = self.lsp.signature_help_provider.clone() else {
            self.close_signature_help(cx);
            return;
        };
        let text = self.text.clone();
        let offset = self.cursor();
        let generation = self.lsp.begin_signature_help_request();
        let editor = cx.entity();

        self.lsp._signature_help_task = cx.spawn(async move |_, cx| {
            if !delay.is_zero() {
                cx.background_executor().timer(delay).await;
            }
            let request = editor.update(cx, |state, cx| {
                (state.lsp.signature_help_generation == generation)
                    .then(|| provider.signature_help(&text, offset, context, cx))
            });
            let Some(request) = request else {
                return;
            };
            let result = request.await;
            let _ = editor.update(cx, |state, cx| {
                if state.lsp.signature_help_generation != generation {
                    return;
                }
                match result.ok().flatten().and_then(normalize_signature_help) {
                    Some(help) => {
                        if let Some(popover) = state.signature_help_popover.as_ref() {
                            popover.update(cx, |popover, cx| {
                                popover.set(offset, help, cx);
                            });
                        } else {
                            state.signature_help_popover =
                                Some(SignatureHelpPopover::new(cx.entity(), offset, help, cx));
                        }
                    }
                    None => state.signature_help_popover = None,
                }
                cx.notify();
            });
        });
    }

    /// Trigger parameter hints explicitly, matching VS Code's Ctrl/Cmd+Shift+Space action.
    pub fn trigger_parameter_hints(&mut self, cx: &mut Context<Self>) {
        let active_signature_help = self.active_signature_help(cx);
        self.request_signature_help(
            SignatureHelpContext {
                trigger_kind: SignatureHelpTriggerKind::INVOKED,
                trigger_character: None,
                is_retrigger: active_signature_help.is_some(),
                active_signature_help,
            },
            Duration::ZERO,
            cx,
        );
    }

    pub(crate) fn handle_signature_help_text_change(
        &mut self,
        allow_new_trigger: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(provider) = self.lsp.signature_help_provider.clone() else {
            return;
        };
        let active_signature_help = self.active_signature_help(cx);
        if !allow_new_trigger && active_signature_help.is_none() {
            return;
        }
        let mut trigger_characters = provider.trigger_characters(cx);
        if active_signature_help.is_some() {
            trigger_characters.extend(provider.retrigger_characters(cx));
        }
        trigger_characters.sort();
        trigger_characters.dedup();
        let trigger_character = trigger_at(&self.text, self.cursor(), &trigger_characters);
        if trigger_character.is_none() && active_signature_help.is_none() {
            return;
        }
        self.request_signature_help(
            SignatureHelpContext {
                trigger_kind: if trigger_character.is_some() {
                    SignatureHelpTriggerKind::TRIGGER_CHARACTER
                } else {
                    SignatureHelpTriggerKind::CONTENT_CHANGE
                },
                trigger_character,
                is_retrigger: active_signature_help.is_some(),
                active_signature_help,
            },
            SIGNATURE_HELP_DELAY,
            cx,
        );
    }

    pub(crate) fn retrigger_signature_help_after_cursor_move(&mut self, cx: &mut Context<Self>) {
        let Some(active_signature_help) = self.active_signature_help(cx) else {
            return;
        };
        self.request_signature_help(
            SignatureHelpContext {
                trigger_kind: SignatureHelpTriggerKind::CONTENT_CHANGE,
                trigger_character: None,
                is_retrigger: true,
                active_signature_help: Some(active_signature_help),
            },
            SIGNATURE_HELP_DELAY,
            cx,
        );
    }

    /// Select the previous overload. At the first overload, close the hints,
    /// matching VS Code's default non-cycling parameter-hint setting.
    pub fn previous_parameter_hint(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(popover) = self.signature_help_popover.as_ref().cloned() else {
            return false;
        };
        let keep_open = popover.update(cx, |popover, cx| popover.previous(cx));
        if !keep_open {
            self.close_signature_help(cx);
        }
        true
    }

    /// Select the next overload. At the last overload, close the hints.
    pub fn next_parameter_hint(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(popover) = self.signature_help_popover.as_ref().cloned() else {
            return false;
        };
        let keep_open = popover.update(cx, |popover, cx| popover.next(cx));
        if !keep_open {
            self.close_signature_help(cx);
        }
        true
    }

    /// Close parameter hints and cancel any pending provider request.
    pub fn close_signature_help(&mut self, cx: &mut Context<Self>) -> bool {
        let was_visible = self.signature_help_popover.take().is_some();
        self.lsp.cancel_signature_help_request();
        if was_visible {
            cx.notify();
        }
        was_visible
    }

    /// Return the normalized signature-help snapshot exposed by the editor.
    pub fn signature_help(&self, cx: &App) -> Option<SignatureHelp> {
        self.active_signature_help(cx)
    }

    pub(crate) fn signature_help_has_multiple(&self, cx: &App) -> bool {
        self.signature_help_popover
            .as_ref()
            .is_some_and(|popover| popover.read(cx).help().signatures.len() > 1)
    }

    pub(crate) fn on_action_trigger_parameter_hints(
        &mut self,
        _: &crate::input::TriggerParameterHints,
        _: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        self.trigger_parameter_hints(cx);
    }
}

impl super::Lsp {
    pub(super) fn begin_signature_help_request(&mut self) -> u64 {
        self.signature_help_generation = self.signature_help_generation.wrapping_add(1);
        self._signature_help_task = Task::ready(());
        self.signature_help_generation
    }

    pub(super) fn cancel_signature_help_request(&mut self) {
        self.signature_help_generation = self.signature_help_generation.wrapping_add(1);
        self._signature_help_task = Task::ready(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{ParameterInformation, ParameterLabel};

    #[test]
    fn trigger_matching_prefers_the_longest_advertised_suffix() {
        let text = Rope::from("call::");
        assert_eq!(
            trigger_at(&text, text.len(), &[":".into(), "::".into()]),
            Some("::".into())
        );
        assert_eq!(trigger_at(&text, text.len(), &["(".into()]), None);
    }

    #[test]
    fn normalization_bounds_signatures_parameters_and_active_indices() {
        let signature = SignatureInformation {
            label: "call(value)".into(),
            documentation: None,
            parameters: Some(
                (0..300)
                    .map(|_| ParameterInformation {
                        label: ParameterLabel::Simple("value".into()),
                        documentation: None,
                    })
                    .collect(),
            ),
            active_parameter: None,
        };
        let help = normalize_signature_help(SignatureHelp {
            signatures: vec![signature; 120],
            active_signature: Some(900),
            active_parameter: Some(900),
        })
        .unwrap();
        assert_eq!(help.signatures.len(), MAX_SIGNATURES);
        assert_eq!(
            help.signatures[0].parameters.as_ref().unwrap().len(),
            MAX_PARAMETERS
        );
        assert_eq!(help.active_signature, Some(99));
        assert_eq!(help.active_parameter, Some(255));
    }

    #[test]
    fn normalization_rejects_empty_or_unbounded_signature_sets() {
        assert!(
            normalize_signature_help(SignatureHelp {
                signatures: Vec::new(),
                active_signature: None,
                active_parameter: None,
            })
            .is_none()
        );
        assert!(
            normalize_signature_help(SignatureHelp {
                signatures: vec![SignatureInformation {
                    label: "x".repeat(MAX_SIGNATURE_LABEL_CHARS + 1),
                    documentation: None,
                    parameters: None,
                    active_parameter: None,
                }],
                active_signature: None,
                active_parameter: None,
            })
            .is_none()
        );
    }

    #[test]
    fn normalization_bounds_retained_documentation_and_parameter_labels() {
        let help = normalize_signature_help(SignatureHelp {
            signatures: (0..MAX_SIGNATURES)
                .map(|index| SignatureInformation {
                    label: format!("call_{index}(value)"),
                    documentation: Some(Documentation::String(
                        "d".repeat(MAX_DOCUMENTATION_CHARS * 2),
                    )),
                    parameters: Some(vec![ParameterInformation {
                        label: ParameterLabel::Simple("value".repeat(MAX_PARAMETER_LABEL_CHARS)),
                        documentation: Some(Documentation::String(
                            "p".repeat(MAX_DOCUMENTATION_CHARS * 2),
                        )),
                    }]),
                    active_parameter: Some(0),
                })
                .collect(),
            active_signature: None,
            active_parameter: Some(0),
        })
        .unwrap();

        let retained_chars = help
            .signatures
            .iter()
            .map(|signature| {
                let signature_documentation = signature
                    .documentation
                    .as_ref()
                    .map_or(0, documentation_char_count);
                let parameter_chars = signature
                    .parameters
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .map(|parameter| {
                        let label = match &parameter.label {
                            ParameterLabel::Simple(label) => label.chars().count(),
                            ParameterLabel::LabelOffsets(_) => 0,
                        };
                        label
                            + parameter
                                .documentation
                                .as_ref()
                                .map_or(0, documentation_char_count)
                    })
                    .sum::<usize>();
                signature.label.chars().count() + signature_documentation + parameter_chars
            })
            .sum::<usize>();
        assert!(retained_chars <= MAX_RETAINED_SIGNATURE_HELP_CHARS);
        let parameter = &help.signatures[0].parameters.as_ref().unwrap()[0];
        let ParameterLabel::Simple(label) = &parameter.label else {
            panic!("expected a simple parameter label");
        };
        assert_eq!(label.chars().count(), MAX_PARAMETER_LABEL_CHARS);
        assert!(label.ends_with('…'));
    }

    #[test]
    fn normalization_preserves_the_active_signature_after_filtering() {
        let help = normalize_signature_help(SignatureHelp {
            signatures: vec![
                SignatureInformation {
                    label: "x".repeat(MAX_SIGNATURE_LABEL_CHARS + 1),
                    documentation: None,
                    parameters: None,
                    active_parameter: None,
                },
                SignatureInformation {
                    label: "selected(value)".into(),
                    documentation: None,
                    parameters: None,
                    active_parameter: None,
                },
            ],
            active_signature: Some(1),
            active_parameter: None,
        })
        .unwrap();

        assert_eq!(help.signatures.len(), 1);
        assert_eq!(help.signatures[0].label, "selected(value)");
        assert_eq!(help.active_signature, Some(0));
    }

    fn documentation_char_count(documentation: &Documentation) -> usize {
        match documentation {
            Documentation::String(value) => value.chars().count(),
            Documentation::MarkupContent(content) => content.value.chars().count(),
        }
    }
}
