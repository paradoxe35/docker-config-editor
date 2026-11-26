//! Caddyfile language: the configuration of the Caddy web server, with a JSON
//! Schema of its common directives and global options.
//!
//! The data model is a simplification of the Caddyfile:
//! - Global options: a `{ ... }` block on its own at the top of the file. Its
//!   lines become top-level keys (`email`, `servers`, ...), as do top-level
//!   lines naming one of those options with no block.
//! - A site is a top-level key named by its address (the words before its
//!   `{`, joined by spaces). Its directives are the keys of that object.
//! - A directive with arguments only is an array of its argument strings:
//!   `reverse_proxy localhost:8080` is `"reverse_proxy": ["localhost:8080"]`.
//!   A directive with no arguments is `[]`.
//! - A directive with a block is an object: its arguments, if any, go under
//!   `"args"`, and its subdirectives are its keys.
//! - A named matcher `@name` is a key starting with `@`.
//!
//! Simplified: a directive repeated in one block keeps its last occurrence;
//! a one-line block (`handle { respond "x" }`) is a syntax error; quoted
//! arguments lose their quotes; placeholders such as `{env.VAR}` are plain
//! text; the arguments of a block-form directive (`handle_path /api/* { }`)
//! have no schema of their own.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::OnceLock;

use leptos_config_editor::language::{
    CompletionContext, ConfigLanguage, Hover, InsertContext, InsertPlan, LanguageConfig,
    LanguageInfo, ParseError, SchemaPathSegment, Span, Text, Token as Run, TokenKind, Tokens,
    utf16_to_byte_index,
};
use leptos_config_editor::{FieldPath, Language};
use serde_json::{Map, Value};

use SchemaPathSegment as Seg;

/// The Caddyfile language.
pub struct Caddyfile;

/// The global options the schema lists. A top-level `{ ... }` block or line
/// naming one of these is an option; any other name opens a site.
const GLOBAL_OPTIONS: &[&str] = &[
    "email",
    "acme_ca",
    "acme_ca_root",
    "acme_eab",
    "auto_https",
    "admin",
    "debug",
    "default_bind",
    "default_sni",
    "grace_period",
    "http_port",
    "https_port",
    "key_type",
    "local_certs",
    "log",
    "metrics",
    "on_demand_tls",
    "order",
    "servers",
    "shutdown_delay",
    "skip_install_trust",
    "storage",
];

/// One word of a line: its unquoted text and where it sits in the line.
#[derive(Clone, Debug)]
struct Token {
    text: String,
    quoted: bool,
    /// Byte offsets in the line, the quotes included.
    start: usize,
    end: usize,
}

fn is_word(token: &Token, text: &str) -> bool {
    !token.quoted && token.text == text
}

/// The words of a line, and whether it ends in a comment. A lenient read
/// keeps an unterminated quote as a word running to the end of the text
/// (what is being typed), where a strict read fails with its byte offset.
fn tokenize(line: &str, lenient: bool) -> Result<(Vec<Token>, bool), (usize, &'static str)> {
    let mut tokens = Vec::new();
    let mut chars = line.char_indices().peekable();
    while let Some(&(start, c)) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        if c == '#' {
            return Ok((tokens, true));
        }
        if c == '"' || c == '`' {
            chars.next();
            let mut text = String::new();
            let mut end = None;
            while let Some((at, ch)) = chars.next() {
                if ch == c {
                    end = Some(at + ch.len_utf8());
                    break;
                }
                if c == '"' && ch == '\\' {
                    if let Some((_, escaped)) = chars.next() {
                        text.push(escaped);
                        continue;
                    }
                }
                text.push(ch);
            }
            match end {
                Some(end) => tokens.push(Token {
                    text,
                    quoted: true,
                    start,
                    end,
                }),
                None if lenient => tokens.push(Token {
                    text,
                    quoted: true,
                    start,
                    end: line.len(),
                }),
                None => return Err((start, "unterminated quoted string")),
            }
            continue;
        }
        let mut end = start;
        while let Some(&(at, ch)) = chars.peek() {
            if ch.is_whitespace() {
                break;
            }
            end = at + ch.len_utf8();
            chars.next();
        }
        tokens.push(Token {
            text: line[start..end].to_string(),
            quoted: false,
            start,
            end,
        });
    }
    Ok((tokens, false))
}

/// Where each key and each argument of a document is written, keyed by the
/// JSON pointer of its path.
#[derive(Default)]
struct Locations {
    keys: HashMap<String, Span>,
    values: HashMap<String, Span>,
}

/// What a block's closing brace hands back to its parent.
enum Attach {
    /// The top level, which never closes.
    Root,
    /// Stored under this key in the parent.
    Key(String),
    /// The global options block: its entries join the top level.
    Global,
}

struct Frame {
    /// The schema path of the entries inside the block.
    path: Vec<Seg>,
    map: Map<String, Value>,
    attach: Attach,
    /// One-based line and column of the opening `{`.
    opened: (usize, usize),
    /// How many directives of each name the block holds so far.
    counts: HashMap<String, usize>,
}

/// The key of the next directive named `name` in a block that already holds
/// `counts` of it: the name the first time, then `name#2`, `name#3`, ... so a
/// repeated directive keeps every occurrence (Caddy sorts same-named
/// directives by their matchers).
fn occurrence_key(counts: &HashMap<String, usize>, name: &str) -> String {
    match counts.get(name).copied().unwrap_or(0) {
        0 => name.to_string(),
        seen => format!("{name}#{}", seen + 1),
    }
}

/// Reads a Caddyfile line by line, building the data model, the locations
/// of its keys and arguments, and any syntax errors. Reading a prefix of a
/// document (for completion) uses the same walk, ignoring its errors.
struct Walker {
    stack: Vec<Frame>,
    locations: Locations,
    errors: Vec<ParseError>,
}

fn pointer(path: &[Seg]) -> String {
    FieldPath::from(path.to_vec()).to_pointer()
}

/// One-based character column of byte `at` in `line`.
fn column(line: &str, at: usize) -> usize {
    line[..at].chars().count() + 1
}

fn span(line_no: usize, line: &str, word: &Token) -> Span {
    Span::from((
        line_no,
        column(line, word.start),
        column(line, word.end),
    ))
}

fn words_value(words: &[Token]) -> Value {
    Value::Array(
        words
            .iter()
            .map(|word| Value::String(word.text.clone()))
            .collect(),
    )
}

impl Walker {
    fn new() -> Self {
        Walker {
            stack: vec![Frame {
                path: Vec::new(),
                map: Map::new(),
                attach: Attach::Root,
                opened: (0, 0),
                counts: HashMap::new(),
            }],
            locations: Locations::default(),
            errors: Vec::new(),
        }
    }

    fn top(&self) -> &Frame {
        self.stack.last().expect("the top level is always open")
    }

    /// Whether directives written in the top block take occurrence keys: a
    /// site's or a directive's block does, the top level and the global
    /// options block do not.
    fn numbers_directives(&self) -> bool {
        self.stack.len() > 1 && !matches!(self.top().attach, Attach::Global)
    }

    /// The key a directive named `name` gets in the top block, before it is
    /// counted (see [`occurrence_key`]).
    fn peek_key(&self, name: &str) -> String {
        if self.numbers_directives() {
            occurrence_key(&self.top().counts, name)
        } else {
            name.to_string()
        }
    }

    /// The key of a directive just written in the top block, which counts it.
    fn directive_key(&mut self, name: &str) -> String {
        let key = self.peek_key(name);
        if self.numbers_directives() {
            let top = self.stack.last_mut().expect("the top level is open");
            *top.counts.entry(name.to_string()).or_insert(0) += 1;
        }
        key
    }

    fn error(&mut self, line_no: usize, line: &str, at: usize, message: &str) {
        self.errors.push(ParseError::new(
            message,
            Some(line_no),
            Some(column(line, at)),
        ));
    }

    fn record_args(&mut self, base: &[Seg], line_no: usize, line: &str, words: &[Token]) {
        for (index, word) in words.iter().enumerate() {
            let mut path = base.to_vec();
            path.push(Seg::ArrayItem(index));
            self.locations
                .values
                .insert(pointer(&path), span(line_no, line, word));
        }
    }

    fn line(&mut self, line_no: usize, line: &str) {
        let words = match tokenize(line, false) {
            Ok((words, _)) => words,
            Err((at, message)) => return self.error(line_no, line, at, message),
        };
        let Some(first) = words.first() else {
            return;
        };

        if is_word(first, "}") {
            if let Some(extra) = words.get(1) {
                self.error(line_no, line, extra.start, "text after `}`");
            }
            if self.stack.len() == 1 {
                return self.error(line_no, line, first.start, "unexpected `}`");
            }
            let frame = self.stack.pop().expect("a block is open");
            let parent = self.stack.last_mut().expect("the top level is open");
            match frame.attach {
                Attach::Key(name) => {
                    parent.map.insert(name, Value::Object(frame.map));
                }
                Attach::Global => parent.map.extend(frame.map),
                Attach::Root => {}
            }
            return;
        }

        let last = words.len() - 1;
        let opens = is_word(&words[last], "{");
        for (index, word) in words.iter().enumerate() {
            let opener = is_word(word, "{") && !(opens && index == last);
            if opener || is_word(word, "}") {
                return self.error(
                    line_no,
                    line,
                    word.start,
                    "a block must open at the end of its line and close on a line of its own",
                );
            }
        }
        let head = if opens { &words[..last] } else { &words[..] };
        let Some(name) = head.first() else {
            // A bare `{`: the global options block, at the top level only.
            if self.stack.len() == 1 {
                let opened = (line_no, column(line, words[0].start));
                self.stack.push(Frame {
                    path: Vec::new(),
                    map: Map::new(),
                    attach: Attach::Global,
                    opened,
                    counts: HashMap::new(),
                });
            } else {
                self.error(
                    line_no,
                    line,
                    words[0].start,
                    "a block needs a directive name before its `{`",
                );
            }
            return;
        };
        let opened = (line_no, column(line, words[last].start));

        if self.stack.len() == 1 && opens && !GLOBAL_OPTIONS.contains(&name.text.as_str()) {
            // A site: every word before its `{` is the address.
            let address = head
                .iter()
                .map(|word| word.text.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            let path = vec![Seg::Key(address.clone())];
            let final_word = head.last().expect("the address has a word");
            let whole = Span::from((
                line_no,
                column(line, name.start),
                column(line, final_word.end),
            ));
            self.locations.keys.insert(pointer(&path), whole);
            self.stack.push(Frame {
                path,
                map: Map::new(),
                attach: Attach::Key(address),
                opened,
                counts: HashMap::new(),
            });
            return;
        }

        let key = self.directive_key(&name.text);
        let mut path = self.top().path.clone();
        path.push(Seg::Key(key.clone()));
        self.locations
            .keys
            .insert(pointer(&path), span(line_no, line, name));
        let args = &head[1..];
        if opens {
            let mut map = Map::new();
            if !args.is_empty() {
                let mut args_path = path.clone();
                args_path.push(Seg::Key("args".to_string()));
                self.record_args(&args_path, line_no, line, args);
                map.insert("args".to_string(), words_value(args));
            }
            self.stack.push(Frame {
                path,
                map,
                attach: Attach::Key(key),
                opened,
                counts: HashMap::new(),
            });
        } else {
            self.record_args(&path, line_no, line, args);
            let top = self.stack.last_mut().expect("the top level is open");
            top.map.insert(key, words_value(args));
        }
    }

    /// Ends the walk: unclosed blocks are errors, and the top level is the
    /// document.
    fn finish(mut self) -> (Value, Locations, Vec<ParseError>) {
        while self.stack.len() > 1 {
            let frame = self.stack.pop().expect("a block is open");
            self.errors.push(ParseError::new(
                "this `{` is never closed",
                Some(frame.opened.0),
                Some(frame.opened.1),
            ));
        }
        let root = self.stack.swap_remove(0).map;
        (Value::Object(root), self.locations, self.errors)
    }
}

/// The data model, the locations and the syntax errors of a whole document.
fn walk(source: &str) -> (Value, Locations, Vec<ParseError>) {
    let mut walker = Walker::new();
    for (index, line) in source.lines().enumerate() {
        walker.line(index + 1, line);
    }
    walker.finish()
}

/// The walk of the lines of `prefix` (whole lines), for the structure around
/// a line that follows them.
fn structure(prefix: &str) -> Walker {
    let mut walker = Walker::new();
    for (index, line) in prefix.lines().enumerate() {
        walker.line(index + 1, line);
    }
    walker
}

/// Byte offsets of the line at zero-based `index`, without its line break.
fn line_bounds(source: &str, index: usize) -> Option<(usize, usize)> {
    let mut start = 0;
    for (at, line) in source.split('\n').enumerate() {
        let end = start + line.len();
        if at == index {
            return Some((start, end));
        }
        start = end + 1;
    }
    None
}

impl Hover for Caddyfile {
    /// The directive (or option, or site) named by the first word of a line.
    /// Its schema path is the enclosing block's path plus the name, so the
    /// hover card reads that directive's own docs.
    fn hover_path(
        &self,
        source: &str,
        line_index: usize,
        column: usize,
    ) -> Option<(Vec<Seg>, String, usize)> {
        let (line_start, line_end) = line_bounds(source, line_index)?;
        let line = &source[line_start..line_end];
        let (words, _) = tokenize(line, true).ok()?;
        let word = words.first()?;
        if word.quoted || is_word(word, "{") || is_word(word, "}") {
            return None;
        }
        let start = line[..word.start].chars().count();
        let end = start + line[word.start..word.end].chars().count();
        if !(start..end).contains(&column) {
            return None;
        }
        let walker = structure(&source[..line_start]);
        let mut path = walker.top().path.clone();
        path.push(Seg::Key(walker.peek_key(&word.text)));
        Some((path, word.text.clone(), start))
    }
}

impl ConfigLanguage for Caddyfile {
    fn info(&self) -> LanguageInfo {
        LanguageInfo {
            label: "Caddyfile",
            icon: "C",
            extensions: &["caddyfile"],
        }
    }

