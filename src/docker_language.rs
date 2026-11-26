//! Dockerfile as a config language.
//!
//! The data model is a list with one item per instruction. Each item is an
//! object with a single key, the instruction keyword (`FROM`, `RUN`, ...),
//! whose value holds that instruction's fields: `line`, `arguments`, typed
//! fields such as `env` or `ports`, and `flags` for `--name=value` options.
//! Keying by keyword lets the schema describe each instruction on its own, so
//! hover on `FROM` and hover on `RUN` show different docs, and Ctrl+Space at
//! the start of a line lists the instructions with their descriptions.
//! Comments and blank lines are not part of the data.

use leptos_config_editor::language::{
    CompletionContext, ConfigLanguage, Hover, InsertContext, InsertPlan, LanguageConfig,
    LanguageInfo, ParseError, SchemaPathSegment, Span, Token, TokenKind, Tokens,
    utf16_to_byte_index,
};
use leptos_config_editor::{FieldPath, Language};
use serde_json::{Map, Value};

/// The instruction keywords, in Dockerfile reference order.
const KEYWORDS: [&str; 18] = [
    "FROM", "RUN", "CMD", "LABEL", "MAINTAINER", "EXPOSE", "ENV", "ADD", "COPY", "ENTRYPOINT",
    "VOLUME", "USER", "WORKDIR", "ARG", "ONBUILD", "STOPSIGNAL", "HEALTHCHECK", "SHELL",
];

/// Flags whose value is a boolean: a bare `--link` or `--link=true`.
const BOOLEAN_FLAGS: [&str; 4] = ["link", "parents", "keep-git-dir", "unpack"];

/// The argument form each instruction writes when accepted: the text up to
/// the caret, then the placeholder the caret lands on (its start). Every
/// instruction of the Dockerfile reference has one.
const TEMPLATES: &[(&str, &str, &str)] = &[
    ("FROM", "FROM ", "image"),
    ("RUN", "RUN ", "<command>"),
    ("CMD", "CMD [\"", "executable\",\"arg\"]"),
    ("LABEL", "LABEL ", "key=value"),
    ("MAINTAINER", "MAINTAINER ", "name"),
    ("EXPOSE", "EXPOSE ", "port"),
    ("ENV", "ENV ", "key=value"),
    ("ADD", "ADD ", "<src> <dest>"),
    ("COPY", "COPY ", "<src> <dest>"),
    ("ENTRYPOINT", "ENTRYPOINT [\"", "executable\"]"),
    ("VOLUME", "VOLUME [\"", "/data\"]"),
    ("USER", "USER ", "user"),
    ("WORKDIR", "WORKDIR ", "/path"),
    ("ARG", "ARG ", "name=default"),
    ("ONBUILD", "ONBUILD ", "<instruction>"),
    ("STOPSIGNAL", "STOPSIGNAL ", "signal"),
    ("HEALTHCHECK", "HEALTHCHECK CMD ", "<command>"),
    ("SHELL", "SHELL [\"", "executable\"]"),
];

/// One instruction: its logical text, which may span continuation lines.
#[derive(Debug, Clone)]
struct Item {
    /// The keyword, uppercased.
    keyword: String,
    /// 0-based line of the keyword.
    start: usize,
    /// 0-based last line of the instruction (inclusive).
    end: usize,
    /// Byte offsets of the keyword within line `start`.
    kw_start: usize,
    kw_end: usize,
    /// The text after the keyword, continuation lines joined with spaces.
    text: String,
}

/// Splits `source` into instructions, skipping blank and comment lines and
/// joining lines ending in a backslash.
fn scan(source: &str) -> Vec<Item> {
    let lines: Vec<&str> = source.lines().collect();
    let mut items = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let raw = lines[i];
        let trimmed = raw.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            i += 1;
            continue;
        }
        let kw_start = raw.len() - trimmed.len();
        let mut logical = String::new();
        let mut end = i;
        loop {
            let full = lines[end].trim_end();
            let (body, more) = match full.strip_suffix('\\') {
                Some(body) => (body, true),
                None => (full, false),
            };
            if end == i {
                logical.push_str(body.trim_start());
            } else if !(body.trim().is_empty() || body.trim_start().starts_with('#')) {
                logical.push(' ');
                logical.push_str(body.trim());
            }
            if !more || end + 1 >= lines.len() {
                break;
            }
            end += 1;
        }
        let logical = logical.trim_start();
        let keyword = logical.split_whitespace().next().unwrap_or("");
        if !keyword.is_empty() {
            items.push(Item {
                keyword: keyword.to_uppercase(),
                start: i,
                end,
                kw_start,
                kw_end: kw_start + keyword.len(),
                text: logical[keyword.len()..].trim().to_string(),
            });
        }
        i = end + 1;
    }
    items
}

/// The item whose lines include `line` (0-based), with its index.
fn item_at(items: &[Item], line: usize) -> Option<(usize, &Item)> {
    items
        .iter()
        .enumerate()
        .find(|(_, item)| item.start <= line && line <= item.end)
}

/// The field holding an instruction's arguments as data.
fn field_of(keyword: &str) -> &'static str {
    match keyword {
        "ENV" => "env",
        "LABEL" => "labels",
        "ARG" => "args",
        "EXPOSE" => "ports",
        "VOLUME" => "volumes",
        _ => "arguments",
    }
}

/// Characters (not bytes) before byte `at` of `line`.
fn char_column(line: &str, at: usize) -> usize {
    line[..at].chars().count()
}

/// The byte offset of character `column` of `line` (the end when past it).
fn byte_at_column(line: &str, column: usize) -> usize {
    line.char_indices()
        .nth(column)
        .map_or(line.len(), |(at, _)| at)
}

/// Byte ranges of the whitespace-separated tokens of `line` from byte `from`.
fn token_ranges(line: &str, from: usize) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start = None;
    for (offset, ch) in line[from..].char_indices() {
        let at = from + offset;
        if ch.is_whitespace() {
            if let Some(begin) = start.take() {
                ranges.push((begin, at));
            }
        } else if start.is_none() {
            start = Some(at);
        }
    }
    if let Some(begin) = start {
        ranges.push((begin, line.len()));
    }
    ranges
}

/// Byte ranges of the quoted strings in `code`, quotes included. A double
/// quote honors backslash escapes; a string left open runs to the end.
fn quoted_spans(code: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut chars = code.char_indices();
    while let Some((open, quote)) = chars.next() {
        if quote != '"' && quote != '\'' {
            continue;
        }
        let mut end = code.len();
        let mut escaped = false;
        for (at, ch) in chars.by_ref() {
            if escaped {
                escaped = false;
            } else if quote == '"' && ch == '\\' {
                escaped = true;
            } else if ch == quote {
                end = at + ch.len_utf8();
                break;
            }
        }
        spans.push((open, end));
    }
    spans
}

