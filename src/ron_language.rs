//! RON (Rusty Object Notation) as a config language.
//!
//! A RON file is one value, usually a named struct such as `AppConfig( ... )`.
//! Its data model is JSON:
//! - a struct or enum variant with named fields (`Server(host: "0.0.0.0")`)
//!   is an object, and one with positional fields, a tuple or `()` is an array;
//! - a list `[...]` is an array, and a map `{ "k": v }` is an object keyed by
//!   its keys (strings, numbers, booleans or names);
//! - `Some(x)` is `x` (the wrapper is not kept) and `None` is `null`;
//! - a bare name, such as a unit variant `Info`, is its name as a string;
//! - strings, raw strings, chars, numbers and booleans are the JSON scalars.
//!
//! Syntax with no JSON form is a parse error: byte strings and bytes, ranges,
//! `inf` and `NaN`, and the `unwrap_newtypes` and `unwrap_variant_newtypes`
//! extensions, which change what a value means. An enum variant with data
//! reads as its data alone, so the variant name is not in the value. Comments
//! are `//` and `/* */`, and block comments nest, as the RON spec says.
//!
//! Completion and hover do not parse the whole file: they scan the tokens
//! before the caret, which keeps working while the text is still broken.
//!
//! Conversion (Edit-as) writes the data back as RON: objects as `(name: v, ..)`
//! (or `{ "k": v }` when a key is not an identifier, and `{}` when empty),
//! arrays as `[..]`, strings quoted, `null` as `None`. The data model has no
//! struct or variant names, so a conversion back drops them: `AppConfig(` and
//! `Server(` come back as `(`, and a unit variant `Info` comes back as the
//! string `"Info"`. That is the same value (a unit variant is its name as a
//! string), but the spelling and the struct names are not kept.
//!
//! Colors: a struct, variant or enum name (`AppConfig`, `Server`, `Info`, any
//! word that starts with a capital or is followed by `(`) is a
//! `TokenKind::Key`, the same as a field name, so a type reads as a heading
//! and not as a value. Field names before `:` are `Key`. Strings, raw strings
//! and chars are `String`; `//` and `/* */` comments are `Comment`; numbers,
//! `true`, `false`, `None` and `Some` are `Literal`; `( ) [ ] { } , :` are
//! `Punctuation`; other words are `Value`. Strings and comments that span
//! lines come from `Text::comments` and `Text::multiline_strings`, which is
//! where the editor reads lines past the first one.

use leptos_config_editor::FieldPath;
use leptos_config_editor::Language;
use leptos_config_editor::language::{
    CompletionContext, ConfigLanguage, Hover, InsertContext, InsertPlan, LanguageConfig,
    LanguageInfo, ParseError, Planned, SchemaPathSegment, Span, Text, TokenKind, Tokens,
    utf16_to_byte_index,
};
use leptos_config_editor::language::Token as Highlight;
use serde_json::{Number, Value};
use std::ops::Range;

/// The RON language, for the editor.
pub const RON: Language = Language::new(&Ron);

/// The RON language's type; see [`RON`].
pub struct Ron;

/// The JSON Schema of the example config: a service's name, version, server,
/// logging and features, as a RON file would hold them.
#[cfg(test)]
pub const RON_SCHEMA: &str = r##"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "title": "AppConfig",
  "description": "A service configuration in RON. The file holds one value, normally a struct written AppConfig( name: ..., ... ). Field names are the keys. The struct's name is not part of the data.",
  "x-docs-url": "https://github.com/ron-rs/ron",
  "type": "object",
  "required": ["name", "version", "server", "log"],
  "additionalProperties": false,
  "properties": {
    "name": {
      "type": "string",
      "title": "Service name",
      "description": "The service's name, as it appears in its logs. A quoted string.",
      "examples": ["billing-api"]
    },
    "version": {
      "type": "string",
      "title": "Version",
      "description": "The release version as a quoted string, such as \"1.4.2\"."
    },
    "server": {
      "type": "object",
      "title": "Server",
      "description": "Where and how the service listens. Written as Server( host: ..., port: ..., tls: ... ).",
      "required": ["host", "port"],
      "additionalProperties": false,
      "properties": {
        "host": {
          "type": "string",
          "title": "Bind address",
          "description": "The address to listen on, as a quoted string. \"0.0.0.0\" listens on every interface.",
          "default": "0.0.0.0"
        },
        "port": {
          "type": "integer",
          "title": "Port",
          "minimum": 1,
          "maximum": 65535,
          "description": "The TCP port to listen on. A number written without quotes.",
          "default": 8080
        },
        "tls": {
          "title": "TLS",
          "description": "None serves plain HTTP. Some(Tls( ... )) serves HTTPS. The Some( ) wrapper is not part of the data, so the value is the Tls struct or null.",
          "anyOf": [
            {
              "type": "null",
              "description": "Plain HTTP, written as None."
            },
            {
              "type": "object",
              "title": "Tls",
              "description": "Certificate and key for HTTPS, written as Tls( cert_path: ..., key_path: ... ).",
              "required": ["cert_path", "key_path"],
              "additionalProperties": false,
              "properties": {
                "cert_path": {
                  "type": "string",
                  "description": "Path to the PEM certificate file, as a quoted string."
                },
                "key_path": {
                  "type": "string",
                  "description": "Path to the PEM private key file, as a quoted string."
                }
              }
            }
          ]
        }
      }
    },
    "log": {
      "type": "object",
      "title": "Log",
      "description": "Logging settings, written as Log( level: ..., targets: [...] ).",
      "required": ["level"],
      "additionalProperties": false,
      "properties": {
        "level": {
          "enum": ["Trace", "Debug", "Info", "Warn", "Error"],
          "x-enum-descriptions": [
            "The most detailed level, for following single steps.",
            "Detail for developers, below normal operation.",
            "Normal operation: start-up, configuration and significant events.",
            "Something unexpected that the service recovered from.",
            "A failure that needs attention."
          ],
          "title": "Level",
          "description": "The least severe message that is logged. Written as a bare name, such as Info.",
          "default": "Info"
        },
        "targets": {
          "type": "array",
          "title": "Targets",
          "description": "Where log lines are written: a list of quoted paths or names, such as [\"stdout\"].",
          "items": {
            "type": "string"
          }
        }
      }
    },
    "features": {
      "type": "array",
      "title": "Features",
      "description": "Optional features to switch on, written as a list of names such as [Metrics, Tracing]. A name may appear once.",
      "uniqueItems": true,
      "items": {
        "enum": ["Metrics", "Tracing", "Profiling"],
        "x-enum-descriptions": [
          "Exports request and queue counters.",
          "Records a trace span for every request.",
          "Samples CPU and memory profiles, at a cost in overhead."
        ]
      }
    }
  }
}"##;

// ---------------------------------------------------------------------------
// Lexing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    /// An identifier, keyword or bare name; a raw `r#name` without its `r#`.
    Ident(String),
    /// A string (plain or raw), with its escapes decoded.
    Str(String),
    /// A char literal, with its escapes decoded.
    Char(char),
    /// A number as written, sign and suffix included.
    Number(String),
    /// One of `( ) [ ] { } , : # !`.
    Punct(char),
    /// A `//` comment, up to the line end.
    LineComment,
    /// A complete `/* */` comment, nested ones included.
    BlockComment,
    /// A string still open at the end of the text.
    OpenStr,
    /// A block comment still open at the end of the text.
    OpenBlock,
    /// A lexical error, which the parser reports.
    Bad(&'static str),
}

#[derive(Debug, Clone)]
struct Token {
    tok: Tok,
    /// Byte offsets of the token in the text.
    start: usize,
    end: usize,
}

const UNTERMINATED_ESCAPE: &str = "unterminated escape";
const BAD_ESCAPE: &str = "unknown or malformed escape";
const NON_FINITE: &str = "infinite and NaN numbers have no JSON form";
const INVALID_NUMBER: &str = "invalid number";
const INTEGER_RANGE: &str = "integer out of range";
const FLOAT_RANGE: &str = "float out of range";

struct Cursor<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn rest(&self) -> &'a str {
        &self.src[self.pos..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn eat(&mut self, want: char) -> bool {
        if self.peek() == Some(want) {
            self.bump();
            true
        } else {
            false
        }
    }
}

/// Splits `src` into tokens. Never fails: errors are `Tok::Bad` tokens, and
/// text cut short by the end is `Tok::OpenStr` or `Tok::OpenBlock`.
fn lex(src: &str) -> Vec<Token> {
    let mut cur = Cursor { src, pos: 0 };
    let mut out = Vec::new();
    while let Some(c) = cur.peek() {
        let start = cur.pos;
        let tok = if c.is_whitespace() {
            cur.bump();
            continue;
        } else if cur.rest().starts_with("//") {
            let end = cur.rest().find('\n').map_or(src.len(), |at| start + at);
            cur.pos = end;
            Tok::LineComment
        } else if cur.rest().starts_with("/*") {
            match block_comment_end(src, start) {
                Some(end) => {
                    cur.pos = end;
                    Tok::BlockComment
                }
                None => {
                    cur.pos = src.len();
                    Tok::OpenBlock
                }
            }
        } else if c == '"' {
            cur.bump();
            lex_string(&mut cur)
        } else if raw_string_ahead(cur.rest()) {
            lex_raw_string(&mut cur)
        } else if raw_ident_ahead(cur.rest()) {
            cur.bump();
            cur.bump();
            Tok::Ident(take_ident(&mut cur))
        } else if (c == 'b' && matches!(cur.rest().chars().nth(1), Some('"' | '\'')))
            || (cur.rest().starts_with("br")
                && matches!(cur.rest().chars().nth(2), Some('"' | '#')))
        {
            // Only the `b` is consumed; the string or char after it lexes next.
            cur.bump();
            Tok::Bad("byte strings and bytes are not supported")
        } else if c.is_alphabetic() || c == '_' {
            Tok::Ident(take_ident(&mut cur))
        } else if c.is_ascii_digit()
            || number_sign_ahead(cur.rest())
            || (c == '.' && cur.rest()[1..].starts_with(|d: char| d.is_ascii_digit()))
        {
            lex_number(&mut cur)
        } else if c == '\'' {
            lex_char(&mut cur)
        } else if "()[]{},:#!".contains(c) {
            cur.bump();
            Tok::Punct(c)
        } else if c == '.' {
            cur.bump();
            if cur.eat('.') {
                cur.eat('=');
            }
            Tok::Bad("ranges are not supported")
        } else {
            cur.bump();
            Tok::Bad("unexpected character")
        };
        out.push(Token {
            tok,
            start,
            end: cur.pos,
        });
    }
    out
}

