//! `.env` files: one `KEY=value` entry per line, `#` comments, optional quotes
//! around a value.
//!
//! A nested document is written as flat names. Nested keys join with a double
//! underscore, each key written exactly as it is named:
//! `{"server": {"listen": "0.0.0.0:7835"}}` is `server__listen=0.0.0.0:7835`.
//! An array element is its index: `{"forwards": [{"name": "ssh"}]}` is
//! `forwards__0__name=ssh`. Reading maps those names back unchanged, and
//! completion, hover and key/value spans map a flat name to its path in the
//! nested schema.
//!
//! The schema describes the variables of an example application. Its
//! descriptions are written for this demo, not taken from any standard.

use leptos_config_editor::language::{
    CompletionContext, ConfigLanguage, Fill, Hover, InsertContext, InsertPlan, LanguageConfig,
    LanguageInfo, ParseError, Planned, RequiredChild, SchemaPathSegment, Span, Token, TokenKind,
    Tokens, utf16_to_byte_index,
};
use leptos_config_editor::{FieldPath, Language};
use serde_json::{Map, Number, Value};
use std::collections::HashSet;

/// The `.env` language, for the editor.
pub const ENV: Language = Language::new(&Env);

/// Joins the keys of a nested document in a flat name.
const NESTING: &str = "__";

/// The largest array index read; a larger one is refused rather than padded.
const MAX_INDEX: usize = 10_000;

/// The JSON Schema of the example application's variables.
#[cfg(test)]
pub const ENV_SCHEMA: &str = r#"{
  "type": "object",
  "title": "Application environment",
  "description": "Variables read when the application starts. Each line is KEY=value.",
  "properties": {
    "PORT": {
      "type": "integer",
      "minimum": 1,
      "maximum": 65535,
      "description": "TCP port the application listens on.",
      "default": 8080
    },
    "HOST": {
      "type": "string",
      "description": "Address to bind to. 0.0.0.0 accepts connections from every interface.",
      "default": "127.0.0.1"
    },
    "DATABASE_URL": {
      "type": "string",
      "description": "Connection string of the database, such as postgres://user:password@host/name."
    },
    "LOG_LEVEL": {
      "enum": ["debug", "info", "warn", "error"],
      "x-enum-descriptions": [
        "Everything, including per-request detail.",
        "Startup, configuration and significant events.",
        "Unexpected situations the application recovered from.",
        "Failures that need attention."
      ],
      "description": "Lowest severity written to the log.",
      "default": "info"
    },
    "DEBUG": {
      "type": "boolean",
      "description": "Enables debug endpoints. Do not enable in production.",
      "default": false
    },
    "WORKERS": {
      "type": "integer",
      "minimum": 1,
      "description": "Number of worker threads."
    }
  }
}"#;

/// The `.env` language as the editor's `ConfigLanguage`.
pub struct Env;

/// One variable an accepted suggestion writes.
#[derive(Clone, Debug, PartialEq)]
struct Line {
    /// The flat name, joined with [`NESTING`].
    name: String,
    /// The value as text, empty to type.
    value: String,
    /// The caret goes at the start of the value.
    caret: bool,
}

/// The lines of an insert as text, and the caret's UTF-16 offset in it. The
/// first line replaces the typed word, so it is written relative to it; each
/// later line carries the container's path, which the document already holds
/// before the typed word.
fn render(lines: &[Line], path: &[SchemaPathSegment]) -> (String, usize) {
    let prefix = if path.is_empty() {
        String::new()
    } else {
        format!("{}{NESTING}", flat_name(path))
    };
    let mut text = String::new();
    let mut caret = None;
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            text.push('\n');
            text.push_str(&prefix);
        }
        text.push_str(&line.name);
        text.push('=');
        if line.caret && caret.is_none() {
            caret = Some(text.len());
        }
        text.push_str(&line.value);
    }
    let caret = caret.unwrap_or(text.len());
    let units = text[..caret].encode_utf16().count();
    (text, units)
}

/// One step of a flat name read back: a key, or an index into a list.
#[derive(Clone, Debug, PartialEq)]
enum Step {
    Key(String),
    Index(usize),
}

/// Whether `segment` is all digits, so it names a list item.
fn is_index(segment: &str) -> bool {
    !segment.is_empty() && segment.bytes().all(|byte| byte.is_ascii_digit())
}

/// The schema path of a flat name. A name joined by `__` is nested: its digit
/// segments are items, and its other segments are property names as written.
fn path_of(name: &str, nested: bool) -> Vec<SchemaPathSegment> {
    name.split(NESTING)
        .map(|segment| match segment.parse::<usize>() {
            Ok(index) if nested && is_index(segment) => SchemaPathSegment::ArrayItem(index),
            _ => SchemaPathSegment::Key(segment.to_string()),
        })
        .collect()
}

/// The flat name a schema path is written as: keys as they are named, items by index.
fn flat_name(segments: &[SchemaPathSegment]) -> String {
    segments
        .iter()
        .map(|segment| match segment {
            SchemaPathSegment::Key(key) => key.clone(),
            SchemaPathSegment::ArrayItem(index) => index.to_string(),
        })
        .collect::<Vec<_>>()
        .join(NESTING)
}

/// A flat name as the steps of a document it sets. The first is always a key
/// (the document is an object); a later digit segment is a list index.
fn steps_of(name: &str) -> Result<Vec<Step>, String> {
    let nested = name.contains(NESTING);
    name.split(NESTING)
        .enumerate()
        .map(|(position, segment)| {
            if segment.is_empty() {
                return Err(format!(
                    "a variable name has an empty part between its {NESTING} separators"
                ));
            }
            if nested && position > 0 && is_index(segment) {
                if let Ok(index) = segment.parse::<usize>() {
                    return Ok(Step::Index(index));
                }
            }
            Ok(Step::Key(segment.to_string()))
        })
        .collect()
}

/// An empty container for the step after `next`: a list before an index.
fn empty_for(next: Option<&Step>) -> Value {
    match next {
        Some(Step::Index(_)) => Value::Array(Vec::new()),
        _ => Value::Object(Map::new()),
    }
}