/// Whitespace-separated words, keeping spaces inside quotes.
fn words(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for ch in text.chars() {
        match quote {
            Some(open) => {
                current.push(ch);
                if ch == open {
                    quote = None;
                }
            }
            None if ch == '"' || ch == '\'' => {
                quote = Some(ch);
                current.push(ch);
            }
            None if ch.is_whitespace() => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            None => current.push(ch),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// A word without one pair of surrounding quotes.
fn unquote(word: &str) -> String {
    for quote in ['"', '\''] {
        if let Some(inner) = word
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return inner.to_string();
        }
    }
    word.to_string()
}

/// Adds a flag value, turning a repeated flag into an array of its values.
fn insert_flag(flags: &mut Map<String, Value>, name: &str, value: Value) {
    match flags.remove(name) {
        None => {
            flags.insert(name.to_string(), value);
        }
        Some(Value::Array(mut values)) => {
            values.push(value);
            flags.insert(name.to_string(), Value::Array(values));
        }
        Some(previous) => {
            flags.insert(name.to_string(), Value::Array(vec![previous, value]));
        }
    }
}

/// The leading `--name[=value]` flags of an instruction, and the rest.
fn split_flags(text: &str) -> (Map<String, Value>, &str) {
    let mut flags = Map::new();
    let mut rest = text.trim_start();
    loop {
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let token = &rest[..end];
        let Some(body) = token.strip_prefix("--") else {
            break;
        };
        if body.is_empty() {
            break;
        }
        let (name, value) = match body.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (body, None),
        };
        // Only boolean flags turn `true`/`false` into booleans; a bare flag is
        // `true`, which serializes back to the bare form.
        let value = match value {
            None => Value::Bool(true),
            Some("true") if BOOLEAN_FLAGS.contains(&name) => Value::Bool(true),
            Some("false") if BOOLEAN_FLAGS.contains(&name) => Value::Bool(false),
            Some(text) => Value::String(text.to_string()),
        };
        insert_flag(&mut flags, name, value);
        rest = rest[end..].trim_start();
    }
    (flags, rest)
}

/// Why an instruction cannot be read: an unknown keyword, or a `FROM`,
/// `COPY` or `ADD` without the arguments it needs. Reported at the keyword.
fn instruction_error(item: &Item, line: &str) -> Option<ParseError> {
    let at = |message: String| {
        ParseError::new(
            message,
            Some(item.start + 1),
            Some(char_column(line, item.kw_start) + 1),
        )
    };
    if !KEYWORDS.contains(&item.keyword.as_str()) {
        return Some(at(format!(
            "{} is not a Dockerfile instruction",
            item.keyword
        )));
    }
    let (_, rest) = split_flags(&item.text);
    let arguments = words(rest).len();
    match item.keyword.as_str() {
        "FROM" if arguments == 0 => Some(at(
            "FROM needs a base image, such as FROM alpine:3.20".to_string(),
        )),
        keyword @ ("COPY" | "ADD") if arguments < 2 => Some(at(format!(
            "{keyword} needs a source and a destination, such as {keyword} . /app"
        ))),
        _ => None,
    }
}

/// The fields of one instruction, as data.
fn body_value(body: &mut Map<String, Value>, keyword: &str, text: &str) {
    let (flags, rest) = split_flags(text);
    if !flags.is_empty() {
        body.insert("flags".to_string(), Value::Object(flags));
    }
    let list = words(rest);
    match keyword {
        "ENV" | "LABEL" if !list.is_empty() && list.iter().all(|word| word.contains('=')) => {
            let pairs: Map<String, Value> = list
                .iter()
                .filter_map(|word| word.split_once('='))
                .map(|(key, value)| (key.to_string(), Value::String(unquote(value))))
                .collect();
            let field = if keyword == "ENV" { "env" } else { "labels" };
            body.insert(field.to_string(), Value::Object(pairs));
        }
        "ARG" => {
            let pairs: Map<String, Value> = list
                .iter()
                .map(|word| match word.split_once('=') {
                    Some((name, default)) => (name.to_string(), Value::String(unquote(default))),
                    None => (word.clone(), Value::Null),
                })
                .collect();
            body.insert("args".to_string(), Value::Object(pairs));
        }
        "EXPOSE" => {
            let ports = list.into_iter().map(Value::String).collect();
            body.insert("ports".to_string(), Value::Array(ports));
        }
        "VOLUME" if !rest.starts_with('[') => {
            let volumes = list.into_iter().map(Value::String).collect();
            body.insert("volumes".to_string(), Value::Array(volumes));
        }
        _ if !rest.is_empty() => {
            body.insert("arguments".to_string(), Value::String(rest.to_string()));
        }
        _ => {}
    }
}

/// `key=value` pairs written on one line, quoting values with spaces.
fn pairs_text(map: &Map<String, Value>) -> String {
    map.iter()
        .map(|(key, value)| match value {
            Value::Null => key.clone(),
            Value::String(text) if text.chars().any(char::is_whitespace) => {
                format!("{key}=\"{text}\"")
            }
            Value::String(text) => format!("{key}={text}"),
            other => format!("{key}={other}"),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The text an instruction's fields are written as, after its keyword and flags.
/// Fields the text cannot hold are an error rather than dropped.
fn body_text(keyword: &str, fields: &Map<String, Value>) -> Result<String, String> {
    const BODY_FIELDS: [&str; 6] = ["arguments", "env", "labels", "args", "ports", "volumes"];
    for name in fields.keys() {
        if !["line", "flags"].contains(&name.as_str()) && !BODY_FIELDS.contains(&name.as_str()) {
            return Err(format!("{keyword} has no field {name}"));
        }
    }
    let present: Vec<&str> = BODY_FIELDS
        .into_iter()
        .filter(|name| fields.contains_key(*name))
        .collect();
    if present.len() > 1 {
        return Err(format!("{keyword} has more than one of {}", present.join(", ")));
    }
    if let Some(Value::Object(map)) = fields.get("env").or(fields.get("labels")) {
        if map.values().any(Value::is_null) {
            return Err(format!("{keyword} values must be text, not null"));
        }
    }
    for name in ["arguments", "env", "labels", "args"] {
        match fields.get(name) {
            None => continue,
            Some(Value::String(text)) => return Ok(text.clone()),
            Some(Value::Object(map)) => return Ok(pairs_text(map)),
            Some(other) => return Err(format!("{name} must be text or a map, not {other}")),
        }
    }
    for name in ["ports", "volumes"] {
        if let Some(value) = fields.get(name) {
            let Value::Array(items) = value else {
                return Err(format!("{name} must be a list"));
            };
            let words: Vec<String> = items
                .iter()
                .map(|item| match item {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                })
                .collect();
            return Ok(words.join(" "));
        }
    }
    Ok(String::new())
}

/// The `--name[=value]` texts of an instruction's flags, repeats expanded.
fn flag_texts(flags: &Map<String, Value>) -> Vec<String> {
    let mut out = Vec::new();
    for (name, value) in flags {
        let values: Vec<&Value> = match value {
            Value::Array(items) => items.iter().collect(),
            single => vec![single],
        };
        for value in values {
            out.push(match value {
                Value::Bool(true) => format!("--{name}"),
                Value::Bool(false) => format!("--{name}=false"),
                Value::String(text) => format!("--{name}={text}"),
                other => format!("--{name}={other}"),
            });
        }
    }
    out
}

/// The token of `line` after `from` whose name is `name`, as a byte range
/// (for a flag, the range from its `--`).
fn token_named(line: &str, from: usize, name: &str, flag: bool) -> Option<(usize, usize)> {
    token_ranges(line, from).into_iter().find(|(start, end)| {
        let token = &line[*start..*end];
        let bare = if flag {
            match token.strip_prefix("--") {
                Some(bare) => bare,
                None => return false,
            }
        } else {
            token
        };
        bare.split('=').next() == Some(name)
    })
}

/// The `Schema` path segments of an instruction, `[ArrayItem(i), Key(keyword), ...]`.
fn instruction_path(segments: &[SchemaPathSegment]) -> Option<(usize, &str, &[SchemaPathSegment])> {
    match segments {
        [
            SchemaPathSegment::ArrayItem(index),
            SchemaPathSegment::Key(keyword),
            tail @ ..,
        ] => Some((*index, keyword.as_str(), tail)),
        _ => None,
    }
}

/// Dockerfile language implementation.
#[derive(Debug, Default, Clone, Copy)]
pub struct Dockerfile;

impl ConfigLanguage for Dockerfile {
    fn info(&self) -> LanguageInfo {
        LanguageInfo {
            label: "Dockerfile",
            icon: "D",
            extensions: &["dockerfile", "Dockerfile"],
        }
    }

    fn config(&self) -> LanguageConfig {
        LanguageConfig {
            line_comments: &["#"],
            block_comment: None,
            brackets: &[('[', ']')],
            quotes: &["\"", "'"],
            ..Default::default()
        }
    }

    fn parse(&self, source: &str) -> Result<Value, ParseError> {
        let lines: Vec<&str> = source.lines().collect();
        let mut items = Vec::new();
        for item in scan(source) {
            let line = lines.get(item.start).copied().unwrap_or("");
            if let Some(error) = instruction_error(&item, line) {
                return Err(error);
            }
            let mut body = Map::new();
            body.insert("line".to_string(), Value::from(item.start + 1));
            body_value(&mut body, &item.keyword, &item.text);
            let mut wrapper = Map::new();
            wrapper.insert(item.keyword, Value::Object(body));
            items.push(Value::Object(wrapper));
        }
        Ok(Value::Array(items))
    }

    fn serialize(&self, value: &Value, _previous: Option<&str>) -> Result<String, String> {
        let items = match value {
            Value::Array(items) => items,
            // An empty object is the empty document: the editor serializes
            // one to learn whether a language can write documents at all.
            Value::Object(map) if map.is_empty() => return Ok(String::new()),
            _ => return Err("A Dockerfile is a list of instructions".to_string()),
        };
        let mut out = String::new();
        for item in items {
            let Value::Object(wrapper) = item else {
                return Err(format!("An instruction must be an object, not {item}"));
            };
            let mut entries = wrapper.iter();
            let (keyword, fields) = match (entries.next(), entries.next()) {
                (Some(entry), None) => entry,
                _ => return Err("Each instruction has exactly one keyword key".to_string()),
            };
            let keyword = keyword.to_uppercase();
            if !KEYWORDS.contains(&keyword.as_str()) {
                return Err(format!("{keyword} is not a Dockerfile instruction"));
            }
            let Value::Object(fields) = fields else {
                return Err(format!("{keyword} must be an object"));
            };
            let mut parts = vec![keyword.clone()];
            if let Some(flags) = fields.get("flags") {
                let Value::Object(flags) = flags else {
                    return Err(format!("{keyword} flags must be an object, not {flags}"));
                };
                parts.extend(flag_texts(flags));
            }
            let body = body_text(&keyword, fields)?;
            if !body.is_empty() {
                parts.push(body);
            }
            out.push_str(&parts.join(" "));
            out.push('\n');
        }
        Ok(out)
    }

    fn key_span(&self, source: &str, path: &FieldPath) -> Option<Span> {
        let (index, _, tail) = instruction_path(path.segments())?;
        let items = scan(source);
        let item = items.get(index)?;
        let line = source.lines().nth(item.start)?;
        let keyword_span = || Span {
            line: item.start + 1,
            start_column: char_column(line, item.kw_start) + 1,
            end_column: char_column(line, item.kw_end) + 1,
        };
        let name = match tail.last() {
            Some(SchemaPathSegment::Key(name)) if name != "flags" && name != "arguments" => name,
            _ => return Some(keyword_span()),
        };
        let flag = tail
            .iter()
            .any(|segment| matches!(segment, SchemaPathSegment::Key(key) if key == "flags"));
        let Some((start, _)) = token_named(line, item.kw_end, name, flag) else {
            return Some(keyword_span());
        };
        let start = start + if flag { 2 } else { 0 };
        let column = char_column(line, start) + 1;
        Some(Span {
            line: item.start + 1,
            start_column: column,
            end_column: column + name.chars().count(),
        })
    }

    fn value_span(&self, source: &str, path: &FieldPath) -> Option<Span> {
        let (index, _, tail) = instruction_path(path.segments())?;
        let items = scan(source);
        let item = items.get(index)?;
        let line = source.lines().nth(item.start)?;
        let (value_start, value_end) = match tail.last() {
            Some(SchemaPathSegment::Key(name)) if name != "flags" && name != "arguments" => {
                let flag = tail
                    .iter()
                    .any(|segment| matches!(segment, SchemaPathSegment::Key(key) if key == "flags"));
                let (start, end) = token_named(line, item.kw_end, name, flag)?;
                let token = &line[start..end];
                let eq = token.find('=')?;
                (start + eq + 1, end)
            }
            _ => {
                let first = token_ranges(line, item.kw_end).first()?.0;
                (first, line.trim_end().len())
            }
        };
        Some(Span {
            line: item.start + 1,
            start_column: char_column(line, value_start) + 1,
            end_column: char_column(line, value_end) + 1,
        })
    }

    fn highlight(&self, line: &str) -> Vec<Token> {
        let mut tokens = Tokens::new();
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        if trimmed.starts_with('#') {
            tokens.push(indent, line.len(), TokenKind::Comment);
            return tokens.finish();
        }
        // A trailing backslash continues the instruction on the next line.
        let visible = line.trim_end();
        let (code_end, continued) = match visible.strip_suffix('\\') {
            Some(code) => (code.len(), true),
            None => (visible.len(), false),
        };
        let code = &line[indent..code_end];
        let mut spans: Vec<(usize, usize, TokenKind)> = Vec::new();
        if continued {
            spans.push((code_end, visible.len(), TokenKind::Punctuation));
        }
        // The keyword, then the leading `--name` flags; a flag on a
        // continuation line is recognized the same way.
        let mut leading = true;
        for (index, (start, end)) in token_ranges(code, 0).into_iter().enumerate() {
            let word = &code[start..end];
            if index == 0 && KEYWORDS.contains(&word.to_uppercase().as_str()) {
                spans.push((indent + start, indent + end, TokenKind::Key));
            } else if leading && word.starts_with("--") {
                let name_len = word
                    .find(|ch| matches!(ch, '=' | '"' | '\''))
                    .unwrap_or(word.len());
                if name_len > 2 {
                    spans.push((indent + start, indent + start + name_len, TokenKind::Key));
                }
            } else {
                leading = false;
            }
        }
        for (start, end) in quoted_spans(code) {
            spans.push((indent + start, indent + end, TokenKind::String));
        }
        spans.sort_by_key(|span| span.0);
        for (start, end, kind) in spans {
            tokens.push(start, end, kind);
        }
        tokens.finish()
    }

    fn completion_context(&self, source: &str, cursor: usize) -> CompletionContext {
        let byte = utf16_to_byte_index(source, cursor);
        let line_start = source[..byte].rfind('\n').map_or(0, |at| at + 1);
        let line_index = source[..line_start].matches('\n').count();
        let before = &source[line_start..byte];
        let trimmed = before.trim_start();
        let indent = before.len() - trimmed.len();
        let base_indent = before[..indent].to_string();
        let utf16 = |at: usize| source[..at].encode_utf16().count();
        let caret = utf16(byte);
        let no_properties = || CompletionContext {
            no_properties: true,
            base_indent: base_indent.clone(),
            ..Default::default()
        };

        if trimmed.starts_with('#') {
            return no_properties();
        }
        let items = scan(source);
        let Some((index, item)) = item_at(&items, line_index) else {
            // A blank line, or one after the last instruction: a new instruction,
            // which would be the item at the index of the instructions above it.
            if trimmed.contains(char::is_whitespace) {
                return no_properties();
            }
            let above = items.iter().filter(|item| item.start < line_index).count();
            let blank_document = source.trim().is_empty();
            return CompletionContext {
                path: vec![SchemaPathSegment::ArrayItem(above)],
                typed: trimmed.to_string(),
                replace_start: utf16(line_start + indent),
                replace_end: caret,
                is_array_item: true,
                explicit_entry_start: true,
                root_item: blank_document.then_some(("", "", "")),
                base_indent,
                ..Default::default()
            };
        };

        if line_index == item.start && byte <= line_start + item.kw_end {
            // The caret is on the keyword: the instruction's own key.
            return CompletionContext {
                path: vec![SchemaPathSegment::ArrayItem(index)],
                typed: before[indent..].to_string(),
                replace_start: utf16(line_start + indent),
                replace_end: caret,
                is_array_item: true,
                explicit_entry_start: true,
                base_indent,
                ..Default::default()
            };
        }

        let word_start = before
            .char_indices()
            .rev()
            .find(|(_, ch)| ch.is_whitespace())
            .map_or(0, |(at, ch)| at + ch.len_utf8());
        let word = &before[word_start..];
        let keyword = item.keyword.clone();
        if let Some(flag) = word.strip_prefix("--").filter(|_| line_index == item.start) {
            return CompletionContext {
                path: vec![
                    SchemaPathSegment::ArrayItem(index),
                    SchemaPathSegment::Key(keyword),
                    SchemaPathSegment::Key("flags".to_string()),
                ],
                typed: flag.to_string(),
                replace_start: utf16(line_start + word_start + 2),
                replace_end: caret,
                explicit_entry_start: true,
                base_indent,
                ..Default::default()
            };
        }

        // Arguments: the field the instruction keeps them in. Only the schema
        // decides what it offers there, which for free text is nothing.
        let field = field_of(&item.keyword).to_string();
        CompletionContext {
            path: vec![
                SchemaPathSegment::ArrayItem(index),
                SchemaPathSegment::Key(keyword),
            ],
            value_key: Some(field),
            replace_start: caret,
            replace_end: caret,
            base_indent,
            ..Default::default()
        }
    }

    fn write_insert(
        &self,
        key: &str,
        plan: &InsertPlan<'_>,
        context: &InsertContext<'_>,
    ) -> Option<(String, usize)> {
        let in_flags = matches!(
            context.path.last(),
            Some(SchemaPathSegment::Key(name)) if name == "flags"
        );
        if in_flags {
            let text = if BOOLEAN_FLAGS.contains(&key) {
                key.to_string()
            } else {
                format!("{key}=")
            };
            let caret = text.encode_utf16().count();
            return Some((text, caret));
        }
        // An instruction is an object in the schema: its template, if any,
        // is written with the caret on the first placeholder.
        if plan.object
            && let Some((_, lead, placeholder)) =
                TEMPLATES.iter().find(|(keyword, _, _)| *keyword == key)
        {
            let text = format!("{lead}{placeholder}");
            let caret = lead.encode_utf16().count();
            return Some((text, caret));
        }
        let text = format!("{key} ");
        let caret = text.encode_utf16().count();
        Some((text, caret))
    }

    fn format_value(&self, value: &Value) -> String {
        match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        }
    }

    fn hover_key(&self, line: &str, column: usize) -> Option<(String, usize)> {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        let word = trimmed.split_whitespace().next()?;
        let keyword = word.to_uppercase();
        if !KEYWORDS.contains(&keyword.as_str()) {
            return None;
        }
        let start = char_column(line, indent);
        let end = start + word.chars().count();
        (start..end).contains(&column).then_some((keyword, start))
    }

    fn hover(&self) -> &dyn Hover {
        self
    }
}

impl Hover for Dockerfile {
    /// The path of the hovered key itself, ending in that key: the keyword
    /// (`[ArrayItem(i), Key("FROM")]`), an argument's field
    /// (`[ArrayItem(i), Key("FROM"), Key("arguments")]`) or a flag
    /// (`[ArrayItem(i), Key("COPY"), Key("flags"), Key("from")]`).
    fn hover_path(
        &self,
        source: &str,
        line_index: usize,
        column: usize,
    ) -> Option<(Vec<SchemaPathSegment>, String, usize)> {
        let items = scan(source);
        let (index, item) = item_at(&items, line_index)?;
        let line = source.lines().nth(line_index)?;
        let byte = byte_at_column(line, column);
        let instruction = |key: &str| {
            vec![
                SchemaPathSegment::ArrayItem(index),
                SchemaPathSegment::Key(item.keyword.clone()),
                SchemaPathSegment::Key(key.to_string()),
            ]
        };
        if line_index != item.start {
            let field = field_of(&item.keyword);
            return Some((instruction(field), field.to_string(), column));
        }
        if byte >= item.kw_start && byte < item.kw_end {
            return Some((
                vec![
                    SchemaPathSegment::ArrayItem(index),
                    SchemaPathSegment::Key(item.keyword.clone()),
                ],
                item.keyword.clone(),
                char_column(line, item.kw_start),
            ));
        }
        let (start, end) = token_ranges(line, item.kw_end)
            .into_iter()
            .find(|(start, end)| byte >= *start && byte < *end)?;
        let token = &line[start..end];
        if let Some(flag) = token.strip_prefix("--") {
            let name = flag.split('=').next().unwrap_or("");
            if name.is_empty() {
                return None;
            }
            let path = vec![
                SchemaPathSegment::ArrayItem(index),
                SchemaPathSegment::Key(item.keyword.clone()),
                SchemaPathSegment::Key("flags".to_string()),
                SchemaPathSegment::Key(name.to_string()),
            ];
            return Some((path, name.to_string(), char_column(line, start + 2)));
        }
        let field = field_of(&item.keyword);
        Some((instruction(field), field.to_string(), char_column(line, start)))
    }
}

/// Dockerfile language handle.
pub const DOCKERFILE: Language = Language::new(&Dockerfile);

/// JSON Schema for Dockerfile validation, completion and hover. Each
/// instruction keyword is a property of the item, described per instruction
/// from the Dockerfile reference (docs.docker.com/reference/dockerfile).
pub const DOCKERFILE_SCHEMA: &str = r#"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "type": "array",
  "title": "Dockerfile",
  "description": "A Dockerfile as a list with one item per instruction, keyed by the instruction keyword. Comments and blank lines are not part of the data.",
  "items": {
    "type": "object",
    "title": "Instruction",
    "description": "One Dockerfile instruction. Its single key is the keyword.",
    "minProperties": 1,
    "maxProperties": 1,
    "additionalProperties": false,
    "properties": {
      "FROM": {
        "type": "object",
        "title": "Base image",
        "description": "Starts a build stage and sets the base image for the instructions that follow. Syntax: FROM [--platform=<platform>] <image> [AS <name>].",
        "properties": {
          "line": { "type": "integer", "minimum": 1, "title": "Line", "description": "The 1-based line of the instruction in the Dockerfile." },
          "arguments": { "type": "string", "title": "Image", "description": "The image reference, optionally followed by AS <stage name>, for example alpine:3.20 AS build." },
          "flags": {
            "type": "object",
            "title": "Flags",
            "description": "Flags given before the image.",
            "properties": {
              "platform": { "type": "string", "title": "Platform", "description": "Target platform for a multi-platform build, such as linux/amd64. Defaults to the build's target platform." }
            },
            "additionalProperties": false
          }
        },
        "additionalProperties": false
      },
      "RUN": {
        "type": "object",
        "title": "Run command",
        "description": "Runs a command in a new layer on top of the current image and commits the result. Shell form RUN <command> or exec form RUN [\"executable\", \"param\"].",
        "properties": {
          "line": { "type": "integer", "minimum": 1, "title": "Line", "description": "The 1-based line of the instruction in the Dockerfile." },
          "arguments": { "type": "string", "title": "Command", "description": "The command, in shell form or as a JSON array in exec form." },
          "flags": {
            "type": "object",
            "title": "Flags",
            "description": "Flags given before the command.",
            "properties": {
              "device": { "type": "string", "title": "Device", "description": "Requests CDI devices for this step, as name or name,[required]. Needs BuildKit 0.20.0 or later." },
              "mount": { "type": ["string", "array"], "items": { "type": "string" }, "title": "Mount", "description": "A filesystem mount for this step: type=bind, cache, tmpfs, secret or ssh, with options such as target=, id=, sharing=. Repeat the flag for more mounts." },
              "network": { "type": "string", "enum": ["default", "none", "host"], "title": "Network", "description": "Network mode for this step: default, none or host." },
              "security": { "type": "string", "enum": ["sandbox", "insecure"], "title": "Security", "description": "Security mode: sandbox (the default) or insecure, which runs without the sandbox like docker run --privileged." }
            },
            "additionalProperties": false
          }
        },
        "additionalProperties": false
      },
      "CMD": {
        "type": "object",
        "title": "Default command",
        "description": "Sets the default command for containers started from the image. Only the last CMD takes effect. Exec form CMD [\"executable\", \"param\"], shell form CMD <command>, or default parameters for ENTRYPOINT.",
        "properties": {
          "line": { "type": "integer", "minimum": 1, "title": "Line", "description": "The 1-based line of the instruction in the Dockerfile." },
          "arguments": { "type": "string", "title": "Command", "description": "The default command, as a JSON array in exec form or as shell text." }
        },
        "additionalProperties": false
      },
      "LABEL": {
        "type": "object",
        "title": "Metadata labels",
        "description": "Adds key=value metadata labels to the image. Quote values that contain spaces.",
        "properties": {
          "line": { "type": "integer", "minimum": 1, "title": "Line", "description": "The 1-based line of the instruction in the Dockerfile." },
          "arguments": { "type": "string", "title": "Labels", "description": "Labels that could not be read as key=value pairs." },
          "labels": { "type": "object", "title": "Labels", "description": "Label keys and values.", "additionalProperties": { "type": "string" } }
        },
        "additionalProperties": false
      },
      "MAINTAINER": {
        "type": "object",
        "title": "Author (deprecated)",
        "description": "Sets the Author field of the generated image. Deprecated: use LABEL instead, such as LABEL org.opencontainers.image.authors=\"name\".",
        "deprecated": true,
        "properties": {
          "line": { "type": "integer", "minimum": 1, "title": "Line", "description": "The 1-based line of the instruction in the Dockerfile." },
          "arguments": { "type": "string", "title": "Author", "description": "The author name, and optionally an email address." }
        },
        "additionalProperties": false
      },
      "EXPOSE": {
        "type": "object",
        "title": "Exposed ports",
        "description": "Documents the ports the container listens on at runtime. It does not publish them. Format <port>[/<protocol>], TCP by default.",
        "properties": {
          "line": { "type": "integer", "minimum": 1, "title": "Line", "description": "The 1-based line of the instruction in the Dockerfile." },
          "ports": {
            "type": "array",
            "title": "Ports",
            "description": "Ports, each as <port> or <port>/<tcp|udp>.",
            "items": { "type": "string", "pattern": "^[0-9]+(-[0-9]+)?(/(tcp|udp|sctp))?$" }
          }
        },
        "additionalProperties": false
      },
      "ENV": {
        "type": "object",
        "title": "Environment variables",
        "description": "Sets environment variables for the instructions that follow and for containers started from the image. Format key=value.",
        "properties": {
          "line": { "type": "integer", "minimum": 1, "title": "Line", "description": "The 1-based line of the instruction in the Dockerfile." },
          "arguments": { "type": "string", "title": "Variables", "description": "Text in the legacy form ENV <key> <value>, kept as written." },
          "env": { "type": "object", "title": "Environment variables", "description": "Variable names and values.", "additionalProperties": { "type": "string" } }
        },
        "additionalProperties": false
      },
      "ADD": {
        "type": "object",
        "title": "Add files",
        "description": "Copies files, directories, remote URLs, tar archives or Git repositories from the build context or a URL into the image. Syntax: ADD [--options] <src>... <dest>.",
        "properties": {
          "line": { "type": "integer", "minimum": 1, "title": "Line", "description": "The 1-based line of the instruction in the Dockerfile." },
          "arguments": { "type": "string", "title": "Sources and destination", "description": "The source paths or URLs, then the destination path in the image." },
          "flags": {
            "type": "object",
            "title": "Flags",
            "description": "Flags given before the sources.",
            "properties": {
              "keep-git-dir": { "type": "boolean", "title": "Keep .git", "description": "Keeps the .git directory when adding a Git repository. Excluded by default." },
              "checksum": { "type": "string", "title": "Checksum", "description": "Verifies a Git commit (full SHA or prefix) or an HTTP source (sha256:<hash>)." },
              "chmod": { "type": "string", "title": "Permissions", "description": "Permissions for the added files, in octal (755) or symbolic (+x) form." },
              "chown": { "type": "string", "title": "Owner", "description": "Owner of the added files as user:group, by name or numeric ID." },
              "link": { "type": "boolean", "title": "Link", "description": "Copies into an independent layer, which improves cache reuse." },
              "unpack": { "type": "boolean", "title": "Unpack", "description": "Unpacks tar archives. Local archives unpack by default; remote ones do not." },
              "exclude": { "type": ["string", "array"], "items": { "type": "string" }, "title": "Exclude", "description": "Excludes paths matching the pattern. Repeat the flag for more patterns." }
            },
            "additionalProperties": false
          }
        },
        "additionalProperties": false
      },
      "COPY": {
        "type": "object",
        "title": "Copy files",
        "description": "Copies files and directories from the build context, a build stage, a named context or an image into the image. Syntax: COPY [--options] <src>... <dest>.",
        "properties": {
          "line": { "type": "integer", "minimum": 1, "title": "Line", "description": "The 1-based line of the instruction in the Dockerfile." },
          "arguments": { "type": "string", "title": "Sources and destination", "description": "The source paths, then the destination path in the image." },
          "flags": {
            "type": "object",
            "title": "Flags",
            "description": "Flags given before the sources.",
            "properties": {
              "from": { "type": "string", "title": "From", "description": "Copies from an image, a build stage or a named context instead of the build context." },
              "chmod": { "type": "string", "title": "Permissions", "description": "Permissions for the copied files, in octal (755) or symbolic (+x) form. Not supported on Windows." },
              "chown": { "type": "string", "title": "Owner", "description": "Owner of the copied files as user:group, by name or numeric ID. Defaults to 0:0. Not supported on Windows." },
              "link": { "type": "boolean", "title": "Link", "description": "Copies into an empty layer linked on top of the prior layers, which improves cache reuse." },
              "parents": { "type": "boolean", "title": "Parents", "description": "Keeps the source's parent directories in the destination. Defaults to false." },
              "exclude": { "type": ["string", "array"], "items": { "type": "string" }, "title": "Exclude", "description": "Excludes paths matching the pattern. Repeat the flag for more patterns." }
            },
            "additionalProperties": false
          }
        },
        "additionalProperties": false
      },
      "ENTRYPOINT": {
        "type": "object",
        "title": "Entrypoint",
        "description": "Configures the container to run as an executable. Exec form ENTRYPOINT [\"executable\", \"param\"] is preferred over the shell form.",
        "properties": {
          "line": { "type": "integer", "minimum": 1, "title": "Line", "description": "The 1-based line of the instruction in the Dockerfile." },
          "arguments": { "type": "string", "title": "Entrypoint", "description": "The executable, as a JSON array in exec form or as shell text." }
        },
        "additionalProperties": false
      },
      "VOLUME": {
        "type": "object",
        "title": "Volumes",
        "description": "Creates a mount point with the given name for externally mounted volumes. Syntax: VOLUME [\"/data\"] or VOLUME /data.",
        "properties": {
          "line": { "type": "integer", "minimum": 1, "title": "Line", "description": "The 1-based line of the instruction in the Dockerfile." },
          "volumes": { "type": "array", "title": "Mount points", "description": "Mount point paths in the image.", "items": { "type": "string" } },
          "arguments": { "type": "string", "title": "Mount points", "description": "The mount points as a JSON array, kept as written." }
        },
        "additionalProperties": false
      },
      "USER": {
        "type": "object",
        "title": "User",
        "description": "Sets the user name or UID, and optionally the group or GID, for later RUN, CMD and ENTRYPOINT instructions.",
        "properties": {
          "line": { "type": "integer", "minimum": 1, "title": "Line", "description": "The 1-based line of the instruction in the Dockerfile." },
          "arguments": { "type": "string", "title": "User", "description": "The user, as name or UID, optionally followed by :group or :GID." }
        },
        "additionalProperties": false
      },
      "WORKDIR": {
        "type": "object",
        "title": "Working directory",
        "description": "Sets the working directory for the RUN, CMD, ENTRYPOINT, COPY and ADD instructions that follow. The directory is created if it does not exist.",
        "properties": {
          "line": { "type": "integer", "minimum": 1, "title": "Line", "description": "The 1-based line of the instruction in the Dockerfile." },
          "arguments": { "type": "string", "title": "Directory", "description": "The directory path, absolute or relative to the previous WORKDIR." }
        },
        "additionalProperties": false
      },
      "ARG": {
        "type": "object",
        "title": "Build argument",
        "description": "Defines a build-time variable, passed with docker build --build-arg. Format name[=default]. Unlike ENV, it is not kept in the image.",
        "properties": {
          "line": { "type": "integer", "minimum": 1, "title": "Line", "description": "The 1-based line of the instruction in the Dockerfile." },
          "args": {
            "type": "object",
            "title": "Build arguments",
            "description": "Variable names, each with its default value or null for none.",
            "additionalProperties": { "type": ["string", "null"] }
          }
        },
        "additionalProperties": false
      },
      "ONBUILD": {
        "type": "object",
        "title": "Build trigger",
        "description": "Registers an instruction to run when this image is used as the base of a later build's FROM. ONBUILD ONBUILD and ONBUILD FROM or MAINTAINER are not allowed.",
        "properties": {
          "line": { "type": "integer", "minimum": 1, "title": "Line", "description": "The 1-based line of the instruction in the Dockerfile." },
          "arguments": { "type": "string", "title": "Trigger", "description": "The instruction to run later, for example RUN make." }
        },
        "additionalProperties": false
      },
      "STOPSIGNAL": {
        "type": "object",
        "title": "Stop signal",
        "description": "Sets the system signal that docker stop sends to exit the container. The default is SIGTERM.",
        "properties": {
          "line": { "type": "integer", "minimum": 1, "title": "Line", "description": "The 1-based line of the instruction in the Dockerfile." },
          "arguments": { "type": "string", "title": "Signal", "description": "A signal name such as SIGKILL, or a number such as 9." }
        },
        "additionalProperties": false
      },
      "HEALTHCHECK": {
        "type": "object",
        "title": "Health check",
        "description": "Sets the command that checks the container's health, or NONE to disable a health check inherited from the base image. Only the last HEALTHCHECK takes effect. Exit 0 is healthy, 1 unhealthy.",
        "properties": {
          "line": { "type": "integer", "minimum": 1, "title": "Line", "description": "The 1-based line of the instruction in the Dockerfile." },
          "arguments": { "type": "string", "title": "Command", "description": "CMD <command> to run as the check, or NONE." },
          "flags": {
            "type": "object",
            "title": "Flags",
            "description": "Timing options given before CMD.",
            "properties": {
              "interval": { "type": "string", "title": "Interval", "description": "Time between checks, such as 30s. Default 30s." },
              "timeout": { "type": "string", "title": "Timeout", "description": "Maximum time a check may run before it is killed. Default 30s." },
              "start-period": { "type": "string", "title": "Start period", "description": "Startup time for slow containers. Failures during it do not count toward retries. Default 0s." },
              "start-interval": { "type": "string", "title": "Start interval", "description": "Check frequency during the start period. Default 5s. Needs Docker Engine 25.0 or later." },
              "retries": { "type": "string", "title": "Retries", "description": "Consecutive failures before the container is marked unhealthy. Default 3." }
            },
            "additionalProperties": false
          }
        },
        "additionalProperties": false
      },
      "SHELL": {
        "type": "object",
        "title": "Shell",
        "description": "Overrides the default shell for shell-form RUN, CMD and ENTRYPOINT. Must use JSON form, such as [\"/bin/bash\", \"-c\"]. Defaults to [\"/bin/sh\", \"-c\"] on Linux.",
        "properties": {
          "line": { "type": "integer", "minimum": 1, "title": "Line", "description": "The 1-based line of the instruction in the Dockerfile." },
          "arguments": { "type": "string", "title": "Shell", "description": "A JSON array holding the shell executable and its parameters." }
        },
        "additionalProperties": false
      }
    }
  }
}"#;

#[cfg(test)]
mod tests {
    use super::*;
    use leptos_config_editor::{Schema, TextPosition};

    fn schema() -> Schema {
        Schema::compile(DOCKERFILE_SCHEMA).unwrap()
    }

    /// The description the schema gives for the instruction at `line`/`column`
    /// (1-based, as the editor's hover sees them).
    fn hover_description(source: &str, line: usize, column: usize) -> Option<String> {
        schema()
            .hover(DOCKERFILE, source, TextPosition::new(line, column))
            .and_then(|docs| docs.description)
    }

    fn labels(source: &str, cursor: usize) -> Vec<(String, String)> {
        schema()
            .suggestions(DOCKERFILE, source, cursor)
            .iter()
            .map(|s| (s.label().to_string(), s.detail().to_string()))
            .collect()
    }

    #[test]
    fn dockerfile_parses_basic() {
        let source = "FROM alpine:latest\nRUN apk add --no-cache curl\nENV PORT=8080\nEXPOSE 8080\nCMD [\"curl\", \"localhost:8080\"]\n";
        let value = Dockerfile.parse(source).unwrap();
        let items = value.as_array().unwrap();
        let keys: Vec<&str> = items
            .iter()
            .map(|item| item.as_object().unwrap().keys().next().unwrap().as_str())
            .collect();
        assert_eq!(keys, ["FROM", "RUN", "ENV", "EXPOSE", "CMD"]);
        assert_eq!(items[0]["FROM"]["arguments"], "alpine:latest");
        assert_eq!(items[0]["FROM"]["line"], 1);
        assert_eq!(items[1]["RUN"]["arguments"], "apk add --no-cache curl");
        assert_eq!(items[4]["CMD"]["line"], 5);
    }

    #[test]
    fn dockerfile_parses_env_as_object() {
        let value = Dockerfile.parse("ENV PORT=8080 HOST=localhost\n").unwrap();
        assert_eq!(value[0]["ENV"]["env"]["PORT"], "8080");
        assert_eq!(value[0]["ENV"]["env"]["HOST"], "localhost");
    }

    #[test]
    fn dockerfile_parses_labels() {
        let value = Dockerfile
            .parse("LABEL version=1.0 maintainer=\"test person\"\n")
            .unwrap();
        assert_eq!(value[0]["LABEL"]["labels"]["version"], "1.0");
        assert_eq!(value[0]["LABEL"]["labels"]["maintainer"], "test person");
    }

    #[test]
    fn dockerfile_parses_flags_and_continuations() {
        let source = "COPY --from=builder --chown=app:app \\\n  src/ dst/\nADD --link --exclude=*.md --exclude=tmp . /app\n";
        let value = Dockerfile.parse(source).unwrap();
        assert_eq!(value[0]["COPY"]["line"], 1);
        assert_eq!(value[0]["COPY"]["flags"]["from"], "builder");
        assert_eq!(value[0]["COPY"]["flags"]["chown"], "app:app");
        assert_eq!(value[0]["COPY"]["arguments"], "src/ dst/");
        assert_eq!(value[1]["ADD"]["flags"]["link"], true);
        assert_eq!(value[1]["ADD"]["flags"]["exclude"], serde_json::json!(["*.md", "tmp"]));
    }

    #[test]
    fn dockerfile_comments_and_blank_lines_are_not_data() {
        let value = Dockerfile.parse("# syntax=docker/dockerfile:1\n\nFROM alpine\n").unwrap();
        assert_eq!(value.as_array().unwrap().len(), 1);
    }

    #[test]
    fn dockerfile_roundtrip() {
        let source = "FROM alpine:latest\nENV PORT=8080\nEXPOSE 8080\nCMD [\"server\"]\nCOPY --from=builder --link a b\n";
        let value = Dockerfile.parse(source).unwrap();
        let serialized = ConfigLanguage::serialize(&Dockerfile, &value, None).unwrap();
        assert_eq!(serialized, source);
        assert_eq!(Dockerfile.parse(&serialized).unwrap(), value);
    }

    #[test]
    fn dockerfile_highlighting() {
        let tokens = Dockerfile.highlight("FROM alpine:latest");
        let kinds: Vec<TokenKind> = tokens.iter().map(|t| t.kind).collect();
        assert!(kinds.contains(&TokenKind::Key));
    }

    /// The `(start, end, kind)` runs `Dockerfile.highlight` gives for `line`.
    fn runs(line: &str) -> Vec<(usize, usize, TokenKind)> {
        Dockerfile
            .highlight(line)
            .iter()
            .map(|t| (t.start, t.end, t.kind))
            .collect()
    }

    #[test]
    fn dockerfile_highlights_instruction_keyword() {
        assert_eq!(runs("FROM alpine"), [(0, 4, TokenKind::Key)]);
        assert_eq!(runs("  run echo"), [(2, 5, TokenKind::Key)]);
        // A keyword later in the line is an argument, not an instruction.
        assert_eq!(runs("RUN ENV"), [(0, 3, TokenKind::Key)]);
    }

    #[test]
    fn dockerfile_highlights_flags_as_keys() {
        assert_eq!(
            runs("RUN --mount=type=cache,target=/a true"),
            [(0, 3, TokenKind::Key), (4, 11, TokenKind::Key)]
        );
        // Two leading flags; a `--` after the arguments is just text.
        assert_eq!(
            runs("COPY --from=b --link a --b"),
            [
                (0, 4, TokenKind::Key),
                (5, 11, TokenKind::Key),
                (14, 20, TokenKind::Key)
            ]
        );
    }

    #[test]
    fn dockerfile_highlights_strings_including_json_arrays() {
        assert_eq!(
            runs("CMD [\"server\"]"),
            [(0, 3, TokenKind::Key), (5, 13, TokenKind::String)]
        );
        // A quoted value with a space is one string, not two words.
        assert_eq!(
            runs("ENV MSG=\"a b\" X='c'"),
            [
                (0, 3, TokenKind::Key),
                (8, 13, TokenKind::String),
                (16, 19, TokenKind::String)
            ]
        );
    }

    #[test]
    fn dockerfile_highlights_comments() {
        assert_eq!(runs("  # note"), [(2, 8, TokenKind::Comment)]);
        assert_eq!(runs("# RUN x"), [(0, 7, TokenKind::Comment)]);
    }

    #[test]
    fn dockerfile_highlights_continuation_backslash() {
        // The backslash is the last byte of the line.
        assert_eq!(
            runs("RUN echo hi \\"),
            [(0, 3, TokenKind::Key), (12, 13, TokenKind::Punctuation)]
        );
        // A flag on a continuation line is a key too.
        assert_eq!(
            runs("  --chown=a:b \\"),
            [(2, 9, TokenKind::Key), (14, 15, TokenKind::Punctuation)]
        );
    }

    #[test]
    fn dockerfile_converts_to_yaml_and_back() {
        let source = "FROM alpine:latest\nENV PORT=8080\nEXPOSE 8080\nCMD [\"server\"]\n";
        let yaml = DOCKERFILE.convert(source, Language::YAML).unwrap().text;
        let back = Language::YAML.convert(&yaml, DOCKERFILE).unwrap().text;
        assert_eq!(back, source);
    }

    #[test]
    fn dockerfile_serialize_accepts_the_empty_document() {
        let empty = serde_json::json!({});
        assert_eq!(ConfigLanguage::serialize(&Dockerfile, &empty, None).unwrap(), "");
        assert_eq!(ConfigLanguage::serialize(&Dockerfile, &serde_json::json!([]), None).unwrap(), "");
    }

    #[test]
    fn dockerfile_serialize_refuses_fields_it_cannot_write() {
        let cases = [
            (r#"[{"RUN": {"arguments": "a", "env": {"X": "1"}}}]"#, "more than one"),
            (r#"[{"RUN": {"arguments": "a", "shell": "x"}}]"#, "has no field shell"),
            (r#"[{"RUN": {"arguments": "a", "flags": "x"}}]"#, "flags must be an object"),
            (r#"[{"ENV": {"env": {"X": null}}}]"#, "must be text, not null"),
        ];
        for (json, message) in cases {
            let value: Value = serde_json::from_str(json).unwrap();
            let error = ConfigLanguage::serialize(&Dockerfile, &value, None).expect_err(json);
            assert!(error.contains(message), "{json}: {error}");
        }
    }

    #[test]
    fn dockerfile_schema_validation() {
        let source = "FROM alpine:latest\nENV PORT=8080\nEXPOSE 8080\nHEALTHCHECK --interval=30s CMD curl -f http://localhost/\n";
        let problems = schema().validate(DOCKERFILE, source);
        assert!(problems.is_empty(), "Expected no problems, got: {:?}", problems);
    }

    #[test]
    fn dockerfile_schema_catches_invalid_instruction() {
        let problems = schema().validate(DOCKERFILE, "INVALID instruction\n");
        assert!(!problems.is_empty());
    }

    #[test]
    fn dockerfile_schema_catches_unknown_flag() {
        let problems = schema().validate(DOCKERFILE, "FROM --chown=a alpine\n");
        assert!(!problems.is_empty());
    }

    #[test]
    fn dockerfile_hover_differs_per_instruction() {
        let from = hover_description("FROM alpine:latest\n", 1, 1).unwrap();
        let run = hover_description("RUN apk add curl\n", 1, 1).unwrap();
        let env = hover_description("ENV PORT=8080\n", 1, 1).unwrap();
        assert!(from.contains("base image"), "{from}");
        assert!(run.contains("Runs a command"), "{run}");
        assert!(env.contains("environment variables"), "{env}");
        assert_ne!(from, run);
        assert_ne!(run, env);
        assert_ne!(from, env);
    }

    #[test]
    fn dockerfile_hover_on_keyword_on_later_line() {
        let source = "FROM alpine\nRUN true\nENV A=1\n";
        let env = hover_description(source, 3, 2).unwrap();
        assert!(env.contains("environment variables"), "{env}");
        let run = hover_description(source, 2, 2).unwrap();
        assert!(run.contains("Runs a command"), "{run}");
    }

    #[test]
    fn dockerfile_hover_on_argument_shows_field_docs() {
        let docs = schema()
            .hover(DOCKERFILE, "FROM alpine:latest\n", TextPosition::new(1, 8))
            .unwrap();
        assert_eq!(docs.key, "arguments");
        assert!(docs.description.unwrap().contains("image reference"));
    }

    #[test]
    fn dockerfile_hover_path_returns_keyword() {
        let source = "FROM alpine:latest\n";
        let (path, key, start) = Dockerfile.hover_path(source, 0, 0).unwrap();
        assert_eq!(key, "FROM");
        assert_eq!(start, 0);
        assert_eq!(
            path,
            vec![
                SchemaPathSegment::ArrayItem(0),
                SchemaPathSegment::Key("FROM".to_string())
            ]
        );
    }

    #[test]
    fn dockerfile_hover_key_returns_actual_keyword() {
        let (key, _) = Dockerfile.hover_key("FROM alpine:latest", 1).unwrap();
        assert_eq!(key, "FROM");
        let (key, _) = Dockerfile.hover_key("volume /data", 1).unwrap();
        assert_eq!(key, "VOLUME");
    }

    #[test]
    fn dockerfile_hover_on_comment_is_none() {
        assert!(Dockerfile.hover_key("# comment", 0).is_none());
        assert!(Dockerfile.hover_path("# comment", 0, 0).is_none());
        assert!(hover_description("# comment\n", 1, 1).is_none());
    }

    #[test]
    fn dockerfile_key_span() {
        let source = "FROM alpine:latest\nENV PORT=8080\n";
        let span = Dockerfile.key_span(source, &FieldPath::from("/0/FROM"));
        assert_eq!(span, Some(Span { line: 1, start_column: 1, end_column: 5 }));
        let span = Dockerfile.key_span(source, &FieldPath::from("/1/ENV"));
        assert_eq!(span, Some(Span { line: 2, start_column: 1, end_column: 4 }));
    }

    #[test]
    fn dockerfile_env_key_span() {
        let source = "ENV PORT=8080 HOST=localhost\n";
        let span = Dockerfile.key_span(source, &FieldPath::from("/0/ENV/env/HOST"));
        assert_eq!(span, Some(Span { line: 1, start_column: 15, end_column: 19 }));
    }

    #[test]
    fn dockerfile_schema_describes_every_instruction() {
        let schema: Value = serde_json::from_str(DOCKERFILE_SCHEMA).unwrap();
        let properties = schema["items"]["properties"].as_object().unwrap();
        assert_eq!(properties.len(), KEYWORDS.len());
        for keyword in KEYWORDS {
            let description = properties[keyword]["description"].as_str();
            assert!(description.is_some_and(|text| !text.is_empty()), "{keyword}");
        }
        assert_eq!(properties["MAINTAINER"]["deprecated"], true);
    }

    #[test]
    fn dockerfile_completion_lists_every_instruction_at_line_start() {
        let offered = labels("", 0);
        let names: Vec<&str> = offered.iter().map(|(label, _)| label.as_str()).collect();
        for keyword in KEYWORDS {
            assert!(names.contains(&keyword), "missing {keyword} in {names:?}");
        }
        assert_eq!(offered.len(), KEYWORDS.len());
        for (label, detail) in &offered {
            assert!(!detail.is_empty(), "{label} has no detail");
        }
        let run = offered.iter().find(|(label, _)| label == "RUN").unwrap();
        assert!(run.1.contains("Runs a command"), "{}", run.1);
    }

    #[test]
    fn dockerfile_completion_on_a_new_line_after_instructions() {
        let source = "FROM alpine\n";
        let offered = labels(source, source.encode_utf16().count());
        assert_eq!(offered.len(), KEYWORDS.len());
    }

    #[test]
    fn dockerfile_completion_filters_by_typed_keyword() {
        let offered = labels("RU", 2);
        let names: Vec<&str> = offered.iter().map(|(label, _)| label.as_str()).collect();
        assert!(names.contains(&"RUN"), "{names:?}");
        assert!(!names.contains(&"FROM"), "{names:?}");
    }

    #[test]
    fn dockerfile_completion_in_arguments_offers_no_keywords() {
        let offered = labels("FROM ", 5);
        assert!(offered.iter().all(|(label, _)| !KEYWORDS.contains(&label.as_str())));
    }

    #[test]
    fn dockerfile_completion_context_comment_returns_empty() {
        let ctx = Dockerfile.completion_context("# comment", 3);
        assert!(ctx.no_properties);
        assert!(labels("# comment", 9).is_empty());
    }

    #[test]
    fn dockerfile_completion_context_keyword_path() {
        let ctx = Dockerfile.completion_context("FROM alpine\nRU", 14);
        assert_eq!(ctx.path, vec![SchemaPathSegment::ArrayItem(1)]);
        assert_eq!(ctx.typed, "RU");
    }

    #[test]
    fn dockerfile_completion_context_arguments() {
        let ctx = Dockerfile.completion_context("ENV PORT=8080", 13);
        assert_eq!(ctx.value_key, Some("env".to_string()));
        assert_eq!(
            ctx.path,
            vec![
                SchemaPathSegment::ArrayItem(0),
                SchemaPathSegment::Key("ENV".to_string())
            ]
        );
    }

    #[test]
    fn dockerfile_insert_writes_run_template() {
        let source = "";
        let offered = schema().suggestions(DOCKERFILE, source, 0);
        let run = offered.iter().find(|s| s.label() == "RUN").unwrap();
        assert_eq!(run.accept(source).0, "RUN <command>");
        assert_eq!(run.accept(source).1, 4..4);
    }

    /// Accepts `label` on an empty line and returns the text and the caret.
    fn accept_on_empty_line(label: &str) -> (String, std::ops::Range<usize>) {
        let offered = schema().suggestions(DOCKERFILE, "", 0);
        let suggestion = offered.iter().find(|s| s.label() == label).unwrap();
        suggestion.accept("")
    }

    #[test]
    fn dockerfile_accept_writes_every_instruction_template() {
        // (keyword, text written, caret as a UTF-16 offset on the first placeholder)
        let cases: [(&str, &str, usize); 18] = [
            ("FROM", "FROM image", 5),
            ("RUN", "RUN <command>", 4),
            ("CMD", "CMD [\"executable\",\"arg\"]", 6),
            ("LABEL", "LABEL key=value", 6),
            ("MAINTAINER", "MAINTAINER name", 11),
            ("EXPOSE", "EXPOSE port", 7),
            ("ENV", "ENV key=value", 4),
            ("ADD", "ADD <src> <dest>", 4),
            ("COPY", "COPY <src> <dest>", 5),
            ("ENTRYPOINT", "ENTRYPOINT [\"executable\"]", 13),
            ("VOLUME", "VOLUME [\"/data\"]", 9),
            ("USER", "USER user", 5),
            ("WORKDIR", "WORKDIR /path", 8),
            ("ARG", "ARG name=default", 4),
            ("ONBUILD", "ONBUILD <instruction>", 8),
            ("STOPSIGNAL", "STOPSIGNAL signal", 11),
            ("HEALTHCHECK", "HEALTHCHECK CMD <command>", 16),
            ("SHELL", "SHELL [\"executable\"]", 8),
        ];
        for (keyword, text, caret) in cases {
            assert_eq!(
                accept_on_empty_line(keyword),
                (text.to_string(), caret..caret),
                "{keyword}"
            );
        }
    }

    #[test]
    fn dockerfile_accept_flag_writes_equals_with_caret_after_it() {
        // The editor has the "--" typed already and replaces the word after it.
        let source = "COPY --ch";
        let offered = schema().suggestions(DOCKERFILE, source, source.encode_utf16().count());
        let chown = offered.iter().find(|s| s.label() == "chown").unwrap();
        assert_eq!(chown.accept(source), ("COPY --chown=".to_string(), 13..13));
    }

    #[test]
    fn dockerfile_accept_over_typed_keyword_replaces_it() {
        // Accepting over a typed "EN" replaces it; the caret lands on the key.
        let source = "EN";
        let offered = schema().suggestions(DOCKERFILE, source, 2);
        let env = offered.iter().find(|s| s.label() == "ENV").unwrap();
        assert_eq!(env.accept(source), ("ENV key=value".to_string(), 4..4));
    }

    #[test]
    fn dockerfile_accept_inside_quotes_does_not_double_them() {
        // A quoted or JSON-array argument has its opening quote typed already,
        // with the caret between the quotes. Accepting any value offered there
        // must replace the content between the quotes, so the quote count holds.
        // Nothing is offered inside these quotes today, so this checks no value.
        let cases = [
            "CMD [\"",
            "CMD [\"a\", \"",
            "ENV KEY=\"",
            "ENV KEY=\"x",
            "RUN --network=\"",
            "RUN --security=\"d",
        ];
        for source in cases {
            let cursor = source.encode_utf16().count();
            let offered = schema().suggestions(DOCKERFILE, source, cursor);
            for suggestion in offered.iter() {
                let (text, _) = suggestion.accept(source);
                assert_eq!(
                    text.matches('"').count(),
                    source.matches('"').count(),
                    "{source:?} accepting {:?} gave {text:?}",
                    suggestion.label()
                );
            }
        }
    }

    #[test]
    fn dockerfile_repeated_mount_flag_validates() {
        let source = "RUN --mount=type=cache,target=/a --mount=type=secret,id=x true\n";
        let problems = schema().validate(DOCKERFILE, source);
        assert!(problems.is_empty(), "{problems:?}");
        let value = Dockerfile.parse(source).unwrap();
        assert_eq!(value[0]["RUN"]["flags"]["mount"].as_array().unwrap().len(), 2);
        let serialized = ConfigLanguage::serialize(&Dockerfile, &value, None).unwrap();
        assert_eq!(serialized, source);
    }

    #[test]
    fn dockerfile_string_flag_keeps_true_and_false_text() {
        let source = "COPY --from=true --link=false a b\n";
        let value = Dockerfile.parse(source).unwrap();
        assert_eq!(value[0]["COPY"]["flags"]["from"], "true");
        assert_eq!(value[0]["COPY"]["flags"]["link"], false);
        let serialized = ConfigLanguage::serialize(&Dockerfile, &value, None).unwrap();
        assert_eq!(serialized, source);
    }

    #[test]
    fn dockerfile_trailing_backslash_has_no_trailing_space() {
        let value = Dockerfile.parse("RUN echo hi \\\n").unwrap();
        assert_eq!(value[0]["RUN"]["arguments"], "echo hi");
        let value = Dockerfile.parse("RUN \\").unwrap();
        assert!(value[0]["RUN"].get("arguments").is_none());
    }

    #[test]
    fn dockerfile_partial_input_does_not_panic() {
        let sources = [
            "",
            "\n",
            "\\",
            "FROM",
            "FROM ",
            "RUN \\",
            "RUN foo \\",
            "RUN foo \\\n",
            "COPY --",
            "COPY --from",
            "COPY --from=",
            "COPY --=x",
            "COPY --from=x \\\n",
            "COPY --mount=\"",
            "RUN --mount=type=cache,target=\"",
            "LABEL a=\"x",
            "ENV A='",
            "ENV A=\"foo bar",
            "ARG",
            "ENV",
            "EXPOSE ",
            "VOLUME [",
            "CMD [\"a\"",
            "HEALTHCHECK --interval",
            "ONBUILD",
            "# \\",
            "é FROM",
            "FROM é\n--",
            "FROM alpine AS \u{1F600}\nRUN \u{1F600} --from=\u{1F600}",
        ];
        let schema = schema();
        for source in sources {
            let _ = Dockerfile.parse(source);
            let _ = schema.validate(DOCKERFILE, source);
            let lines = source.lines().count().max(1);
            let width = source.chars().count() + 2;
            for line in 1..=lines + 1 {
                for column in 1..=width {
                    let _ = schema.hover(DOCKERFILE, source, TextPosition::new(line, column));
                    let _ = Dockerfile.hover_key(source.lines().nth(line - 1).unwrap_or(""), column);
                }
            }
            for cursor in 0..=source.encode_utf16().count() + 1 {
                let _ = schema.suggestions(DOCKERFILE, source, cursor);
                let _ = Dockerfile.completion_context(source, cursor);
            }
        }
    }

    #[test]
    fn dockerfile_flag_suggestions_have_descriptions() {
        let offered = labels("COPY --", 7);
        let names: Vec<&str> = offered.iter().map(|(label, _)| label.as_str()).collect();
        for flag in ["from", "chmod", "chown", "link", "parents", "exclude"] {
            assert!(names.contains(&flag), "missing {flag} in {names:?}");
        }
        assert!(offered.iter().all(|(_, detail)| !detail.is_empty()));
    }

    #[test]
    fn dockerfile_bad_instructions_are_parse_errors_at_the_keyword() {
        let cases = [
            ("FROM", 1, "FROM needs a base image"),
            ("FORM alpine", 1, "FORM is not a Dockerfile instruction"),
            ("FROM alpine:3.20\nCOPY", 2, "COPY needs a source and a destination"),
            ("FROM alpine:3.20\n  ADD app", 2, "ADD needs a source and a destination"),
        ];
        for (source, line, message) in cases {
            let error = Dockerfile.parse(source).expect_err(source);
            assert_eq!(error.line, Some(line), "{source:?}");
            assert!(error.message.contains(message), "{source:?}: {}", error.message);
        }
        // Valid forms still parse, including flags before the arguments.
        assert!(Dockerfile.parse("FROM alpine:3.20\nCOPY --chown=app:app . /srv").is_ok());
    }
}