/// The end of the block comment opening at `at`, nesting counted.
fn block_comment_end(src: &str, at: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = at;
    while i < src.len() {
        let rest = &src[i..];
        if rest.starts_with("/*") {
            depth += 1;
            i += 2;
        } else if rest.starts_with("*/") {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return Some(i);
            }
        } else {
            i += rest.chars().next().map_or(1, char::len_utf8);
        }
    }
    None
}

/// `r"`, or `r`, hashes and a quote.
fn raw_string_ahead(rest: &str) -> bool {
    rest.starts_with('r') && rest[1..].trim_start_matches('#').starts_with('"')
}

/// `r#` and then an identifier start.
fn raw_ident_ahead(rest: &str) -> bool {
    rest.starts_with("r#")
        && rest[2..]
            .chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
}

/// A sign and then a digit, `.`, `inf` or `NaN`.
fn number_sign_ahead(rest: &str) -> bool {
    (rest.starts_with('+') || rest.starts_with('-'))
        && rest[1..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit() || matches!(c, '.' | 'i' | 'N'))
}

fn take_ident(cur: &mut Cursor) -> String {
    let mut name = String::new();
    while let Some(c) = cur.peek() {
        if c.is_alphanumeric() || c == '_' {
            name.push(c);
            cur.bump();
        } else {
            break;
        }
    }
    name
}

fn lex_number(cur: &mut Cursor) -> Tok {
    let start = cur.pos;
    if matches!(cur.peek(), Some('+' | '-')) {
        cur.bump();
    }
    let hex = cur.rest().starts_with("0x");
    let mut prev = ' ';
    while let Some(c) = cur.peek() {
        let take = if c.is_ascii_alphanumeric() || c == '_' {
            true
        } else if c == '.' {
            !cur.rest()[1..].starts_with('.')
        } else if c == '+' || c == '-' {
            !hex && matches!(prev, 'e' | 'E')
        } else {
            false
        };
        if !take {
            break;
        }
        prev = c;
        cur.bump();
    }
    Tok::Number(cur.src[start..cur.pos].to_string())
}

fn lex_char(cur: &mut Cursor) -> Tok {
    cur.bump(); // the opening quote
    let value = match cur.bump() {
        Some('\\') => match escape(cur, false) {
            Ok(Some(c)) => c,
            _ => return Tok::Bad(BAD_ESCAPE),
        },
        Some('\'') | None => return Tok::Bad("empty or unterminated character"),
        Some(c) => c,
    };
    if cur.eat('\'') {
        Tok::Char(value)
    } else {
        Tok::Bad("unterminated character")
    }
}

/// A string body after its opening quote, through the closing quote.
fn lex_string(cur: &mut Cursor) -> Tok {
    let mut out = String::new();
    let mut bad = None;
    loop {
        match cur.bump() {
            None => return Tok::OpenStr,
            Some('"') => return bad.map_or(Tok::Str(out), Tok::Bad),
            Some('\\') => match escape(cur, true) {
                Ok(Some(c)) => out.push(c),
                Ok(None) => {}
                Err(message) => bad = bad.or(Some(message)),
            },
            Some(c) => out.push(c),
        }
    }
}

/// A raw string at `r`: `r#"..."#` with as many hashes as it opens with.
fn lex_raw_string(cur: &mut Cursor) -> Tok {
    cur.bump(); // r
    let mut hashes = 0;
    while cur.eat('#') {
        hashes += 1;
    }
    cur.bump(); // the opening quote
    let closing = format!("\"{}", "#".repeat(hashes));
    match cur.rest().find(&closing) {
        Some(at) => {
            let text = cur.rest()[..at].to_string();
            cur.pos += at + closing.len();
            Tok::Str(text)
        }
        None => {
            cur.pos = cur.src.len();
            Tok::OpenStr
        }
    }
}

/// An escape after its backslash. `Ok(None)` is a line continuation, which
/// only a string has.
fn escape(cur: &mut Cursor, in_string: bool) -> Result<Option<char>, &'static str> {
    let c = cur.bump().ok_or(UNTERMINATED_ESCAPE)?;
    let value = match c {
        '\'' | '"' | '\\' => c,
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        '0' => '\0',
        'x' => {
            let high = hex_digit(cur)?;
            let low = hex_digit(cur)?;
            char::from_u32(high * 16 + low).ok_or(BAD_ESCAPE)?
        }
        'u' => {
            if !cur.eat('{') {
                return Err(BAD_ESCAPE);
            }
            let mut value = 0u32;
            let mut digits = 0;
            loop {
                match cur.bump() {
                    Some('}') if digits > 0 => break,
                    Some(d) if d.is_ascii_hexdigit() && digits < 6 => {
                        value = value * 16 + d.to_digit(16).unwrap_or(0);
                        digits += 1;
                    }
                    _ => return Err(BAD_ESCAPE),
                }
            }
            char::from_u32(value).ok_or(BAD_ESCAPE)?
        }
        '\n' | '\r' if in_string => {
            while matches!(cur.peek(), Some(' ' | '\t' | '\r' | '\n')) {
                cur.bump();
            }
            return Ok(None);
        }
        _ => return Err(BAD_ESCAPE),
    };
    Ok(Some(value))
}

fn hex_digit(cur: &mut Cursor) -> Result<u32, &'static str> {
    cur.bump().and_then(|c| c.to_digit(16)).ok_or(BAD_ESCAPE)
}

/// Numbers as RON writes them, with `_` separators and `0x`/`0o`/`0b`
/// radixes, into a JSON number. Floats need a `.` or an exponent.
fn parse_number(text: &str) -> Result<Value, &'static str> {
    let clean: String = text.chars().filter(|c| *c != '_').collect();
    let (negative, unsigned) = match clean.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, clean.strip_prefix('+').unwrap_or(&clean)),
    };
    if unsigned == "inf" || unsigned == "NaN" {
        return Err(NON_FINITE);
    }
    let radix = match unsigned.get(..2) {
        Some("0x") => 16,
        Some("0o") => 8,
        Some("0b") => 2,
        _ => 10,
    };
    let body = strip_suffix(unsigned, radix == 10);
    if radix != 10 {
        let magnitude = u128::from_str_radix(&body[2..], radix).map_err(|_| INVALID_NUMBER)?;
        if magnitude > i128::MAX as u128 {
            return Err(INTEGER_RANGE);
        }
        let signed = if negative {
            -(magnitude as i128)
        } else {
            magnitude as i128
        };
        return integer_value(signed);
    }
    if body.is_empty() {
        return Err(INVALID_NUMBER);
    }
    let sign = if negative { "-" } else { "" };
    if body.contains(['.', 'e', 'E']) {
        let float: f64 = format!("{sign}{body}")
            .parse()
            .map_err(|_| INVALID_NUMBER)?;
        return Number::from_f64(float)
            .map(Value::Number)
            .ok_or(FLOAT_RANGE);
    }
    // Only digits can make an integer: `80x80` is not a number that is too
    // large, it is not a number at all.
    if !body.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(INVALID_NUMBER);
    }
    let integer: i128 = format!("{sign}{body}").parse().map_err(|_| INTEGER_RANGE)?;
    integer_value(integer)
}

/// Removes an integer or float suffix such as `u8` or `f64`.
fn strip_suffix(text: &str, decimal: bool) -> &str {
    for suffix in [
        "i8", "i16", "i32", "i64", "i128", "u8", "u16", "u32", "u64", "u128",
    ] {
        if let Some(rest) = text.strip_suffix(suffix) {
            return rest;
        }
    }
    if decimal {
        for suffix in ["f32", "f64"] {
            if let Some(rest) = text.strip_suffix(suffix) {
                return rest;
            }
        }
    }
    text
}

fn integer_value(value: i128) -> Result<Value, &'static str> {
    if let Ok(small) = i64::try_from(value) {
        Ok(Value::Number(Number::from(small)))
    } else if let Ok(large) = u64::try_from(value) {
        Ok(Value::Number(Number::from(large)))
    } else {
        Err(INTEGER_RANGE)
    }
}

// ---------------------------------------------------------------------------
// Parsing into a tree with spans
// ---------------------------------------------------------------------------

/// A parsed value and where it is written.
#[derive(Debug, Clone)]
struct Node {
    span: Range<usize>,
    kind: Kind,
}

#[derive(Debug, Clone)]
enum Kind {
    Scalar(Value),
    Array(Vec<Node>),
    Object(Vec<Field>),
}

#[derive(Debug, Clone)]
struct Field {
    key: String,
    key_span: Range<usize>,
    value: Node,
}

impl Node {
    fn to_value(&self) -> Value {
        match &self.kind {
            Kind::Scalar(value) => value.clone(),
            Kind::Array(items) => Value::Array(items.iter().map(Node::to_value).collect()),
            Kind::Object(fields) => Value::Object(
                fields
                    .iter()
                    .map(|field| (field.key.clone(), field.value.to_value()))
                    .collect(),
            ),
        }
    }
}