/// Sets `value` at `steps` in `root`, building the objects and lists on the
/// way. A flat name that would overwrite a container, or reach into a scalar,
/// is refused.
fn insert(root: &mut Value, steps: &[Step], value: Value, name: &str) -> Result<(), String> {
    let clash = || format!("a variable name clashes with an earlier one: {name}");
    let mut node = root;
    for (position, step) in steps.iter().enumerate() {
        let next = steps.get(position + 1);
        match step {
            Step::Key(key) => {
                let Value::Object(map) = node else {
                    return Err(clash());
                };
                match next {
                    None => {
                        if matches!(map.get(key), Some(Value::Object(_) | Value::Array(_))) {
                            return Err(clash());
                        }
                        map.insert(key.clone(), value);
                        return Ok(());
                    }
                    Some(_) => node = map.entry(key.clone()).or_insert_with(|| empty_for(next)),
                }
            }
            Step::Index(index) => {
                let Value::Array(items) = node else {
                    return Err(clash());
                };
                if *index >= MAX_INDEX {
                    return Err(format!("a list index is too large: {name}"));
                }
                if items.len() <= *index {
                    items.resize(*index + 1, Value::Null);
                }
                match next {
                    None => {
                        items[*index] = value;
                        return Ok(());
                    }
                    Some(_) => {
                        if items[*index].is_null() {
                            items[*index] = empty_for(next);
                        }
                        node = &mut items[*index];
                    }
                }
            }
        }
    }
    Ok(())
}

/// A JSON Pointer to the value at `parts`, for the messages that name it.
fn pointer(parts: &[String]) -> String {
    parts
        .iter()
        .map(|part| format!("/{}", part.replace('~', "~0").replace('/', "~1")))
        .collect()
}

/// A line the document writes: its flat name, the JSON Pointer of its value
/// and its text.
struct Written {
    name: String,
    pointer: String,
    text: String,
}

/// The entries of `map`, written under `trail` (the keys and indices above it).
fn write_members(
    map: &Map<String, Value>,
    trail: &mut Vec<String>,
    lines: &mut Vec<Written>,
) -> Result<(), String> {
    for (key, child) in map {
        let mut full = trail.clone();
        full.push(key.clone());
        let at = pointer(&full);
        if key.is_empty()
            || key.starts_with('#')
            || key.contains(|character: char| character.is_whitespace() || character == '=')
        {
            return Err(format!(
                "a .env variable name cannot be empty, start with #, or hold a space or '=': {at}"
            ));
        }
        if key.contains(NESTING) {
            return Err(format!(
                "a .env variable name cannot contain {NESTING}, which joins nested keys: {at}"
            ));
        }
        if is_index(key) {
            return Err(format!(
                "a .env key cannot be all digits, since it would read back as a list item: {at}"
            ));
        }
        trail.push(key.clone());
        let result = write_child(child, trail, lines);
        trail.pop();
        result?;
    }
    Ok(())
}

/// The value at `trail`: a scalar as one line, a container as its entries.
fn write_child(
    value: &Value,
    trail: &mut Vec<String>,
    lines: &mut Vec<Written>,
) -> Result<(), String> {
    let at = pointer(trail);
    match value {
        Value::Null => Err(format!("a .env value cannot be null: {at}")),
        Value::Object(map) if map.is_empty() => Err(format!(
            "a .env value cannot be an empty object or list: {at}"
        )),
        Value::Object(map) => write_members(map, trail, lines),
        Value::Array(items) if items.is_empty() => Err(format!(
            "a .env value cannot be an empty object or list: {at}"
        )),
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                trail.push(index.to_string());
                let result = write_child(child, trail, lines);
                trail.pop();
                result?;
            }
            Ok(())
        }
        Value::Bool(flag) => {
            lines.push(written(trail, flag.to_string()));
            Ok(())
        }
        Value::Number(number) => {
            if !number.is_i64() {
                return Err(format!(
                    "a .env number must be a whole number that fits in 64 bits: {at}"
                ));
            }
            lines.push(written(trail, number.to_string()));
            Ok(())
        }
        Value::String(text) => {
            let text = Env::write_string(&at, text)?;
            lines.push(written(trail, text));
            Ok(())
        }
    }
}

/// A scalar's line: its flat name is the trail joined with `__`.
fn written(trail: &[String], text: String) -> Written {
    let segments: Vec<SchemaPathSegment> = trail
        .iter()
        .map(|part| match part.parse::<usize>() {
            Ok(index) if is_index(part) => SchemaPathSegment::ArrayItem(index),
            _ => SchemaPathSegment::Key(part.clone()),
        })
        .collect();
    Written {
        name: flat_name(&segments),
        pointer: pointer(trail),
        text,
    }
}

/// The byte range of the flat name written on `line`, if it writes one. An
/// `export` before it is not part of the name.
fn name_range(line: &str) -> Option<(usize, usize)> {
    let indent = line.len() - line.trim_start().len();
    let code = &line[indent..];
    if code.starts_with('#') {
        return None;
    }
    let equals = code.find('=')?;
    let written = code[..equals].trim_end();
    let name = written.rsplit(char::is_whitespace).next().unwrap_or(written);
    if name.is_empty() {
        return None;
    }
    let start = indent + written.len() - name.len();
    Some((start, start + name.len()))
}

impl Env {
    /// A value as data: booleans and integers when the text is one, else the
    /// text without its surrounding quotes.
    fn value(text: &str) -> Value {
        let unquoted = text
            .strip_prefix('"')
            .and_then(|rest| rest.strip_suffix('"'))
            .or_else(|| {
                text.strip_prefix('\'')
                    .and_then(|rest| rest.strip_suffix('\''))
            });
        if let Some(inner) = unquoted {
            return Value::String(inner.to_string());
        }
        match text {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => text
                .parse::<i64>()
                .map(|number| Value::Number(Number::from(number)))
                .unwrap_or_else(|_| Value::String(text.to_string())),
        }
    }

    /// The byte index of the closing quote of a quoted value that starts at
    /// `text[0]`, or `None` when it is not closed on this line. A double
    /// quote is closed by the first unescaped `"`; a single quote is literal.
    fn closing_quote(text: &str) -> Option<usize> {
        let quote = text.chars().next()?;
        let mut escaped = false;
        for (index, character) in text.char_indices().skip(1) {
            if escaped {
                escaped = false;
            } else if quote == '"' && character == '\\' {
                escaped = true;
            } else if character == quote {
                return Some(index);
            }
        }
        None
    }

