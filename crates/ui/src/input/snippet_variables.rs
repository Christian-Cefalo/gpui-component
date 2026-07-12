use std::{collections::BTreeMap, path::PathBuf};

use chrono::{Datelike as _, Local, Timelike as _};
use ropey::{LineType, Rope};
use uuid::Uuid;

use super::RopeExt as _;

const MAX_VARIABLE_VALUE_BYTES: usize = 4 * 1024 * 1024;

/// Stable document and language metadata used while expanding completion
/// snippets. Volatile values such as selection, cursor, clipboard, and time
/// are captured by the editor when the completion is accepted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SnippetVariableContext {
    document_path: Option<PathBuf>,
    workspace_root: Option<PathBuf>,
    line_comment: Option<String>,
    block_comment: Option<(String, String)>,
}

impl SnippetVariableContext {
    pub fn new(document_path: impl Into<PathBuf>) -> Self {
        Self {
            document_path: Some(document_path.into()),
            ..Self::default()
        }
    }

    pub fn workspace_root(mut self, workspace_root: impl Into<PathBuf>) -> Self {
        self.workspace_root = Some(workspace_root.into());
        self
    }

    pub fn line_comment(mut self, token: impl Into<String>) -> Self {
        self.line_comment = Some(token.into());
        self
    }