fn scalar(span: Range<usize>, value: Value) -> Node {
    Node {
        span,
        kind: Kind::Scalar(value),
    }
}

/// A syntax error: its byte offset and message.
struct Fail {
    at: usize,
    message: &'static str,
}

struct Parser {
    end: usize,
    tokens: Vec<Token>,
    at: usize,
}

impl Parser {
    fn here(&self) -> usize {
        self.tokens
            .get(self.at)
            .map_or(self.end, |token| token.start)
    }

    fn fail<T>(&self, message: &'static str) -> Result<T, Fail> {
        Err(Fail {
            at: self.here(),
            message,
        })
    }

    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.at).map(|token| &token.tok)
    }

    fn peek_at(&self, ahead: usize) -> Option<&Tok> {
        self.tokens.get(self.at + ahead).map(|token| &token.tok)
    }

    fn next_token(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.at).cloned()?;
        self.at += 1;
        Some(token)
    }

    fn is_punct(&self, c: char) -> bool {
        self.peek() == Some(&Tok::Punct(c))
    }

    fn eat(&mut self, c: char) -> bool {
        if self.is_punct(c) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn close(&mut self, closer: char) -> Result<Token, Fail> {
        if self.is_punct(closer)
            && let Some(token) = self.next_token()
        {
            return Ok(token);
        }
        self.fail(match closer {
            ')' => "expected `)`",
            ']' => "expected `]`",
            _ => "expected `}`",
        })
    }

    fn document(&mut self) -> Result<Node, Fail> {
        self.extensions()?;
        let node = self.value()?;
        if self.at < self.tokens.len() {
            return self.fail("unexpected text after the value");
        }
        Ok(node)
    }

    /// `#![enable(...)]` lines. Only the two that keep the data as it is are
    /// accepted.
    fn extensions(&mut self) -> Result<(), Fail> {
        while self.is_punct('#') && self.peek_at(1) == Some(&Tok::Punct('!')) {
            self.at += 2;
            if !self.eat('[') {
                return self.fail("expected `[` after `#!`");
            }
            match self.next_token() {
                Some(Token {
                    tok: Tok::Ident(name),
                    ..
                }) if name == "enable" => {}
                _ => return self.fail("expected `enable`"),
            }
            if !self.eat('(') {
                return self.fail("expected `(` after `enable`");
            }
            while !self.is_punct(')') {
                let name = match self.next_token() {
                    Some(Token {
                        tok: Tok::Ident(name),
                        ..
                    }) => name,
                    _ => return self.fail("expected an extension name"),
                };
                if !matches!(name.as_str(), "implicit_some" | "explicit_struct_names") {
                    return self.fail("an extension that changes the data is not supported");
                }
                if !self.eat(',') {
                    break;
                }
            }
            if !(self.eat(')') && self.eat(']')) {
                return self.fail("expected `)]` to close the extensions");
            }
        }
        Ok(())
    }

    fn value(&mut self) -> Result<Node, Fail> {
        let Some(token) = self.next_token() else {
            return self.fail("expected a value");
        };
        let (start, end) = (token.start, token.end);
        match token.tok {
            Tok::Str(text) => Ok(scalar(start..end, Value::String(text))),
            Tok::Char(c) => Ok(scalar(start..end, Value::String(c.to_string()))),
            Tok::Number(text) => match parse_number(&text) {
                Ok(value) => Ok(scalar(start..end, value)),
                Err(message) => Err(Fail { at: start, message }),
            },
            Tok::Ident(name) => self.named(name, start, end),
            Tok::Punct('(') => self.group(start),
            Tok::Punct('[') => self.list(start),
            Tok::Punct('{') => self.map(start),
            _ => Err(Fail {
                at: start,
                message: "expected a value",
            }),
        }
    }

    /// A word in value position: a keyword, a `Name(...)` struct or variant,
    /// `Some(...)`, or a bare name.
    fn named(&mut self, name: String, start: usize, end: usize) -> Result<Node, Fail> {
        let span = start..end;
        if name == "true" || name == "false" {
            return Ok(scalar(span, Value::Bool(name == "true")));
        }
        if name == "None" {
            return Ok(scalar(span, Value::Null));
        }
        if name == "inf" || name == "NaN" {
            return Err(Fail {
                at: start,
                message: NON_FINITE,
            });
        }
        if name == "Some" && self.eat('(') {
            let inner = self.value()?;
            let close = self.close(')')?;
            return Ok(Node {
                span: start..close.end,
                kind: inner.kind,
            });
        }
        if self.eat('(') {
            return self.group(start);
        }
        Ok(scalar(span, Value::String(name)))
    }

    /// The body of a `(...)` after its opening parenthesis: named fields make
    /// an object, anything else an array.
    fn group(&mut self, start: usize) -> Result<Node, Fail> {
        let named =
            matches!(self.peek(), Some(Tok::Ident(_))) && self.peek_at(1) == Some(&Tok::Punct(':'));
        let kind = if named {
            let mut fields: Vec<Field> = Vec::new();
            while !self.is_punct(')') {
                let field = self.field(&fields)?;
                fields.push(field);
                if !self.eat(',') {
                    break;
                }
            }
            Kind::Object(fields)
        } else {
            let mut items = Vec::new();
            while !self.is_punct(')') {
                items.push(self.value()?);
                if !self.eat(',') {
                    break;
                }
            }
            Kind::Array(items)
        };
        let close = self.close(')')?;
        Ok(Node {
            span: start..close.end,
            kind,
        })
    }

    fn field(&mut self, existing: &[Field]) -> Result<Field, Fail> {
        let (key, key_span) = match self.next_token() {
            Some(Token {
                tok: Tok::Ident(name),
                start,
                end,
            }) => (name, start..end),
            None => return self.fail("expected `)`"),
            _ => return self.fail("expected a field name"),
        };
        if !self.eat(':') {
            return self.fail("expected `:` after a field name");
        }
        if existing.iter().any(|field| field.key == key) {
            return Err(Fail {
                at: key_span.start,
                message: "duplicate field name",
            });
        }
        let value = self.value()?;
        Ok(Field {
            key,
            key_span,
            value,
        })
    }

    fn list(&mut self, start: usize) -> Result<Node, Fail> {
        let mut items = Vec::new();
        while !self.is_punct(']') {
            items.push(self.value()?);
            if !self.eat(',') {
                break;
            }
        }
        let close = self.close(']')?;
        Ok(Node {
            span: start..close.end,
            kind: Kind::Array(items),
        })
    }

    fn map(&mut self, start: usize) -> Result<Node, Fail> {
        let mut fields: Vec<Field> = Vec::new();
        while !self.is_punct('}') {
            let key_node = self.value()?;
            let Some(key) = map_key(&key_node) else {
                return Err(Fail {
                    at: key_node.span.start,
                    message: "a map key must be a string, number, boolean or name",
                });
            };
            if !self.eat(':') {
                return self.fail("expected `:` after a map key");
            }
            if fields.iter().any(|field| field.key == key) {
                return Err(Fail {
                    at: key_node.span.start,
                    message: "duplicate map key",
                });
            }
            let value = self.value()?;
            fields.push(Field {
                key,
                key_span: key_node.span.clone(),
                value,
            });
            if !self.eat(',') {
                break;
            }
        }
        let close = self.close('}')?;
        Ok(Node {
            span: start..close.end,
            kind: Kind::Object(fields),
        })
    }
}

/// The JSON key a map key reads as.
fn map_key(node: &Node) -> Option<String> {
    match &node.kind {
        Kind::Scalar(Value::String(text)) => Some(text.clone()),
        Kind::Scalar(Value::Number(number)) => Some(number.to_string()),
        Kind::Scalar(Value::Bool(flag)) => Some(flag.to_string()),
        _ => None,
    }
}

/// The byte offset and message of the first lexical error, or the tree.
fn parse_tree(source: &str) -> Result<Node, ParseError> {
    let lexed = lex(source);
    if let Some(bad) = lexed
        .iter()
        .find(|token| matches!(token.tok, Tok::Bad(_) | Tok::OpenStr | Tok::OpenBlock))
    {
        let message = match bad.tok {
            Tok::Bad(message) => message,
            Tok::OpenStr => "unterminated string",
            _ => "unterminated block comment",
        };
        return Err(error_at(source, bad.start, message));
    }
    let tokens: Vec<Token> = lexed
        .into_iter()
        .filter(|token| !is_comment(token))
        .collect();
    let mut parser = Parser {
        end: source.len(),
        tokens,
        at: 0,
    };
    parser
        .document()
        .map_err(|fail| error_at(source, fail.at, fail.message))
}

fn is_comment(token: &Token) -> bool {
    matches!(token.tok, Tok::LineComment | Tok::BlockComment)
}

/// The error at byte `at`, with its one-based line and character column.
fn error_at(source: &str, at: usize, message: &str) -> ParseError {
    let (line, column) = line_and_column(source, at);
    ParseError::new(message, Some(line + 1), Some(column + 1))
}

/// The zero-based line of byte `at`, and its zero-based character column.
fn line_and_column(source: &str, at: usize) -> (usize, usize) {
    let line_start = source[..at].rfind('\n').map_or(0, |newline| newline + 1);
    (
        source[..at].matches('\n').count(),
        source[line_start..at].chars().count(),
    )
}