    /// The value text without an inline comment: a quoted value ends at its
    /// closing quote, and an unquoted one before a `#` preceded by whitespace.
    fn without_comment(text: &str) -> &str {
        if text.starts_with(['"', '\'']) {
            return match Self::closing_quote(text) {
                Some(close) => &text[..=close],
                None => text,
            };
        }
        let cut = text.char_indices().find_map(|(index, character)| {
            (character == '#' && (index == 0 || text[..index].ends_with(char::is_whitespace)))
                .then_some(index)
        });
        text[..cut.unwrap_or(text.len())].trim_end()
    }

    /// Whether `text` is a number literal such as `8080`, `-1` or `0.5`.
    fn is_number(text: &str) -> bool {
        let digits = text.strip_prefix(['-', '+']).unwrap_or(text);
        digits.chars().any(|c| c.is_ascii_digit())
            && digits.chars().all(|c| c.is_ascii_digit() || c == '.')
            && digits.matches('.').count() <= 1
    }

    /// Tokens for the value text `value` that starts at byte `base` of the
    /// line: a quoted string (its quotes included), then an inline comment
    /// that follows it, or an unquoted scalar up to a `#` preceded by
    /// whitespace. Shapes that `.env` readers agree on: docker compose
    /// keeps a `#` written after a non-space character, and takes a comment
    /// after a quoted value only past its closing quote.
    fn highlight_value(value: &str, base: usize, tokens: &mut Tokens) {
        if value.is_empty() {
            return;
        }
        if value.starts_with(['"', '\'']) {
            let Some(close) = Self::closing_quote(value) else {
                tokens.push(base, base + value.len(), TokenKind::String);
                return;
            };
            let end = close + 1;
            tokens.push(base, base + end, TokenKind::String);
            let after = &value[end..];
            if let Some(hash) = after.find('#') {
                if after[..hash].trim().is_empty() {
                    tokens.push(base + end + hash, base + value.len(), TokenKind::Comment);
                }
            }
            return;
        }
        let comment = value.char_indices().find_map(|(index, character)| {
            let starts_comment =
                character == '#' && (index == 0 || value[..index].ends_with(char::is_whitespace));
            starts_comment.then_some(index)
        });
        let scalar_end = comment.unwrap_or(value.len());
        let scalar = value[..scalar_end].trim_end();
        let kind = match scalar {
            "true" | "false" => Some(TokenKind::Literal),
            number if Self::is_number(number) => Some(TokenKind::Literal),
            "" => None,
            _ => Some(TokenKind::Value),
        };
        if let Some(kind) = kind {
            tokens.push(base, base + scalar.len(), kind);
        }
        if let Some(hash) = comment {
            tokens.push(base + hash, base + value.len(), TokenKind::Comment);
        }
    }

    /// One value as `.env` text: bare when it reads back as itself, else in
    /// quotes. A string that would read back as a number or a boolean is
    /// quoted too, so it stays a string. `at` is the JSON Pointer of the
    /// value, named in the error.
    fn write_string(at: &str, text: &str) -> Result<String, String> {
        if text.contains(['\n', '\r']) {
            return Err(format!("a .env value cannot hold a line break: {at}"));
        }
        let looks_typed = matches!(text, "true" | "false") || Self::is_number(text);
        let needs_quotes = looks_typed
            || text.is_empty()
            || text.contains(['#', ' ', '\t'])
            || text.starts_with(['"', '\''])
            || text.trim() != text;
        if !needs_quotes {
            return Ok(text.to_string());
        }
        if !text.contains('"') {
            Ok(format!("\"{text}\""))
        } else if !text.contains('\'') {
            Ok(format!("'{text}'"))
        } else {
            Err(format!(
                "a .env value cannot hold both quote characters: {at}"
            ))
        }
    }

    /// The span of the flat name `path` writes, matched case-insensitively:
    /// its key, or its value (`value`). The last line writing it wins.
    fn flat_span(&self, source: &str, path: &FieldPath, value: bool) -> Option<Span> {
        let segments = path.segments();
        if segments.is_empty() {
            return None;
        }
        let target = flat_name(segments);
        let found = source
            .split('\n')
            .enumerate()
            .filter_map(|(index, line)| {
                let (start, end) = name_range(line)?;
                (&line[start..end] == target).then_some((index, line, start, end))
            })
            .last();
        let (index, line, start, end) = found?;
        if value {
            let name = &line[start..end];
            let span = self
                .config()
                .flat_value_span(line, &FieldPath::from(format!("/{name}")))?;
            return Some(Span {
                line: index + 1,
                ..span
            });
        }
        let column = |byte: usize| line[..byte].chars().count() + 1;
        Some(Span {
            line: index + 1,
            start_column: column(start),
            end_column: column(end),
        })
    }
}

impl Env {
    /// A scalar as the document writes it after `=`, quoted where it must be.
    fn scalar_text(&self, value: &Value) -> Option<String> {
        // An empty text is left empty (`KEY=`), not quoted.
        if value.as_str() == Some("") {
            return Some(String::new());
        }
        let entry = Value::Object(Map::from_iter([("x".to_string(), value.clone())]));
        let text = self.serialize(&entry, None).ok()?;
        let line = text.strip_prefix("x=")?.strip_suffix('\n')?;
        (!line.contains('\n')).then(|| line.to_string())
    }

    /// The lines writing `value` under `name`: a scalar as one line, an object
    /// or list as one line per scalar inside it, named by its keys and indexes.
    fn value_lines(&self, name: &str, value: &Value, out: &mut Vec<Line>) -> Option<()> {
        match value {
            Value::Object(map) if !map.is_empty() => {
                for (key, item) in map {
                    self.value_lines(&format!("{name}{NESTING}{key}"), item, out)?;
                }
            }
            Value::Array(items) if !items.is_empty() => {
                for (index, item) in items.iter().enumerate() {
                    self.value_lines(&format!("{name}{NESTING}{index}"), item, out)?;
                }
            }
            _ => out.push(Line {
                name: name.to_string(),
                value: self.scalar_text(value)?,
                caret: false,
            }),
        }
        Some(())
    }