    fn config(&self) -> LanguageConfig {
        LanguageConfig {
            line_comments: &["#"],
            block_comment: None,
            brackets: &[('{', '}')],
            separators: &[],
            quotes: &["\"", "`"],
            ..LanguageConfig::default()
        }
    }

    fn parse(&self, source: &str) -> Result<Value, ParseError> {
        let (value, _, errors) = walk(source);
        match errors.into_iter().min_by_key(|error| (error.line, error.column)) {
            Some(error) => Err(error),
            None => Ok(value),
        }
    }

    fn key_span(&self, source: &str, path: &FieldPath) -> Option<Span> {
        walk(source).1.keys.get(&path.to_pointer()).copied()
    }

    fn value_span(&self, source: &str, path: &FieldPath) -> Option<Span> {
        walk(source).1.values.get(&path.to_pointer()).copied()
    }

    /// The block holding the caret; a directive's arguments; a directive name
    /// being typed. Only the words before the caret are read.
    fn completion_context(&self, source: &str, cursor: usize) -> CompletionContext {
        let byte = utf16_to_byte_index(source, cursor);
        let line_start = source[..byte].rfind('\n').map_or(0, |at| at + 1);
        let line_end = source[byte..].find('\n').map_or(source.len(), |at| byte + at);
        let line = &source[line_start..line_end];
        let prefix = &source[line_start..byte];
        let utf16 = |at: usize| source[..at].encode_utf16().count();
        let walker = structure(&source[..line_start]);
        let block = walker.top().path.clone();
        let base_indent: String = line
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();

        let (words, comment) = tokenize(prefix, true).unwrap_or_default();
        if comment {
            return CompletionContext {
                path: block,
                no_properties: true,
                base_indent,
                ..Default::default()
            };
        }
        let ends_space = prefix.chars().last().is_none_or(char::is_whitespace);
        if words.first().is_some_and(|word| is_word(word, "}")) {
            let parent = walker
                .stack
                .len()
                .checked_sub(2)
                .map(|at| walker.stack[at].path.clone())
                .unwrap_or_default();
            return CompletionContext {
                path: parent,
                no_properties: true,
                base_indent,
                ..Default::default()
            };
        }

        // A directive name is being typed: the first word, with no space after.
        if words.len() == 1 && !ends_space || words.is_empty() {
            let (typed, start) = match words.first() {
                Some(word) => (&prefix[word.start..], line_start + word.start),
                None => ("", byte),
            };
            return CompletionContext {
                path: block,
                typed: typed.to_string(),
                replace_start: utf16(start),
                replace_end: cursor,
                base_indent,
                ..Default::default()
            };
        }

        // Past the first word: an argument of that directive.
        let name = &words[0];
        if block.is_empty() && !GLOBAL_OPTIONS.contains(&name.text.as_str()) {
            // A site address takes no arguments.
            return CompletionContext {
                path: block,
                no_properties: true,
                base_indent,
                ..Default::default()
            };
        }
        let full = tokenize(line, true).map(|(words, _)| words).unwrap_or_default();
        let opens = full.last().is_some_and(|word| is_word(word, "{"));
        let mut path = block;
        path.push(Seg::Key(walker.peek_key(&name.text)));
        if opens {
            path.push(Seg::Key("args".to_string()));
        }
        let (index, typed, start) = match words.last() {
            Some(last) if !ends_space => {
                // Inside an unterminated quote, the typed text starts after
                // the opening quote, so an accepted value replaces the
                // content between the quotes and keeps the quotes.
                let open = last.quoted && tokenize(prefix, false).is_err();
                let from = if open { last.start + 1 } else { last.start };
                (words.len() - 2, &prefix[from..], line_start + from)
            }
            _ => (words.len() - 1, "", byte),
        };
        path.push(Seg::ArrayItem(index));
        CompletionContext {
            path,
            typed: typed.to_string(),
            replace_start: utf16(start),
            replace_end: cursor,
            is_array_item: true,
            base_indent,
            ..Default::default()
        }
    }

    /// A directive is written as its [`Template`] says, which follows its
    /// schema. Any other directive with a block opens it on its line, its
    /// body indented one level; anything else is written as `key ` for its
    /// arguments.
    fn write_insert(
        &self,
        key: &str,
        plan: &InsertPlan<'_>,
        context: &InsertContext<'_>,
    ) -> Option<(String, usize)> {
        if let Some(written) = template(key).and_then(|t| templated(key, t, context)) {
            return Some(written);
        }
        if plan.object {
            return Some(block_insert(key, context));
        }
        let text = format!("{key} ");
        let caret = text.encode_utf16().count();
        Some((text, caret))
    }

    /// Arguments are written bare: `gzip`, not `"gzip"`.
    fn format_value(&self, value: &Value) -> String {
        match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        }
    }

    /// Writes the data model back as a Caddyfile: the global options in one
    /// block at the top, then one block per site. Values the syntax cannot
    /// hold (null, objects inside an argument list, names with spaces) are
    /// an error.
    fn serialize(&self, value: &Value, previous: Option<&str>) -> Result<String, String> {
        let _ = previous;
        let Value::Object(map) = value else {
            return Err("a Caddyfile is a set of global options and site blocks".to_string());
        };
        let mut globals = String::new();
        let mut sites = String::new();
        for (key, value) in map {
            if GLOBAL_OPTIONS.contains(&key.as_str()) {
                write_directive(&mut globals, key, value, 1)?;
                continue;
            }
            let Value::Object(body) = value else {
                return Err(format!(
                    "{key}: a site is a block of directives, so it cannot be written from this value"
                ));
            };
            let address = key.split_whitespace().map(word).collect::<Vec<_>>().join(" ");
            if address.is_empty() {
                return Err("a site has no address to write before its block".to_string());
            }
            if !sites.is_empty() {
                sites.push('\n');
            }
            sites.push_str(&format!("{address} {{\n"));
            write_block(&mut sites, body, 1)?;
            sites.push_str("}\n");
        }
        let mut text = String::new();
        if !globals.is_empty() {
            text.push_str("{\n");
            text.push_str(&globals);
            text.push_str("}\n");
        }
        if !globals.is_empty() && !sites.is_empty() {
            text.push('\n');
        }
        text.push_str(&sites);
        Ok(text)
    }

    fn highlight(&self, line: &str) -> Vec<Run> {
        scan(line, true)
    }

    fn hover(&self) -> &dyn Hover {
        self
    }

    fn text(&self) -> &dyn Text {
        self
    }
}

impl Text for Caddyfile {
    /// Quoted strings may run over lines (`"..."` and backticks), so each
    /// line of one past its first is colored as a string.
    fn multiline_strings(&self, source: &str) -> Vec<Range<usize>> {
        string_ranges(source)
    }

    /// The rest of a string opened on an earlier line: it closes at the
    /// first quote mark in `text` (a backtick or a double quote), and the
    /// text after that is colored like a line of its own.
    fn highlight_continued(&self, language: &dyn ConfigLanguage, text: &str) -> Vec<Run> {
        let _ = language;
        let Some(close) = text.find(['"', '`']) else {
            let mut tokens = Tokens::new();
            tokens.push(0, text.len(), TokenKind::String);
            return tokens.finish();
        };
        let mut tokens = Tokens::new();
        tokens.push(0, close + 1, TokenKind::String);
        let offset = close + 1;
        let mut runs = tokens.finish();
        runs.extend(
            scan(&text[offset..], false)
                .into_iter()
                .map(|run| Run::new(run.start + offset, run.end + offset, run.kind)),
        );
        runs
    }
}