/// The span of `range` on its first line, as the editor counts it.
fn one_line_span(source: &str, range: Range<usize>) -> Span {
    let (line, start_column) = line_and_column(source, range.start);
    let line_end = source[range.start..]
        .find('\n')
        .map_or(source.len(), |at| range.start + at);
    let end = range.end.min(line_end).max(range.start);
    let (_, end_column) = line_and_column(source, end);
    Span {
        line: line + 1,
        start_column: start_column + 1,
        end_column: end_column + 1,
    }
}

/// The node at `segments`, and the field whose key is the last segment.
fn walk<'n>(
    root: &'n Node,
    segments: &[SchemaPathSegment],
) -> Option<(&'n Node, Option<&'n Field>)> {
    let mut node = root;
    let mut field = None;
    for segment in segments {
        match (&node.kind, segment) {
            (Kind::Object(fields), SchemaPathSegment::Key(key)) => {
                let found = fields.iter().find(|candidate| candidate.key == *key)?;
                node = &found.value;
                field = Some(found);
            }
            (Kind::Array(items), SchemaPathSegment::ArrayItem(index)) => {
                node = items.get(*index)?;
                field = None;
            }
            _ => return None,
        }
    }
    Some((node, field))
}

// ---------------------------------------------------------------------------
// Scanning the tokens before the caret, for completion and hover
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// The text as a whole: one value, the top-level one.
    Root,
    /// `( ... )`: named fields, or positional items.
    Group,
    /// `[ ... ]`
    List,
    /// `{ ... }`
    Map,
    /// The `Some(` wrapper, which is not part of the data's path.
    Some,
}