    /// The lines of the required `children` of the object at `base`, each as
    /// its fill writes it: a value written, the caret where the fill leaves a
    /// value to type.
    fn child_lines(
        &self,
        base: &str,
        children: &[RequiredChild<'_>],
        out: &mut Vec<Line>,
    ) -> Option<()> {
        for child in children {
            let name = format!("{base}{NESTING}{}", child.key);
            match &child.fill {
                Fill::Children(nested) => self.child_lines(&name, nested, out)?,
                Fill::Caret { .. } => out.push(Line {
                    name,
                    value: String::new(),
                    caret: true,
                }),
                Fill::Empty(None) => {}
                fill => {
                    let value = fill.value()?;
                    let first = out.len();
                    self.value_lines(&name, &value, out)?;
                    let caret = matches!(fill, Fill::CaretAt(_) | Fill::List { caret: true, .. });
                    if let Some(line) = out.get_mut(first).filter(|_| caret) {
                        line.caret = true;
                    }
                }
            }
        }
        Some(())
    }

    /// The lines an accepted `key` writes for `plan`: its schema default (the
    /// caret on its first value), or its required children (the caret on the
    /// first value left to type). `None` leaves the key to the legacy forms.
    fn plan_lines(&self, key: &str, plan: &InsertPlan<'_>, out: &mut Vec<Line>) -> Option<()> {
        match &plan.value {
            Planned::Value(value) => {
                self.value_lines(key, value, out)?;
                out.first_mut()?.caret = true;
            }
            Planned::List { .. } if plan.list_of_objects && !plan.children.is_empty() => {
                self.child_lines(&format!("{key}{NESTING}0"), &plan.children, out)?;
            }
            Planned::Object { .. } if !plan.children.is_empty() => {
                self.child_lines(key, &plan.children, out)?;
            }
            _ => return None,
        }
        (!out.is_empty()).then_some(())
    }
}

impl ConfigLanguage for Env {
    fn info(&self) -> LanguageInfo {
        LanguageInfo {
            label: "Env",
            icon: "E",
            extensions: &["env"],
        }
    }

    fn config(&self) -> LanguageConfig {
        LanguageConfig {
            line_comments: &["#"],
            separators: &["="],
            written_separator: Some("="),
            quotes: &["\"", "'"],
            ..LanguageConfig::default()
        }
    }

    fn parse(&self, source: &str) -> Result<Value, ParseError> {
        let mut root = Value::Object(Map::new());
        for (index, line) in source.lines().enumerate() {
            let line_number = index + 1;
            let fail = |message: String| ParseError {
                message,
                line: Some(line_number),
                column: Some(1),
                quotes: None,
            };
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let Some((key, value)) = trimmed.split_once('=') else {
                return Err(fail("expected KEY=value".to_string()));
            };
            let key = key.trim();
            if key.is_empty() || key.contains(char::is_whitespace) {
                return Err(fail("a variable name has no spaces".to_string()));
            }
            let steps = steps_of(key).map_err(|message| fail(message))?;
            let value = Self::value(Self::without_comment(value.trim()));
            insert(&mut root, &steps, value, key).map_err(|message| fail(message))?;
        }
        Ok(root)
    }

    fn serialize(&self, value: &Value, previous: Option<&str>) -> Result<String, String> {
        let _ = previous;
        let Value::Object(map) = value else {
            return Err(
                "a .env file is a list of KEY=value lines, so its top level must be an object"
                    .to_string(),
            );
        };
        let mut lines = Vec::new();
        write_members(map, &mut Vec::new(), &mut lines)?;
        let mut seen: HashSet<String> = HashSet::new();
        let mut text = String::new();
        for line in lines {
            if !seen.insert(line.name.clone()) {
                return Err(format!(
                    "two keys write the same variable name {}: {}",
                    line.name, line.pointer
                ));
            }
            text.push_str(&format!("{}={}\n", line.name, line.text));
        }
        Ok(text)
    }

    fn highlight(&self, line: &str) -> Vec<Token> {
        let mut tokens = Tokens::new();
        let indent = line.len() - line.trim_start().len();
        let body = &line[indent..];
        if body.starts_with('#') {
            tokens.push(indent, line.len(), TokenKind::Comment);
            return tokens.finish();
        }
        let Some(equals) = body.find('=') else {
            return tokens.finish();
        };
        // `export KEY=value` is accepted by the dotenv readers; `export` itself stays plain.
        let before = body[..equals].trim_end();
        let name = before.trim_start();
        let name = name
            .strip_prefix("export")
            .filter(|rest| rest.starts_with(char::is_whitespace))
            .map_or(name, str::trim_start);
        let name_start = indent + before.len() - name.len();
        tokens.push(name_start, name_start + name.len(), TokenKind::Key);
        tokens.push(indent + equals, indent + equals + 1, TokenKind::Punctuation);
        let rest = &body[equals + 1..];
        let value = rest.trim_start();
        let base = indent + equals + 1 + (rest.len() - value.len());
        Self::highlight_value(value, base, &mut tokens);
        tokens.finish()
    }

    fn completion_context(&self, source: &str, cursor: usize) -> CompletionContext {
        let nothing = CompletionContext {
            no_properties: true,
            replace_start: cursor,
            replace_end: cursor,
            ..CompletionContext::default()
        };
        let cursor_byte = utf16_to_byte_index(source, cursor.min(source.encode_utf16().count()));
        let at = source[..cursor_byte].encode_utf16().count();
        let units = |text: &str| text.encode_utf16().count();
        let line_start = source[..cursor_byte].rfind('\n').map_or(0, |index| index + 1);
        let line_end = source[cursor_byte..]
            .find('\n')
            .map_or(source.len(), |index| cursor_byte + index);
        let before = &source[line_start..cursor_byte];
        let rest = &source[cursor_byte..line_end];
        let code = before.trim_start();
        let base_indent = before[..before.len() - code.len()].to_string();
        if code.starts_with('#') {
            return nothing;
        }
        let word = code
            .strip_prefix("export")
            .filter(|tail| tail.starts_with(char::is_whitespace))
            .map_or(code, str::trim_start);

        // After `=`: the value of the flat name before it.
        if let Some(equals) = word.find('=') {
            let name = word[..equals].trim_end();
            let typed = word[equals + 1..].trim_start();
            if name.is_empty() || name.contains(char::is_whitespace) {
                return nothing;
            }
            let steps = path_of(name, name.contains(NESTING));
            if steps
                .iter()
                .any(|segment| matches!(segment, SchemaPathSegment::Key(key) if key.is_empty()))
            {
                return nothing;
            }
            let Some((SchemaPathSegment::Key(key), prefix)) = steps.split_last() else {
                return nothing;
            };
            return CompletionContext {
                path: prefix.to_vec(),
                typed: typed.to_string(),
                replace_start: at - units(typed),
                replace_end: at,
                value_key: Some(key.clone()),
                base_indent,
                ..CompletionContext::default()
            };
        }

        // Before `=`: a flat name. Its text after the last `__` is the word
        // typed; the names before it are the path into the nested schema.
        let written = word.rsplit(char::is_whitespace).next().unwrap_or(word);
        let (prefix, typed) = match written.rfind(NESTING) {
            Some(index) => (&written[..index], &written[index + NESTING.len()..]),
            None => ("", written),
        };
        let nested = written.contains(NESTING);
        let path = if nested {
            let segments = path_of(prefix, true);
            if segments
                .iter()
                .any(|segment| matches!(segment, SchemaPathSegment::Key(key) if key.is_empty()))
            {
                return nothing;
            }
            segments
        } else {
            Vec::new()
        };
        // The rest of the word after the caret is replaced too.
        let rest_word: String = rest
            .chars()
            .take_while(|character| character.is_alphanumeric() || *character == '-')
            .collect();
        CompletionContext {
            path,
            typed: typed.to_string(),
            replace_start: at - units(typed),
            replace_end: at + units(&rest_word),
            // Typing `__` after a nested path opens its children without a
            // typed word; the crate hides an empty query unless marked explicit.
            explicit_entry_start: nested && typed.is_empty(),
            base_indent,
            ..CompletionContext::default()
        }
    }