/// The colored runs of one line of Caddyfile, outside any string that
/// began on an earlier line.
fn scan(line: &str, first_is_name: bool) -> Vec<Run> {
    let mut tokens = Tokens::new();
    let bytes = line.as_bytes();
    let mut at = 0;
    let mut first = first_is_name;
    while at < bytes.len() {
        if bytes[at].is_ascii_whitespace() {
            at += 1;
            continue;
        }
        let after_space = at == 0 || bytes[at - 1].is_ascii_whitespace();
        if bytes[at] == b'#' && after_space {
            tokens.push(at, line.len(), TokenKind::Comment);
            break;
        }
        if bytes[at] == b'"' || bytes[at] == b'`' {
            let end = quoted_end(bytes, at);
            push_text(&mut tokens, line, at, end, TokenKind::String);
            at = end;
            first = false;
            continue;
        }
        let end = bytes[at..]
            .iter()
            .position(|byte| byte.is_ascii_whitespace())
            .map_or(line.len(), |offset| at + offset);
        let word = &line[at..end];
        if word == "{" || word == "}" {
            tokens.push(at, end, TokenKind::Punctuation);
        } else if word.starts_with('@') {
            tokens.push(at, end, TokenKind::Reference);
        } else if word.contains('{') && word.contains('}') {
            let base = if first { TokenKind::Key } else { TokenKind::Value };
            push_text(&mut tokens, line, at, end, base);
        } else if first {
            tokens.push(at, end, TokenKind::Key);
        } else if is_literal(word) {
            tokens.push(at, end, TokenKind::Literal);
        } else {
            tokens.push(at, end, TokenKind::Value);
        }
        at = end;
        first = false;
    }
    tokens.finish()
}

/// Pushes `start..end` as `base`, with each placeholder in it (`{env.VAR}`,
/// `{$VAR}`) as an interpolation.
fn push_text(tokens: &mut Tokens, line: &str, start: usize, end: usize, base: TokenKind) {
    let bytes = line.as_bytes();
    let mut plain = start;
    let mut at = start;
    while at < end {
        if bytes[at] == b'{'
            && let Some(offset) = line[at..end].find('}')
        {
            let close = at + offset + 1;
            tokens.push(plain, at, base);
            tokens.push(at, close, TokenKind::Interpolation);
            plain = close;
            at = close;
            continue;
        }
        at += 1;
    }
    tokens.push(plain, end, base);
}

/// The index just past the quoted string that opens at `at`, or the end of
/// `bytes` when it is not closed there. Double quotes honor backslashes.
fn quoted_end(bytes: &[u8], at: usize) -> usize {
    let quote = bytes[at];
    let mut at = at + 1;
    while at < bytes.len() {
        if quote == b'"' && bytes[at] == b'\\' {
            at += 2;
            continue;
        }
        if bytes[at] == quote {
            return at + 1;
        }
        at += 1;
    }
    bytes.len()
}

/// Whether a bare word is a number or a boolean.
fn is_literal(word: &str) -> bool {
    matches!(word, "true" | "false")
        || (word.starts_with(|c: char| c.is_ascii_digit() || matches!(c, '-' | '+' | '.'))
            && word.parse::<f64>().is_ok())
}

/// The byte ranges of the quoted strings in `source` that run over lines.
fn string_ranges(source: &str) -> Vec<Range<usize>> {
    let bytes = source.as_bytes();
    let mut ranges = Vec::new();
    let mut at = 0;
    let mut after_space = true;
    while at < bytes.len() {
        let byte = bytes[at];
        if byte == b'#' && after_space {
            at = bytes[at..]
                .iter()
                .position(|&b| b == b'\n')
                .map_or(bytes.len(), |offset| at + offset);
            continue;
        }
        if (byte == b'"' || byte == b'`') && after_space {
            let end = quoted_end(bytes, at);
            if source[at..end].contains('\n') {
                ranges.push(at..end);
            }
            at = end;
            after_space = false;
            continue;
        }
        after_space = byte.is_ascii_whitespace();
        at += 1;
    }
    ranges
}

/// How an accepted directive is written, as its schema documents it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Template {
    /// The name alone: `file_server` serves with no arguments.
    Bare,
    /// A block: `handle {`, the caret on an indented line, `}` below.
    Block,
    /// A free argument, written as a placeholder: `reverse_proxy {upstream}`.
    Placeholder(&'static str),
    /// The value the schema documents for the directive, its `Default:`
    /// note or else its first `enum` value: `encode gzip`.
    Documented,
    /// A block holding one subdirective written with its documented value:
    /// `log {` over `level INFO`, or `try_files {files} {` over `policy
    /// first_exist`. `arguments` is what the head writes before its `{`.
    Child {
        arguments: &'static str,
        child: &'static str,
    },
}

/// The template of each directive written in full when accepted. A
/// directive not listed is written by its schema's shape (see `write_insert`).
fn template(key: &str) -> Option<Template> {
    Some(match key {
        "file_server" => Template::Bare,
        "handle" | "handle_path" | "handle_errors" | "route" => Template::Block,
        "reverse_proxy" => Template::Placeholder("upstream"),
        "tls" => Template::Placeholder("email"),
        "encode" => Template::Documented,
        "log" => Template::Child {
            arguments: "",
            child: "level",
        },
        "try_files" => Template::Child {
            arguments: "{files}",
            child: "policy",
        },
        _ => return None,
    })
}

/// The schema of a directive's block: `$defs/block/properties/<key>`.
fn directive_schema(key: &str) -> Option<&'static Value> {
    static SCHEMA: OnceLock<Value> = OnceLock::new();
    SCHEMA
        .get_or_init(|| serde_json::from_str(CADDY_SCHEMA).expect("the Caddyfile schema is JSON"))
        .pointer(&format!("/$defs/block/properties/{key}"))
}

/// The first `enum` value in a schema, in document order.
fn first_enum(schema: &Value) -> Option<&Value> {
    match schema {
        Value::Object(map) => match map.get("enum") {
            Some(Value::Array(values)) => values.first(),
            _ => map.values().find_map(first_enum),
        },
        Value::Array(items) => items.iter().find_map(first_enum),
        _ => None,
    }
}

/// The text a `template` writes for `key`, and the caret's UTF-16 offset in
/// it. `None` when the schema lacks what the template writes (no enum).
fn templated(key: &str, template: Template, context: &InsertContext<'_>) -> Option<(String, usize)> {
    let caret = |text: &str| text.encode_utf16().count();
    match template {
        Template::Bare => Some((key.to_string(), caret(key))),
        Template::Block => Some(block_insert(key, context)),
        Template::Placeholder(name) => {
            let head = format!("{key} {{");
            Some((format!("{head}{name}}}"), caret(&head)))
        }
        Template::Documented => {
            let value = documented(directive_schema(key)?)?;
            let head = format!("{key} ");
            Some((format!("{head}{value}"), caret(&head)))
        }
        Template::Child { arguments, child } => child_block(key, arguments, child, context),
    }
}

/// The value a schema documents: the word after its `Default: ` note in
/// the description, else its first `enum` value.
fn documented(schema: &Value) -> Option<String> {
    const NOTE: &str = "Default: ";
    if let Some(text) = schema.get("description").and_then(Value::as_str) {
        if let Some(at) = text.find(NOTE) {
            let word = text[at + NOTE.len()..].split_whitespace().next()?;
            return Some(word.trim_end_matches('.').to_string());
        }
    }
    first_enum(schema)?.as_str().map(str::to_string)
}

/// The property `name` of a schema, looked up through its `anyOf` and
/// `oneOf` branches as well (a directive that takes a block is one).
fn find_property<'a>(schema: &'a Value, name: &str) -> Option<&'a Value> {
    if let Some(property) = schema.get("properties").and_then(|map| map.get(name)) {
        return Some(property);
    }
    ["anyOf", "oneOf"]
        .iter()
        .filter_map(|keyword| schema.get(*keyword)?.as_array())
        .flatten()
        .find_map(|branch| find_property(branch, name))
}

/// A block of `key` holding `child` written with its documented value, the
/// caret after the child's name on its indented line and `}` below.
fn child_block(
    key: &str,
    arguments: &str,
    child: &str,
    context: &InsertContext<'_>,
) -> Option<(String, usize)> {
    let value = documented(find_property(directive_schema(key)?, child)?)?;
    let head = if arguments.is_empty() {
        format!("{key} {{")
    } else {
        format!("{key} {arguments} {{")
    };
    let inner = format!("{}{}{child} ", context.base_indent, context.unit);
    let caret = format!("{head}\n{inner}").encode_utf16().count();
    Some((
        format!("{head}\n{inner}{value}\n{}}}", context.base_indent),
        caret,
    ))
}

/// A block opened on the key's line, its caret on an indented line below
/// and the closing `}` on a line of its own.
fn block_insert(key: &str, context: &InsertContext<'_>) -> (String, usize) {
    let head = format!("{key} {{\n{}{}", context.base_indent, context.unit);
    let caret = head.encode_utf16().count();
    (format!("{head}\n{}}}", context.base_indent), caret)
}

/// Writes the directives of `map` at `depth`, one per line.
fn write_block(out: &mut String, map: &Map<String, Value>, depth: usize) -> Result<(), String> {
    for (name, value) in map {
        write_directive(out, name, value, depth)?;
    }
    Ok(())
}

/// Writes one directive. An array is its arguments on one line; an object
/// is a block, its `args` the arguments on the opening line and its other
/// keys the directives inside; a scalar is one argument.
fn write_directive(out: &mut String, name: &str, value: &Value, depth: usize) -> Result<(), String> {
    if name.is_empty() || name.contains(char::is_whitespace) || name.contains(['{', '}', '"', '`', '#']) {
        return Err(format!("{name:?} is not a directive name a Caddyfile can write"));
    }
    let indent = "    ".repeat(depth);
    match value {
        Value::Object(map) => {
            let mut head = name.to_string();
            if let Some(args) = map.get("args") {
                head.push_str(&arguments(name, args)?);
            }
            out.push_str(&format!("{indent}{head} {{\n"));
            let mut children = map.clone();
            children.remove("args");
            write_block(out, &children, depth + 1)?;
            out.push_str(&format!("{indent}}}\n"));
        }
        other => {
            let args = arguments(name, other)?;
            out.push_str(&format!("{indent}{name}{args}\n"));
        }
    }
    Ok(())
}

/// The arguments of a directive as text, each with a leading space.
fn arguments(name: &str, value: &Value) -> Result<String, String> {
    match value {
        Value::Array(items) => items
            .iter()
            .map(|item| Ok(format!(" {}", argument(name, item)?)))
            .collect(),
        Value::Null | Value::Object(_) => Err(format!(
            "{name}: this value cannot be written as the arguments of a directive"
        )),
        scalar => Ok(format!(" {}", argument(name, scalar)?)),
    }
}

/// One argument as it is written: a string (quoted when it must be), a
/// number or a boolean as its text.
fn argument(name: &str, value: &Value) -> Result<String, String> {
    match value {
        Value::String(text) => Ok(word(text)),
        Value::Number(_) | Value::Bool(_) => Ok(value.to_string()),
        other => Err(format!("{name}: {other} is not an argument a Caddyfile can write")),
    }
}

/// A word as the Caddyfile reads it: bare, or quoted with backticks (which
/// keep every character) or double quotes when a backtick is in it.
fn word(text: &str) -> String {
    let needs_quotes = text.is_empty()
        || text.starts_with('#')
        || text == "{"
        || text == "}"
        || text.chars().any(|c| c.is_whitespace() || c == '"' || c == '`');
    if !needs_quotes {
        return text.to_string();
    }
    if text.contains('`') {
        format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        format!("`{text}`")
    }
}

/// The Caddyfile language.
pub const CADDY: Language = Language::new(&Caddyfile);

