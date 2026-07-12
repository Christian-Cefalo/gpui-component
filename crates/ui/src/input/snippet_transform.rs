use std::collections::HashSet;

use fancy_regex::{Captures, Regex, RegexBuilder};

const MAX_BACKTRACK_STEPS: usize = 250_000;
const MAX_COMPILED_REGEX_BYTES: usize = 2 * 1024 * 1024;
const MAX_TRANSFORM_MATCHES: usize = 10_000;
const MAX_TRANSFORM_OUTPUT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug)]
pub(super) struct SnippetTransform {
    pattern: String,
    options: String,
    regex: Regex,
    format: Vec<FormatPart>,
    global: bool,
    has_else_branch: bool,
}

impl PartialEq for SnippetTransform {
    fn eq(&self, other: &Self) -> bool {
        self.pattern == other.pattern
            && self.options == other.options
            && self.format == other.format
    }
}

impl Eq for SnippetTransform {}

impl SnippetTransform {
    pub(super) fn parse(pattern: &str, format: &str, options: &str) -> Option<Self> {
        let mut seen = HashSet::new();
        for option in options.chars() {
            if !seen.insert(option) || !matches!(option, 'g' | 'i' | 'm' | 's' | 'u') {
                return None;
            }
        }

        let mut builder = RegexBuilder::new(pattern);
        builder
            .case_insensitive(options.contains('i'))
            .multi_line(options.contains('m'))
            .dot_matches_new_line(options.contains('s'))
            .unicode_mode(true)
            .backtrack_limit(MAX_BACKTRACK_STEPS)
            .delegate_size_limit(MAX_COMPILED_REGEX_BYTES)
            .delegate_dfa_size_limit(MAX_COMPILED_REGEX_BYTES);
        let regex = builder.build().ok()?;
        let format_parts = parse_format(format);
        let has_else_branch = format_parts.iter().any(FormatPart::has_else_branch);

        Some(Self {
            pattern: pattern.to_string(),
            options: options.to_string(),
            regex,
            format: format_parts,
            global: options.contains('g'),
            has_else_branch,
        })
    }