impl Shape {
    fn of_open(c: char) -> Shape {
        match c {
            '[' => Shape::List,
            '{' => Shape::Map,
            _ => Shape::Group,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Expect {
    /// A key, or a positional item, may start here.
    Key,
    /// A key-like token was read; a colon makes it a key.
    Pending(String, usize, usize),
    /// A value starts here.
    Value,
    /// A word in value position; a `(` makes it a struct or variant.
    Name(String),
    /// A value is complete; a separator or a closer comes next.
    Sep,
}

#[derive(Debug, Clone)]
struct Frame {
    shape: Shape,
    /// The path segment of the value open inside this frame.
    child: Option<SchemaPathSegment>,
    expect: Expect,
    /// The positional index of the next item.
    index: usize,
}

/// A field key found while scanning: its byte range, the path of the
/// container holding it, and the key.
#[derive(Debug, Clone)]
struct KeyEvent {
    start: usize,
    end: usize,
    container: Vec<SchemaPathSegment>,
    key: String,
}

/// The structure of a token stream, tolerant of text that does not parse.
struct Scanner {
    /// `frames[0]` is the root; the top-level value is `frames[1]`.
    frames: Vec<Frame>,
    keys: Vec<KeyEvent>,
}

impl Scanner {
    fn new() -> Self {
        Scanner {
            frames: vec![Frame {
                shape: Shape::Root,
                child: None,
                expect: Expect::Value,
                index: 0,
            }],
            keys: Vec::new(),
        }
    }

    fn top(&self) -> &Frame {
        self.frames.last().expect("the root frame is never removed")
    }

    fn top_mut(&mut self) -> &mut Frame {
        self.frames
            .last_mut()
            .expect("the root frame is never removed")
    }

    /// The schema path of the value that frame `depth` holds: the segments
    /// of the frames above it, each the child it has open.
    fn path_of(&self, depth: usize) -> Vec<SchemaPathSegment> {
        self.frames
            .iter()
            .take(depth)
            .skip(1)
            .filter_map(|frame| frame.child.clone())
            .collect()
    }

    /// The child of the top frame is a value now starting: a list item, or a
    /// positional item of a group.
    fn begin_value(&mut self) {
        let top = self.top_mut();
        let index = top.index;
        match top.shape {
            Shape::List => top.child = Some(SchemaPathSegment::ArrayItem(index)),
            Shape::Group if top.child.is_none() => {
                top.child = Some(SchemaPathSegment::ArrayItem(index));
            }
            _ => {}
        }
    }

    fn push(&mut self, shape: Shape) {
        self.top_mut().expect = Expect::Sep;
        let expect = match shape {
            Shape::Group | Shape::Map => Expect::Key,
            _ => Expect::Value,
        };
        self.frames.push(Frame {
            shape,
            child: None,
            expect,
            index: 0,
        });
    }

    fn pop(&mut self) {
        if self.frames.len() > 1 {
            self.frames.pop();
            self.top_mut().expect = Expect::Sep;
        }
    }

    /// A separator: the next entry or item starts.
    fn next_entry(&mut self) {
        let top = self.top_mut();
        top.child = None;
        if matches!(top.shape, Shape::List | Shape::Group) {
            top.index += 1;
        }
        top.expect = match top.shape {
            Shape::Group | Shape::Map => Expect::Key,
            _ => Expect::Value,
        };
    }

    fn feed(&mut self, token: &Token) {
        if let Expect::Pending(name, start, end) = self.top().expect.clone() {
            if token.tok == Tok::Punct(':') {
                let depth = self.frames.len() - 1;
                let container = self.path_of(depth);
                self.keys.push(KeyEvent {
                    start,
                    end,
                    container,
                    key: name.clone(),
                });
                let top = self.top_mut();
                top.child = Some(SchemaPathSegment::Key(name));
                top.expect = Expect::Value;
                return;
            }
            // Not a key: the word is a positional value.
            self.begin_value();
            self.top_mut().expect = Expect::Name(name);
        }
        if let Expect::Name(name) = self.top().expect.clone() {
            if token.tok == Tok::Punct('(') {
                let shape = if name == "Some" {
                    Shape::Some
                } else {
                    Shape::Group
                };
                self.push(shape);
                return;
            }
            self.top_mut().expect = Expect::Sep;
        }
        match self.top().expect.clone() {
            Expect::Key => self.key_position(token),
            Expect::Value => self.value_position(token),
            Expect::Sep => self.sep_position(token),
            Expect::Pending(..) | Expect::Name(_) => {}
        }
    }

    fn key_position(&mut self, token: &Token) {
        match &token.tok {
            Tok::Ident(name) | Tok::Str(name) => {
                self.top_mut().expect = Expect::Pending(name.clone(), token.start, token.end);
            }
            Tok::Punct(c @ ('(' | '[' | '{')) => {
                self.begin_value();
                self.push(Shape::of_open(*c));
            }
            Tok::Punct(')' | ']' | '}') => self.pop(),
            Tok::Punct(',' | ':') => {}
            _ => {
                self.begin_value();
                self.top_mut().expect = Expect::Sep;
            }
        }
    }

    fn value_position(&mut self, token: &Token) {
        match &token.tok {
            Tok::Ident(name) => {
                self.begin_value();
                self.top_mut().expect = Expect::Name(name.clone());
            }
            Tok::Punct(c @ ('(' | '[' | '{')) => {
                self.begin_value();
                self.push(Shape::of_open(*c));
            }
            Tok::Punct(')' | ']' | '}') => self.pop(),
            Tok::Punct(',' | ':') => {}
            _ => {
                self.begin_value();
                self.top_mut().expect = Expect::Sep;
            }
        }
    }

    fn sep_position(&mut self, token: &Token) {
        match &token.tok {
            Tok::Punct(',') => self.next_entry(),
            Tok::Punct(')' | ']' | '}') => self.pop(),
            // A missing comma: the token starts the next entry.
            _ => {
                self.next_entry();
                self.feed(token);
            }
        }
    }
}

/// The tokens that matter for structure: no comments, no `#![...]` header.
fn significant_tokens(tokens: &[Token]) -> Vec<Token> {
    let kept: Vec<Token> = tokens
        .iter()
        .filter(|token| !is_comment(token))
        .cloned()
        .collect();
    let mut skip = 0;
    loop {
        let is_header = matches!(kept.get(skip).map(|t| &t.tok), Some(Tok::Punct('#')))
            && matches!(kept.get(skip + 1).map(|t| &t.tok), Some(Tok::Punct('!')));
        if !is_header {
            break;
        }
        while let Some(token) = kept.get(skip) {
            skip += 1;
            if token.tok == Tok::Punct(']') {
                break;
            }
        }
    }
    kept.into_iter().skip(skip).collect()
}

fn scan(tokens: &[Token]) -> Scanner {
    let mut scanner = Scanner::new();
    for token in tokens {
        scanner.feed(token);
    }
    scanner
}

/// Where the caret's value or key goes, before the completion context is built.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Key,
    Value,
    Sep,
    Other,
}

fn item_path(mut path: Vec<SchemaPathSegment>, index: usize) -> Vec<SchemaPathSegment> {
    path.push(SchemaPathSegment::ArrayItem(index));
    path
}

/// A key as RON writes it: an identifier, or a quoted string.
fn key_text(key: &str) -> String {
    if is_identifier(key) {
        key.to_string()
    } else {
        quote(key)
    }
}

fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && chars.all(|c| c.is_alphanumeric() || c == '_')
}

/// A string value that RON writes without quotes: an identifier that is not
/// one of its keywords (a unit variant such as `Info`).
fn is_bare(text: &str) -> bool {
    is_identifier(text) && !matches!(text, "true" | "false" | "None" | "Some" | "inf" | "NaN")
}

/// The indentation for a field typed at the start of a line: that of the
/// nearest non-blank line above it (a sibling field), one unit deeper when
/// that line opens a block. Without any indented line yet, a unit is four
/// spaces.
fn sibling_indent(source: &str, line_start: usize) -> Option<String> {
    let above = source[..line_start].strip_suffix('\n')?;
    let line = above.lines().rev().find(|line| !line.trim().is_empty())?;
    let indent = leading_blanks(line);
    if !line.trim_end().ends_with(['(', '[', '{']) {
        return Some(indent.to_string());
    }
    let unit = source
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(leading_blanks)
        .find(|blanks| !blanks.is_empty())
        .unwrap_or("    ");
    Some(format!("{indent}{unit}"))
}

fn leading_blanks(line: &str) -> &str {
    &line[..line.len() - line.trim_start_matches([' ', '\t']).len()]
}

/// Whether a field follows the caret in its struct: the text after it, past
/// blanks and comments, starts something other than a closer or a comma.
fn fields_follow(rest: &str) -> bool {
    match significant_tokens(&lex(rest)).first() {
        None => false,
        Some(token) => !matches!(token.tok, Tok::Punct(')' | ']' | '}' | ',')),
    }
}

fn quote(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ---------------------------------------------------------------------------
// The language
// ---------------------------------------------------------------------------

impl ConfigLanguage for Ron {
    fn info(&self) -> LanguageInfo {
        LanguageInfo {
            label: "RON",
            icon: "R",
            extensions: &["ron"],
        }
    }

    fn config(&self) -> LanguageConfig {
        LanguageConfig {
            line_comments: &["//"],
            block_comment: Some(("/*", "*/")),
            brackets: &[('(', ')'), ('[', ']'), ('{', '}')],
            separators: &[":"],
            quotes: &["\""],
            map_brackets: Some(('{', '}')),
            list_brackets: Some(('[', ']')),
            item_separator: Some(','),
            written_separator: Some(": "),
            ..LanguageConfig::default()
        }
    }

    fn parse(&self, source: &str) -> Result<Value, ParseError> {
        parse_tree(source).map(|node| node.to_value())
    }

    fn key_span(&self, source: &str, path: &FieldPath) -> Option<Span> {
        let root = parse_tree(source).ok()?;
        let segments = SchemaPathSegment::from_pointer(&path.to_pointer());
        let (_, field) = walk(&root, &segments)?;
        Some(one_line_span(source, field?.key_span.clone()))
    }

    fn value_span(&self, source: &str, path: &FieldPath) -> Option<Span> {
        let root = parse_tree(source).ok()?;
        let segments = SchemaPathSegment::from_pointer(&path.to_pointer());
        let (node, _) = walk(&root, &segments)?;
        Some(one_line_span(source, node.span.clone()))
    }

    fn completion_context(&self, source: &str, cursor: usize) -> CompletionContext {
        let byte = utf16_to_byte_index(source, cursor);
        let prefix = &source[..byte];
        let line_start = prefix.rfind('\n').map_or(0, |at| at + 1);
        let base_indent: String = source[line_start..]
            .chars()
            .take_while(|c| matches!(c, ' ' | '\t'))
            .collect();
        let utf16 = |at: usize| source[..at].encode_utf16().count();
        let mut context = CompletionContext {
            base_indent,
            replace_start: cursor,
            replace_end: cursor,
            ..Default::default()
        };

        let tokens = lex(prefix);
        let last = tokens.last();
        let in_comment = last.is_some_and(|token| {
            token.end == byte && matches!(token.tok, Tok::LineComment | Tok::OpenBlock)
        });
        let in_string = last.is_some_and(|token| matches!(token.tok, Tok::OpenStr));
        if in_comment || in_string {
            let scanner = scan(&significant_tokens(&tokens[..tokens.len() - 1]));
            context.path = scanner.path_of(scanner.frames.len() - 1);
            context.no_properties = true;
            return context;
        }

        // A word typed right before the caret is what completion replaces.
        let (typed, word_start, before) = match tokens.last() {
            Some(Token {
                tok: Tok::Ident(name),
                start,
                end,
            }) if *end == byte => (name.clone(), *start, &tokens[..tokens.len() - 1]),
            _ => (String::new(), byte, &tokens[..]),
        };
        let significant = significant_tokens(before);
        let scanner = scan(&significant);
        let depth = scanner.frames.len() - 1;
        let top = scanner.top().clone();
        context.typed = typed;
        context.replace_start = utf16(word_start);
        context.replace_end = cursor;

        // `Some(` is transparent: the caret is the value of its parent's key.
        let (owner, mode) = match top.shape {
            Shape::Some => (
                depth - 1,
                if top.expect == Expect::Sep {
                    Mode::Other
                } else {
                    Mode::Value
                },
            ),
            _ => (
                depth,
                match top.expect {
                    Expect::Key => Mode::Key,
                    Expect::Value => Mode::Value,
                    Expect::Sep => Mode::Sep,
                    Expect::Pending(..) | Expect::Name(_) => Mode::Other,
                },
            ),
        };
        let owner_frame = scanner.frames[owner].clone();
        let base = scanner.path_of(owner);
        let index = owner_frame.index;
        let in_struct = matches!(owner_frame.shape, Shape::Group | Shape::Map);
        // A field typed before a field that follows needs its comma; a
        // separator already there, or a closer, needs none.
        let suffix = if fields_follow(&source[byte..]) {
            ",".to_string()
        } else {
            String::new()
        };
        if mode == Mode::Sep {
            // The separator is written right after the value, so the blanks
            // between the value and the word typed are replaced with it.
            let before_word = prefix[..word_start].trim_end_matches([' ', '\t']);
            context.replace_start = utf16(before_word.len());
        }
        match (mode, owner_frame.shape) {
            (Mode::Key, Shape::Group | Shape::Map) => {
                context.path = base;
                context.entry_suffix = suffix.clone();
            }
            (Mode::Value, Shape::Root) => {
                context.path = base;
                if significant.is_empty() {
                    // An empty file: a key typed here opens the top-level struct.
                    context.entry_wrap = Some(("(", ")"));
                } else {
                    context.no_properties = true;
                }
            }
            (Mode::Value, Shape::Group | Shape::Map) => match owner_frame.child {
                Some(SchemaPathSegment::Key(key)) => {
                    context.path = base;
                    context.value_key = Some(key);
                    context.value_needs_space = prefix[..word_start].ends_with(':');
                }
                _ => {
                    context.is_array_item = true;
                    context.path = item_path(base, index);
                }
            },
            (Mode::Value, Shape::List) => {
                context.is_array_item = true;
                context.path = item_path(base, index);
            }
            (Mode::Sep, Shape::Group | Shape::Map) => {
                context.path = base;
                context.entry_prefix = ", ".to_string();
                context.entry_suffix = suffix.clone();
            }
            (Mode::Sep, Shape::List) => {
                context.is_array_item = true;
                context.path = item_path(base, index + 1);
                context.entry_prefix = ", ".to_string();
            }
            _ => context.no_properties = true,
        }
        // A field typed at the very start of a line, with no indentation
        // before it, takes its siblings' indentation: the replaced range
        // starts at the line start, so the indent is written once.
        if mode == Mode::Key && in_struct && word_start == line_start && context.base_indent.is_empty()
            && let Some(indent) = sibling_indent(source, line_start)
            && !indent.is_empty()
        {
            context.replace_start = utf16(line_start);
            context.base_indent = indent.clone();
            context.entry_prefix = indent;
        }
        context
    }

    /// The value template of an accepted field: the schema's default or const
    /// (caret at its end, which the editor selects), `[]` for a list, an
    /// object as `( ` with its closer on a line of its own at the field's
    /// indentation, `""` for a string, and a bare `name: ` for a value the
    /// schema does not type. The plan's `string` flag also covers an enum whose
    /// placeholder is a string, so an untyped enum without a default is `""`.
    /// The comma after the field, when fields follow it, is the entry suffix
    /// `completion_context` gives.
    fn write_insert(
        &self,
        key: &str,
        plan: &InsertPlan<'_>,
        context: &InsertContext<'_>,
    ) -> Option<(String, usize)> {
        let name = key_text(key);
        let indent = context.base_indent;
        let unit = context.unit;
        let utf16 = |text: &str| text.encode_utf16().count();
        match plan.value {
            Planned::Value(value) => {
                let text = format!("{name}: {}", self.format_value(value));
                let caret = utf16(&text);
                Some((text, caret))
            }
            Planned::List { .. } => {
                let head = format!("{name}: [");
                let caret = utf16(&head);
                Some((format!("{head}]"), caret))
            }
            Planned::Object { .. } => {
                let head = format!("{name}: (\n{indent}{unit}");
                let caret = utf16(&head);
                Some((format!("{head}\n{indent})"), caret))
            }
            Planned::Empty { string: true } => {
                let head = format!("{name}: \"");
                let caret = utf16(&head);
                Some((format!("{head}\""), caret))
            }
            _ => {
                let text = format!("{name}: ");
                let caret = utf16(&text);
                Some((text, caret))
            }
        }
    }

    fn format_value(&self, value: &Value) -> String {
        match value {
            Value::Null => "None".to_string(),
            Value::Bool(flag) => flag.to_string(),
            Value::Number(number) => number.to_string(),
            Value::String(text) if is_bare(text) => text.clone(),
            Value::String(text) => quote(text),
            Value::Array(items) => format!(
                "[{}]",
                items
                    .iter()
                    .map(|item| self.format_value(item))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Value::Object(fields) => format!(
                "({})",
                fields
                    .iter()
                    .map(|(key, item)| format!("{}: {}", key_text(key), self.format_value(item)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    fn serialize(&self, value: &Value, _previous: Option<&str>) -> Result<String, String> {
        let mut text = String::new();
        write_ron(value, 0, &mut text);
        text.push('\n');
        Ok(text)
    }

    fn highlight(&self, line: &str) -> Vec<Highlight> {
        let tokens = lex(line);
        let mut out = Tokens::new();
        for (index, token) in tokens.iter().enumerate() {
            let next = tokens.get(index + 1).map(|token| &token.tok);
            let kind = match &token.tok {
                Tok::Ident(name) => Some(ident_kind(name, next)),
                Tok::Str(_) | Tok::OpenStr | Tok::Char(_) => Some(TokenKind::String),
                Tok::Number(_) => Some(TokenKind::Literal),
                Tok::LineComment | Tok::BlockComment | Tok::OpenBlock => Some(TokenKind::Comment),
                Tok::Punct(c) if "()[]{},:".contains(*c) => Some(TokenKind::Punctuation),
                _ => None,
            };
            if let Some(kind) = kind {
                out.push(token.start, token.end, kind);
            }
        }
        out.finish()
    }

    fn text(&self) -> &dyn Text {
        self
    }

    fn hover(&self) -> &dyn Hover {
        self
    }
}

impl Text for Ron {
    /// Line comments and block comments (nested ones whole), so a block
    /// comment's lines past the first are colored as comments.
    fn comments(&self, _language: &dyn ConfigLanguage, source: &str) -> Vec<Range<usize>> {
        lex(source)
            .iter()
            .filter(|token| {
                matches!(
                    token.tok,
                    Tok::LineComment | Tok::BlockComment | Tok::OpenBlock
                )
            })
            .map(|token| token.start..token.end)
            .collect()
    }

    /// Strings (plain and raw) that span lines.
    fn multiline_strings(&self, source: &str) -> Vec<Range<usize>> {
        lex(source)
            .iter()
            .filter(|token| matches!(token.tok, Tok::Str(_) | Tok::OpenStr))
            .filter(|token| source[token.start..token.end].contains('\n'))
            .map(|token| token.start..token.end)
            .collect()
    }
}

/// The kind of an identifier on a line, given the token after it.
fn ident_kind(name: &str, next: Option<&Tok>) -> TokenKind {
    if matches!(name, "true" | "false" | "None" | "Some") {
        return TokenKind::Literal;
    }
    let capital = name.chars().next().is_some_and(char::is_uppercase);
    if capital || matches!(next, Some(Tok::Punct('('))) || matches!(next, Some(Tok::Punct(':'))) {
        return TokenKind::Key;
    }
    TokenKind::Value
}

/// Writes `value` as RON at `depth` (four spaces per level). Objects with
/// identifier keys are `(name: v, ..)`; others, and empty ones, are maps.
fn write_ron(value: &Value, depth: usize, out: &mut String) {
    match value {
        Value::Null => out.push_str("None"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => out.push_str(&number.to_string()),
        Value::String(text) => out.push_str(&quote(text)),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_ron(item, depth, out);
            }
            out.push(']');
        }
        Value::Object(fields) => {
            let named = !fields.is_empty() && fields.keys().all(|key| is_identifier(key));
            let (open, close) = if named { ("(", ")") } else { ("{", "}") };
            if fields.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push_str(open);
            out.push('\n');
            for (key, item) in fields {
                out.push_str(&"    ".repeat(depth + 1));
                if named {
                    out.push_str(key);
                } else {
                    out.push_str(&quote(key));
                }
                out.push_str(": ");
                write_ron(item, depth + 1, out);
                out.push_str(",\n");
            }
            out.push_str(&"    ".repeat(depth));
            out.push_str(close);
        }
    }
}

impl Hover for Ron {
    /// The field whose key is under `column` of zero-based `line_index`, with
    /// its full path, its key and the column the key starts at.
    fn hover_path(
        &self,
        source: &str,
        line_index: usize,
        column: usize,
    ) -> Option<(Vec<SchemaPathSegment>, String, usize)> {
        let scanner = scan(&significant_tokens(&lex(source)));
        scanner.keys.iter().find_map(|event| {
            let (line, start_column) = line_and_column(source, event.start);
            let (end_line, end_column) = line_and_column(source, event.end);
            if line != line_index || end_line != line_index {
                return None;
            }
            (start_column <= column && column < end_column).then(|| {
                let mut path = event.container.clone();
                path.push(SchemaPathSegment::Key(event.key.clone()));
                (path, event.key.clone(), start_column)
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leptos_config_editor::{Schema, TextPosition};
    use serde_json::json;

    const SAMPLE: &str = r#"// The billing service.
AppConfig(
    name: "billing-api",
    version: "1.4.2",
    server: Server(
        host: "0.0.0.0",
        port: 8443,
        tls: Some(Tls(
            cert_path: "/etc/billing/cert.pem",
            key_path: "/etc/billing/key.pem",
        )),
    ),
    log: Log(
        level: Info,
        targets: ["stdout", "/var/log/billing.log"],
    ),
    features: [Metrics, Tracing],
)
"#;

    const PORT_DOC: &str = "The TCP port to listen on. A number written without quotes.";
    const INFO_DOC: &str = "Normal operation: start-up, configuration and significant events.";

    fn schema() -> Schema {
        Schema::compile(RON_SCHEMA).expect("the RON schema compiles")
    }

    /// A position inside the first `needle` (on its second character).
    fn inside(text: &str, needle: &str) -> TextPosition {
        let at = text.find(needle).expect("the needle is in the text");
        let line_start = text[..at].rfind('\n').map_or(0, |newline| newline + 1);
        let line = text[..at].matches('\n').count() + 1;
        let column = text[line_start..at].chars().count() + 2;
        TextPosition::new(line, column)
    }

    /// The (label, detail) pairs Ctrl+Space offers at `cursor`.
    fn offered(text: &str, cursor: usize) -> Vec<(String, String)> {
        schema()
            .suggestions(RON, text, cursor)
            .iter()
            .map(|suggestion| {
                (
                    suggestion.label().to_string(),
                    suggestion.detail().to_string(),
                )
            })
            .collect()
    }

    fn labels(text: &str, cursor: usize) -> Vec<String> {
        offered(text, cursor)
            .into_iter()
            .map(|(label, _)| label)
            .collect()
    }

    fn text_of(source: &str, span: Span) -> String {
        source
            .lines()
            .nth(span.line - 1)
            .expect("the span's line")
            .chars()
            .skip(span.start_column - 1)
            .take(span.end_column - span.start_column)
            .collect()
    }

    #[test]
    fn hover_on_a_field_key_shows_its_description() {
        let docs = schema()
            .hover(RON, SAMPLE, inside(SAMPLE, "port:"))
            .expect("docs over `port`");
        assert_eq!(docs.key, "port");
        assert_eq!(docs.description.as_deref(), Some(PORT_DOC));

        let docs = schema()
            .hover(RON, SAMPLE, inside(SAMPLE, "cert_path:"))
            .expect("docs over `cert_path`");
        assert_eq!(
            docs.description.as_deref(),
            Some("Path to the PEM certificate file, as a quoted string.")
        );
    }

    #[test]
    fn hover_on_an_enum_value_shows_the_entry_description() {
        let docs = schema()
            .hover(RON, SAMPLE, inside(SAMPLE, "Info"))
            .expect("docs over `Info`");
        assert_eq!(docs.key, "level");
        let value = docs.value.clone().expect("the value's docs");
        assert_eq!(value.value, "Info");
        assert_eq!(
            value.description.as_deref(),
            Some("Normal operation: start-up, configuration and significant events.")
        );
        assert_eq!(
            docs.description.as_deref(),
            Some("The least severe message that is logged. Written as a bare name, such as Info.")
        );
        let span = Ron
            .value_span(SAMPLE, &FieldPath::from("/log/level"))
            .expect("the value span of `level`");
        assert_eq!(text_of(SAMPLE, span), "Info");

        let schema_json: Value = serde_json::from_str(RON_SCHEMA).expect("valid JSON");
        let level = &schema_json["properties"]["log"]["properties"]["level"];
        let index = level["enum"]
            .as_array()
            .expect("an enum")
            .iter()
            .position(|variant| variant == "Info")
            .expect("Info is a variant");
        assert_eq!(level["x-enum-descriptions"][index], INFO_DOC);
    }

    #[test]
    fn completion_inside_a_struct_lists_its_fields_with_descriptions() {
        let text = "AppConfig(\n    name: \"x\",\n    ";
        let found = offered(text, text.encode_utf16().count());
        let names: Vec<&str> = found.iter().map(|(label, _)| label.as_str()).collect();
        // The required fields come first, in the editor's order.
        assert_eq!(names, ["log", "server", "version", "features"]);
        let (_, detail) = found
            .iter()
            .find(|(label, _)| label == "server")
            .expect("server is offered");
        assert!(
            detail.contains("Where and how the service listens"),
            "{detail}"
        );
    }

    #[test]
    fn completion_on_a_blank_line_inside_a_struct_lists_the_unset_fields() {
        let text = "AppConfig(\n    name: \"billing-api\",\n    version: \"1.4.2\",\n    \n)\n";
        // The caret sits after the indent of the blank line (line index 3).
        let cursor = text.find("\n)").expect("the blank line");
        let names = labels(text, text[..cursor].encode_utf16().count());
        for field in ["server", "log", "features"] {
            assert!(names.iter().any(|name| name == field), "{field} in {names:?}");
        }
    }

    #[test]
    fn completion_on_an_empty_line_at_column_zero_inside_a_struct() {
        let text = "AppConfig(\n    name: \"billing-api\",\n\n)\n";
        // The caret sits at column 0 of the empty line (line index 2).
        let cursor = text.find("\n\n)").expect("the empty line") + 1;
        let names = labels(text, text[..cursor].encode_utf16().count());
        for field in ["server", "log", "features"] {
            assert!(names.iter().any(|name| name == field), "{field} in {names:?}");
        }
    }

    #[test]
    fn completion_for_an_enum_field_lists_its_variants() {
        let text = "AppConfig(\n    log: Log(\n        level: ";
        let names = labels(text, text.encode_utf16().count());
        assert_eq!(names, ["Trace", "Debug", "Info", "Warn", "Error"]);
    }

    #[test]
    fn completion_of_a_list_item_lists_its_variants() {
        let text = "AppConfig(\n    features: [";
        let names = labels(text, text.encode_utf16().count());
        assert_eq!(names, ["Metrics", "Tracing", "Profiling"]);
    }

    #[test]
    fn completion_replaces_the_word_typed_before_the_caret() {
        let text = "AppConfig(\n    na";
        let suggestions = schema().suggestions(RON, text, text.encode_utf16().count());
        let name = suggestions
            .iter()
            .find(|suggestion| suggestion.label() == "name")
            .expect("name is offered");
        assert_eq!(name.accept(text).0, "AppConfig(\n    name: \"\"");
    }

    #[test]
    fn completion_after_a_value_adds_the_separating_comma() {
        let text = "AppConfig(\n    name: \"x\" ve";
        let suggestions = schema().suggestions(RON, text, text.encode_utf16().count());
        let version = suggestions
            .iter()
            .find(|suggestion| suggestion.label() == "version")
            .expect("version is offered");
        assert_eq!(
            version.accept(text).0,
            "AppConfig(\n    name: \"x\", version: \"\""
        );
    }

    /// The document after accepting the `label` suggestion at the end of
    /// `text`, with the caret as `|` and a selection as `<...>`.
    fn accepted(schema: &Schema, text: &str, label: &str) -> String {
        let cursor = text.encode_utf16().count();
        let suggestion = schema
            .suggestions(RON, text, cursor)
            .into_iter()
            .find(|suggestion| suggestion.label() == label)
            .unwrap_or_else(|| panic!("{label} is offered"));
        let (out, range) = suggestion.accept(text);
        let units: Vec<u16> = out.encode_utf16().collect();
        let part = |from: usize, to: usize| String::from_utf16(&units[from..to]).unwrap();
        if range.start == range.end {
            format!(
                "{}|{}",
                part(0, range.start),
                part(range.start, units.len())
            )
        } else {
            format!(
                "{}<{}>{}",
                part(0, range.start),
                part(range.start, range.end),
                part(range.end, units.len())
            )
        }
    }

    #[test]
    fn accepted_string_field_writes_empty_quotes_with_the_caret_inside() {
        let text = "AppConfig(\n    ";
        assert_eq!(
            accepted(&schema(), text, "name"),
            "AppConfig(\n    name: \"|\""
        );
    }

    #[test]
    fn accepted_integer_field_writes_its_default_selected() {
        let text = "AppConfig(\n    server: Server(\n        ";
        assert_eq!(
            accepted(&schema(), text, "port"),
            "AppConfig(\n    server: Server(\n        port: <8080>"
        );
    }

    #[test]
    fn accepted_boolean_field_writes_its_default_selected() {
        let schema = Schema::compile(
            r#"{"type": "object", "properties": {"enabled": {"type": "boolean", "default": false}}}"#,
        )
        .expect("the schema compiles");
        assert_eq!(
            accepted(&schema, "AppConfig(\n    ", "enabled"),
            "AppConfig(\n    enabled: <false>"
        );
    }

    #[test]
    fn accepted_field_without_a_default_writes_its_key_alone() {
        let schema = Schema::compile(
            r#"{"type": "object", "properties": {"enabled": {"type": "boolean"}}}"#,
        )
        .expect("the schema compiles");
        assert_eq!(
            accepted(&schema, "AppConfig(\n    ", "enabled"),
            "AppConfig(\n    enabled: |"
        );
    }

    #[test]
    fn accepted_enum_field_writes_its_default_variant_selected() {
        let text = "AppConfig(\n    log: Log(\n        ";
        assert_eq!(
            accepted(&schema(), text, "level"),
            "AppConfig(\n    log: Log(\n        level: <Info>"
        );
    }

    #[test]
    fn accepted_array_field_writes_empty_brackets_with_the_caret_inside() {
        let text = "AppConfig(\n    ";
        assert_eq!(
            accepted(&schema(), text, "features"),
            "AppConfig(\n    features: [|]"
        );
    }

    #[test]
    fn accepted_list_of_strings_writes_empty_brackets_with_the_caret_inside() {
        let text = "AppConfig(\n    log: Log(\n        ";
        assert_eq!(
            accepted(&schema(), text, "targets"),
            "AppConfig(\n    log: Log(\n        targets: [|]"
        );
    }

    /// The document after accepting `label` where the caret `|` stands in
    /// `marked`, with the caret and selection marked as in [`accepted`].
    fn accepted_at(schema: &Schema, marked: &str, label: &str) -> String {
        let caret = marked.find('|').expect("the marked text has a caret");
        let text = format!("{}{}", &marked[..caret], &marked[caret + 1..]);
        let cursor = marked[..caret].encode_utf16().count();
        let suggestion = schema
            .suggestions(RON, &text, cursor)
            .into_iter()
            .find(|suggestion| suggestion.label() == label)
            .unwrap_or_else(|| panic!("{label} is offered"));
        let (out, range) = suggestion.accept(&text);
        let units: Vec<u16> = out.encode_utf16().collect();
        let part = |from: usize, to: usize| String::from_utf16(&units[from..to]).unwrap();
        if range.start == range.end {
            format!("{}|{}", part(0, range.start), part(range.start, units.len()))
        } else {
            format!(
                "{}<{}>{}",
                part(0, range.start),
                part(range.start, range.end),
                part(range.end, units.len())
            )
        }
    }

    fn layout_schema() -> Schema {
        Schema::compile(
            r#"{"type": "object", "properties": {
                "name": {"type": "string"},
                "version": {"type": "string"},
                "server": {"type": "object", "properties": {
                    "host": {"type": "string", "default": "/burrow"},
                    "port": {"type": "integer", "default": 8080},
                    "nodelay": {"type": "boolean", "default": true}
                }},
                "log": {"type": "object", "properties": {"level": {"type": "string"}}}
            }}"#,
        )
        .expect("the schema compiles")
    }

    #[test]
    fn accepted_field_takes_the_struct_indent_and_a_comma_when_fields_follow() {
        assert_eq!(
            accepted_at(
                &layout_schema(),
                "AppConfig(\n    server: Server(\n        host: \"a\",\n        |\n        port: 8080,\n    ),\n)",
                "nodelay",
            ),
            "AppConfig(\n    server: Server(\n        host: \"a\",\n        nodelay: <true>,\n        port: 8080,\n    ),\n)"
        );
    }

    #[test]
    fn accepted_field_typed_at_column_zero_takes_its_siblings_indent() {
        assert_eq!(
            accepted_at(
                &layout_schema(),
                "AppConfig(\n    server: Server(\n        host: \"a\",\nnod|\n        port: 8080,\n    ),\n)",
                "nodelay",
            ),
            "AppConfig(\n    server: Server(\n        host: \"a\",\n        nodelay: <true>,\n        port: 8080,\n    ),\n)"
        );
    }

    #[test]
    fn accepted_integer_and_boolean_defaults_are_written_bare() {
        assert_eq!(
            accepted_at(
                &layout_schema(),
                "AppConfig(\n    server: Server(\n        host: \"a\",\n        |\n    ),\n)",
                "port",
            ),
            "AppConfig(\n    server: Server(\n        host: \"a\",\n        port: <8080>\n    ),\n)"
        );
    }

    #[test]
    fn accepted_string_default_is_written_quoted_with_the_text_selected() {
        assert_eq!(
            accepted_at(
                &layout_schema(),
                "AppConfig(\n    server: Server(\n        |\n    ),\n)",
                "host",
            ),
            "AppConfig(\n    server: Server(\n        host: \"</burrow>\"\n    ),\n)"
        );
    }

    #[test]
    fn accepted_object_field_opens_an_indented_block_with_the_caret_on_its_own_line() {
        let text = "AppConfig(\n    name: \"x\",\n    ";
        assert_eq!(
            accepted(&schema(), text, "server"),
            "AppConfig(\n    name: \"x\",\n    server: (\n        |\n    )"
        );
    }

    #[test]
    fn accepted_object_field_before_more_fields_ends_with_a_comma() {
        assert_eq!(
            accepted_at(
                &layout_schema(),
                "AppConfig(\n    name: \"x\",\n    |\n    version: \"1\",\n)",
                "server",
            ),
            "AppConfig(\n    name: \"x\",\n    server: (\n        |\n    ),\n    version: \"1\",\n)"
        );
    }

    #[test]
    fn a_key_typed_in_an_empty_file_opens_the_top_level_struct() {
        let suggestions = schema().suggestions(RON, "", 0);
        let name = suggestions
            .iter()
            .find(|suggestion| suggestion.label() == "name")
            .expect("name is offered in an empty file");
        assert_eq!(name.accept("").0, "(name: \"\")");
    }

    #[test]
    fn nothing_is_offered_inside_a_comment() {
        let text = "AppConfig(\n    // na";
        assert!(labels(text, text.encode_utf16().count()).is_empty());
    }

    #[test]
    fn a_realistic_config_parses_and_validates() {
        let value = Ron.parse(SAMPLE).expect("the sample parses");
        assert_eq!(
            value,
            json!({
                "name": "billing-api",
                "version": "1.4.2",
                "server": {
                    "host": "0.0.0.0",
                    "port": 8443,
                    "tls": {
                        "cert_path": "/etc/billing/cert.pem",
                        "key_path": "/etc/billing/key.pem"
                    }
                },
                "log": {
                    "level": "Info",
                    "targets": ["stdout", "/var/log/billing.log"]
                },
                "features": ["Metrics", "Tracing"]
            })
        );
        assert!(schema().validate(RON, SAMPLE).is_empty());

        let wrong = SAMPLE.replace("port: 8443", "port: \"eighty\"");
        assert!(!schema().validate(RON, &wrong).is_empty());
    }

    #[test]
    fn some_and_none_read_as_their_value_and_null() {
        let plain = "AppConfig(\n    name: \"a\",\n    version: \"1\",\n    server: Server(\n        host: \"h\",\n        port: 1,\n        tls: None,\n    ),\n    log: Log(level: Warn, targets: []),\n    features: [],\n)";
        let value = Ron.parse(plain).expect("the sample parses");
        assert_eq!(value["server"]["tls"], Value::Null);
        assert_eq!(value["log"]["level"], "Warn");
        assert!(schema().validate(RON, plain).is_empty());

        // `Some(x)` is `x`: the wrapper is not in the data.
        let wrapped = SAMPLE.replace("tls: Some(Tls(", "tls: Some(Tls(");
        let value = Ron.parse(&wrapped).expect("the sample parses");
        assert_eq!(value["server"]["tls"]["key_path"], "/etc/billing/key.pem");
    }

    #[test]
    fn parses_the_remaining_ron_forms() {
        let source = r##"#![enable(implicit_some)]
        /* outer /* nested */ still a comment */
        Config(
            empty: (),
            tuple: (1, "two", 'c'),
            map: { "a": 1, 2: true },
            raw: r#"say "hi" // not a comment"#,
            hex: 0xFF_u8,
            float: 1_000.5e-1,
            neg: -7,
            none: None,
            some: Some(3),
            bare: Some(Plain),
            unit: Plain,
            variant: Tcp(port: 80),
            nested: [[1, 2], []],
            escaped: "tab\tline\n\u{1F600}\x41 \
                joined",
        )"##;
        assert_eq!(
            Ron.parse(source).expect("the forms parse"),
            json!({
                "empty": [],
                "tuple": [1, "two", "c"],
                "map": { "a": 1, "2": true },
                "raw": "say \"hi\" // not a comment",
                "hex": 255,
                "float": 100.05,
                "neg": -7,
                "none": null,
                "some": 3,
                "bare": "Plain",
                "unit": "Plain",
                "variant": { "port": 80 },
                "nested": [[1, 2], []],
                "escaped": "tab\tline\n\u{1F600}A joined"
            })
        );
    }

    #[test]
    fn unsupported_syntax_is_a_parse_error_with_its_position() {
        let cases = [
            ("Config(\n    data: b\"raw\",\n)", 2, "byte strings"),
            ("Config(\n    limit: inf,\n)", 2, "infinite"),
            ("Config(\n    span: 1..5,\n)", 2, "ranges"),
            ("#![enable(unwrap_newtypes)]\nConfig()", 1, "extension"),
            ("Config(\n    a: 1,\n    a: 2,\n)", 3, "duplicate"),
            ("AppConfig(\n    name: \"billing-api\",\n", 3, "expected `)`"),
            ("AppConfig(\n    server: Server(port: 80x80),\n)", 2, "invalid number"),
        ];
        for (source, line, message) in cases {
            let error = Ron.parse(source).expect_err(source);
            assert_eq!(error.line, Some(line), "{source:?}");
            assert!(
                error.message.contains(message),
                "{source:?}: {}",
                error.message
            );
        }
    }

    #[test]
    fn spans_locate_keys_and_values_for_diagnostics() {
        let path = FieldPath::from("/server/port");
        let key = Ron.key_span(SAMPLE, &path).expect("the key span");
        assert_eq!(text_of(SAMPLE, key), "port");
        let value = Ron.value_span(SAMPLE, &path).expect("the value span");
        assert_eq!(text_of(SAMPLE, value), "8443");
    }

    #[test]
    fn format_value_writes_schema_values_as_ron() {
        assert_eq!(Ron.format_value(&json!("Info")), "Info");
        assert_eq!(Ron.format_value(&json!("0.0.0.0")), "\"0.0.0.0\"");
        assert_eq!(Ron.format_value(&json!(8080)), "8080");
        assert_eq!(Ron.format_value(&json!(true)), "true");
        // Identifier-like strings are bare at every depth, as unit variants are.
        assert_eq!(Ron.format_value(&json!(["a", "b"])), "[a, b]");
        assert_eq!(Ron.format_value(&json!("two words")), "\"two words\"");
        assert_eq!(Ron.format_value(&json!({"port": 1})), "(port: 1)");
    }

    #[test]
    fn a_broken_record_does_not_offer_its_written_fields() {
        let text = "AppConfig(\n    name: \"billing-api\",\n    log: Log(level: Info\n    ";
        let cursor = text.encode_utf16().count();
        let labels: Vec<String> = offered(text, cursor).into_iter().map(|(l, _)| l).collect();
        assert!(!labels.contains(&"name".to_string()), "{labels:?}");
        assert!(!labels.contains(&"log".to_string()), "{labels:?}");
    }

    /// The (text, kind) of each token `line` is colored with.
    fn colored(line: &str) -> Vec<(&str, TokenKind)> {
        Ron.highlight(line)
            .into_iter()
            .map(|token| (&line[token.start..token.end], token.kind))
            .collect()
    }

    #[test]
    fn highlight_colors_names_fields_strings_comments_and_literals() {
        let line = "    server: Server(host: \"0.0.0.0\", port: 8080) // on";
        let tokens = colored(line);
        assert!(tokens.contains(&("server", TokenKind::Key)), "{tokens:?}");
        assert!(tokens.contains(&("Server", TokenKind::Key)), "{tokens:?}");
        assert!(tokens.contains(&("host", TokenKind::Key)), "{tokens:?}");
        assert!(tokens.contains(&("\"0.0.0.0\"", TokenKind::String)), "{tokens:?}");
        assert!(tokens.contains(&("8080", TokenKind::Literal)), "{tokens:?}");
        assert!(tokens.contains(&("(", TokenKind::Punctuation)), "{tokens:?}");
        assert!(tokens.contains(&("// on", TokenKind::Comment)), "{tokens:?}");
        assert!(
            colored("    level: Info, tls: None, on: true")
                .iter()
                .any(|(text, kind)| *text == "None" && *kind == TokenKind::Literal),
        );
        assert!(colored("    x: Other").contains(&("Other", TokenKind::Key)));
        assert!(colored("    x: other").contains(&("other", TokenKind::Value)));
    }

    #[test]
    fn highlight_colors_raw_strings_and_chars() {
        let tokens = colored("    doc: r#\"a \"quoted\" b\"#, c: 'x'");
        assert!(
            tokens.contains(&("r#\"a \"quoted\" b\"#", TokenKind::String)),
            "{tokens:?}"
        );
        assert!(tokens.contains(&("'x'", TokenKind::String)), "{tokens:?}");
    }

    #[test]
    fn comments_and_strings_over_lines_are_found_whole() {
        let source = "a: 1, /* x /* y */\n z */ b: 2\nc: r#\"one\ntwo\"#";
        let comments = Ron.comments(&Ron, source);
        assert_eq!(comments.len(), 1);
        assert_eq!(&source[comments[0].clone()], "/* x /* y */\n z */");
        let strings = Ron.multiline_strings(source);
        assert_eq!(strings.len(), 1);
        assert_eq!(&source[strings[0].clone()], "r#\"one\ntwo\"#");
    }

    #[test]
    fn serialize_writes_objects_arrays_and_scalars_as_ron() {
        let value = json!({
            "name": "x",
            "log": {"level": "Info", "targets": ["stdout"]},
            "tls": null,
            "features": [],
        });
        let text = Ron.serialize(&value, None).expect("serializes");
        assert_eq!(
            text,
            "(\n    name: \"x\",\n    log: (\n        level: \"Info\",\n        targets: [\"stdout\"],\n    ),\n    tls: None,\n    features: [],\n)\n"
        );
        assert_eq!(Ron.parse(&text).unwrap(), value);
    }

    #[test]
    fn serialize_writes_non_identifier_keys_as_a_map() {
        let value = json!({"a-b": 1, "c": [true, null]});
        let text = Ron.serialize(&value, None).expect("serializes");
        assert_eq!(Ron.parse(&text).unwrap(), value, "{text}");
    }

    #[test]
    fn value_accepted_inside_an_empty_string_does_not_double_the_quotes() {
        // The caret is inside `name: ""`. Completion is suppressed inside an
        // open string, so no value is offered there; any value that is
        // offered must replace the quoted content, not add a second pair.
        let text = "(\n    name: \"\",\n)\n";
        let cursor = "(\n    name: \"".encode_utf16().count();
        let schema = Schema::compile(RON_SCHEMA).expect("the schema compiles");
        for suggestion in schema.suggestions(RON, text, cursor) {
            let (out, _) = suggestion.accept(text);
            assert!(
                !out.contains("\"\"\""),
                "{} doubled the quotes: {out}",
                suggestion.label()
            );
            assert!(
                !out.contains("name: \"\"\""),
                "{} doubled the quotes: {out}",
                suggestion.label()
            );
        }
    }

    #[test]
    fn edit_as_yaml_and_back_keeps_the_data() {
        let sample = "AppConfig(\n    name: \"billing-api\",\n    version: \"1.4.2\",\n    server: Server(\n        host: \"0.0.0.0\",\n        port: 8080,\n        tls: None,\n    ),\n    log: Log(\n        level: Info,\n        targets: [\"stdout\"],\n    ),\n    features: [Metrics],\n)\n";
        let yaml = RON.convert(sample, Language::YAML).expect("RON converts to YAML");
        let back = Language::YAML
            .convert(&yaml.text, RON)
            .expect("YAML converts back to RON");
        assert_eq!(
            Ron.parse(&back.text).unwrap(),
            Ron.parse(sample).unwrap(),
            "{}",
            back.text
        );
        assert!(back.losses.is_empty(), "{:?}", back.losses);
    }
}