/// The JSON Schema of the Caddyfile data model: the global options at the
/// top, each site address a block of directives. Directive docs and the
/// `x-docs-url` links follow the Caddy documentation.
pub const CADDY_SCHEMA: &str = r##"{
  "type": "object",
  "description": "A Caddyfile: an optional global options block at the top, then one block per site address. A site's directives are the keys of its block.",
  "properties": {
    "email": {
      "description": "Sets the email address used when creating an ACME account with the CA.",
      "x-docs-url": "https://caddyserver.com/docs/caddyfile/options#email",
      "type": "array",
      "items": { "type": "string" }
    },
    "acme_ca": {
      "description": "Sets the ACME CA directory URL, such as Let's Encrypt's staging endpoint for testing.",
      "x-docs-url": "https://caddyserver.com/docs/caddyfile/options#acme_ca",
      "type": "array",
      "items": { "type": "string" }
    },
    "auto_https": {
      "description": "Controls automatic certificate management and HTTP-to-HTTPS redirects.",
      "x-docs-url": "https://caddyserver.com/docs/caddyfile/options#auto_https",
      "type": "array",
      "items": {
        "type": "string",
        "enum": ["off", "disable_redirects", "disable_certs", "ignore_loaded_certs"],
        "x-enum-descriptions": [
          "Turns off automatic HTTPS: no certificate management and no redirects.",
          "Keeps certificate management, but turns off HTTP-to-HTTPS redirects.",
          "Turns off certificate management, but keeps the HTTP-to-HTTPS redirects.",
          "Ignores the certificates loaded manually, such as by the tls directive."
        ]
      }
    },
    "admin": {
      "description": "Configures the admin API endpoint, or disables it with `off`. Default: localhost:2019.",
      "x-docs-url": "https://caddyserver.com/docs/caddyfile/options#admin",
      "type": "array",
      "items": {
        "anyOf": [
          { "type": "string", "enum": ["off"], "x-enum-descriptions": ["Disables the admin API."] },
          { "type": "string", "description": "The address the admin API listens on, such as localhost:2019." }
        ]
      }
    },
    "debug": {
      "description": "Sets the default logger to DEBUG level, for troubleshooting. A flag: takes no arguments.",
      "x-docs-url": "https://caddyserver.com/docs/caddyfile/options#debug",
      "type": "array",
      "maxItems": 0
    },
    "servers": {
      "description": "Applies server-level settings (listeners, timeouts, protocols, trusted proxies and more) to the HTTP servers.",
      "x-docs-url": "https://caddyserver.com/docs/caddyfile/options#servers",
      "anyOf": [
        { "type": "array", "items": { "type": "string" } },
        { "type": "object" }
      ]
    },
    "log": {
      "description": "Configures named loggers: where logs are written, their format and level, and which logs to include or exclude.",
      "x-docs-url": "https://caddyserver.com/docs/caddyfile/options#log",
      "anyOf": [
        { "type": "array", "items": { "type": "string" } },
        {
          "type": "object",
          "properties": {
            "output": { "description": "Where logs are written: stderr (default), stdout, discard, file <filename> or net <address>.", "type": "array", "items": { "type": "string" } },
            "format": { "description": "How log entries are encoded. Default: console when stderr is a terminal, otherwise json.", "anyOf": [
              { "type": "array", "items": { "type": "string", "enum": ["console", "json", "filter", "append"], "x-enum-descriptions": [
                "Human-readable output that keeps some structure.",
                "Each log entry is written as a JSON object.",
                "Applies per-field actions such as delete, rename, replace, ip_mask, query, cookie, set_cookie, regexp and hash.",
                "Adds fixed fields to every log entry."
              ] } },
              { "type": "object" }
            ] },
            "level": { "description": "The minimum level of entries to log. Default: INFO.", "type": "array", "items": { "type": "string" } }
          }
        }
      ]
    },
    "order": {
      "description": "Assigns the execution order of HTTP handler directives: first, last, before or after another directive.",
      "x-docs-url": "https://caddyserver.com/docs/caddyfile/options#order",
      "type": "array",
      "items": { "type": "string" }
    },
    "http_port": {
      "description": "Sets the port Caddy uses internally for HTTP, without changing the port clients connect to. Default: 80.",
      "x-docs-url": "https://caddyserver.com/docs/caddyfile/options#http_port",
      "type": "array",
      "items": { "type": "string" }
    },
    "https_port": {
      "description": "Sets the port Caddy uses internally for HTTPS, without changing the port clients connect to. Default: 443.",
      "x-docs-url": "https://caddyserver.com/docs/caddyfile/options#https_port",
      "type": "array",
      "items": { "type": "string" }
    },
    "default_bind": {
      "description": "Sets the default bind address or addresses for sites that don't use the bind directive.",
      "x-docs-url": "https://caddyserver.com/docs/caddyfile/options#default_bind",
      "type": "array",
      "items": { "type": "string" }
    },
    "default_sni": {
      "description": "Sets a default TLS ServerName for clients that don't send SNI.",
      "x-docs-url": "https://caddyserver.com/docs/caddyfile/options#default_sni",
      "type": "array",
      "items": { "type": "string" }
    },
    "storage": {
      "description": "Configures the storage module for certificates and keys. Default: file_system.",
      "x-docs-url": "https://caddyserver.com/docs/caddyfile/options#storage",
      "anyOf": [
        { "type": "array", "items": { "type": "string" } },
        { "type": "object" }
      ]
    },
    "grace_period": {
      "description": "Sets how long servers wait for active connections to finish during shutdown or config changes. Default: unlimited.",
      "x-docs-url": "https://caddyserver.com/docs/caddyfile/options#grace_period",
      "type": "array",
      "items": { "type": "string" }
    },
    "shutdown_delay": {
      "description": "Sets a delay before the grace period, during which a stopping server keeps running and exposes shutdown placeholders for health checks.",
      "x-docs-url": "https://caddyserver.com/docs/caddyfile/options#shutdown_delay",
      "type": "array",
      "items": { "type": "string" }
    },
    "metrics": {
      "description": "Enables metrics collection, with the per_host, observe_catchall_hosts and otlp sub-options.",
      "x-docs-url": "https://caddyserver.com/docs/caddyfile/options#metrics",
      "anyOf": [
        { "type": "array", "items": { "type": "string" } },
        { "type": "object" }
      ]
    },
    "local_certs": {
      "description": "Issues all certificates internally instead of through a public ACME CA.",
      "x-docs-url": "https://caddyserver.com/docs/caddyfile/options#local_certs",
      "type": "array",
      "maxItems": 0
    },
    "skip_install_trust": {
      "description": "Skips installing the local CA root into the system and browser trust stores.",
      "x-docs-url": "https://caddyserver.com/docs/caddyfile/options#skip_install_trust",
      "type": "array",
      "maxItems": 0
    },
    "acme_ca_root": {
      "description": "Sets a PEM file with a trusted root certificate for ACME CA endpoints.",
      "x-docs-url": "https://caddyserver.com/docs/caddyfile/options#acme_ca_root",
      "type": "array",
      "items": { "type": "string" }
    },
    "acme_eab": {
      "description": "Sets External Account Binding credentials for ACME transactions, with the key_id and mac_key sub-options.",
      "x-docs-url": "https://caddyserver.com/docs/caddyfile/options#acme_eab",
      "anyOf": [
        { "type": "array", "items": { "type": "string" } },
        { "type": "object" }
      ]
    },
    "on_demand_tls": {
      "description": "Configures On-Demand TLS permissions without enabling it, with the ask and permission sub-options.",
      "x-docs-url": "https://caddyserver.com/docs/caddyfile/options#on_demand_tls",
      "anyOf": [
        { "type": "array", "items": { "type": "string" } },
        { "type": "object" }
      ]
    },
    "key_type": {
      "description": "Sets the key type for generated TLS certificates: ed25519, p256, p384, rsa2048 or rsa4096.",
      "x-docs-url": "https://caddyserver.com/docs/caddyfile/options#key_type",
      "type": "array",
      "items": { "type": "string" }
    }
  },
  "additionalProperties": { "$ref": "#/$defs/block" },
  "$defs": {
    "block": {
      "type": "object",
      "description": "The directives of a site, or of a directive's block.",
      "properties": {
        "abort": {
          "description": "Aborts the HTTP request: the connection is closed without writing a response. Takes an optional matcher.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/abort",
          "type": "array", "items": { "type": "string" }
        },
        "basic_auth": {
          "description": "Enforces HTTP Basic Authentication. Takes an optional matcher, a hash algorithm (bcrypt by default; argon2id must be named) and a realm. The block lists username and hashed password pairs; hash passwords with `caddy hash-password`.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/basic_auth",
          "anyOf": [
            { "type": "array", "items": { "type": "string" } },
            { "type": "object" }
          ]
        },
        "encode": {
          "description": "Encodes (usually compresses) responses. The formats are gzip and zstd; with none given, both are enabled and zstd is preferred.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/encode",
          "anyOf": [
            {
              "type": "array",
              "items": {
                "anyOf": [
                  { "type": "string", "enum": ["gzip", "zstd"], "x-enum-descriptions": [
                    "Gzip compression, optionally at a level: gzip [<level>].",
                    "Zstandard compression, optionally at a level: default, fastest, better or best."
                  ] },
                  { "type": "string", "description": "A named matcher, such as @static." }
                ]
              }
            },
            {
              "type": "object",
              "properties": {
                "gzip": { "description": "Enables gzip compression, optionally at a level.", "type": "array", "items": { "type": "string" } },
                "zstd": { "description": "Enables zstandard compression, optionally at a level: default, fastest, better or best.", "type": "array", "items": { "type": "string" } },
                "minimum_length": { "description": "The minimum response size in bytes before encoding applies. Default: 512.", "type": "array", "items": { "type": "string" } },
                "match": { "description": "Limits encoding to responses that match an inline response matcher.", "type": "array", "items": { "type": "string" } }
              }
            }
          ]
        },
        "error": {
          "description": "Triggers an error, with an optional status code and message.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/error",
          "type": "array", "items": { "type": "string" }
        },
        "file_server": {
          "description": "Serves files from disk, from the site root. Its browse, hide, index, precompressed and status subdirectives control directory listings and file lookup.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/file_server",
          "anyOf": [
            {
              "type": "array",
              "items": {
                "anyOf": [
                  { "type": "string", "enum": ["browse"], "x-enum-descriptions": ["Enables directory listings for directories without an index file."] },
                  { "type": "string", "description": "A named matcher, such as @static." }
                ]
              }
            },
            {
              "type": "object",
              "properties": {
                "fs": { "description": "The file system to use for file I/O.", "type": "array", "items": { "type": "string" } },
                "root": { "description": "The path to serve files from.", "type": "array", "items": { "type": "string" } },
                "hide": { "description": "Hides the listed files or folders, so they appear not to exist. Glob patterns allowed.", "type": "array", "items": { "type": "string" } },
                "index": { "description": "The filenames checked as index files. Default: index.html index.txt.", "type": "array", "items": { "type": "string" } },
                "browse": { "description": "Enables directory listings, with optional sort and file_limit subdirectives.", "anyOf": [{ "type": "array", "items": { "type": "string" } }, { "type": "object" }] },
                "precompressed": { "description": "Serves precompressed sidecar files: br, zstd and gzip, in that order by default.", "type": "array", "items": { "type": "string" } },
                "status": { "description": "Overrides the response status code, such as 404.", "type": "array", "items": { "type": "string" } },
                "disable_canonical_uris": { "description": "Stops redirecting to canonical URIs (such as adding a trailing slash to directories).", "type": "array", "items": { "type": "string" } },
                "pass_thru": { "description": "Passes the request to the next handler when the file is missing, rather than returning 404.", "type": "array", "items": { "type": "string" } }
              }
            }
          ]
        },
        "handle": {
          "description": "A mutually-exclusive group of directives: the first handle block whose matcher matches runs, and the rest are skipped.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/handle",
          "anyOf": [
            { "type": "array", "items": { "type": "string" } },
            { "$ref": "#/$defs/block" }
          ]
        },
        "handle_path": {
          "description": "Like handle, but strips the matched path prefix from the request URI before its directives run.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/handle_path",
          "anyOf": [
            { "type": "array", "items": { "type": "string" } },
            { "$ref": "#/$defs/block" }
          ]
        },
        "header": {
          "description": "Sets, adds, defaults, replaces or removes response headers: `Name value` sets, `+Name value` adds, `-Name` removes, `?Name value` sets only when absent, `>Name value` defers.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/header",
          "anyOf": [
            { "type": "array", "items": { "type": "string" } },
            { "$ref": "#/$defs/block" }
          ]
        },
        "import": {
          "description": "Includes snippets or files in place: a named snippet declared as `(name) { ... }`, or the contents of a file.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/import",
          "type": "array", "items": { "type": "string" }
        },
        "log": {
          "description": "Enables access logging for the site, with an optional logger name and output, format, level and hostnames subdirectives.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/log",
          "anyOf": [
            { "type": "array", "items": { "type": "string" } },
            {
              "type": "object",
              "properties": {
                "output": { "description": "Where logs are written: stderr (default), stdout, discard, file <filename> or net <address>.", "type": "array", "items": { "type": "string" } },
                "format": { "description": "How log entries are encoded. Default: console when stderr is a terminal, otherwise json.", "anyOf": [
                  { "type": "array", "items": { "type": "string", "enum": ["console", "json", "filter", "append"], "x-enum-descriptions": [
                    "Human-readable output that keeps some structure.",
                    "Each log entry is written as a JSON object.",
                    "Applies per-field actions such as delete, rename, replace, ip_mask, query, cookie, set_cookie, regexp and hash.",
                    "Adds fixed fields to every log entry."
                  ] } },
                  { "type": "object" }
                ] },
                "level": { "description": "The minimum level of entries to log. Default: INFO.", "type": "array", "items": { "type": "string" } },
                "hostnames": { "description": "Logs only requests for these hostnames.", "type": "array", "items": { "type": "string" } },
                "no_hostname": { "description": "Stops the log from being keyed by hostname.", "type": "array", "items": { "type": "string" } },
                "sampling": { "description": "Samples the log entries: interval, first and thereafter.", "type": "object" }
              }
            }
          ]
        },
        "map": {
          "description": "Maps an input value (usually a placeholder such as {host}) to one or more outputs, which become placeholders.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/map",
          "anyOf": [
            { "type": "array", "items": { "type": "string" } },
            { "$ref": "#/$defs/block" }
          ]
        },
        "php_fastcgi": {
          "description": "Serves PHP sites over FastCGI, forwarding to one or more PHP-FPM gateways, with rewrite rules for front controllers.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/php_fastcgi",
          "anyOf": [
            { "type": "array", "items": { "type": "string" } },
            {
              "type": "object",
              "properties": {
                "root": { "description": "The site root folder. Default: the global root, or the working directory.", "type": "array", "items": { "type": "string" } },
                "split": { "description": "Splits the URI into the script name and PATH_INFO. Default: .php.", "type": "array", "items": { "type": "string" } },
                "index": { "description": "The directory index filename (default index.php), or off.", "type": "array", "items": { "type": "string" } },
                "try_files": { "description": "Overrides the default rewrite rules. Default: {path} {path}/index.php index.php.", "type": "array", "items": { "type": "string" } },
                "env": { "description": "An extra environment variable: env <key> <value>. Can be repeated.", "type": "array", "items": { "type": "string" } },
                "resolve_root_symlink": { "description": "Resolves the root path if it is a symlink.", "type": "array", "items": { "type": "string" } },
                "capture_stderr": { "description": "Logs the upstream stderr output.", "type": "array", "items": { "type": "string" } },
                "dial_timeout": { "description": "Limits how long connecting to the upstream may take. Default: 3s.", "type": "array", "items": { "type": "string" } },
                "read_timeout": { "description": "Limits how long reading from the upstream may take. Default: no timeout.", "type": "array", "items": { "type": "string" } },
                "write_timeout": { "description": "Limits how long sending to the upstream may take. Default: no timeout.", "type": "array", "items": { "type": "string" } }
              }
            }
          ]
        },
        "redir": {
          "description": "Issues an HTTP redirect to the client, to a target location, with a status code (temporary, 302, by default).",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/redir",
          "type": "array",
          "items": {
            "anyOf": [
              { "type": "string", "enum": ["temporary", "permanent", "html"], "x-enum-descriptions": [
                "Redirects with 302 Found. The default.",
                "Redirects with 301 Moved Permanently.",
                "Redirects with an HTML document: works for browsers, not API clients."
              ] },
              { "type": "string", "description": "The target location, or a status code such as 308." }
            ]
          }
        },
        "request_body": {
          "description": "Manipulates the request body, such as limiting its size with max_size.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/request_body",
          "anyOf": [
            { "type": "array", "items": { "type": "string" } },
            { "$ref": "#/$defs/block" }
          ]
        },
        "respond": {
          "description": "Writes a hard-coded response to the client: a body and an optional status code, with an optional matcher.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/respond",
          "type": "array", "items": { "type": "string" }
        },
        "reverse_proxy": {
          "description": "A powerful and extensible reverse proxy: forwards requests to one or more upstreams, with load balancing, health checks and transport options.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/reverse_proxy",
          "anyOf": [
            { "type": "array", "items": { "type": "string" } },
            {
              "type": "object",
              "properties": {
                "to": { "description": "Lists upstream addresses, one or more per line, instead of inline.", "type": "array", "items": { "type": "string" } },
                "dynamic": { "description": "Gets the upstream list at request time, such as from DNS a or srv records.", "type": "array", "items": { "type": "string" } },
                "lb_policy": {
                  "description": "The load-balancing policy, such as random, round_robin, least_conn or header. Default: random.",
                  "type": "array",
                  "items": {
                    "anyOf": [
                      { "type": "string", "enum": ["random", "first", "round_robin", "least_conn", "ip_hash", "client_ip_hash", "uri_hash"], "x-enum-descriptions": [
                        "Randomly chooses an upstream.",
                        "Chooses the first available upstream, in the order they are defined in the config.",
                        "Iterates each upstream in turn.",
                        "Chooses the upstream with the fewest current requests.",
                        "Maps the remote IP (the immediate peer) to a sticky upstream.",
                        "Maps the client IP to a sticky upstream.",
                        "Maps the request URI (path and query) to a sticky upstream."
                      ] },
                      { "type": "string", "description": "An argument of a policy that takes one, or the name of one with arguments: header <field>, cookie [<name> [<secret>]], query [<key>], random_choose <n>, weighted_round_robin <weights>." }
                    ]
                  }
                },
                "lb_retries": { "description": "How many times to retry another upstream when one is unavailable. Default: off.", "type": "array", "items": { "type": "string" } },
                "lb_try_duration": { "description": "How long to keep trying to find an available upstream before failing.", "type": "array", "items": { "type": "string" } },
                "health_uri": { "description": "The path used for active health checks, which run in the background.", "type": "array", "items": { "type": "string" } },
                "health_interval": { "description": "How often active health checks run. Default: 30s.", "type": "array", "items": { "type": "string" } },
                "fail_duration": { "description": "Enables passive health checks: how long a failed request is remembered.", "type": "array", "items": { "type": "string" } },
                "transport": { "description": "How Caddy talks to the backend, most commonly `transport http`.", "type": "array", "items": { "type": "string" } },
                "header_up": { "description": "Sets (name value), adds (+name value) or deletes (-name) a request header sent to the backend.", "type": "array", "items": { "type": "string" } },
                "header_down": { "description": "Sets, adds or deletes a response header sent back to the client.", "type": "array", "items": { "type": "string" } },
                "rewrite": { "description": "Changes the URI of the proxied request.", "type": "array", "items": { "type": "string" } },
                "flush_interval": { "description": "How often buffered response data is flushed to the client.", "type": "array", "items": { "type": "string" } },
                "handle_response": { "description": "Intercepts backend responses that match a matcher and handles them with a custom block.", "type": "array", "items": { "type": "string" } }
              }
            }
          ]
        },
        "rewrite": {
          "description": "Rewrites the request URI internally, without redirecting the client.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/rewrite",
          "type": "array", "items": { "type": "string" }
        },
        "root": {
          "description": "Sets the path to the site root: the folder files are served from. Takes an optional matcher.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/root",
          "type": "array", "items": { "type": "string" }
        },
        "route": {
          "description": "A group of directives treated literally as a single unit, in the order written, without the usual reordering.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/route",
          "anyOf": [
            { "type": "array", "items": { "type": "string" } },
            { "$ref": "#/$defs/block" }
          ]
        },
        "templates": {
          "description": "Executes the response body as a Go template, with optional mime types limiting which responses are processed.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/templates",
          "anyOf": [
            { "type": "array", "items": { "type": "string" } },
            { "$ref": "#/$defs/block" }
          ]
        },
        "try_files": {
          "description": "Rewrites the request to the first of the listed files that exists, relative to the site root. A policy subdirective chooses how the file is picked.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/try_files",
          "anyOf": [
            { "type": "array", "items": { "type": "string" } },
            {
              "type": "object",
              "properties": {
                "policy": {
                  "description": "How the file is chosen from the list. Default: first_exist.",
                  "type": "array",
                  "items": {
                    "type": "string",
                    "enum": ["first_exist", "first_exist_fallback", "smallest_size", "largest_size", "most_recently_modified"],
                    "x-enum-descriptions": [
                      "Rewrites to the first existing file; no rewrite if none exists.",
                      "Always rewrites to the last file in the list when nothing else matches.",
                      "A file-size policy named in the syntax; the docs give no further detail.",
                      "A file-size policy named in the syntax; the docs give no further detail.",
                      "Chooses the most recently modified matching file, such as a hashed index.<hash>.html for index.html."
                    ]
                  }
                }
              }
            }
          ]
        },
        "tls": {
          "description": "Customizes TLS for the site: `internal` for a local CA, an ACME account email, or a certificate and key file, plus a block of advanced options.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/tls",
          "anyOf": [
            {
              "type": "array",
              "items": {
                "anyOf": [
                  { "type": "string", "enum": ["internal", "force_automate"], "x-enum-descriptions": [
                    "Issues certificates from Caddy's internal, locally trusted CA.",
                    "Forces Caddy to automate certificates for the site, even if other managed certificates apply."
                  ] },
                  { "type": "string", "description": "An ACME account email address, or a certificate file followed by its key file." }
                ]
              }
            },
            {
              "type": "object",
              "properties": {
                "protocols": { "description": "The minimum and maximum TLS versions.", "type": "array", "items": { "type": "string" } },
                "ciphers": { "description": "The cipher suites, in preference order.", "type": "array", "items": { "type": "string" } },
                "curves": { "description": "The elliptic curve groups to support.", "type": "array", "items": { "type": "string" } },
                "alpn": { "description": "The ALPN values advertised in the handshake.", "type": "array", "items": { "type": "string" } },
                "load": { "description": "Loads certificate and key bundles from folders.", "type": "array", "items": { "type": "string" } },
                "ca": { "description": "Changes the ACME CA endpoint.", "type": "array", "items": { "type": "string" } },
                "ca_root": { "description": "A root PEM for the ACME CA, if it is not in the system trust store.", "type": "array", "items": { "type": "string" } },
                "key_type": { "description": "The key type used for generated keys and CSRs.", "type": "array", "items": { "type": "string" } },
                "dns": { "description": "Enables the DNS challenge with a named DNS provider plugin.", "type": "array", "items": { "type": "string" } },
                "propagation_timeout": { "description": "The maximum wait for DNS TXT records to appear.", "type": "array", "items": { "type": "string" } },
                "propagation_delay": { "description": "How long to wait before starting DNS propagation checks.", "type": "array", "items": { "type": "string" } },
                "dns_ttl": { "description": "The TTL of the DNS challenge TXT record.", "type": "array", "items": { "type": "string" } },
                "resolvers": { "description": "The DNS resolvers used for the DNS challenge.", "type": "array", "items": { "type": "string" } },
                "eab": { "description": "External account binding, with a key ID and MAC key.", "type": "array", "items": { "type": "string" } },
                "on_demand": { "description": "Enables On-Demand TLS for the site's hostnames.", "type": "array", "items": { "type": "string" } },
                "reuse_private_keys": { "description": "Reuses private keys when certificates are renewed.", "type": "array", "items": { "type": "string" } },
                "client_auth": { "description": "TLS client authentication, with the subdirectives mode, trust_pool and verifier.", "anyOf": [{ "type": "array", "items": { "type": "string" } }, { "type": "object" }] },
                "issuer": { "description": "A custom certificate issuer. Can be repeated for fallback issuers.", "type": "array", "items": { "type": "string" } },
                "get_certificate": { "description": "Gets certificates from a manager module at handshake time.", "type": "array", "items": { "type": "string" } },
                "renewal_window_ratio": { "description": "The share of the remaining lifetime at which renewal is attempted.", "type": "array", "items": { "type": "string" } }
              }
            }
          ]
        },
        "uri": {
          "description": "Manipulates the request URI: strip_prefix, strip_suffix, replace, path_regexp and query operations.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/uri",
          "anyOf": [
            { "type": "array", "items": { "type": "string" } },
            { "$ref": "#/$defs/block" }
          ]
        },
        "acme_server": {
          "description": "Runs an embedded ACME server, which issues certificates to ACME clients.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/acme_server",
          "anyOf": [
            { "type": "array", "items": { "type": "string" } },
            { "$ref": "#/$defs/block" }
          ]
        },
        "bind": {
          "description": "Customizes the server's socket address, such as the interface and port it listens on.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/bind",
          "type": "array", "items": { "type": "string" }
        },
        "forward_auth": {
          "description": "Delegates authentication to an external service: each request is checked there before it reaches the site.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/forward_auth",
          "anyOf": [
            { "type": "array", "items": { "type": "string" } },
            { "$ref": "#/$defs/block" }
          ]
        },
        "fs": {
          "description": "Sets the file system used for file I/O, by the name of a file system declared in the global options.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/fs",
          "type": "array", "items": { "type": "string" }
        },
        "handle_errors": {
          "description": "Defines routes for handling errors: the directives run in place of the normal response when a request fails.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/handle_errors",
          "anyOf": [
            { "type": "array", "items": { "type": "string" } },
            { "$ref": "#/$defs/block" }
          ]
        },
        "intercept": {
          "description": "Intercepts responses written by other handlers, so that they can be replaced or changed.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/intercept",
          "anyOf": [
            { "type": "array", "items": { "type": "string" } },
            { "$ref": "#/$defs/block" }
          ]
        },
        "invoke": {
          "description": "Invokes a named route, as defined by a route block elsewhere in the site.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/invoke",
          "type": "array", "items": { "type": "string" }
        },
        "log_append": {
          "description": "Appends a field to the access log.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/log_append",
          "type": "array", "items": { "type": "string" }
        },
        "log_level": {
          "description": "Overrides the access log level for matched requests.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/log_level",
          "type": "array", "items": { "type": "string" }
        },
        "log_name": {
          "description": "Overrides the name or names of the loggers that matched requests are written to.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/log_name",
          "type": "array", "items": { "type": "string" }
        },
        "log_skip": {
          "description": "Skips access logging for matched requests.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/log_skip",
          "type": "array", "items": { "type": "string" }
        },
        "method": {
          "description": "Changes the HTTP method of the request internally, without redirecting the client.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/method",
          "type": "array", "items": { "type": "string" }
        },
        "metrics": {
          "description": "Configures the Prometheus metrics exposition endpoint.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/metrics",
          "type": "array", "items": { "type": "string" }
        },
        "push": {
          "description": "Pushes content to the client using HTTP/2 server push.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/push",
          "type": "array", "items": { "type": "string" }
        },
        "request_header": {
          "description": "Manipulates request headers: sets, adds or removes them.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/request_header",
          "type": "array", "items": { "type": "string" }
        },
        "timeouts": {
          "description": "Sets the idle read and write timeouts for the site.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/timeouts",
          "type": "array", "items": { "type": "string" }
        },
        "tracing": {
          "description": "Integrates with OpenTelemetry tracing, recording spans for the site's requests.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/tracing",
          "anyOf": [
            { "type": "array", "items": { "type": "string" } },
            { "type": "object" }
          ]
        },
        "vars": {
          "description": "Sets an arbitrary variable (placeholder) for later directives to read: vars <key> <value>.",
          "x-docs-url": "https://caddyserver.com/docs/caddyfile/directives/vars",
          "type": "array", "items": { "type": "string" }
        }
      },
      "patternProperties": {
        "^abort#[0-9]+$": { "$ref": "#/$defs/block/properties/abort" },
        "^basic_auth#[0-9]+$": { "$ref": "#/$defs/block/properties/basic_auth" },
        "^encode#[0-9]+$": { "$ref": "#/$defs/block/properties/encode" },
        "^error#[0-9]+$": { "$ref": "#/$defs/block/properties/error" },
        "^file_server#[0-9]+$": { "$ref": "#/$defs/block/properties/file_server" },
        "^handle#[0-9]+$": { "$ref": "#/$defs/block/properties/handle" },
        "^handle_path#[0-9]+$": { "$ref": "#/$defs/block/properties/handle_path" },
        "^header#[0-9]+$": { "$ref": "#/$defs/block/properties/header" },
        "^import#[0-9]+$": { "$ref": "#/$defs/block/properties/import" },
        "^log#[0-9]+$": { "$ref": "#/$defs/block/properties/log" },
        "^map#[0-9]+$": { "$ref": "#/$defs/block/properties/map" },
        "^php_fastcgi#[0-9]+$": { "$ref": "#/$defs/block/properties/php_fastcgi" },
        "^redir#[0-9]+$": { "$ref": "#/$defs/block/properties/redir" },
        "^request_body#[0-9]+$": { "$ref": "#/$defs/block/properties/request_body" },
        "^respond#[0-9]+$": { "$ref": "#/$defs/block/properties/respond" },
        "^reverse_proxy#[0-9]+$": { "$ref": "#/$defs/block/properties/reverse_proxy" },
        "^rewrite#[0-9]+$": { "$ref": "#/$defs/block/properties/rewrite" },
        "^root#[0-9]+$": { "$ref": "#/$defs/block/properties/root" },
        "^route#[0-9]+$": { "$ref": "#/$defs/block/properties/route" },
        "^templates#[0-9]+$": { "$ref": "#/$defs/block/properties/templates" },
        "^try_files#[0-9]+$": { "$ref": "#/$defs/block/properties/try_files" },
        "^tls#[0-9]+$": { "$ref": "#/$defs/block/properties/tls" },
        "^uri#[0-9]+$": { "$ref": "#/$defs/block/properties/uri" },
        "^acme_server#[0-9]+$": { "$ref": "#/$defs/block/properties/acme_server" },
        "^bind#[0-9]+$": { "$ref": "#/$defs/block/properties/bind" },
        "^forward_auth#[0-9]+$": { "$ref": "#/$defs/block/properties/forward_auth" },
        "^fs#[0-9]+$": { "$ref": "#/$defs/block/properties/fs" },
        "^handle_errors#[0-9]+$": { "$ref": "#/$defs/block/properties/handle_errors" },
        "^intercept#[0-9]+$": { "$ref": "#/$defs/block/properties/intercept" },
        "^invoke#[0-9]+$": { "$ref": "#/$defs/block/properties/invoke" },
        "^log_append#[0-9]+$": { "$ref": "#/$defs/block/properties/log_append" },
        "^log_level#[0-9]+$": { "$ref": "#/$defs/block/properties/log_level" },
        "^log_name#[0-9]+$": { "$ref": "#/$defs/block/properties/log_name" },
        "^log_skip#[0-9]+$": { "$ref": "#/$defs/block/properties/log_skip" },
        "^method#[0-9]+$": { "$ref": "#/$defs/block/properties/method" },
        "^metrics#[0-9]+$": { "$ref": "#/$defs/block/properties/metrics" },
        "^push#[0-9]+$": { "$ref": "#/$defs/block/properties/push" },
        "^request_header#[0-9]+$": { "$ref": "#/$defs/block/properties/request_header" },
        "^timeouts#[0-9]+$": { "$ref": "#/$defs/block/properties/timeouts" },
        "^tracing#[0-9]+$": { "$ref": "#/$defs/block/properties/tracing" },
        "^vars#[0-9]+$": { "$ref": "#/$defs/block/properties/vars" },
        "^@": {
          "description": "A named matcher: `@name { path /x }` or `@name path /x`.",
          "anyOf": [
            { "type": "array", "items": { "type": "string" } },
            { "type": "object" }
          ]
        }
      }
    }
  }
}"##;