    fn write_insert(
        &self,
        key: &str,
        plan: &InsertPlan<'_>,
        context: &InsertContext<'_>,
    ) -> Option<(String, usize)> {
        let mut lines = Vec::new();
        if self.plan_lines(key, plan, &mut lines).is_some() {
            return Some(render(&lines, context.path));
        }
        let name = key;
        let text = if plan.list_of_objects {
            format!("{name}__0__")
        } else if plan.list {
            format!("{name}__0=")
        } else if plan.object {
            format!("{name}__")
        } else {
            format!("{name}=")
        };
        let caret = text.encode_utf16().count();
        Some((text, caret))
    }

    fn format_value(&self, value: &Value) -> String {
        self.config().flat_format_value(value)
    }

    fn hover(&self) -> &dyn Hover {
        self
    }

    fn hover_key(&self, line: &str, column: usize) -> Option<(String, usize)> {
        self.config().flat_hover_key(line, column)
    }

    fn key_span(&self, source: &str, path: &FieldPath) -> Option<Span> {
        self.flat_span(source, path, false)
    }

    fn value_span(&self, source: &str, path: &FieldPath) -> Option<Span> {
        self.flat_span(source, path, true)
    }
}

impl Hover for Env {
    fn hover_path(
        &self,
        source: &str,
        line_index: usize,
        column: usize,
    ) -> Option<(Vec<SchemaPathSegment>, String, usize)> {
        let line = source.split('\n').nth(line_index)?;
        let (name, start) = self.config().flat_hover_key(line, column)?;
        Some((path_of(&name, name.contains(NESTING)), name, start))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leptos_config_editor::{Schema, TextPosition};
    use serde_json::json;

    /// A small nested schema: a section with a scalar and an object, and a
    /// list of objects.
    const NESTED_SCHEMA: &str = r#"{
      "type": "object",
      "properties": {
        "server": {
          "type": "object",
          "description": "The HTTP server.",
          "properties": {
            "listen": {"type": "string", "description": "Address the server listens on."},
            "tls": {
              "type": "object",
              "properties": {
                "cert": {"type": "string", "description": "Certificate file."}
              }
            }
          }
        },
        "forwards": {
          "type": "array",
          "description": "Ports forwarded to the clients.",
          "items": {
            "type": "object",
            "properties": {
              "name": {"type": "string", "description": "Name of the forward."},
              "port": {"type": "integer", "description": "Port on the client."}
            }
          }
        }
      }
    }"#;

    fn offered(schema: &str, text: &str) -> Vec<String> {
        let mut labels: Vec<String> = Schema::compile(schema)
            .expect("schema")
            .suggestions(ENV, text, text.encode_utf16().count())
            .iter()
            .map(|suggestion| suggestion.label().to_string())
            .collect();
        labels.sort();
        labels
    }

    fn offered_flat(text: &str) -> Vec<String> {
        offered(ENV_SCHEMA, text)
    }

    fn nested_document() -> Value {
        json!({
            "server": {"listen": "0.0.0.0:7835", "tls": {"cert": "/etc/cert.pem"}},
            "forwards": [
                {"name": "ssh", "port": 22},
                {"name": "web", "port": 443}
            ]
        })
    }

    #[test]
    fn parses_keys_values_quotes_and_typed_values() {
        let value = Env
            .parse("PORT=8080\nDEBUG=true\nHOST=\"0.0.0.0\"\n# note\n")
            .expect("parses");
        assert_eq!(value["PORT"], Value::from(8080));
        assert_eq!(value["DEBUG"], Value::Bool(true));
        assert_eq!(value["HOST"], Value::String("0.0.0.0".into()));
    }

    #[test]
    fn an_inline_comment_after_a_value_is_not_part_of_it() {
        let value = Env
            .parse("NAME='a b' # trailing\nPORT=8080 # port\nA=b#c\n")
            .expect("parses");
        assert_eq!(value["NAME"], Value::String("a b".into()));
        assert_eq!(value["PORT"], Value::from(8080));
        assert_eq!(value["A"], Value::String("b#c".into()));
    }

    #[test]
    fn a_line_without_equals_is_a_parse_error_on_its_line() {
        let error = Env.parse("PORT=1\nNOPE\n").unwrap_err();
        assert_eq!(error.line, Some(2));
    }

    #[test]
    fn valid_document_validates_against_the_schema() {
        let schema = Schema::compile(ENV_SCHEMA).expect("schema");
        let text = "PORT=8080\nDEBUG=false\nLOG_LEVEL=info\n";
        assert!(schema.validate(ENV, text).is_empty());
    }

    #[test]
    fn completion_at_a_new_line_offers_the_unwritten_variables() {
        let names = offered_flat("PORT=8080\n");
        assert!(names.contains(&"LOG_LEVEL".to_string()), "{names:?}");
        assert!(!names.contains(&"PORT".to_string()), "{names:?}");
    }