    pub fn block_comment(mut self, start: impl Into<String>, end: impl Into<String>) -> Self {
        self.block_comment = Some((start.into(), end.into()));
        self
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct SnippetVariables {
    values: BTreeMap<String, String>,
}

impl SnippetVariables {
    pub(super) fn for_completion(
        context: &SnippetVariableContext,
        text: &Rope,
        cursor: usize,
        selection: &str,
        clipboard: Option<&str>,
    ) -> Self {
        let mut variables = Self::default();
        variables.insert_nonempty("SELECTION", selection);
        variables.insert_nonempty("TM_SELECTED_TEXT", selection);
        variables.insert_nonempty("CLIPBOARD", clipboard.unwrap_or_default());

        let cursor = cursor.min(text.len());
        let line_index = text.byte_to_line_idx(cursor, LineType::LF);
        let current_line = text
            .line(line_index, LineType::LF)
            .to_string()
            .trim_end_matches(['\r', '\n'])
            .to_string();
        variables.insert("TM_CURRENT_LINE", current_line);
        variables.insert_nonempty("TM_CURRENT_WORD", &text.word_at(cursor));
        variables.insert("TM_LINE_INDEX", line_index.to_string());
        variables.insert("TM_LINE_NUMBER", (line_index + 1).to_string());
        variables.insert("CURSOR_INDEX", "0");
        variables.insert("CURSOR_NUMBER", "1");

        variables.insert_path_values(context);
        variables.insert_comment_values(context);
        variables.insert_time_values();
        variables.insert_random_values();
        variables
    }

    pub(super) fn resolve(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    fn insert_path_values(&mut self, context: &SnippetVariableContext) {
        if let Some(path) = context.document_path.as_deref() {
            self.insert("TM_FILEPATH", path.to_string_lossy());
            if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
                self.insert("TM_FILENAME", name);
                let base = name
                    .rfind('.')
                    .filter(|index| *index > 0)
                    .map(|index| &name[..index])
                    .unwrap_or(name);
                self.insert("TM_FILENAME_BASE", base);
            }
            if let Some(directory) = path.parent() {
                self.insert("TM_DIRECTORY", directory.to_string_lossy());
                if let Some(name) = directory.file_name().and_then(|name| name.to_str()) {
                    self.insert("TM_DIRECTORY_BASE", name);
                }
            }
            let relative = context
                .workspace_root
                .as_deref()
                .and_then(|root| path.strip_prefix(root).ok())
                .unwrap_or(path);
            self.insert("RELATIVE_FILEPATH", relative.to_string_lossy());
        }

        if let Some(root) = context.workspace_root.as_deref() {
            self.insert("WORKSPACE_FOLDER", root.to_string_lossy());
            if let Some(name) = root.file_name().and_then(|name| name.to_str()) {
                self.insert("WORKSPACE_NAME", name);
            }
        }
    }

    fn insert_comment_values(&mut self, context: &SnippetVariableContext) {
        if let Some(line_comment) = context.line_comment.as_deref() {
            self.insert("LINE_COMMENT", line_comment);
        }
        if let Some((start, end)) = context.block_comment.as_ref() {
            self.insert("BLOCK_COMMENT_START", start);
            self.insert("BLOCK_COMMENT_END", end);
        }
    }

    fn insert_time_values(&mut self) {
        let now = Local::now();
        self.insert("CURRENT_YEAR", format!("{:04}", now.year()));
        self.insert("CURRENT_YEAR_SHORT", format!("{:02}", now.year() % 100));
        self.insert("CURRENT_MONTH", format!("{:02}", now.month()));
        self.insert("CURRENT_DATE", format!("{:02}", now.day()));
        self.insert("CURRENT_HOUR", format!("{:02}", now.hour()));
        self.insert("CURRENT_MINUTE", format!("{:02}", now.minute()));
        self.insert("CURRENT_SECOND", format!("{:02}", now.second()));
        self.insert(
            "CURRENT_MILLISECOND",
            format!("{:03}", now.timestamp_subsec_millis()),
        );
        self.insert("CURRENT_DAY_NAME", now.format("%A").to_string());
        self.insert("CURRENT_DAY_NAME_SHORT", now.format("%a").to_string());
        self.insert("CURRENT_MONTH_NAME", now.format("%B").to_string());
        self.insert("CURRENT_MONTH_NAME_SHORT", now.format("%b").to_string());
        self.insert("CURRENT_SECONDS_UNIX", now.timestamp().to_string());
        self.insert(
            "CURRENT_MILLISECONDS_UNIX",
            now.timestamp_millis().to_string(),
        );

        let offset_seconds = now.offset().local_minus_utc();
        let sign = if offset_seconds < 0 { '-' } else { '+' };
        let absolute = offset_seconds.unsigned_abs();
        self.insert(
            "CURRENT_TIMEZONE_OFFSET",
            format!("{sign}{:02}:{:02}", absolute / 3600, (absolute % 3600) / 60),
        );
        self.insert("CURRENT_TIMEZONE_NAME", now.format("%Z").to_string());
    }

    fn insert_random_values(&mut self) {
        let uuid = Uuid::new_v4();
        let seed = uuid.as_u128();
        self.insert("RANDOM", format!("{:06}", seed % 1_000_000));
        self.insert("RANDOM_HEX", format!("{:06x}", seed & 0x00ff_ffff));
        self.insert("UUID", uuid.to_string());
    }

    fn insert_nonempty(&mut self, name: &str, value: &str) {
        if !value.is_empty() {
            self.insert(name, value);
        }
    }

    fn insert(&mut self, name: &str, value: impl ToString) {
        self.values
            .insert(name.to_string(), truncate_utf8(value.to_string()));
    }
}

fn truncate_utf8(mut value: String) -> String {
    if value.len() <= MAX_VARIABLE_VALUE_BYTES {
        return value;
    }
    let mut end = MAX_VARIABLE_VALUE_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_variables_cover_document_workspace_selection_and_cursor() {
        let context = SnippetVariableContext::new("/workspace/app/src/main.rs")
            .workspace_root("/workspace/app")
            .line_comment("//")
            .block_comment("/*", "*/");
        let text = Rope::from_str("fn alpha() {\n    value_here\n}\n");
        let cursor = "fn alpha() {\n    value".len();
        let variables =
            SnippetVariables::for_completion(&context, &text, cursor, "selected", Some("copied"));

        assert_eq!(variables.resolve("TM_FILENAME"), Some("main.rs"));
        assert_eq!(variables.resolve("TM_FILENAME_BASE"), Some("main"));
        assert_eq!(variables.resolve("TM_DIRECTORY_BASE"), Some("src"));
        assert_eq!(variables.resolve("RELATIVE_FILEPATH"), Some("src/main.rs"));
        assert_eq!(variables.resolve("WORKSPACE_NAME"), Some("app"));
        assert_eq!(variables.resolve("TM_CURRENT_LINE"), Some("    value_here"));
        assert_eq!(variables.resolve("TM_CURRENT_WORD"), Some("value_here"));
        assert_eq!(variables.resolve("TM_LINE_INDEX"), Some("1"));
        assert_eq!(variables.resolve("TM_LINE_NUMBER"), Some("2"));
        assert_eq!(variables.resolve("TM_SELECTED_TEXT"), Some("selected"));
        assert_eq!(variables.resolve("CLIPBOARD"), Some("copied"));
        assert_eq!(variables.resolve("LINE_COMMENT"), Some("//"));
        assert_eq!(variables.resolve("BLOCK_COMMENT_END"), Some("*/"));
    }

    #[test]
    fn empty_optional_values_remain_unresolved_for_snippet_defaults() {
        let variables = SnippetVariables::for_completion(
            &SnippetVariableContext::default(),
            &Rope::from_str(""),
            0,
            "",
            None,
        );
        assert_eq!(variables.resolve("TM_SELECTED_TEXT"), None);
        assert_eq!(variables.resolve("CLIPBOARD"), None);
        assert_eq!(variables.resolve("TM_CURRENT_WORD"), None);
        assert_eq!(variables.resolve("TM_LINE_NUMBER"), Some("1"));
        assert_eq!(variables.resolve("CURSOR_NUMBER"), Some("1"));
        assert_eq!(variables.resolve("UUID").map(str::len), Some(36));
    }
}