#[cfg(test)]
mod tests {
    use super::*;
    use leptos_config_editor::{Schema, TextPosition};

    /// The directives of https://caddyserver.com/docs/caddyfile/directives.
    const DIRECTIVES: &[&str] = &[
        "abort", "acme_server", "basic_auth", "bind", "encode", "error", "file_server",
        "forward_auth", "fs", "handle", "handle_errors", "handle_path", "header", "import",
        "intercept", "invoke", "log", "log_append", "log_level", "log_name", "log_skip", "map",
        "method", "metrics", "php_fastcgi", "push", "redir", "request_body", "request_header",
        "respond", "reverse_proxy", "rewrite", "root", "route", "templates", "timeouts", "tls",
        "tracing", "try_files", "uri", "vars",
    ];

    /// A realistic Caddyfile: global options, a site with a reverse proxy,
    /// compression, a handle block and a named matcher, and a second site.
    const REALISTIC: &str = "{
	email admin@example.com
	servers {
		timeouts {
		}
	}
}

example.com, www.example.com {
	encode gzip zstd
	log {
		output file /var/log/caddy.log
		format json
	}
	handle /api/* {
		reverse_proxy localhost:8080
	}
	@backend path /x
	reverse_proxy @backend localhost:9000
	# a comment
}

:8080 {
	respond \"Hello {env.NAME}\" 200
}
";

    fn schema() -> Schema {
        Schema::compile(CADDY_SCHEMA).expect("the Caddyfile schema compiles")
    }

    fn hover_description(text: &str, line: usize, column: usize) -> (String, Option<String>) {
        let docs = schema()
            .hover(CADDY, text, TextPosition::new(line, column))
            .expect("a directive has docs");
        (docs.key, docs.description)
    }

    #[test]
    fn caddy_hover_names_each_directive_with_its_own_docs() {
        let text = "example.com {\n\treverse_proxy localhost:8080\n\tencode gzip zstd\n}\n";
        let (key, reverse) = hover_description(text, 2, 2);
        assert_eq!(key, "reverse_proxy");
        let (key, encode) = hover_description(text, 3, 2);
        assert_eq!(key, "encode");
        let reverse = reverse.expect("reverse_proxy has a description");
        let encode = encode.expect("encode has a description");
        assert!(reverse.contains("reverse proxy"), "{reverse}");
        assert!(encode.contains("Encodes"), "{encode}");
        assert_ne!(reverse, encode);
    }

    #[test]
    fn caddy_hover_on_an_argument_reads_the_directive_it_belongs_to() {
        // `encode gzip`: the caret on `gzip` (column 9, one-based).
        let text = "example.com {\n\tencode gzip zstd\n}\n";
        let docs = schema()
            .hover(CADDY, text, TextPosition::new(2, 9))
            .expect("the argument hovers its directive");
        assert_eq!(docs.key, "encode");
        // `tls internal`: the value's own description is held by the
        // schema's enum docs, which Schema::hover does not return.
        let text = "example.com {\n\ttls internal\n}\n";
        let docs = schema()
            .hover(CADDY, text, TextPosition::new(2, 6))
            .expect("the argument hovers its directive");
        assert_eq!(docs.key, "tls");
    }

    #[test]
    fn caddy_completion_in_a_site_block_lists_directives_with_descriptions() {
        let text = "example.com {\n\t\n}\n";
        let cursor = "example.com {\n\t".encode_utf16().count();
        let offered = schema().suggestions(CADDY, text, cursor);
        let labels: Vec<&str> = offered.iter().map(|s| s.label()).collect();
        for name in DIRECTIVES {
            assert!(labels.contains(&name), "missing {name} in {labels:?}");
        }
        let reverse = offered
            .iter()
            .find(|s| s.label() == "reverse_proxy")
            .expect("reverse_proxy is offered");
        assert!(reverse.detail().contains("reverse proxy"), "{}", reverse.detail());
        let (inserted, _) = reverse.accept(text);
        assert!(inserted.contains("reverse_proxy "), "{inserted}");
    }

    #[test]
    fn caddy_completion_at_the_top_level_lists_global_options() {
        let offered = schema().suggestions(CADDY, "", 0);
        let labels: Vec<&str> = offered.iter().map(|s| s.label()).collect();
        for name in GLOBAL_OPTIONS {
            assert!(labels.contains(name), "missing {name} in {labels:?}");
        }
    }

    #[test]
    fn caddy_partial_input_never_panics() {
        let samples = [
            "",
            "\n\n",
            "{",
            "}",
            "# only a comment",
            "example.com {",
            "example.com {\n\trespond \"unterminated",
            "example.com {\n\thandle /api/* {\n\t\treverse_proxy \"a b\n",
            "example.com {\n\tencode {env.X}\n",
            "{\n\tservers {\n",
            "example.com { respond x }",
            "@",
            "\t\t\t",
            "a\u{1F600}b.com {\n\tencode \u{1F600} \u{e9}\n\t@m path /\u{fc}\n}\n",
        ];
        let schema = schema();
        for sample in samples {
            let _ = Caddyfile.parse(sample);
            let _ = schema.validate(CADDY, sample);
            for cursor in 0..=sample.encode_utf16().count() {
                let _ = schema.suggestions(CADDY, sample, cursor);
            }
            for (index, line) in sample.split('\n').enumerate() {
                for column in 1..=line.chars().count() + 1 {
                    let _ = schema.hover(CADDY, sample, TextPosition::new(index + 1, column));
                }
            }
        }
        assert!(Caddyfile.parse("").is_ok(), "an empty file is an empty document");
        assert!(Caddyfile.parse("example.com {\n").is_err(), "an unclosed block is an error");
        assert!(Caddyfile.parse("example.com {\n\trespond \"x").is_err());
    }

    #[test]
    fn caddy_completion_after_encode_lists_the_encodings_with_descriptions() {
        let text = "example.com {\n\tencode \n}\n";
        let cursor = "example.com {\n\tencode ".encode_utf16().count();
        let offered = schema().suggestions(CADDY, text, cursor);
        let gzip = offered
            .iter()
            .find(|s| s.label() == "gzip")
            .expect("gzip is offered after encode");
        assert!(gzip.detail().contains("Gzip"), "{}", gzip.detail());
        let zstd = offered
            .iter()
            .find(|s| s.label() == "zstd")
            .expect("zstd is offered after encode");
        assert!(zstd.detail().contains("Zstandard"), "{}", zstd.detail());
    }

    #[test]
    fn caddy_accepting_a_value_inside_open_quotes_replaces_the_content_between_them() {
        let text = "example.com {\n\tencode \"\"\n}\n";
        let cursor = "example.com {\n\tencode \"".encode_utf16().count();
        let offered = schema().suggestions(CADDY, text, cursor);
        let gzip = offered
            .iter()
            .find(|s| s.label() == "gzip")
            .expect("gzip is offered inside the quotes of encode");
        let (written, selection) = gzip.accept(text);
        assert_eq!(written, "example.com {\n\tencode \"gzip\"\n}\n");
        let caret = "example.com {\n\tencode \"gzip\"".encode_utf16().count();
        assert_eq!(selection, caret..caret, "the caret sits after the closing quote");
    }

    /// Accepts the suggestion `label` for the directive `typed` (the caret
    /// after it, on its own line in a site block): the document and the
    /// selection after accepting it.
    fn accept_in_site(typed: &str, label: &str) -> (String, Range<usize>) {
        let text = format!("example.com {{\n\t{typed}\n}}\n");
        let cursor = format!("example.com {{\n\t{typed}").encode_utf16().count();
        let offered = schema().suggestions(CADDY, &text, cursor);
        let suggestion = offered
            .iter()
            .find(|s| s.label() == label)
            .unwrap_or_else(|| panic!("{label} is offered after {typed:?}"));
        suggestion.accept(&text)
    }

    #[test]
    fn caddy_accepting_reverse_proxy_writes_a_placeholder_with_the_caret_in_it() {
        let (text, selection) = accept_in_site("rev", "reverse_proxy");
        assert_eq!(text, "example.com {\n\treverse_proxy {upstream}\n}\n");
        let caret = "example.com {\n\treverse_proxy {".encode_utf16().count();
        assert_eq!(selection, caret..caret, "the caret sits before the placeholder");
    }

    #[test]
    fn caddy_accepting_encode_writes_the_first_documented_encoding() {
        let (text, _) = accept_in_site("enc", "encode");
        assert_eq!(text, "example.com {\n\tencode gzip\n}\n");
    }

    #[test]
    fn caddy_accepting_file_server_writes_the_name_alone() {
        let (text, selection) = accept_in_site("file", "file_server");
        assert_eq!(text, "example.com {\n\tfile_server\n}\n");
        let caret = "example.com {\n\tfile_server".encode_utf16().count();
        assert_eq!(selection, caret..caret);
    }

    #[test]
    fn caddy_accepting_tls_writes_an_email_placeholder() {
        let (text, _) = accept_in_site("tl", "tls");
        assert_eq!(text, "example.com {\n\ttls {email}\n}\n");
    }

    #[test]
    fn caddy_accepting_handle_opens_a_block_with_the_caret_on_the_indented_line() {
        let (text, selection) = accept_in_site("han", "handle");
        assert_eq!(text, "example.com {\n\thandle {\n\t\t\n\t}\n}\n");
        let caret = "example.com {\n\thandle {\n\t\t".encode_utf16().count();
        assert_eq!(selection, caret..caret);
    }

    /// Accepts `label` for the directive `typed` on an indented line of a
    /// site block written with four spaces: the document and the caret.
    fn accept_indented(typed: &str, label: &str) -> (String, usize) {
        let text = format!("example.com {{\n    {typed}\n}}\n");
        let cursor = format!("example.com {{\n    {typed}").encode_utf16().count();
        let offered = schema().suggestions(CADDY, &text, cursor);
        let suggestion = offered
            .iter()
            .find(|s| s.label() == label)
            .unwrap_or_else(|| panic!("{label} is offered after {typed:?}"));
        let (written, selection) = suggestion.accept(&text);
        (written, selection.start)
    }

    #[test]
    fn caddy_accepting_an_indented_encode_keeps_the_indent_and_writes_gzip() {
        let (text, caret) = accept_indented("enc", "encode");
        assert_eq!(text, "example.com {\n    encode gzip\n}\n");
        let before = "example.com {\n    encode ".encode_utf16().count();
        assert_eq!(caret, before, "the caret sits on the value");
    }

    #[test]
    fn caddy_accepting_an_indented_handle_closes_on_the_block_indent() {
        let (text, caret) = accept_indented("han", "handle");
        assert_eq!(text, "example.com {\n    handle {\n        \n    }\n}\n");
        let before = "example.com {\n    handle {\n        ".encode_utf16().count();
        assert_eq!(caret, before, "the caret sits on the indented line");
    }

    #[test]
    fn caddy_accepting_log_writes_its_documented_level() {
        let (text, caret) = accept_indented("lo", "log");
        assert_eq!(text, "example.com {\n    log {\n        level INFO\n    }\n}\n");
        let before = "example.com {\n    log {\n        level ".encode_utf16().count();
        assert_eq!(caret, before, "the caret sits on the level");
    }

    #[test]
    fn caddy_accepting_try_files_writes_its_documented_policy() {
        let (text, caret) = accept_indented("try", "try_files");
        assert_eq!(
            text,
            "example.com {\n    try_files {files} {\n        policy first_exist\n    }\n}\n"
        );
        let before = "example.com {\n    try_files {files} {\n        policy ".encode_utf16().count();
        assert_eq!(caret, before, "the caret sits on the policy");
    }

    #[test]
    fn caddy_accepting_a_global_option_writes_its_name_and_a_space() {
        let text = "em";
        let offered = schema().suggestions(CADDY, text, 2);
        let email = offered
            .iter()
            .find(|s| s.label() == "email")
            .expect("email is offered at the top level");
        let (written, selection) = email.accept(text);
        assert_eq!(written, "email ");
        assert_eq!(selection, 6..6, "the caret after the space");
    }

    #[test]
    fn caddy_templates_follow_the_schema() {
        let root: Value = serde_json::from_str(CADDY_SCHEMA).expect("the schema is JSON");
        let block = &root["$defs"]["block"]["properties"];
        for key in ["file_server", "handle", "handle_path", "handle_errors", "route", "reverse_proxy", "tls", "encode"] {
            assert!(block.get(key).is_some(), "{key} is a documented directive");
        }
        // A block template is a directive whose schema takes a block.
        for key in ["handle", "handle_path", "handle_errors", "route"] {
            assert!(
                block[key].to_string().contains("#/$defs/block"),
                "{key} takes a block in the schema"
            );
        }
        // An enum template's value is one the schema lists.
        let encode = first_enum(&block["encode"]).expect("encode lists an enum");
        assert_eq!(encode, "gzip");
    }

    #[test]
    fn caddy_parses_a_realistic_caddyfile_into_the_schema_model() {
        let value = Caddyfile.parse(REALISTIC).expect("the sample parses");
        assert_eq!(value["email"], serde_json::json!(["admin@example.com"]));
        let site = &value["example.com, www.example.com"];
        assert_eq!(site["encode"], serde_json::json!(["gzip", "zstd"]));
        assert_eq!(
            site["reverse_proxy"],
            serde_json::json!(["@backend", "localhost:9000"])
        );
        assert_eq!(site["@backend"], serde_json::json!(["path", "/x"]));
        assert_eq!(
            site["handle"],
            serde_json::json!({"args": ["/api/*"], "reverse_proxy": ["localhost:8080"]})
        );
        assert_eq!(site["log"]["format"], serde_json::json!(["json"]));
        assert_eq!(value[":8080"]["respond"], serde_json::json!(["Hello {env.NAME}", "200"]));

        let diagnostics = schema().validate(CADDY, REALISTIC);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn caddy_try_files_accepts_every_documented_policy() {
        for policy in [
            "first_exist",
            "first_exist_fallback",
            "smallest_size",
            "largest_size",
            "most_recently_modified",
        ] {
            let text = format!(
                "example.com {{\n\ttry_files {{\n\t\tpolicy {policy}\n\t}}\n}}\n"
            );
            let diagnostics = schema().validate(CADDY, &text);
            assert!(diagnostics.is_empty(), "{policy}: {diagnostics:?}");
        }
        let text = "example.com {\n\ttry_files {\n\t\tpolicy bogus\n\t}\n}\n";
        assert!(!schema().validate(CADDY, text).is_empty(), "an unknown policy is flagged");
    }

    #[test]
    fn caddy_lb_policy_accepts_the_argument_forms() {
        for policy in [
            "header X-Foo",
            "random_choose 2",
            "cookie lb_name",
            "query key",
            "weighted_round_robin 5 1",
            "round_robin",
        ] {
            let text = format!(
                "example.com {{\n\treverse_proxy a:1 b:2 {{\n\t\tlb_policy {policy}\n\t}}\n}}\n"
            );
            let diagnostics = schema().validate(CADDY, &text);
            assert!(diagnostics.is_empty(), "{policy}: {diagnostics:?}");
        }
    }

    #[test]
    fn caddy_lb_policy_enum_descriptions_match_their_values() {
        let root: Value = serde_json::from_str(CADDY_SCHEMA).expect("the schema is JSON");
        let branches = &root["$defs"]["block"]["properties"]["reverse_proxy"]["anyOf"][1]
            ["properties"]["lb_policy"]["items"]["anyOf"];
        let values = branches[0]["enum"].as_array().expect("the policies are an enum");
        let docs = branches[0]["x-enum-descriptions"]
            .as_array()
            .expect("each policy has its docs");
        assert_eq!(values.len(), docs.len());
        let round_robin = values
            .iter()
            .position(|value| value == "round_robin")
            .expect("round_robin is a documented policy");
        assert!(docs[round_robin].as_str().unwrap().contains("upstream in turn"));
    }

    /// Two `reverse_proxy` blocks in one site, each with its own `lb_policy`.
    const TWO_PROXIES: &str = "example.com {\n\treverse_proxy a:1 {\n\t\tlb_policy round_robin\n\t}\n\treverse_proxy c:1 {\n\t\tlb_policy header X-Foo\n\t}\n}\n";

    #[test]
    fn caddy_keeps_every_occurrence_of_a_repeated_directive() {
        let value = Caddyfile.parse(TWO_PROXIES).expect("two proxies parse");
        let site = &value["example.com"];
        assert_eq!(site["reverse_proxy"]["lb_policy"], serde_json::json!(["round_robin"]));
        assert_eq!(site["reverse_proxy#2"]["lb_policy"], serde_json::json!(["header", "X-Foo"]));
        let diagnostics = schema().validate(CADDY, TWO_PROXIES);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn caddy_hovers_and_locates_each_occurrence_of_a_repeated_directive() {
        // The second block's key and its lb_policy value sit on their own lines.
        let policy = |site_key: &str| {
            FieldPath::from(vec![
                Seg::Key("example.com".to_string()),
                Seg::Key(site_key.to_string()),
                Seg::Key("lb_policy".to_string()),
                Seg::ArrayItem(0),
            ])
        };
        let first = Caddyfile
            .value_span(TWO_PROXIES, &policy("reverse_proxy"));
        assert_eq!(first.map(|s| s.line), Some(3));
        let second = Caddyfile
            .value_span(TWO_PROXIES, &policy("reverse_proxy#2"));
        assert_eq!(second.map(|s| s.line), Some(6));

        let (docs_key, description) = {
            let docs = schema()
                .hover(CADDY, TWO_PROXIES, TextPosition::new(5, 2))
                .expect("the second reverse_proxy has docs");
            (docs.key, docs.description)
        };
        assert!(docs_key.starts_with("reverse_proxy"), "{docs_key}");
        assert!(description.unwrap_or_default().contains("reverse proxy"));
    }

    #[test]
    fn caddy_reports_unclosed_blocks_and_one_line_blocks() {
        let error = Caddyfile.parse("example.com {\n\trespond \"x\"\n").unwrap_err();
        assert_eq!(error.line, Some(1));
        let error = Caddyfile
            .parse("example.com {\n\thandle { respond \"x\" }\n}\n")
            .unwrap_err();
        assert_eq!(error.line, Some(2));
    }
}

#[cfg(test)]
mod highlight_and_convert_tests {
    use super::*;
    use leptos_config_editor::language::Text;

    /// The sample of the Edit-as round trip.
    const SAMPLE: &str = "{\n    email admin@example.com\n}\n\nexample.com {\n    encode gzip\n    reverse_proxy localhost:8080\n}\n";

    /// `(text, kind)` of each run of `line`.
    fn runs(line: &str) -> Vec<(&str, TokenKind)> {
        Caddyfile
            .highlight(line)
            .iter()
            .map(|run| (&line[run.start..run.end], run.kind))
            .collect()
    }

    #[test]
    fn a_directive_line_colors_its_name_and_arguments() {
        assert_eq!(
            runs("reverse_proxy localhost:8080"),
            [
                ("reverse_proxy", TokenKind::Key),
                ("localhost:8080", TokenKind::Value)
            ]
        );
        assert_eq!(
            runs("    handle {"),
            [("handle", TokenKind::Key), ("{", TokenKind::Punctuation)]
        );
        assert_eq!(runs("}"), [("}", TokenKind::Punctuation)]);
    }

    #[test]
    fn a_quoted_string_is_a_string_with_its_placeholders_and_a_number_a_literal() {
        assert_eq!(
            runs("respond \"Hello {env.NAME}\" 200"),
            [
                ("respond", TokenKind::Key),
                ("\"Hello ", TokenKind::String),
                ("{env.NAME}", TokenKind::Interpolation),
                ("\"", TokenKind::String),
                ("200", TokenKind::Literal),
            ]
        );
    }

    #[test]
    fn a_hash_comment_runs_to_the_end_of_the_line() {
        assert_eq!(
            runs("encode gzip # note"),
            [
                ("encode", TokenKind::Key),
                ("gzip", TokenKind::Value),
                ("# note", TokenKind::Comment),
            ]
        );
        // A # inside a word is not a comment.
        assert_eq!(runs("uri /a#b"), [("uri", TokenKind::Key), ("/a#b", TokenKind::Value)]);
    }

    #[test]
    fn a_matcher_and_a_placeholder_are_colored_apart() {
        assert_eq!(
            runs("@api path /api/*"),
            [
                ("@api", TokenKind::Reference),
                ("path", TokenKind::Value),
                ("/api/*", TokenKind::Value),
            ]
        );
        assert_eq!(
            runs("redir {$HOST}"),
            [("redir", TokenKind::Key), ("{$HOST}", TokenKind::Interpolation)]
        );
        assert_eq!(runs("debug"), [("debug", TokenKind::Key)]);
        assert_eq!(runs("tls true"), [("tls", TokenKind::Key), ("true", TokenKind::Literal)]);
    }

    #[test]
    fn a_string_over_lines_is_found_and_its_continuation_colored() {
        let source = "respond \"a\nb\" 200\nencode gzip\n";
        let ranges = Caddyfile.multiline_strings(source);
        assert_eq!(ranges, [8..13]);
        assert_eq!(&source[ranges[0].clone()], "\"a\nb\"");
        // The rest of a backtick string opened earlier closes at its quote.
        let rest = Caddyfile.text().highlight_continued(&Caddyfile, "b` 200");
        let colored: Vec<(&str, TokenKind)> = rest
            .iter()
            .map(|run| (&"b` 200"[run.start..run.end], run.kind))
            .collect();
        assert_eq!(colored, [("b`", TokenKind::String), ("200", TokenKind::Literal)]);
    }

    #[test]
    fn the_sample_converts_to_yaml_and_back_unchanged() {
        let yaml = CADDY.convert(SAMPLE, Language::YAML).expect("the sample converts to YAML");
        assert!(yaml.losses.is_empty(), "losses: {:?}", yaml.losses);
        let back = Language::YAML
            .convert(&yaml.text, CADDY)
            .expect("the YAML converts back to a Caddyfile");
        assert_eq!(back.text, SAMPLE);
    }

    #[test]
    fn the_sample_converts_to_json_and_back_unchanged() {
        let json = CADDY.convert(SAMPLE, Language::JSON).expect("the sample converts to JSON");
        let back = Language::JSON
            .convert(&json.text, CADDY)
            .expect("the JSON converts back to a Caddyfile");
        assert_eq!(back.text, SAMPLE);
    }

    #[test]
    fn a_value_the_caddyfile_cannot_hold_is_an_error() {
        let error = Language::JSON
            .convert(r#"{"example.com": {"encode": null}}"#, CADDY)
            .expect_err("null has no Caddyfile form");
        assert!(error.to_string().contains("encode"), "{error}");
        assert!(Language::JSON.convert(r#"{"example.com": "gzip"}"#, CADDY).is_err());
    }
}