    #[test]
    fn hover_on_an_enum_value_shows_the_entry_description() {
        let schema = Schema::compile(ENV_SCHEMA).expect("schema");
        let text = "LOG_LEVEL=info\n";
        let docs = schema
            .hover(ENV, text, TextPosition::new(1, 12))
            .expect("docs over the value");
        assert_eq!(docs.key, "LOG_LEVEL");
        let value = docs.value.expect("the value's docs");
        assert_eq!(value.value, "info");
        assert_eq!(
            value.description.as_deref(),
            Some("Startup, configuration and significant events.")
        );
    }

    /// Each token's text and kind, in order, for one line.
    fn colored(line: &str) -> Vec<(&str, TokenKind)> {
        Env.highlight(line)
            .iter()
            .map(|token| (&line[token.start..token.end], token.kind))
            .collect()
    }

    #[test]
    fn highlight_colors_key_punctuation_and_literals() {
        assert_eq!(
            colored("DEBUG=true"),
            [
                ("DEBUG", TokenKind::Key),
                ("=", TokenKind::Punctuation),
                ("true", TokenKind::Literal)
            ]
        );
        assert_eq!(colored("PORT=8080")[2], ("8080", TokenKind::Literal));
        assert_eq!(
            colored("DATABASE_URL=postgres://app@db/app")[2],
            ("postgres://app@db/app", TokenKind::Value)
        );
    }

    #[test]
    fn highlight_colors_quoted_values_as_strings() {
        assert_eq!(
            colored("HOST=\"0.0.0.0\"")[2],
            ("\"0.0.0.0\"", TokenKind::String)
        );
        assert_eq!(colored("NAME='a b'")[2], ("'a b'", TokenKind::String));
    }

    #[test]
    fn highlight_colors_comments_on_a_line_and_after_a_value() {
        assert_eq!(colored("# note"), [("# note", TokenKind::Comment)]);
        assert_eq!(
            colored("PORT=8080 # port"),
            [
                ("PORT", TokenKind::Key),
                ("=", TokenKind::Punctuation),
                ("8080", TokenKind::Literal),
                ("# port", TokenKind::Comment),
            ]
        );
        // A `#` written inside an unquoted value is part of it, as compose reads it.
        assert_eq!(colored("A=b#c")[2], ("b#c", TokenKind::Value));
        assert_eq!(
            colored("HOST=\"x\" # note")[3],
            ("# note", TokenKind::Comment)
        );
    }

    #[test]
    fn highlight_reads_the_export_prefix_as_part_of_the_line_not_the_key() {
        assert_eq!(colored("export HOST=x")[0], ("HOST", TokenKind::Key));
    }

    #[test]
    fn serialize_writes_flat_values_and_quotes_only_what_needs_it() {
        let value = json!({
            "PORT": 8080,
            "DEBUG": false,
            "NAME": "a b",
            "TAG": "8080",
            "URL": "postgres://app:secret@db/app",
        });
        assert_eq!(
            Env.serialize(&value, None).expect("flat"),
            "PORT=8080\nDEBUG=false\nNAME=\"a b\"\nTAG=\"8080\"\nURL=postgres://app:secret@db/app\n"
        );
    }

    #[test]
    fn a_flat_document_reads_and_writes_unchanged() {
        let text = "PORT=8080\nDEBUG=false\nNAME=\"a b\"\nURL=postgres://app:secret@db/app\n";
        let value = Env.parse(text).expect("parses");
        assert_eq!(value["PORT"], Value::from(8080));
        assert_eq!(Env.serialize(&value, None).expect("writes"), text);
    }

    #[test]
    fn env_converts_to_yaml_and_back() {
        let env = "PORT=8080\nHOST=\"0.0.0.0\"\nLOG_LEVEL=info\nDEBUG=false\nDATABASE_URL=postgres://app:secret@db/app\n";
        let yaml = ENV.convert(env, Language::YAML).expect("to yaml");
        assert!(yaml.losses.is_empty(), "{:?}", yaml.losses);
        let back = Language::YAML
            .convert(yaml.text.as_str(), ENV)
            .expect("back to env");
        assert_eq!(
            back.text,
            "PORT=8080\nHOST=0.0.0.0\nLOG_LEVEL=info\nDEBUG=false\nDATABASE_URL=postgres://app:secret@db/app\n"
        );
        assert_eq!(ENV.parse(&back.text).unwrap(), ENV.parse(env).unwrap());
    }

    #[test]
    fn hover_on_a_variable_name_shows_its_description() {
        let schema = Schema::compile(ENV_SCHEMA).expect("schema");
        let docs = schema
            .hover(ENV, "DEBUG=true\n", TextPosition::new(1, 2))
            .expect("docs");
        assert_eq!(
            docs.description.as_deref(),
            Some("Enables debug endpoints. Do not enable in production.")
        );
    }

    #[test]
    fn a_nested_document_round_trips_through_flat_lines() {
        let document = nested_document();
        let text = Env.serialize(&document, None).expect("writes");
        for line in [
            "server__listen=0.0.0.0:7835",
            "server__tls__cert=/etc/cert.pem",
            "forwards__0__name=ssh",
            "forwards__0__port=22",
            "forwards__1__name=web",
            "forwards__1__port=443",
        ] {
            assert!(text.lines().any(|written| written == line), "{text}");
        }
        assert_eq!(Env.parse(&text).expect("reads"), document);
    }

    #[test]
    fn a_nested_key_is_nested_under_its_parent_when_read() {
        let value = Env.parse("server__listen=0.0.0.0:7835\n").expect("reads");
        assert_eq!(value, json!({"server": {"listen": "0.0.0.0:7835"}}));
    }

    #[test]
    fn an_array_index_maps_to_an_item_in_the_path() {
        let context = Env.completion_context("forwards__0__", 13);
        assert_eq!(
            context.path,
            [
                SchemaPathSegment::Key("forwards".into()),
                SchemaPathSegment::ArrayItem(0),
            ]
        );
        assert_eq!(context.typed, "");
        assert_eq!(context.replace_start, 13);
    }

    #[test]
    fn the_schema_examples_round_trip_exactly_through_the_flat_form() {
        for domain in &crate::schemas::DOMAINS {
            let env = Language::JSON
                .convert(domain.example, ENV)
                .unwrap_or_else(|error| panic!("{} to env: {error:?}", domain.label));
            assert!(env.losses.is_empty(), "{}: {:?}", domain.label, env.losses);
            let back = ENV
                .convert(&env.text, Language::JSON)
                .unwrap_or_else(|error| panic!("{} back: {error:?}", domain.label));
            assert!(back.losses.is_empty(), "{}: {:?}", domain.label, back.losses);
            let original: Value = serde_json::from_str(domain.example).expect("example json");
            let read: Value = serde_json::from_str(&back.text).expect("json");
            assert_eq!(read, original, "{}", domain.label);

            let schema = Schema::compile(domain.schema).expect("schema");
            let problems = schema.validate(Language::JSON, &back.text);
            assert!(problems.is_empty(), "{}: {problems:?}", domain.label);
        }
    }