    pub(super) fn apply(&self, value: &str) -> String {
        let mut rewritten = String::with_capacity(value.len());
        let mut previous_end = 0;
        let mut matched = false;

        for (match_index, captures) in self.regex.captures_iter(value).enumerate() {
            if match_index >= MAX_TRANSFORM_MATCHES {
                return value.to_string();
            }
            if matched && !self.global {
                break;
            }
            let captures = match captures {
                Ok(captures) => captures,
                Err(_) => return value.to_string(),
            };
            let Some(whole_match) = captures.get(0) else {
                return value.to_string();
            };
            if !append_bounded(&mut rewritten, &value[previous_end..whole_match.start()])
                || !render_format(&mut rewritten, &self.format, Some(&captures))
            {
                return value.to_string();
            }
            previous_end = whole_match.end();
            matched = true;
        }

        if matched {
            if !append_bounded(&mut rewritten, &value[previous_end..]) {
                return value.to_string();
            }
            rewritten
        } else if self.has_else_branch {
            if render_format(&mut rewritten, &self.format, None) {
                rewritten
            } else {
                value.to_string()
            }
        } else {
            value.to_string()
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum FormatPart {
    Text(String),
    Capture {
        index: usize,
        operation: CaptureOperation,
    },
}

impl FormatPart {
    fn has_else_branch(&self) -> bool {
        matches!(
            self,
            Self::Capture {
                operation: CaptureOperation::Else(_) | CaptureOperation::IfElse { .. },
                ..
            }
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CaptureOperation {
    Copy,
    Case(String),
    If(String),
    Else(String),
    IfElse {
        if_value: String,
        else_value: String,
    },
}

fn parse_format(source: &str) -> Vec<FormatPart> {
    let mut parts = Vec::new();
    let mut text = String::new();
    let mut offset = 0;

    while offset < source.len() {
        let character = source[offset..].chars().next().unwrap();
        if character == '\\' {
            offset += character.len_utf8();
            if offset < source.len() {
                let escaped = source[offset..].chars().next().unwrap();
                if matches!(escaped, '\\' | '$' | '}' | '/') {
                    text.push(escaped);
                    offset += escaped.len_utf8();
                } else {
                    text.push('\\');
                }
            } else {
                text.push('\\');
            }
            continue;
        }

        if character != '$' {
            text.push(character);
            offset += character.len_utf8();
            continue;
        }

        let expression_start = offset;
        offset += 1;
        if let Some((index, end)) = parse_decimal(source, offset) {
            flush_text(&mut parts, &mut text);
            parts.push(FormatPart::Capture {
                index,
                operation: CaptureOperation::Copy,
            });
            offset = end;
            continue;
        }

        if source[offset..].starts_with('{') {
            offset += 1;
            if let Some((body, end)) = take_braced_body(source, offset) {
                if let Some(capture) = parse_complex_capture(body) {
                    flush_text(&mut parts, &mut text);
                    parts.push(capture);
                    offset = end;
                    continue;
                }
            }
        }

        text.push('$');
        offset = expression_start + 1;
    }

    flush_text(&mut parts, &mut text);
    parts
}

fn parse_complex_capture(body: &str) -> Option<FormatPart> {
    let (index, offset) = parse_decimal(body, 0)?;
    let operation = if offset == body.len() {
        CaptureOperation::Copy
    } else {
        let remainder = body.get(offset..)?.strip_prefix(':')?;
        if let Some(case) = remainder.strip_prefix('/') {
            CaptureOperation::Case(case.to_string())
        } else if let Some(if_value) = remainder.strip_prefix('+') {
            CaptureOperation::If(unescape_format_value(if_value))
        } else if let Some(else_value) = remainder.strip_prefix('-') {
            CaptureOperation::Else(unescape_format_value(else_value))
        } else if let Some(conditional) = remainder.strip_prefix('?') {
            let split = find_unescaped(conditional, ':')?;
            CaptureOperation::IfElse {
                if_value: unescape_format_value(&conditional[..split]),
                else_value: unescape_format_value(&conditional[split + 1..]),
            }
        } else {
            CaptureOperation::Else(unescape_format_value(remainder))
        }
    };
    Some(FormatPart::Capture { index, operation })
}

fn take_braced_body(source: &str, mut offset: usize) -> Option<(&str, usize)> {
    let start = offset;
    let mut escaped = false;
    while offset < source.len() {
        let character = source[offset..].chars().next()?;
        if escaped {
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == '}' {
            return Some((&source[start..offset], offset + character.len_utf8()));
        }
        offset += character.len_utf8();
    }
    None
}

fn parse_decimal(source: &str, mut offset: usize) -> Option<(usize, usize)> {
    let start = offset;
    let mut value = 0usize;
    while offset < source.len() {
        let byte = source.as_bytes()[offset];
        if !byte.is_ascii_digit() {
            break;
        }
        value = value
            .saturating_mul(10)
            .saturating_add((byte - b'0') as usize);
        offset += 1;
    }
    (offset > start).then_some((value, offset))
}

fn find_unescaped(source: &str, needle: char) -> Option<usize> {
    let mut escaped = false;
    for (offset, character) in source.char_indices() {
        if escaped {
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == needle {
            return Some(offset);
        }
    }
    None
}

fn unescape_format_value(source: &str) -> String {
    let mut value = String::with_capacity(source.len());
    let mut characters = source.chars();
    while let Some(character) = characters.next() {
        if character == '\\' {
            match characters.next() {
                Some(escaped) if matches!(escaped, '\\' | '$' | '}' | '/' | ':') => {
                    value.push(escaped);
                }
                Some(escaped) => {
                    value.push('\\');
                    value.push(escaped);
                }
                None => value.push('\\'),
            }
        } else {
            value.push(character);
        }
    }
    value
}

fn flush_text(parts: &mut Vec<FormatPart>, text: &mut String) {
    if !text.is_empty() {
        parts.push(FormatPart::Text(std::mem::take(text)));
    }
}

fn render_format(
    output: &mut String,
    format: &[FormatPart],
    captures: Option<&Captures<'_>>,
) -> bool {
    for part in format {
        match part {
            FormatPart::Text(text) => {
                if !append_bounded(output, text) {
                    return false;
                }
            }
            FormatPart::Capture { index, operation } => {
                let value = captures
                    .and_then(|captures| captures.get(*index))
                    .map(|capture| capture.as_str())
                    .unwrap_or_default();
                if !append_bounded(output, &apply_capture_operation(value, operation)) {
                    return false;
                }
            }
        }
    }
    true
}

fn apply_capture_operation(value: &str, operation: &CaptureOperation) -> String {
    match operation {
        CaptureOperation::Copy => value.to_string(),
        CaptureOperation::Case(case) => apply_case(value, case),
        CaptureOperation::If(if_value) => (!value.is_empty())
            .then(|| if_value.clone())
            .unwrap_or_default(),
        CaptureOperation::Else(else_value) => {
            if value.is_empty() {
                else_value.clone()
            } else {
                value.to_string()
            }
        }
        CaptureOperation::IfElse {
            if_value,
            else_value,
        } => {
            if value.is_empty() {
                else_value.clone()
            } else {
                if_value.clone()
            }
        }
    }
}

fn apply_case(value: &str, case: &str) -> String {
    match case {
        "upcase" => value.to_uppercase(),
        "downcase" => value.to_lowercase(),
        "capitalize" => capitalize(value),
        "pascalcase" => separated_words(value)
            .into_iter()
            .map(capitalize)
            .collect::<String>(),
        "camelcase" => separated_words(value)
            .into_iter()
            .enumerate()
            .map(|(index, word)| {
                if index == 0 {
                    lower_first(word)
                } else {
                    capitalize(word)
                }
            })
            .collect::<String>(),
        "snakecase" => snake_case(value),
        "kebabcase" => kebab_case(value),
        _ => value.to_string(),
    }
}

fn capitalize(value: &str) -> String {
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return String::new();
    };
    first.to_uppercase().chain(characters).collect()
}

fn lower_first(value: &str) -> String {
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return String::new();
    };
    first.to_lowercase().chain(characters).collect()
}

fn separated_words(value: &str) -> Vec<&str> {
    value
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect()
}

fn snake_case(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut previous_was_lower = false;
    let mut pending_separator = false;
    for character in value.chars() {
        if character.is_whitespace() || character == '-' {
            pending_separator = !output.is_empty();
            previous_was_lower = false;
            continue;
        }
        if (pending_separator || (previous_was_lower && character.is_uppercase()))
            && !output.ends_with('_')
        {
            output.push('_');
        }
        pending_separator = false;
        previous_was_lower = character.is_lowercase();
        output.extend(character.to_lowercase());
    }
    output.trim_matches('_').to_string()
}

fn kebab_case(value: &str) -> String {
    let characters = value.chars().collect::<Vec<_>>();
    let mut output = String::with_capacity(value.len());
    for (index, character) in characters.iter().copied().enumerate() {
        if !character.is_alphanumeric() {
            if !output.is_empty() && !output.ends_with('-') {
                output.push('-');
            }
            continue;
        }
        let previous = index.checked_sub(1).and_then(|index| characters.get(index));
        let next = characters.get(index + 1);
        let boundary = character.is_uppercase()
            && previous.is_some_and(|previous| previous.is_lowercase() || previous.is_numeric())
            || character.is_uppercase()
                && previous.is_some_and(|previous| previous.is_uppercase())
                && next.is_some_and(|next| next.is_lowercase());
        if boundary && !output.is_empty() && !output.ends_with('-') {
            output.push('-');
        }
        output.extend(character.to_lowercase());
    }
    output.trim_matches('-').to_string()
}

fn append_bounded(output: &mut String, value: &str) -> bool {
    if output.len().saturating_add(value.len()) > MAX_TRANSFORM_OUTPUT_BYTES {
        return false;
    }
    output.push_str(value);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_capture_case_conditionals_and_global_replacements() {
        let pascal =
            SnippetTransform::parse("([a-z]+)-([a-z]+)", "${1:/capitalize}${2:/capitalize}", "")
                .unwrap();
        assert_eq!(pascal.apply("hello-world"), "HelloWorld");

        let global = SnippetTransform::parse(".", "=", "g").unwrap();
        assert_eq!(global.apply("banner"), "======");

        let conditional = SnippetTransform::parse("(foo)?", "${1:?yes:no}", "").unwrap();
        assert_eq!(conditional.apply("foo"), "yes");
        assert_eq!(conditional.apply(""), "no");
    }

    #[test]
    fn supports_bounded_lookaround_and_backreferences() {
        let lookbehind = SnippetTransform::parse("(?<=foo)(bar)", "${1:/upcase}", "").unwrap();
        assert_eq!(lookbehind.apply("foobar"), "fooBAR");

        let backreference = SnippetTransform::parse(r"(\w+)\s+\1", "$1", "").unwrap();
        assert_eq!(backreference.apply("echo echo"), "echo");
    }

    #[test]
    fn rejects_unknown_or_duplicated_javascript_flags() {
        assert!(SnippetTransform::parse(".", "x", "y").is_none());
        assert!(SnippetTransform::parse(".", "x", "gg").is_none());
    }
}