    #[test]
    fn every_serialize_error_is_a_sentence_naming_its_key_path() {
        let cases = [
            (json!({"server": {"tls": null}}), "/server/tls"),
            (json!({"server": {}}), "/server"),
            (json!({"forwards": []}), "/forwards"),
            (json!({"forwards": [{}]}), "/forwards/0"),
            (json!({"forwards": ["a", null]}), "/forwards/1"),
            (json!({"server": {"1": "x"}}), "/server/1"),
            (json!({"a__b": 1}), "/a__b"),
            (json!({"server": {"listen": [1, {}]}}), "/server/listen/1"),
            (json!({"server": {"a__b": 1}}), "/server/a__b"),
            (json!({"note": "two\nlines"}), "/note"),
            (json!({"ratio": 0.5}), "/ratio"),
            (json!({"name": "it's \"x\""}), "/name"),
        ];
        for (value, path) in cases {
            let error = Env
                .serialize(&value, None)
                .expect_err(&format!("{value} should be refused"));
            assert!(
                error.ends_with(&format!(": {path}")),
                "{value}: {error:?} should name {path}"
            );
        }
    }

    #[test]
    fn partial_input_never_panics_while_typing() {
        let samples = [
            "",
            "SERVER__",
            "SERVER__LI",
            "SERVER__LISTEN",
            "FORWARDS__0__",
            "FORWARDS__0__NAME=ssh\nFORWARDS__",
            "SERVER__LISTEN=0.0.0.0:7835\nSERVER__TLS__",
            "export SER",
            "# note\nSER",
            "PORT=8080\nNOPE\n",
            "A__=1\n__B=2\n",
            "é__ü=1\nSERVER__LISTEN=\"unfinished\n",
            "FORWARDS__99999999999999__NAME=x\n",
        ];
        let schema = Schema::compile(NESTED_SCHEMA).expect("schema");
        for sample in samples {
            let units: Vec<usize> = sample
                .char_indices()
                .map(|(index, _)| sample[..index].encode_utf16().count())
                .chain(std::iter::once(sample.encode_utf16().count()))
                .collect();
            for prefix_end in sample.char_indices().map(|(index, _)| index).chain([sample.len()]) {
                let prefix = &sample[..prefix_end];
                let _ = Env.parse(prefix);
                for &cursor in &units {
                    let _ = Env.completion_context(prefix, cursor.min(prefix.encode_utf16().count()));
                    let _ = schema.suggestions(ENV, prefix, cursor);
                }
                let lines = prefix.split('\n').count();
                for line in 1..=lines + 1 {
                    for column in 1..=prefix.len() + 2 {
                        let _ = schema.hover(ENV, prefix, TextPosition::new(line, column));
                        let _ = Env.hover_path(prefix, line - 1, column - 1);
                    }
                }
            }
        }
    }

    #[test]
    fn typing_a_separator_after_a_nested_path_opens_its_children() {
        let schema = r#"{
          "type": "object",
          "properties": {
            "websocket": {
              "type": "object",
              "properties": {
                "listen": {"type": "string"},
                "enabled": {"type": "boolean"}
              }
            }
          }
        }"#;
        // The empty word after `__` sets the explicit entry start, so the
        // children are offered; a typed word after it keeps them open.
        let context = Env.completion_context("websocket__", 11);
        assert!(context.explicit_entry_start, "{context:?}");
        assert!(context.typed.is_empty(), "{context:?}");
        let names = offered(schema, "websocket__");
        assert!(names.contains(&"listen".to_string()), "{names:?}");
        assert!(names.contains(&"enabled".to_string()), "{names:?}");

        let context = Env.completion_context("websocket__li", 13);
        assert!(!context.explicit_entry_start, "{context:?}");
        assert_eq!(context.typed, "li");
        let names = offered(schema, "websocket__li");
        assert!(names.contains(&"listen".to_string()), "{names:?}");
        assert!(!names.contains(&"enabled".to_string()), "{names:?}");
    }

    #[test]
    fn completion_under_a_section_offers_its_properties() {
        let names = offered(NESTED_SCHEMA, "server__");
        assert!(names.contains(&"listen".to_string()), "{names:?}");
        assert!(names.contains(&"tls".to_string()), "{names:?}");
        let context = Env.completion_context("server__li", 10);
        assert_eq!(context.path, [SchemaPathSegment::Key("server".into())]);
        assert_eq!(context.typed, "li");
        assert_eq!(context.replace_start, 8);
        assert_eq!(context.replace_end, 10);
    }

    #[test]
    fn accepting_a_nested_suggestion_writes_the_full_flat_name() {
        let schema = Schema::compile(NESTED_SCHEMA).expect("schema");
        let text = "server__li";
        let listen = schema
            .suggestions(ENV, text, text.encode_utf16().count())
            .into_iter()
            .find(|suggestion| suggestion.label() == "listen")
            .expect("listen is offered");
        assert_eq!(listen.accept(text).0, "server__listen=");
    }

    #[test]
    fn completion_in_an_array_item_offers_its_properties() {
        let names = offered(NESTED_SCHEMA, "forwards__0__");
        assert!(names.contains(&"name".to_string()), "{names:?}");
        assert!(names.contains(&"port".to_string()), "{names:?}");
    }

    #[test]
    fn hover_on_a_nested_name_shows_the_nested_description() {
        let schema = Schema::compile(NESTED_SCHEMA).expect("schema");
        let docs = schema
            .hover(ENV, "server__listen=0.0.0.0:7835\n", TextPosition::new(1, 5))
            .expect("docs over the nested key");
        assert_eq!(
            docs.description.as_deref(),
            Some("Address the server listens on.")
        );
        let item = schema
            .hover(ENV, "forwards__0__name=ssh\n", TextPosition::new(1, 3))
            .expect("docs over an item's key");
        assert_eq!(item.description.as_deref(), Some("Name of the forward."));
    }

    #[test]
    fn key_and_value_spans_follow_the_nested_pointer() {
        let source = "server__listen=0.0.0.0:7835\nforwards__0__name=ssh\n";
        let key = Env
            .key_span(source, &FieldPath::from("/server/listen"))
            .expect("key span");
        assert_eq!(
            (key.line, key.start_column, key.end_column),
            (1, 1, 15)
        );
        let value = Env
            .value_span(source, &FieldPath::from("/server/listen"))
            .expect("value span");
        assert_eq!(
            (value.line, value.start_column, value.end_column),
            (1, 16, 28)
        );
        let item = Env
            .value_span(source, &FieldPath::from("/forwards/0/name"))
            .expect("item value span");
        assert_eq!((item.line, item.start_column, item.end_column), (2, 19, 22));
    }

    #[test]
    fn serialize_refuses_a_name_that_would_read_back_as_a_list() {
        let error = Env.serialize(&json!({"server": {"0": 1}}), None).unwrap_err();
        assert!(error.contains("list item"), "{error}");
    }

    #[test]
    fn a_nested_name_cannot_clash_with_a_flat_one() {
        let error = Env.parse("server=1\nserver__listen=2\n").unwrap_err();
        assert_eq!(error.line, Some(2), "{error:?}");
        assert!(error.message.contains("clashes"), "{error:?}");
    }

    /// Objects whose children are required, so an insert writes them.
    const REQUIRED_SCHEMA: &str = r#"{
      "type": "object",
      "properties": {
        "server": {
          "type": "object",
          "required": ["listen", "port"],
          "properties": {
            "listen": {"type": "string"},
            "port": {"type": "integer"}
          }
        }
      }
    }"#;

    /// Accepts the suggestion labelled `label` for `text` (typed up to the
    /// caret), giving the text and the caret's UTF-16 offset.
    fn accepted(schema: &str, text: &str, label: &str) -> (String, usize) {
        let suggestions = Schema::compile(schema)
            .expect("schema")
            .suggestions(ENV, text, text.encode_utf16().count());
        let labels: Vec<String> = suggestions
            .iter()
            .map(|suggestion| suggestion.label().to_string())
            .collect();
        let suggestion = suggestions
            .iter()
            .find(|suggestion| suggestion.label() == label)
            .unwrap_or_else(|| panic!("no {label} in {labels:?}"));
        let (inserted, range) = suggestion.accept(text);
        (inserted, range.end)
    }

    /// The caret's UTF-16 offset at the first `=` followed by its value text.
    fn caret_after(text: &str, marker: &str) -> usize {
        let at = text.find(marker).expect("marker") + marker.len();
        text[..at].encode_utf16().count()
    }

    #[test]
    fn an_accepted_integer_writes_its_default_with_the_caret_on_it() {
        // PORT has the default 8080: the caret goes before its first digit.
        let (text, caret) = accepted(ENV_SCHEMA, "PO", "PORT");
        assert_eq!(text, "PORT=8080");
        assert_eq!(caret, caret_after(&text, "PORT="));
    }

    #[test]
    fn an_accepted_boolean_writes_its_default_with_the_caret_on_it() {
        let (text, caret) = accepted(ENV_SCHEMA, "DEB", "DEBUG");
        assert_eq!(text, "DEBUG=false");
        assert_eq!(caret, caret_after(&text, "DEBUG="));
    }

    #[test]
    fn an_accepted_enum_writes_its_default_with_the_caret_on_it() {
        let (text, caret) = accepted(ENV_SCHEMA, "LOG", "LOG_LEVEL");
        assert_eq!(text, "LOG_LEVEL=info");
        assert_eq!(caret, caret_after(&text, "LOG_LEVEL="));
    }

    #[test]
    fn an_accepted_string_default_with_a_space_is_double_quoted_with_the_caret_before_it() {
        let schema = r#"{
          "type": "object",
          "properties": {
            "GREETING": {"type": "string", "default": "hello world"}
          }
        }"#;
        let (text, caret) = accepted(schema, "GRE", "GREETING");
        assert_eq!(text, "GREETING=\"hello world\"");
        assert_eq!(caret, caret_after(&text, "GREETING="));
    }

    #[test]
    fn an_accepted_string_writes_empty_with_the_caret_after_the_equals() {
        let (text, caret) = accepted(ENV_SCHEMA, "DATA", "DATABASE_URL");
        assert_eq!(text, "DATABASE_URL=");
        assert_eq!(caret, text.encode_utf16().count());
    }

    #[test]
    fn an_accepted_object_writes_one_line_per_required_child_with_the_caret_on_the_first() {
        // The crate fills an integer child with its placeholder, 0.
        let (text, caret) = accepted(REQUIRED_SCHEMA, "ser", "server");
        assert_eq!(text, "server__listen=\nserver__port=0");
        assert_eq!(caret, caret_after(&text, "server__listen="));
    }

    #[test]
    fn an_accepted_nested_child_keeps_the_typed_path_on_the_later_lines() {
        // The typed `server__` is already in the document; the first line
        // replaces only the typed word, and later lines repeat the path.
        let schema = r#"{
          "type": "object",
          "properties": {
            "server": {
              "type": "object",
              "properties": {
                "tls": {
                  "type": "object",
                  "required": ["cert", "key"],
                  "properties": {"cert": {"type": "string"}, "key": {"type": "string"}}
                }
              }
            }
          }
        }"#;
        let (text, caret) = accepted(schema, "server__t", "tls");
        assert_eq!(text, "server__tls__cert=\nserver__tls__key=");
        assert_eq!(caret, caret_after(&text, "server__tls__cert="));
    }

    #[test]
    fn accepting_an_enum_value_after_an_opening_quote_never_doubles_the_quotes() {
        // The caret is inside the quotes of `LOG_LEVEL="`. The crate writes the
        // whole entry for the value, so no second pair of quotes can appear.
        let schema = Schema::compile(ENV_SCHEMA).expect("schema");
        let text = "LOG_LEVEL=\"";
        let caret = text.encode_utf16().count();
        let suggestions = schema.suggestions(ENV, text, caret);
        let suggestion = suggestions
            .iter()
            .find(|suggestion| suggestion.label() == "info")
            .expect("the info value is offered");
        let (inserted, range) = suggestion.accept(text);
        assert_eq!(inserted, "LOG_LEVEL=info");
        assert_eq!(range.end, inserted.encode_utf16().count());
        assert!(!inserted.contains("\"\""), "{inserted:?}");
    }
}
