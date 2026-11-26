//! Nickel configuration language.
//!
//! Nickel is a full language: `let` bindings, functions, contracts, imports
//! and merging are evaluated by Nickel itself, which this editor does not do.
//! What the editor shows is the data the file's top-level record holds, read
//! directly from its syntax:
//!
//! - records `{ a = 1, b.c = "x" }` (dotted field names nest), arrays
//!   `[ 1, 2 ]`, strings `"..."` and `m%"..."%` (escapes decoded in the first
//!   form only), numbers, `true`, `false`, `null`, and enum tags `'info`,
//!   which read as the string `info`;
//! - `let name = value in body`, with `name` usable later as a value, and
//!   `field | Contract = value` or `field : Type = value` annotations, which
//!   are skipped (they do not change the data);
//! - a value that is a function (`fun`), an `import`, an `if`, a `match`, an
//!   operator expression or an application is not evaluated: that field is left
//!   out of the data, and an array holding one is left out whole.
//!
//! Hover and Ctrl+Space work on the text as typed, so they read the syntax
//! around the caret and do not need the file to parse.
//!
//! A string written as a tag (`log_level = 'info`) and a string value are the
//! same data, so a string that is an identifier is written as a tag when the
//! editor inserts a value (`format_value`). The editor has no schema for the
//! value at that point, so a plain string default that looks like an
//! identifier is also written as a tag. The schema here has no string
//! defaults.
//!
//! Edit as writes the record the file holds, not the file's program: a `let`
//! is evaluated away, so `let base = 8080 in { port = base }` is written as
//! `{ port = 8080 }`, and the `let` itself is not kept. A value that has no
//! Nickel form (an infinity or NaN) is an error, not a string. When the
//! previous text is given, each string keeps the form it has there (a tag or
//! a quoted string).

use std::collections::HashMap;

use leptos_config_editor::language::{
    CompletionContext, ConfigLanguage, Hover, InsertContext, InsertPlan, LanguageConfig,
    LanguageInfo, LanguageTraits, MultilineConstruct, ParseError, Planned, SchemaPathSegment, Span,
    Text, TokenKind, Tokens, utf16_to_byte_index,
};
use leptos_config_editor::{FieldPath, Language, float_of, string_of};
use serde_json::{Map, Number, Value};

/// A highlighted run of a line, as the crate's token.
type Highlight = leptos_config_editor::language::Token;

/// A path of keys and array indices from the document's top level.
type Path = Vec<SchemaPathSegment>;

/// The Nickel language, for `ConfigEditor`, conversion and schema checks.
pub const NICKEL: Language = Language::new(&Nickel);

/// A JSON Schema for a service configuration written in Nickel.
#[cfg(test)]
pub const NICKEL_SCHEMA: &str = r#"{
  "type": "object",
  "title": "Service configuration",
  "description": "A Nickel file whose top-level record is the service configuration. Nickel evaluates the file; the editor shows the record it produces.",
  "x-docs-url": "https://nickel-lang.org",
  "required": ["name", "port"],
  "properties": {
    "name": {
      "type": "string",
      "description": "Name of the service, used in logs and metrics."
    },
    "port": {
      "type": "integer",
      "minimum": 1,
      "maximum": 65535,
      "description": "TCP port the service listens on."
    },
    "log_level": {
      "description": "Lowest severity written to the log. In Nickel it is written as a tag, such as 'info.",
      "enum": ["debug", "info", "warn", "error"],
      "x-enum-descriptions": [
        "Detailed output for development and debugging.",
        "Normal operation: started, stopped, requests served.",
        "Something unexpected happened that the service recovered from.",
        "A failure that needs attention."
      ]
    },
    "database": {
      "type": "object",
      "description": "Connection to the database the service reads from.",
      "required": ["host", "port"],
      "properties": {
        "host": {
          "type": "string",
          "description": "Host name or address of the database server."
        },
        "port": {
          "type": "integer",
          "minimum": 1,
          "maximum": 65535,
          "description": "Port of the database server."
        },
        "pool_size": {
          "type": "integer",
          "minimum": 1,
          "description": "Number of connections kept open to the database."
        }
      }
    },
    "features": {
      "type": "array",
      "description": "Optional features to switch on, written as a list of tags.",
      "uniqueItems": true,
      "items": {
        "enum": ["metrics", "tracing", "cache"],
        "x-enum-descriptions": [
          "Expose request and error counters.",
          "Record a trace span for each request.",
          "Keep recent responses in memory."
        ]
      }
    }
  }
}"#;

/// The Nickel language as the editor's `ConfigLanguage`.
struct Nickel;

// ---------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    /// An identifier or keyword.
    Ident(String),
    /// A string: its text (escapes decoded for `"..."`) and whether it
    /// interpolates (`%{`), which makes its value unreadable here.
    Str(String, bool),
    /// A number as written (`8080`, `1.5e3`, `0x1F`).
    Num(String),
    /// An enum tag, without its quote.
    Tag(String),
    /// Punctuation and reserved operators.
    P(&'static str),
    /// Any other operator, such as `==` or `@`.
    Op(String),
}

#[derive(Debug, Clone)]
struct Token {
    tok: Tok,
    /// Byte offsets in the source.
    start: usize,
    end: usize,
}

#[derive(Debug, Default)]
struct Lexed {
    tokens: Vec<Token>,
    /// Byte ranges of `#` comments.
    comments: Vec<std::ops::Range<usize>>,
    /// The first error: its byte offset and message.
    error: Option<(usize, String)>,
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '\'')
}

fn is_op_char(c: char) -> bool {
    matches!(
        c,
        '=' | '<' | '>' | '!' | '&' | '|' | '+' | '-' | '*' | '/' | '%' | '^' | '@' | '?'
    )
}

fn text_of(cs: &[(usize, char)], from: usize, to: usize) -> String {
    cs[from..to].iter().map(|&(_, c)| c).collect()
}

fn run_end(cs: &[(usize, char)], from: usize, keep: impl Fn(char) -> bool) -> usize {
    let mut end = from;
    while end < cs.len() && keep(cs[end].1) {
        end += 1;
    }
    end
}

/// A string at `open` (its quote): its decoded text, whether it interpolates,
/// and the index after the closing quote.
fn lex_string(cs: &[(usize, char)], open: usize) -> Result<(String, bool, usize), (String, usize)> {
    let at = |i: usize| cs.get(i).map(|&(_, c)| c);
    let mut text = String::new();
    let mut interpolated = false;
    let mut i = open + 1;
    loop {
        match at(i) {
            None => return Err(("unterminated string".into(), cs.len())),
            Some('"') => return Ok((text, interpolated, i + 1)),
            Some('\\') => {
                let escaped = at(i + 1).ok_or(("unterminated string".to_string(), cs.len()))?;
                match escaped {
                    'n' => text.push('\n'),
                    't' => text.push('\t'),
                    'r' => text.push('\r'),
                    '\\' | '"' | '%' => text.push(escaped),
                    'x' => {
                        let digits: String = (i + 2..i + 4).filter_map(at).collect();
                        let code = u32::from_str_radix(&digits, 16)
                            .ok()
                            .and_then(char::from_u32)
                            .ok_or(("bad \\x escape".to_string(), i + 2))?;
                        text.push(code);
                        i += 4;
                        continue;
                    }
                    'u' => {
                        if at(i + 2) != Some('{') {
                            return Err(("expected `{` after `\\u`".into(), i + 2));
                        }
                        let close = run_end(cs, i + 3, |c| c != '}');
                        let digits = text_of(cs, i + 3, close);
                        let code = u32::from_str_radix(&digits, 16)
                            .ok()
                            .and_then(char::from_u32)
                            .ok_or(("bad \\u escape".to_string(), i + 3))?;
                        text.push(code);
                        i = close + 1;
                        continue;
                    }
                    other => return Err((format!("unknown escape `\\{other}`"), i + 1)),
                }
                i += 2;
            }
            Some('%') if at(i + 1) == Some('{') => {
                interpolated = true;
                text.push('%');
                i += 1;
            }
            Some(c) => {
                text.push(c);
                i += 1;
            }
        }
    }
}

/// A raw string `%"..."%` whose `%` run starts at `from`.
fn lex_raw(cs: &[(usize, char)], from: usize) -> Result<(String, bool, usize), (String, usize)> {
    let at = |i: usize| cs.get(i).map(|&(_, c)| c);
    let mut hashes = 0;
    let mut j = from;
    while at(j) == Some('%') {
        hashes += 1;
        j += 1;
    }
    let body = j + 1;
    let mut k = body;
    while k < cs.len() {
        if cs[k].1 == '"' && (1..=hashes).all(|d| at(k + d) == Some('%')) {
            let raw = text_of(cs, body, k);
            let interpolated = raw.contains("%{");
            return Ok((raw, interpolated, k + 1 + hashes));
        }
        k += 1;
    }
    Err(("unterminated multiline string".into(), cs.len()))
}

/// The token starting at `i` (not whitespace or a comment): it and the index
/// after it, or an error and where lexing resumes.
fn lex_token(cs: &[(usize, char)], i: usize) -> Result<(Tok, usize), (String, usize)> {
    let at = |k: usize| cs.get(k).map(|&(_, c)| c);
    let c = cs[i].1;
    if c == '_' || c.is_ascii_alphabetic() {
        let end = run_end(cs, i, is_ident_char);
        let word = text_of(cs, i, end);
        if at(end) == Some('%')
            && (word == "m" || word.ends_with("-s"))
            && at(run_end(cs, end, |c| c == '%')) == Some('"')
        {
            let (raw, interpolated, next) = lex_raw(cs, end)?;
            return Ok((Tok::Str(raw, interpolated || word != "m"), next));
        }
        return Ok((Tok::Ident(word), end));
    }
    if c.is_ascii_digit() {
        let mut end = i;
        if c == '0' && matches!(at(i + 1), Some('x' | 'o' | 'b')) {
            end = run_end(cs, i + 2, |c| c.is_ascii_hexdigit());
        } else {
            end = run_end(cs, end, |c| c.is_ascii_digit());
            if at(end) == Some('.') && at(end + 1).is_some_and(|c| c.is_ascii_digit()) {
                end = run_end(cs, end + 1, |c| c.is_ascii_digit());
            }
            if matches!(at(end), Some('e' | 'E')) {
                let mut k = end + 1;
                if matches!(at(k), Some('+' | '-')) {
                    k += 1;
                }
                if at(k).is_some_and(|c| c.is_ascii_digit()) {
                    end = run_end(cs, k, |c| c.is_ascii_digit());
                }
            }
        }
        return Ok((Tok::Num(text_of(cs, i, end)), end));
    }
    if c == '"' {
        let (text, interpolated, next) = lex_string(cs, i)?;
        return Ok((Tok::Str(text, interpolated), next));
    }
    if c == '\'' {
        if at(i + 1) == Some('"') {
            let (name, _, next) = lex_string(cs, i + 1)?;
            return Ok((Tok::Tag(name), next));
        }
        let end = run_end(cs, i + 1, is_ident_char);
        if end == i + 1 {
            return Err(("expected a tag name after `'`".into(), i + 1));
        }
        return Ok((Tok::Tag(text_of(cs, i + 1, end)), end));
    }
    let single = |tok: &'static str| Ok((Tok::P(tok), i + 1));
    match c {
        '{' => single("{"),
        '}' => single("}"),
        '(' => single("("),
        ')' => single(")"),
        ']' => single("]"),
        ',' => single(","),
        ':' => single(":"),
        '.' => single("."),
        '[' if at(i + 1) == Some('|') => Ok((Tok::P("[|"), i + 2)),
        '[' => single("["),
        '|' if at(i + 1) == Some(']') => Ok((Tok::P("|]"), i + 2)),
        _ if is_op_char(c) => {
            let end = run_end(cs, i, is_op_char);
            let run = text_of(cs, i, end);
            let tok = match run.as_str() {
                "=" => Tok::P("="),
                "=>" => Tok::P("=>"),
                "|" => Tok::P("|"),
                "-" => Tok::P("-"),
                _ => Tok::Op(run),
            };
            Ok((tok, end))
        }
        _ => Err((format!("unexpected character `{c}`"), i + 1)),
    }
}

/// The tokens of `src`, skipping whitespace and comments. Lexing goes on past
/// an error, so the tokens before and after it are still there.
fn lex(src: &str) -> Lexed {
    let cs: Vec<(usize, char)> = src.char_indices().collect();
    let byte = |i: usize| cs.get(i).map_or(src.len(), |&(at, _)| at);
    let mut out = Lexed::default();
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i].1;
        if c.is_whitespace() {
            i += 1;
        } else if c == '#' {
            let start = i;
            i = run_end(&cs, i, |c| c != '\n');
            out.comments.push(byte(start)..byte(i));
        } else {
            match lex_token(&cs, i) {
                Ok((tok, next)) => {
                    out.tokens.push(Token {
                        tok,
                        start: byte(i),
                        end: byte(next),
                    });
                    i = next;
                }
                Err((message, resume)) => {
                    out.error.get_or_insert((byte(i), message));
                    i = resume.max(i + 1);
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Full parser: the data and the spans of each field
// ---------------------------------------------------------------------------

/// Where a field (or array item) is written: its path, its key and its value.
#[derive(Debug, Clone)]
struct Site {
    path: Path,
    key: Span,
    value: Span,
}

/// What an expression reads as.
enum Expr {
    /// Its data.
    Value(Value),
    /// Something this reader does not evaluate; its field is left out.
    Opaque,
}

struct Parser<'a> {
    src: &'a str,
    line_starts: Vec<usize>,
    toks: Vec<Token>,
    pos: usize,
    /// `let` names in scope, with their values where they have data.
    env: HashMap<String, Value>,
    sites: Vec<Site>,
}

fn line_starts(src: &str) -> Vec<usize> {
    std::iter::once(0)
        .chain(src.match_indices('\n').map(|(at, _)| at + 1))
        .collect()
}

fn parse_document(src: &str) -> Result<(Value, Vec<Site>), ParseError> {
    let lexed = lex(src);
    let mut parser = Parser {
        src,
        line_starts: line_starts(src),
        toks: lexed.tokens,
        pos: 0,
        env: HashMap::new(),
        sites: Vec::new(),
    };
    if let Some((byte, message)) = lexed.error {
        // A `;` right after `=` is a field with no value, not a stray character.
        let after_equals = parser
            .toks
            .iter()
            .rev()
            .find(|token| token.end <= byte)
            .is_some_and(|token| token.tok == Tok::P("="));
        let message = if after_equals && src[byte..].starts_with(';') {
            "expected a value after `=`".to_string()
        } else {
            message
        };
        return Err(parser.error_at(byte, message));
    }
    let first = parser.byte_at(0);
    let value = match parser.expr(Some(&Vec::new()))? {
        Expr::Value(Value::Object(map)) => Value::Object(map),
        Expr::Value(_) => return Err(parser.error_at(first, "the top level must be a record")),
        Expr::Opaque => {
            return Err(parser.error_at(
                first,
                "the top level is not a record literal, so its data cannot be read",
            ));
        }
    };
    if parser.pos < parser.toks.len() {
        return Err(parser.error_here("unexpected token after the top-level value"));
    }
    Ok((value, parser.sites))
}

/// The sites of `source`, or none where it does not parse.
fn sites(source: &str) -> Vec<Site> {
    parse_document(source)
        .map(|(_, sites)| sites)
        .unwrap_or_default()
}

fn site_for<'s>(sites: &'s [Site], path: &FieldPath) -> Option<&'s Site> {
    sites
        .iter()
        .find(|site| site.path.as_slice() == path.segments())
}

/// `{ a.b = 1 }` as nested records.
fn insert_nested(map: &mut Map<String, Value>, keys: &[String], value: Value) {
    match keys {
        [] => {}
        [last] => {
            map.insert(last.clone(), value);
        }
        [first, rest @ ..] => {
            let entry = map
                .entry(first.clone())
                .or_insert_with(|| Value::Object(Map::new()));
            if !entry.is_object() {
                *entry = Value::Object(Map::new());
            }
            if let Value::Object(inner) = entry {
                insert_nested(inner, rest, value);
            }
        }
    }
}

/// A number token as data, negated when `negative`.
fn number(text: &str, negative: bool) -> Option<Value> {
    let signed = |n: i64| Value::from(if negative { -n } else { n });
    for (prefix, radix) in [("0x", 16), ("0o", 8), ("0b", 2)] {
        if let Some(digits) = text.strip_prefix(prefix) {
            return i64::from_str_radix(digits, radix).ok().map(signed);
        }
    }
    if text.contains(['.', 'e', 'E']) {
        let float: f64 = text.parse().ok()?;
        let float = if negative { -float } else { float };
        return Number::from_f64(float).map(Value::Number);
    }
    text.parse::<i64>().ok().map(signed)
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|token| &token.tok)
    }

    fn is(&self, punct: &str) -> bool {
        matches!(self.peek(), Some(Tok::P(p)) if *p == punct)
    }

    fn is_word(&self, word: &str) -> bool {
        matches!(self.peek(), Some(Tok::Ident(w)) if w == word)
    }

    fn byte_at(&self, index: usize) -> usize {
        self.toks
            .get(index)
            .map_or(self.src.len(), |token| token.start)
    }

    fn locate(&self, byte: usize) -> (usize, usize) {
        let line = self.line_starts.partition_point(|&start| start <= byte);
        let column = self.src[self.line_starts[line - 1]..byte].chars().count() + 1;
        (line, column)
    }

    fn error_at(&self, byte: usize, message: impl Into<String>) -> ParseError {
        let (line, column) = self.locate(byte);
        ParseError {
            message: message.into(),
            line: Some(line),
            column: Some(column),
            quotes: None,
        }
    }

    fn error_here(&self, message: &str) -> ParseError {
        self.error_at(self.byte_at(self.pos), message)
    }

    fn expect(&mut self, punct: &str) -> Result<(), ParseError> {
        if self.is(punct) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.error_here(&format!("expected `{punct}`")))
        }
    }

    /// The span from token `first` through token `last` (inclusive). A token
    /// spanning lines is cut to its first line.
    fn span(&self, first: usize, last: usize) -> Span {
        let start = self.byte_at(first);
        let end = self.toks.get(last).map_or(start, |token| token.end);
        let (line, start_column) = self.locate(start);
        let first_line = self.src[start..end].split('\n').next().unwrap_or("");
        Span::from((
            line,
            start_column,
            start_column + first_line.chars().count(),
        ))
    }

    /// Skips tokens up to the `,`, closer or `in` that ends the expression.
    fn skip_expr(&mut self) {
        let mut depth = 0usize;
        let mut lets = 0usize;
        while let Some(tok) = self.peek() {
            match tok {
                Tok::P("(" | "{" | "[" | "[|") => depth += 1,
                Tok::P(")" | "}" | "]" | "|]") => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                }
                Tok::P(",") if depth == 0 => break,
                Tok::Ident(word) if word == "let" => lets += 1,
                Tok::Ident(word) if word == "in" => {
                    if lets > 0 {
                        lets -= 1;
                    } else if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            self.pos += 1;
        }
    }

    /// Skips a `| Contract` or `: Type` annotation up to its `=`, or to the
    /// end of the entry when there is none.
    fn skip_annotation(&mut self) {
        let mut depth = 0usize;
        while let Some(tok) = self.peek() {
            match tok {
                Tok::P("(" | "{" | "[" | "[|") => depth += 1,
                Tok::P(")" | "}" | "]" | "|]") => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                }
                Tok::P("=" | ",") if depth == 0 => break,
                Tok::Ident(word) if depth == 0 && word == "in" => break,
                _ => {}
            }
            self.pos += 1;
        }
    }

    fn at_terminator(&self) -> bool {
        match self.peek() {
            None => true,
            Some(Tok::P("," | "}" | "]" | ")")) => true,
            Some(Tok::Ident(word)) => word == "in",
            _ => false,
        }
    }

    /// One expression. `path` is where its data goes, or `None` when it is not
    /// part of the data (a `let` value), so no sites are recorded for it.
    fn expr(&mut self, path: Option<&Path>) -> Result<Expr, ParseError> {
        let value = self.primary(path)?;
        if self.at_terminator() {
            return Ok(value);
        }
        if matches!(value, Expr::Value(_)) && (self.is(":") || self.is("|")) {
            // A contract does not change the data.
            self.skip_expr();
            return Ok(value);
        }
        self.skip_expr();
        Ok(Expr::Opaque)
    }

    fn primary(&mut self, path: Option<&Path>) -> Result<Expr, ParseError> {
        let Some(token) = self.toks.get(self.pos) else {
            return Err(self.error_here("expected a value"));
        };
        let tok = token.tok.clone();
        match tok {
            Tok::P("{") => {
                let map = self.record(path)?;
                Ok(Expr::Value(Value::Object(map)))
            }
            Tok::P("[") => self.array(path),
            Tok::P("(") => {
                self.pos += 1;
                let inner = self.expr(path)?;
                self.expect(")")?;
                Ok(inner)
            }
            Tok::P("-") => match self.toks.get(self.pos + 1).map(|t| t.tok.clone()) {
                Some(Tok::Num(text)) => {
                    self.pos += 2;
                    Ok(number(&text, true).map_or(Expr::Opaque, Expr::Value))
                }
                _ => {
                    self.skip_expr();
                    Ok(Expr::Opaque)
                }
            },
            Tok::Num(text) => {
                self.pos += 1;
                Ok(number(&text, false).map_or(Expr::Opaque, Expr::Value))
            }
            Tok::Str(text, interpolated) => {
                self.pos += 1;
                Ok(if interpolated {
                    Expr::Opaque
                } else {
                    Expr::Value(Value::String(text))
                })
            }
            Tok::Tag(name) => {
                self.pos += 1;
                Ok(Expr::Value(Value::String(name)))
            }
            Tok::Ident(word) => self.word(word.as_str(), path),
            Tok::Op(_) | Tok::P("[|") => {
                self.skip_expr();
                Ok(Expr::Opaque)
            }
            _ => Err(self.error_here("expected a value")),
        }
    }

    /// A value starting with the identifier or keyword `word`.
    fn word(&mut self, word: &str, path: Option<&Path>) -> Result<Expr, ParseError> {
        match word {
            "true" | "false" => {
                self.pos += 1;
                Ok(Expr::Value(Value::Bool(word == "true")))
            }
            "null" => {
                self.pos += 1;
                Ok(Expr::Value(Value::Null))
            }
            "let" => self.let_expr(path),
            "fun" | "if" | "match" | "import" => {
                self.pos += 1;
                self.skip_expr();
                Ok(Expr::Opaque)
            }
            "in" => Err(self.error_here("expected a value")),
            _ => {
                self.pos += 1;
                let mut value = self.env.get(word).cloned();
                // `name.field` reads a field of a `let` value.
                while self.is(".") {
                    let Some(Tok::Ident(field)) =
                        self.toks.get(self.pos + 1).map(|t| t.tok.clone())
                    else {
                        break;
                    };
                    self.pos += 2;
                    value = value.and_then(|v| v.get(&field).cloned());
                }
                Ok(value.map_or(Expr::Opaque, Expr::Value))
            }
        }
    }

    /// `let a = v, b = w in body`: the bindings are in scope in `body` only.
    fn let_expr(&mut self, path: Option<&Path>) -> Result<Expr, ParseError> {
        self.pos += 1;
        if self.is_word("rec") {
            self.pos += 1;
        }
        let mut previous: Vec<(String, Option<Value>)> = Vec::new();
        loop {
            let Some(Tok::Ident(name)) = self.peek().cloned() else {
                return Err(self.error_here("expected a name after `let`"));
            };
            self.pos += 1;
            if self.is("|") || self.is(":") {
                self.skip_annotation();
            }
            self.expect("=")?;
            let bound = match self.expr(None)? {
                Expr::Value(value) => self.env.insert(name.clone(), value),
                Expr::Opaque => self.env.remove(&name),
            };
            previous.push((name, bound));
            if self.is(",") {
                self.pos += 1;
                continue;
            }
            break;
        }
        if !self.is_word("in") {
            return Err(self.error_here("expected `in` after the let bindings"));
        }
        self.pos += 1;
        let body = self.expr(path);
        for (name, bound) in previous.into_iter().rev() {
            match bound {
                Some(value) => self.env.insert(name, value),
                None => self.env.remove(&name),
            };
        }
        body
    }

    fn field_name(&mut self) -> Result<String, ParseError> {
        let name = match self.peek() {
            Some(Tok::Ident(word)) => word.clone(),
            Some(Tok::Str(text, false)) => text.clone(),
            _ => return Err(self.error_here("expected a field name")),
        };
        self.pos += 1;
        Ok(name)
    }

    /// A record `{ ... }`, with its fields' sites recorded under `path`.
    fn record(&mut self, path: Option<&Path>) -> Result<Map<String, Value>, ParseError> {
        self.expect("{")?;
        let mut map = Map::new();
        while !self.is("}") {
            if self.peek().is_none() {
                return Err(self.error_here("expected `}`"));
            }
            let key_first = self.pos;
            let mut keys = vec![self.field_name()?];
            while self.is(".") {
                self.pos += 1;
                keys.push(self.field_name()?);
            }
            let key_last = self.pos - 1;
            if self.is("|") || self.is(":") {
                self.skip_annotation();
            }
            self.expect("=")?;
            let value_at = self.pos;
            let field_path = path.map(|base| {
                let mut field = base.clone();
                field.extend(keys.iter().cloned().map(SchemaPathSegment::Key));
                field
            });
            if let Expr::Value(value) = self.expr(field_path.as_ref())? {
                if let Some(path) = field_path {
                    let site = Site {
                        path,
                        key: self.span(key_first, key_last),
                        value: self.span(value_at, value_at),
                    };
                    self.sites.push(site);
                }
                insert_nested(&mut map, &keys, value);
            }
            if self.is(",") {
                self.pos += 1;
            } else if !self.is("}") {
                return Err(self.error_here("expected `,` or `}` after a field"));
            }
        }
        self.expect("}")?;
        Ok(map)
    }

    /// An array `[ ... ]`. One item that is not read makes the whole array
    /// unread, since the indices of the items after it would be wrong.
    fn array(&mut self, path: Option<&Path>) -> Result<Expr, ParseError> {
        self.expect("[")?;
        let mut items = Vec::new();
        let mut opaque = false;
        let mut index = 0;
        while !self.is("]") {
            let item_path = path.map(|base| {
                let mut item = base.clone();
                item.push(SchemaPathSegment::ArrayItem(index));
                item
            });
            let first = self.pos;
            match self.expr(item_path.as_ref())? {
                Expr::Value(value) => {
                    if let Some(item) = item_path {
                        let span = self.span(first, first);
                        self.sites.push(Site {
                            path: item,
                            key: span,
                            value: span,
                        });
                    }
                    items.push(value);
                }
                Expr::Opaque => opaque = true,
            }
            index += 1;
            if self.is(",") {
                self.pos += 1;
            } else if !self.is("]") {
                return Err(self.error_here("expected `,` or `]` in an array"));
            }
        }
        self.expect("]")?;
        Ok(if opaque {
            Expr::Opaque
        } else {
            Expr::Value(Value::Array(items))
        })
    }
}

// ---------------------------------------------------------------------------
// Structure scan: the containers around the caret, for completion and hover
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Record,
    Array,
}

/// A container open at the caret, and what its entry being written holds.
#[derive(Debug)]
struct Frame {
    kind: Kind,
    /// The path of the container from the top level.
    path: Path,
    /// Inside something not read as data (a function body, a `let` value).
    opaque: bool,
    /// Array: the index of the item being written.
    index: usize,
    /// Record: the keys of the entry being written (`a.b` gives two).
    cur_keys: Vec<String>,
    /// Record: between the key and its `=`, inside a `| Contract`.
    annotating: bool,
    /// Brackets open inside a `| Contract`.
    ann_depth: usize,
    /// Record: a `let` was seen, its name not yet followed by `=`.
    let_pending: bool,
    /// Record: inside a `let` binding's value, up to its `in`.
    let_value: bool,
    /// Record: the entry's value is a function, `if`, `match` or `import`.
    opaque_value: bool,
    /// Record: the entry's `=` was seen; its value follows.
    in_value: bool,
    /// Record: the keys of the entry's value, once `in_value`.
    value_keys: Vec<String>,
    /// Record: tokens of the value written so far.
    value_tokens: usize,
}

impl Frame {
    fn new(kind: Kind, path: Path, opaque: bool) -> Self {
        Frame {
            kind,
            path,
            opaque,
            index: 0,
            cur_keys: Vec::new(),
            annotating: false,
            ann_depth: 0,
            let_pending: false,
            let_value: false,
            opaque_value: false,
            in_value: false,
            value_keys: Vec::new(),
            value_tokens: 0,
        }
    }

    /// Whether the next name is a key of this record.
    fn key_slot(&self) -> bool {
        self.kind == Kind::Record
            && !self.in_value
            && !self.annotating
            && !self.let_pending
            && !self.let_value
    }
}

fn with_keys(base: &[SchemaPathSegment], keys: &[String]) -> Path {
    let mut path = base.to_vec();
    path.extend(keys.iter().cloned().map(SchemaPathSegment::Key));
    path
}

/// Inside a `| Contract`: tracks brackets, and returns whether the token was
/// consumed. The entry's `=` ends the contract.
fn annotation_step(top: &mut Frame, tok: &Tok) -> bool {
    match tok {
        Tok::P("(" | "{" | "[" | "[|") => {
            top.ann_depth += 1;
            true
        }
        Tok::P(")" | "}" | "]" | "|]") if top.ann_depth > 0 => {
            top.ann_depth -= 1;
            true
        }
        Tok::P("=") if top.ann_depth == 0 => {
            top.annotating = false;
            top.in_value = true;
            top.value_keys = std::mem::take(&mut top.cur_keys);
            top.value_tokens = 0;
            true
        }
        _ => top.ann_depth > 0,
    }
}

fn open(stack: &mut Vec<Frame>, kind: Kind) {
    let parent = stack.last().expect("the root frame");
    let frame = match parent.kind {
        Kind::Array => {
            let mut path = parent.path.clone();
            path.push(SchemaPathSegment::ArrayItem(parent.index));
            Frame::new(kind, path, parent.opaque)
        }
        Kind::Record if parent.in_value => Frame::new(
            kind,
            with_keys(&parent.path, &parent.value_keys),
            parent.opaque || parent.opaque_value || parent.let_value,
        ),
        // The top level, or the body of a `let`.
        Kind::Record => Frame::new(
            kind,
            parent.path.clone(),
            parent.opaque || parent.let_value || parent.opaque_value,
        ),
    };
    stack.push(frame);
}

fn step(stack: &mut Vec<Frame>, tok: &Tok) {
    if stack
        .last()
        .is_some_and(|top| top.annotating && top.kind == Kind::Record)
        && annotation_step(stack.last_mut().expect("the root frame"), tok)
    {
        return;
    }
    match tok {
        Tok::P("{") => return open(stack, Kind::Record),
        Tok::P("[") => return open(stack, Kind::Array),
        Tok::P("}" | "]") => {
            if stack.len() > 1 {
                stack.pop();
                if let Some(parent) = stack.last_mut()
                    && parent.kind == Kind::Record
                    && parent.in_value
                {
                    parent.value_tokens += 1;
                }
            }
            return;
        }
        _ => {}
    }
    let top = stack.last_mut().expect("the root frame");
    match tok {
        Tok::P(",") => match top.kind {
            Kind::Array => top.index += 1,
            Kind::Record => {
                top.in_value = false;
                top.annotating = false;
                top.ann_depth = 0;
                top.opaque_value = false;
                top.cur_keys.clear();
                top.value_keys.clear();
                top.value_tokens = 0;
                // Another `let` binding follows inside the same `let`.
                top.let_pending = top.let_value;
            }
        },
        Tok::P("=") if top.kind == Kind::Record => {
            if top.let_pending {
                top.let_pending = false;
                top.let_value = true;
            } else if top.in_value {
                top.value_tokens += 1;
            } else if !top.cur_keys.is_empty() {
                top.in_value = true;
                top.value_keys = std::mem::take(&mut top.cur_keys);
                top.value_tokens = 0;
            }
        }
        Tok::P("|" | ":") if top.kind == Kind::Record => {
            if top.key_slot() && !top.cur_keys.is_empty() {
                top.annotating = true;
                top.ann_depth = 0;
            } else if top.in_value {
                top.value_tokens += 1;
            }
        }
        Tok::P("=>") if top.kind == Kind::Record => {
            top.opaque_value = true;
            top.value_tokens += usize::from(top.in_value);
        }
        Tok::Ident(word) if top.kind == Kind::Record => match word.as_str() {
            "let" => {
                top.let_pending = true;
                top.cur_keys.clear();
            }
            "in" => {
                top.let_value = false;
                top.let_pending = false;
                top.in_value = false;
                top.opaque_value = false;
                top.cur_keys.clear();
                top.value_keys.clear();
                top.value_tokens = 0;
            }
            "fun" | "if" | "match" | "import" if top.in_value || top.let_value => {
                top.opaque_value = true;
                top.value_tokens += 1;
            }
            _ if top.key_slot() => top.cur_keys.push(word.clone()),
            _ if top.in_value => top.value_tokens += 1,
            _ => {}
        },
        Tok::Str(text, _) if top.kind == Kind::Record => {
            if top.key_slot() {
                top.cur_keys.push(text.clone());
            } else if top.in_value {
                top.value_tokens += 1;
            }
        }
        _ => {
            if top.kind == Kind::Record && top.in_value {
                top.value_tokens += 1;
            }
        }
    }
}

/// The containers open before byte `byte` (the top-level record first), and
/// the index of the identifier or tag being typed there, if one touches the
/// caret.
fn scan(tokens: &[Token], byte: usize) -> (Vec<Frame>, Option<usize>) {
    let end = tokens.partition_point(|token| token.start < byte);
    let typed = end.checked_sub(1).filter(|&index| {
        tokens[index].end >= byte && matches!(tokens[index].tok, Tok::Ident(_) | Tok::Tag(_))
    });
    let scanned = if typed.is_some() {
        &tokens[..end - 1]
    } else {
        &tokens[..end]
    };
    let mut stack = vec![Frame::new(Kind::Record, Vec::new(), false)];
    for token in scanned {
        step(&mut stack, &token.tok);
    }
    (stack, typed)
}

/// The completion context at byte `byte` of `source`.
fn context_at(source: &str, tokens: &[Token], byte: usize) -> CompletionContext {
    let (stack, typed) = scan(tokens, byte);
    let top = stack.last().expect("the root frame");
    let (replace_from, typed_text) = match typed.map(|index| &tokens[index]) {
        // A tag is typed after its quote.
        Some(token) => match &token.tok {
            Tok::Tag(_) => (token.start + 1, source[token.start + 1..byte].to_string()),
            _ => (token.start, source[token.start..byte].to_string()),
        },
        None => (byte, String::new()),
    };
    let line_start = source[..byte].rfind('\n').map_or(0, |at| at + 1);
    let utf16 = |at: usize| source[..at].encode_utf16().count();
    let mut context = CompletionContext {
        typed: typed_text,
        replace_start: utf16(replace_from),
        replace_end: utf16(byte),
        base_indent: source[line_start..]
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect(),
        ..Default::default()
    };
    match top.kind {
        Kind::Array => {
            context.path = top.path.clone();
            context.path.push(SchemaPathSegment::ArrayItem(top.index));
            context.is_array_item = true;
        }
        Kind::Record if top.annotating => {
            context.path = top.path.clone();
            context.no_properties = true;
        }
        Kind::Record if top.in_value && !top.value_keys.is_empty() => {
            let (last, prefix) = top.value_keys.split_last().expect("checked non-empty");
            context.path = with_keys(&top.path, prefix);
            context.value_key = Some(last.clone());
            // A value already written before the caret takes no suggestions.
            context.no_properties = top.value_tokens > 0;
        }
        Kind::Record => {
            context.path = with_keys(&top.path, &top.cur_keys);
            context.no_properties = top.let_pending || top.let_value;
        }
    }
    context.no_properties |= top.opaque;
    // A value written inside a string literal (`key = "|"`) replaces the whole
    // literal, quotes included: the value is written with its own quotes, so
    // the literal's quotes must not be left around it.
    if context.value_key.is_some()
        && let Some(literal) = tokens.iter().find(|token| {
            matches!(token.tok, Tok::Str(..)) && token.start < byte && byte < token.end
        })
    {
        context.replace_start = utf16(literal.start);
        context.replace_end = utf16(literal.end);
    }
    // A key at the start of its line is written at the line's indentation. On
    // a blank line there is none yet: the record's first field gives it, and
    // the insert opens with it (`entry_prefix`).
    let before = &source[line_start..replace_from];
    if context.value_key.is_none()
        && !context.is_array_item
        && !context.no_properties
        && before.chars().all(|c| c == ' ' || c == '\t')
        && before.is_empty()
    {
        let indent = field_indent(source, tokens, line_start).unwrap_or_default();
        context.base_indent = indent.clone();
        context.entry_prefix = indent;
    }
    context
}

/// The indentation of the first field of the document that starts a line
/// other than the one at `caret_line` (a line start byte offset).
fn field_indent(source: &str, tokens: &[Token], caret_line: usize) -> Option<String> {
    tokens.iter().enumerate().find_map(|(index, token)| {
        let is_key = matches!(token.tok, Tok::Ident(_) | Tok::Str(..))
            && matches!(
                tokens.get(index + 1).map(|next| &next.tok),
                Some(Tok::P("=" | "|"))
            );
        if !is_key {
            return None;
        }
        let line = source[..token.start].rfind('\n').map_or(0, |at| at + 1);
        let before = &source[line..token.start];
        (line != caret_line && before.chars().all(|c| c == ' ' || c == '\t'))
            .then(|| before.to_string())
    })
}

// ---------------------------------------------------------------------------
// Text helpers
// ---------------------------------------------------------------------------

/// The byte offset of the start of line `line_index` (0-based).
fn line_offset(source: &str, line_index: usize) -> Option<usize> {
    let mut offset = 0;
    for (index, line) in source.split('\n').enumerate() {
        if index == line_index {
            return Some(offset);
        }
        offset += line.len() + 1;
    }
    None
}

/// The byte offset of character `column` (0-based) of `line`.
fn char_byte(line: &str, column: usize) -> Option<usize> {
    line.char_indices().nth(column).map(|(at, _)| at)
}

/// The 0-based character column of byte `byte` on its line.
fn column_of(source: &str, byte: usize) -> usize {
    let start = source[..byte].rfind('\n').map_or(0, |at| at + 1);
    source[start..byte].chars().count()
}

fn token_at(tokens: &[Token], byte: usize) -> Option<usize> {
    tokens
        .iter()
        .position(|token| token.start <= byte && byte < token.end)
}

const KEYWORDS: [&str; 11] = [
    "let", "rec", "in", "fun", "if", "then", "else", "match", "import", "true", "false",
];

/// Whether `name` can be written without quotes: a Nickel identifier that is
/// not a keyword.
fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let starts = chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic());
    starts && chars.all(is_ident_char) && !KEYWORDS.contains(&name) && name != "null"
}

fn quote(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '%' => out.push_str("\\%"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn key_text(key: &str) -> String {
    if is_identifier(key) {
        key.to_string()
    } else {
        quote(key)
    }
}

/// A value as Nickel writes it: identifier strings as tags, others quoted.
fn write_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) if is_identifier(text) => format!("'{text}"),
        Value::String(text) => quote(text),
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(write_value).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(map) if map.is_empty() => "{}".to_string(),
        Value::Object(map) => format!(
            "{{ {} }}",
            map.iter()
                .map(|(key, value)| format!("{} = {}", key_text(key), write_value(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Where the string of a previous text is written as a tag (`true`) or quoted
/// (`false`), by path. Read from its syntax, so a string kept quoted stays so.
fn tag_forms(previous: Option<&str>) -> Vec<(Path, bool)> {
    let Some(source) = previous else {
        return Vec::new();
    };
    let lines: Vec<&str> = source.lines().collect();
    sites(source)
        .into_iter()
        .filter_map(|site| {
            let line = lines.get(site.value.line.checked_sub(1)?)?;
            let first = line.chars().nth(site.value.start_column.checked_sub(1)?)?;
            matches!(first, '\'' | '"').then_some((site.path, first == '\''))
        })
        .collect()
}

/// Whether the string `text` at `path` is written as a tag: an identifier
/// that the previous text wrote as a tag, or any identifier when there is none.
fn writes_tag(path: &Path, text: &str, forms: &[(Path, bool)]) -> bool {
    let tag = forms
        .iter()
        .find(|(at, _)| at == path)
        .map_or_else(|| is_identifier(text), |(_, tag)| *tag);
    tag && is_identifier(text)
}

/// A field name as written, or an error for the one name Nickel has no
/// spelling for.
fn written_key(key: &str) -> Result<String, String> {
    if key.is_empty() {
        return Err("an empty field name has no Nickel form".into());
    }
    Ok(key_text(key))
}

/// A path as the message of an error names it.
fn describe(path: &Path) -> String {
    let mut text = String::new();
    for segment in path {
        match segment {
            SchemaPathSegment::Key(key) => {
                if !text.is_empty() {
                    text.push('.');
                }
                text.push_str(key);
            }
            SchemaPathSegment::ArrayItem(index) => text.push_str(&format!("[{index}]")),
        }
    }
    if text.is_empty() {
        "the top level".to_string()
    } else {
        format!("`{text}`")
    }
}

/// The Nickel text of the data at `path`, written on one line. A non-finite
/// number is an error: Nickel has no literal for it.
fn write_item(value: &Value, path: &Path, forms: &[(Path, bool)]) -> Result<String, String> {
    if float_of(value).is_some_and(|number| !number.is_finite()) {
        return Err(format!(
            "{} is an infinity or NaN, which Nickel cannot write",
            describe(path)
        ));
    }
    Ok(match value {
        Value::Null => "null".to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(_) => {
            let text = string_of(value).ok_or_else(|| format!("{} is not text", describe(path)))?;
            if writes_tag(path, text, forms) {
                format!("'{text}")
            } else {
                quote(text)
            }
        }
        Value::Array(items) => {
            let mut parts = Vec::with_capacity(items.len());
            for (index, item) in items.iter().enumerate() {
                let mut at = path.clone();
                at.push(SchemaPathSegment::ArrayItem(index));
                parts.push(write_item(item, &at, forms)?);
            }
            format!("[{}]", parts.join(", "))
        }
        Value::Object(map) if map.is_empty() => "{}".to_string(),
        Value::Object(map) => {
            let mut parts = Vec::with_capacity(map.len());
            for (key, item) in map {
                let mut at = path.clone();
                at.push(SchemaPathSegment::Key(key.clone()));
                parts.push(format!(
                    "{} = {}",
                    written_key(key)?,
                    write_item(item, &at, forms)?
                ));
            }
            format!("{{ {} }}", parts.join(", "))
        }
    })
}

// ---------------------------------------------------------------------------
// Highlighting, one line at a time
// ---------------------------------------------------------------------------

/// A run of a line: its byte range and how it is colored.
type Run = (usize, usize, TokenKind);

/// Keywords, colored as references.
const KEYWORD_WORDS: [&str; 9] = [
    "let", "rec", "in", "fun", "if", "then", "else", "match", "import",
];

/// A byte that can continue a word (`my-field`, `'info`).
fn word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'\'')
}

/// The index just past the `}` that closes an interpolation whose body
/// starts at `from`, or the end of `bytes` when it is not closed on the line.
fn interpolation_end(bytes: &[u8], from: usize) -> usize {
    let mut depth = 1usize;
    for (offset, &byte) in bytes.iter().enumerate().skip(from) {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return offset + 1;
                }
            }
            _ => {}
        }
    }
    bytes.len()
}

/// The runs of a string whose text starts at `body` (after its opening quote
/// or `m%"`), the string itself starting at `start`. A `"..."` string ends at
/// its quote, a raw `m%"..."%` one at `"%`. Returns the index after the
/// closer, or `None` when the line ends first.
fn string_runs(
    bytes: &[u8],
    start: usize,
    body: usize,
    raw: bool,
    runs: &mut Vec<Run>,
) -> Option<usize> {
    let mut segment = start;
    let mut i = body;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if !raw => i += 2,
            b'"' if raw && bytes.get(i + 1) == Some(&b'%') => {
                runs.push((segment, i + 2, TokenKind::String));
                return Some(i + 2);
            }
            b'"' if !raw => {
                runs.push((segment, i + 1, TokenKind::String));
                return Some(i + 1);
            }
            b'%' if bytes.get(i + 1) == Some(&b'{') => {
                runs.push((segment, i, TokenKind::String));
                let end = interpolation_end(bytes, i + 2);
                runs.push((i, end, TokenKind::Interpolation));
                i = end;
                segment = end;
            }
            _ => i += 1,
        }
    }
    runs.push((segment, bytes.len(), TokenKind::String));
    None
}

/// Whether the name before `at` is a field: `=` follows it (after spaces),
/// or a dotted name ending in one (`a.b =`). `==` and `=>` are not.
fn field_follows(bytes: &[u8], mut at: usize) -> bool {
    while matches!(bytes.get(at), Some(b' ' | b'\t')) {
        at += 1;
    }
    match bytes.get(at) {
        Some(b'=') => !matches!(bytes.get(at + 1), Some(b'=' | b'>')),
        Some(b'.')
            if bytes
                .get(at + 1)
                .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_') =>
        {
            let mut end = at + 1;
            while end < bytes.len() && word_byte(bytes[end]) {
                end += 1;
            }
            field_follows(bytes, end)
        }
        _ => false,
    }
}

/// The runs of one line. A string or raw string left open runs to the end of
/// the line; the rest of it comes from `multiline_strings`.
fn line_runs(line: &str) -> Vec<Run> {
    let bytes = line.as_bytes();
    let mut runs = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        if byte == b'#' {
            runs.push((i, bytes.len(), TokenKind::Comment));
            break;
        } else if byte == b'"' {
            let mut inner = Vec::new();
            let end = string_runs(bytes, i, i + 1, false, &mut inner).unwrap_or(bytes.len());
            if field_follows(bytes, end) {
                runs.push((i, end, TokenKind::Key));
            } else {
                runs.extend(inner);
            }
            i = end;
        } else if byte == b'm'
            && bytes[i + 1..].starts_with(b"%\"")
            && !(i > 0 && word_byte(bytes[i - 1]))
        {
            let end = string_runs(bytes, i, i + 3, true, &mut runs).unwrap_or(bytes.len());
            i = end;
        } else if byte == b'\'' {
            let end = run_end_at(bytes, i + 1);
            if end > i + 1 {
                runs.push((i, end, TokenKind::Literal));
            }
            i = end.max(i + 1);
        } else if byte.is_ascii_alphabetic() || byte == b'_' {
            let end = run_end_at(bytes, i);
            let word = &line[i..end];
            let kind = match word {
                "true" | "false" | "null" => TokenKind::Literal,
                _ if KEYWORD_WORDS.contains(&word) => TokenKind::Reference,
                _ if field_follows(bytes, end) => TokenKind::Key,
                _ => TokenKind::Value,
            };
            runs.push((i, end, kind));
            i = end;
        } else if byte.is_ascii_digit()
            || (byte == b'-' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit))
        {
            let mut end = i + 1;
            while end < bytes.len()
                && (bytes[end].is_ascii_alphanumeric()
                    || bytes[end] == b'_'
                    || (bytes[end] == b'.' && bytes.get(end + 1).is_some_and(u8::is_ascii_digit)))
            {
                end += 1;
            }
            runs.push((i, end, TokenKind::Literal));
            i = end;
        } else if byte == b'=' && matches!(bytes.get(i + 1), Some(b'=' | b'>')) {
            // `==` and `=>` are operators, colored as plain text.
            i += 2;
        } else if matches!(
            byte,
            b'{' | b'}' | b'[' | b']' | b',' | b'|' | b';' | b':' | b'='
        ) {
            runs.push((i, i + 1, TokenKind::Punctuation));
            i += 1;
        } else {
            i += 1;
        }
    }
    runs
}

/// The index after the word characters starting at `from`.
fn run_end_at(bytes: &[u8], from: usize) -> usize {
    let mut end = from;
    while end < bytes.len() && word_byte(bytes[end]) {
        end += 1;
    }
    end
}

/// The crate's tokens for `runs` of a line that starts at `offset`.
fn tokens_of(runs: &[Run], offset: usize) -> Vec<Highlight> {
    let mut tokens = Tokens::new();
    for &(start, end, kind) in runs {
        tokens.push(start + offset, end + offset, kind);
    }
    tokens.finish()
}

/// The byte ranges of the raw strings `m%"..."%` in `source`, from their `m`
/// to the end of their `"%` (or of the source). A `#` or a quote inside a
/// plain string or a comment does not start one.
fn raw_ranges(source: &str) -> Vec<std::ops::Range<usize>> {
    let bytes = source.as_bytes();
    let mut ranges = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'#' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' && bytes[i] != b'\n' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
            }
            b'm' if bytes[i + 1..].starts_with(b"%\"") && !(i > 0 && word_byte(bytes[i - 1])) => {
                let start = i;
                let end = source[i + 3..]
                    .find("\"%")
                    .map_or(bytes.len(), |at| i + 3 + at + 2);
                ranges.push(start..end);
                i = end;
            }
            _ => i += 1,
        }
    }
    ranges
}

// ---------------------------------------------------------------------------
// The language
// ---------------------------------------------------------------------------

impl ConfigLanguage for Nickel {
    fn info(&self) -> LanguageInfo {
        LanguageInfo {
            label: "Nickel",
            icon: "N",
            extensions: &["ncl"],
        }
    }

    fn config(&self) -> LanguageConfig {
        LanguageConfig {
            line_comments: &["#"],
            block_comment: None,
            brackets: &[('{', '}'), ('[', ']'), ('(', ')')],
            separators: &["="],
            quotes: &["\""],
            map_brackets: Some(('{', '}')),
            list_brackets: Some(('[', ']')),
            item_separator: Some(','),
            written_separator: Some(" = "),
            // Nickel has no infinity or NaN literal; writers are given the
            // data model's values so `serialize` can refuse them.
            traits: LanguageTraits {
                writes_non_finite: true,
                ..LanguageTraits::NONE
            },
            ..LanguageConfig::default()
        }
    }

    fn parse(&self, source: &str) -> Result<Value, ParseError> {
        parse_document(source).map(|(value, _)| value)
    }

    fn serialize(&self, value: &Value, previous: Option<&str>) -> Result<String, String> {
        let Value::Object(map) = value else {
            return Err("a Nickel file is a record of fields, and this value is not one".into());
        };
        if map.is_empty() {
            return Ok("{}\n".to_string());
        }
        let forms = tag_forms(previous);
        let mut text = String::from("{\n");
        for (key, item) in map {
            let path = vec![SchemaPathSegment::Key(key.clone())];
            let field = write_item(item, &path, &forms)?;
            text.push_str(&format!("  {} = {},\n", written_key(key)?, field));
        }
        text.push_str("}\n");
        Ok(text)
    }

    fn highlight(&self, line: &str) -> Vec<Highlight> {
        tokens_of(&line_runs(line), 0)
    }

    fn text(&self) -> &dyn Text {
        self
    }

    fn key_span(&self, source: &str, path: &FieldPath) -> Option<Span> {
        site_for(&sites(source), path).map(|site| site.key)
    }

    fn value_span(&self, source: &str, path: &FieldPath) -> Option<Span> {
        site_for(&sites(source), path).map(|site| site.value)
    }

    fn key_spans(&self, source: &str, paths: &[FieldPath]) -> Vec<Option<Span>> {
        let sites = sites(source);
        paths
            .iter()
            .map(|path| site_for(&sites, path).map(|site| site.key))
            .collect()
    }

    fn value_spans(&self, source: &str, paths: &[FieldPath]) -> Vec<Option<Span>> {
        let sites = sites(source);
        paths
            .iter()
            .map(|path| site_for(&sites, path).map(|site| site.value))
            .collect()
    }

    fn completion_context(&self, source: &str, cursor: usize) -> CompletionContext {
        let byte = utf16_to_byte_index(source, cursor);
        context_at(source, &lex(source).tokens, byte)
    }

    fn write_insert(
        &self,
        key: &str,
        plan: &InsertPlan<'_>,
        context: &InsertContext<'_>,
    ) -> Option<(String, usize)> {
        let name = key_text(key);
        let indent = context.base_indent;
        let unit = context.unit;
        // Every field ends with a comma, as Nickel's records separate their
        // fields; Nickel takes a trailing comma too, so the last field's is
        // harmless and the closing `}` is left alone. The plan says only
        // whether a scalar left empty is text (`Planned::Empty`), so an
        // integer, boolean or enum is left for the user to type.
        let (head, tail) = match &plan.value {
            Planned::Value(value) => (format!("{name} = {}", write_value(value)), ",".to_string()),
            Planned::List { .. } => (format!("{name} = ["), "],".to_string()),
            Planned::Object { .. } => (
                format!("{name} = {{\n{indent}{unit}"),
                format!("\n{indent}}},"),
            ),
            Planned::Empty { string: true } => (format!("{name} = \""), "\",".to_string()),
            // A scalar left to type: the comma keeps the record's next field apart.
            _ => (format!("{name} = "), ",".to_string()),
        };
        let caret = head.encode_utf16().count();
        Some((format!("{head}{tail}"), caret))
    }

    fn format_value(&self, value: &Value) -> String {
        write_value(value)
    }

    fn hover_key(&self, line: &str, column: usize) -> Option<(String, usize)> {
        let tokens = lex(line).tokens;
        let index = token_at(&tokens, char_byte(line, column)?)?;
        let key = match &tokens[index].tok {
            Tok::Ident(word) => word.clone(),
            Tok::Str(text, false) => text.clone(),
            _ => return None,
        };
        let before = index.checked_sub(1).map(|i| &tokens[i].tok);
        let followed = matches!(
            tokens.get(index + 1).map(|t| &t.tok),
            Some(Tok::P("=" | "|" | ":"))
        );
        let dotted = matches!(before, Some(Tok::P(".")));
        let after_let = matches!(before, Some(Tok::Ident(word)) if word == "let");
        (followed && !dotted && !after_let).then(|| (key, column_of(line, tokens[index].start)))
    }

    fn hover(&self) -> &dyn Hover {
        self
    }
}

impl Hover for Nickel {
    /// A segment of a dotted field name (`b` of `a.b = 1`): its path is the
    /// record holding the chain, with the segments before it.
    fn hover_path(
        &self,
        source: &str,
        line_index: usize,
        column: usize,
    ) -> Option<(Path, String, usize)> {
        let line_start = line_offset(source, line_index)?;
        let line = source[line_start..].split('\n').next()?;
        let byte = line_start + char_byte(line, column)?;
        let tokens = lex(source).tokens;
        let index = token_at(&tokens, byte)?;
        let key = match &tokens[index].tok {
            Tok::Ident(word) => word.clone(),
            Tok::Str(text, false) => text.clone(),
            _ => return None,
        };
        let mut first = index;
        while first >= 2 && tokens[first - 1].tok == Tok::P(".") {
            first -= 2;
        }
        let mut last = index;
        while tokens
            .get(last + 1)
            .is_some_and(|token| token.tok == Tok::P("."))
        {
            last += 2;
        }
        if first == index && last == index {
            return None;
        }
        let entry_start = match first.checked_sub(1).map(|i| &tokens[i].tok) {
            None | Some(Tok::P("{" | ",")) => true,
            Some(Tok::Ident(word)) => word == "in",
            _ => false,
        };
        let key_follows = matches!(
            tokens.get(last + 1).map(|t| &t.tok),
            Some(Tok::P("=" | "|" | ":"))
        );
        if !entry_start || !key_follows {
            return None;
        }
        let segment = tokens[index].start;
        let context = context_at(source, &tokens, segment);
        Some((context.path, key, column_of(source, segment)))
    }

    /// Just past the `=` that follows the key under `column`, so the value
    /// context of that key is read.
    fn hover_cursor(&self, line: &str, column: usize) -> usize {
        let tokens = lex(line).tokens;
        let index = char_byte(line, column).and_then(|byte| token_at(&tokens, byte));
        let Some(index) = index else {
            return line.len();
        };
        tokens[index + 1..]
            .iter()
            .find(|token| token.tok == Tok::P("="))
            .map_or(line.len(), |token| token.end)
    }
}

impl Text for Nickel {
    /// The raw strings `m%"..."%` over several lines. Their text is not code,
    /// so a `#` inside one is not a comment.
    fn multiline_strings(&self, source: &str) -> Vec<std::ops::Range<usize>> {
        raw_ranges(source)
    }

    /// The lines wholly inside a raw string: after its opening line and before
    /// the line its `"%` closes on.
    fn string_body_lines(&self, source: &str) -> Vec<std::ops::Range<usize>> {
        raw_ranges(source)
            .into_iter()
            .filter_map(|range| {
                let newlines = |end: usize| {
                    source.as_bytes()[..end]
                        .iter()
                        .filter(|&&b| b == b'\n')
                        .count()
                };
                let first = newlines(range.start) + 1;
                let last = newlines(range.end - 1);
                (first < last).then_some(first..last)
            })
            .collect()
    }

    fn multiline_constructs(
        &self,
        _language: &dyn ConfigLanguage,
        source: &str,
    ) -> Vec<MultilineConstruct> {
        raw_ranges(source)
            .into_iter()
            .map(MultilineConstruct::structure)
            .collect()
    }

    /// The lines of a raw string after its first: the text up to its `"%` is
    /// string (with its `%{...}` interpolations), and the rest of the line is
    /// highlighted as code.
    fn highlight_continued(&self, _language: &dyn ConfigLanguage, text: &str) -> Vec<Highlight> {
        let bytes = text.as_bytes();
        let mut runs = Vec::new();
        let rest = string_runs(bytes, 0, 0, true, &mut runs).unwrap_or(bytes.len());
        let mut tokens = tokens_of(&runs, 0);
        tokens.extend(tokens_of(&line_runs(&text[rest..]), rest));
        tokens
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leptos_config_editor::{Schema, TextPosition};
    use serde_json::json;

    /// A realistic configuration: a `let`, nested records, an array, an enum
    /// tag, a contract annotation, and a function that is left out.
    const CONFIG: &str = r#"# Service configuration.
let base_port = 8080 in
{
  name = "orders",
  port | Number = base_port,
  log_level = 'info,
  database = {
    host = "db.internal",
    port = 5432,
    pool_size = 10,
  },
  features = ['metrics, 'tracing],
  handler = fun req => req,
}
"#;

    fn schema() -> Schema {
        Schema::compile(NICKEL_SCHEMA).expect("the Nickel schema compiles")
    }

    /// The 1-based position `after` characters past the start of `needle`.
    fn at(source: &str, needle: &str, after: usize) -> TextPosition {
        let byte = source.find(needle).expect("needle in source");
        let line = source[..byte].matches('\n').count() + 1;
        let column = source[..byte].rsplit('\n').next().unwrap().chars().count() + 1;
        TextPosition::new(line, column + after)
    }

    /// `marked` without its `|`, and the UTF-16 offset of the `|`.
    fn unmark(marked: &str) -> (String, usize) {
        let at = marked.find('|').expect("a | caret marker");
        (
            marked.replacen('|', "", 1),
            marked[..at].encode_utf16().count(),
        )
    }

    #[test]
    fn parses_a_realistic_config() {
        let value = Nickel.parse(CONFIG).expect("the config parses");
        assert_eq!(
            value,
            json!({
                "name": "orders",
                "port": 8080,
                "log_level": "info",
                "database": { "host": "db.internal", "port": 5432, "pool_size": 10 },
                "features": ["metrics", "tracing"]
            })
        );
    }

    #[test]
    fn the_config_validates_against_the_schema() {
        let diagnostics = schema().validate(NICKEL, CONFIG);
        assert!(
            diagnostics.is_empty(),
            "unexpected diagnostics: {diagnostics:?}"
        );
    }

    #[test]
    fn a_bad_value_is_reported_against_the_schema() {
        let source = "{ name = \"x\", port = 70000 }";
        assert!(!schema().validate(NICKEL, source).is_empty());
    }

    #[test]
    fn parses_let_contracts_tags_raw_strings_and_skips_functions() {
        let source = r#"
            let host = m%"db.internal"% in
            let retries | Number = 3 in
            {
              "quoted key" = 1,
              endpoint.url = "http://x",
              endpoint.port = -2,
              level = 'warn,
              tags = ['a, 'b],
              nested = { ok = true, none = null, ratio = 1.5 },
              ref = host,
              handler = fun x => x + 1,
              imported = import "other.ncl",
              mixed = [1, fun x => x],
            }
        "#;
        let value = Nickel.parse(source).expect("parses");
        assert_eq!(
            value,
            json!({
                "quoted key": 1,
                "endpoint": { "url": "http://x", "port": -2 },
                "level": "warn",
                "tags": ["a", "b"],
                "nested": { "ok": true, "none": null, "ratio": 1.5 },
                "ref": "db.internal"
            })
        );
    }

    #[test]
    fn unparseable_text_is_an_error() {
        assert!(Nickel.parse("{ a = 1").is_err());
        assert!(Nickel.parse("{ a 1 }").is_err());
        assert!(Nickel.parse("1 + 2").is_err());
    }

    #[test]
    fn hover_shows_a_field_description() {
        let schema = schema();
        let docs = schema
            .hover(NICKEL, CONFIG, at(CONFIG, "port | Number", 0))
            .expect("docs over the top-level `port`");
        assert_eq!(docs.key, "port");
        assert_eq!(
            docs.description.as_deref(),
            Some("TCP port the service listens on.")
        );
        let docs = schema
            .hover(NICKEL, CONFIG, at(CONFIG, "host = ", 0))
            .expect("docs over the nested `host`");
        assert_eq!(
            docs.description.as_deref(),
            Some("Host name or address of the database server.")
        );
    }

    /// The tag's own docs, from its `x-enum-descriptions` entry, are on the
    /// public `HoverDocs.value`; the value span covers the tag.
    #[test]
    fn hover_on_an_enum_tag_shows_the_entry_description_and_span() {
        let schema = schema();
        let docs = schema
            .hover(NICKEL, CONFIG, at(CONFIG, "'info", 1))
            .expect("docs over the tag");
        assert_eq!(docs.key, "log_level");
        let value = docs.value.clone().expect("the tag's docs");
        assert_eq!(value.value, "info");
        assert!(value.description.is_some(), "{value:?}");
        let span = Nickel.value_span(CONFIG, &FieldPath::from_pointer("/log_level"));
        assert_eq!(span, Some(Span::from((6, 15, 20))));
        let context = Nickel.completion_context(CONFIG, 0);
        assert_eq!(context.value_key, None);
    }

    #[test]
    fn completion_lists_record_fields_with_descriptions() {
        let schema = schema();
        let (source, cursor) = unmark("{\n  da|");
        let suggestions = schema.suggestions(NICKEL, &source, cursor);
        let database = suggestions
            .iter()
            .find(|s| s.label() == "database")
            .expect("`database` is offered");
        assert!(database.detail().contains("Connection to the database"));
        let (text, _) = database.accept("{\n  da");
        assert_eq!(text, "{\n  database = {\n    \n  },", "accepted: {text:?}");

        let (source, cursor) = unmark("{\n  database = {\n    ho|");
        let labels: Vec<String> = schema
            .suggestions(NICKEL, &source, cursor)
            .iter()
            .map(|s| s.label().to_string())
            .collect();
        assert!(labels.contains(&"host".to_string()), "{labels:?}");
        assert!(!labels.contains(&"database".to_string()), "{labels:?}");
    }

    #[test]
    fn completion_lists_enum_tags_for_an_enum_field() {
        let schema = schema();
        let (source, cursor) = unmark("{\n  log_level = |\n}");
        let suggestions = schema.suggestions(NICKEL, &source, cursor);
        let labels: Vec<String> = suggestions.iter().map(|s| s.label().to_string()).collect();
        for tag in ["debug", "info", "warn", "error"] {
            assert!(labels.contains(&tag.to_string()), "{labels:?}");
        }
        let info = suggestions
            .iter()
            .find(|s| s.label() == "info")
            .expect("info is offered");
        let (text, _) = info.accept(&source);
        assert!(text.contains("log_level = 'info"), "accepted: {text:?}");
    }

    #[test]
    fn value_insert_writes_tags_and_records() {
        assert_eq!(write_value(&json!("info")), "'info");
        assert_eq!(write_value(&json!("a b")), "\"a b\"");
        assert_eq!(
            write_value(&json!({ "host": "x", "port": 1 })),
            "{ host = 'x, port = 1 }"
        );
    }

    #[test]
    fn completion_lists_unset_fields_on_a_blank_line_in_the_top_level_record() {
        let schema = schema();
        let (source, cursor) =
            unmark("let base = 8080 in\n{\n  name = \"billing-api\",\n  port = base,\n|\n}\n");
        let labels: Vec<String> = schema
            .suggestions(NICKEL, &source, cursor)
            .iter()
            .map(|s| s.label().to_string())
            .collect();
        for field in ["log_level", "database", "features"] {
            assert!(labels.contains(&field.to_string()), "{labels:?}");
        }
        assert!(!labels.contains(&"name".to_string()), "{labels:?}");
        assert!(!labels.contains(&"port".to_string()), "{labels:?}");
    }

    #[test]
    fn a_blank_line_offers_no_top_level_field_when_all_are_set() {
        let schema = schema();
        let (source, cursor) = unmark(
            "let base = 8080 in\n{\n  name = \"billing-api\",\n|\n  port = base,\n  log_level = 'info,\n  database = { host = \"db\", port = 5432, pool_size = 10 },\n  features = ['metrics],\n}\n",
        );
        let labels: Vec<String> = schema
            .suggestions(NICKEL, &source, cursor)
            .iter()
            .map(|s| s.label().to_string())
            .collect();
        for field in ["name", "port", "log_level", "database", "features"] {
            assert!(!labels.contains(&field.to_string()), "{labels:?}");
        }
    }

    #[test]
    fn unclosed_record_reports_the_missing_brace_at_the_end() {
        let source = "let base = 8080 in\n{\n  name = \"billing-api\",\n  port = base,\n";
        let error = Nickel.parse(source).expect_err("an unclosed record");
        assert_eq!(error.message, "expected `}`");
        assert_eq!(error.line, Some(5));
    }

    #[test]
    fn a_field_with_no_value_says_so() {
        let source = "let base = 8080 in\n{\n  name = \"billing-api\",\n  port = ;\n}\n";
        let error = Nickel.parse(source).expect_err("a field with no value");
        assert_eq!(error.message, "expected a value after `=`");
        assert_eq!(error.line, Some(4));
    }

    #[test]
    fn a_broken_record_does_not_offer_its_written_fields() {
        let schema = schema();
        let text = "{\n  name = \"billing-api\",\n  log_level = 'info,\n  ";
        let cursor = text.encode_utf16().count();
        let labels: Vec<String> = schema
            .suggestions(NICKEL, text, cursor)
            .iter()
            .map(|s| s.label().to_string())
            .collect();
        assert!(!labels.contains(&"name".to_string()), "{labels:?}");
        assert!(!labels.contains(&"log_level".to_string()), "{labels:?}");
    }

    /// The text and color of each token of `tokens` over `line`.
    fn spans(line: &str, tokens: Vec<Highlight>) -> Vec<(&str, TokenKind)> {
        tokens
            .iter()
            .map(|token| (&line[token.start..token.end], token.kind))
            .collect()
    }

    fn colored(line: &str) -> Vec<(&str, TokenKind)> {
        spans(line, Nickel.highlight(line))
    }

    #[test]
    fn highlight_colors_a_field_its_value_and_its_punctuation() {
        assert_eq!(
            colored("  log_level = 'info,"),
            vec![
                ("log_level", TokenKind::Key),
                ("=", TokenKind::Punctuation),
                ("'info", TokenKind::Literal),
                (",", TokenKind::Punctuation),
            ]
        );
    }

    #[test]
    fn highlight_colors_a_string_and_a_comment() {
        assert_eq!(
            colored("  name = \"billing-api\", # the name"),
            vec![
                ("name", TokenKind::Key),
                ("=", TokenKind::Punctuation),
                ("\"billing-api\"", TokenKind::String),
                (",", TokenKind::Punctuation),
                ("# the name", TokenKind::Comment),
            ]
        );
    }

    #[test]
    fn highlight_colors_keywords_numbers_and_interpolation() {
        assert_eq!(
            colored("let base = 8080 in"),
            vec![
                ("let", TokenKind::Reference),
                ("base", TokenKind::Key),
                ("=", TokenKind::Punctuation),
                ("8080", TokenKind::Literal),
                ("in", TokenKind::Reference),
            ]
        );
        assert_eq!(
            colored("  url = \"http://%{host}/\""),
            vec![
                ("url", TokenKind::Key),
                ("=", TokenKind::Punctuation),
                ("\"http://", TokenKind::String),
                ("%{host}", TokenKind::Interpolation),
                ("/\"", TokenKind::String),
            ]
        );
    }

    #[test]
    fn highlight_colors_a_multiline_string_on_its_first_and_later_lines() {
        assert_eq!(
            colored("  text = m%\"first"),
            vec![
                ("text", TokenKind::Key),
                ("=", TokenKind::Punctuation),
                ("m%\"first", TokenKind::String),
            ]
        );
        let later = "  second %{port} end\"% , port = 1";
        assert_eq!(
            spans(later, Nickel.highlight_continued(&Nickel, later)),
            vec![
                ("  second ", TokenKind::String),
                ("%{port}", TokenKind::Interpolation),
                (" end\"%", TokenKind::String),
                (",", TokenKind::Punctuation),
                ("port", TokenKind::Key),
                ("=", TokenKind::Punctuation),
                ("1", TokenKind::Literal),
            ]
        );
    }

    #[test]
    fn a_hash_inside_a_multiline_string_is_not_a_comment() {
        let source = "a = m%\"x\n# not a comment\n\"%\nb = 2 # real\n";
        let ranges = Nickel.multiline_strings(source);
        assert_eq!(ranges.len(), 1);
        assert_eq!(&source[ranges[0].clone()], "m%\"x\n# not a comment\n\"%");
        let comments: Vec<&str> = Nickel
            .comments(&Nickel, source)
            .into_iter()
            .map(|range| &source[range])
            .collect();
        assert_eq!(comments, vec!["# real"]);
    }

    /// The sample of Edit as: a `let`, and the record it evaluates to.
    const SAMPLE: &str = "let base = 8080 in\n{\n  name = \"billing-api\",\n  port = base,\n  log_level = 'info,\n  database = { host = \"db\", port = 5432, pool_size = 10 },\n  features = ['metrics],\n}\n";

    #[test]
    fn serialize_writes_the_evaluated_record_in_the_forms_of_the_previous_text() {
        let value = Nickel.parse(SAMPLE).expect("the sample parses");
        assert_eq!(
            Nickel.serialize(&value, Some(SAMPLE)),
            Ok(
                "{\n  name = \"billing-api\",\n  port = 8080,\n  log_level = 'info,\n  database = { host = \"db\", port = 5432, pool_size = 10 },\n  features = ['metrics],\n}\n"
                    .to_string()
            )
        );
    }

    #[test]
    fn serialize_without_a_previous_text_writes_identifier_strings_as_tags() {
        let value = json!({
            "name": "billing-api",
            "note": "two words",
            "ok": true,
            "tags": [null, 1.5],
        });
        assert_eq!(
            Nickel.serialize(&value, None),
            Ok("{\n  name = 'billing-api,\n  note = \"two words\",\n  ok = true,\n  tags = [null, 1.5],\n}\n".to_string())
        );
    }

    #[test]
    fn the_sample_converts_to_yaml_and_back_with_the_same_data() {
        let yaml = NICKEL
            .convert(SAMPLE, Language::YAML)
            .expect("converts to YAML");
        assert!(yaml.losses.is_empty(), "{:?}", yaml.losses);
        let back = Language::YAML
            .convert(&yaml.text, NICKEL)
            .expect("converts back to Nickel");
        assert!(back.losses.is_empty(), "{:?}", back.losses);
        assert_eq!(
            Nickel.parse(&back.text).unwrap(),
            Nickel.parse(SAMPLE).unwrap()
        );
    }

    #[test]
    fn serialize_refuses_what_nickel_cannot_write() {
        let rate = leptos_config_editor::float_value(f64::INFINITY);
        assert_eq!(
            Nickel.serialize(&json!({ "rate": rate }), None),
            Err("`rate` is an infinity or NaN, which Nickel cannot write".to_string())
        );
        assert!(Nickel.serialize(&json!([1, 2]), None).is_err());
    }

    /// Accepts the suggestion `label` at the `|` of `marked`; the text it
    /// writes, and the UTF-16 offset of the caret after it.
    fn accepted(marked: &str, label: &str) -> (String, usize) {
        let (text, cursor) = unmark(marked);
        let suggestion = schema()
            .suggestions(NICKEL, &text, cursor)
            .into_iter()
            .find(|suggestion| suggestion.label() == label)
            .unwrap_or_else(|| panic!("{label} is not offered at the caret"));
        let (accepted, selection) = suggestion.accept(&text);
        (accepted, selection.start)
    }

    /// Asserts that accepting `label` at `marked` gives `expected` (with its
    /// `|` at the caret).
    fn assert_accepts(marked: &str, label: &str, expected: &str) {
        let (text, caret) = accepted(marked, label);
        let (want, want_caret) = unmark(expected);
        assert_eq!(text, want, "accepting {label}");
        assert_eq!(caret, want_caret, "caret after accepting {label}");
    }

    #[test]
    fn accepted_string_field_is_written_empty_with_the_caret_between_quotes() {
        assert_accepts("{\n  |\n}", "name", "{\n  name = \"|\",\n}");
    }

    #[test]
    fn accepted_array_field_is_written_empty_with_the_caret_inside() {
        assert_accepts("{\n  |\n}", "features", "{\n  features = [|],\n}");
    }

    #[test]
    fn accepted_object_field_is_written_as_an_indented_block() {
        assert_accepts("{\n  |\n}", "database", "{\n  database = {\n    |\n  },\n}");
    }

    /// The plan says only whether a bare scalar is text: an integer with no
    /// default is left for the user, and an enum of strings reads as text,
    /// so it is written as an empty string rather than its first tag.
    #[test]
    fn accepted_scalar_without_a_default_is_left_for_the_user() {
        assert_accepts("{\n  |\n}", "port", "{\n  port = |,\n}");
        assert_accepts("{\n  |\n}", "log_level", "{\n  log_level = \"|\",\n}");
    }

    /// A schema whose fields carry defaults: an enum, an integer, a boolean
    /// and a string with a space in it.
    const DEFAULTS_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "retry": { "enum": ["slow", "fast"], "default": "fast" },
    "port": { "type": "integer", "default": 8080 },
    "enabled": { "type": "boolean", "default": true },
    "mode": { "type": "string", "default": "plain text" }
  }
}"#;

    /// Accepts `label` at the `|` of `marked` under `schema_json`: the text
    /// written and the selection after it (UTF-16 offsets).
    fn accept_with(
        schema_json: &str,
        marked: &str,
        label: &str,
    ) -> (String, std::ops::Range<usize>) {
        let (text, cursor) = unmark(marked);
        let schema = Schema::compile(schema_json).expect("the test schema compiles");
        let suggestion = schema
            .suggestions(NICKEL, &text, cursor)
            .into_iter()
            .find(|suggestion| suggestion.label() == label)
            .unwrap_or_else(|| panic!("{label} is not offered at the caret"));
        suggestion.accept(&text)
    }

    #[test]
    fn accepted_field_takes_the_indent_of_its_record_and_a_comma() {
        assert_accepts(
            "{\n  name = \"a\",\n  po|\n}",
            "port",
            "{\n  name = \"a\",\n  port = |,\n}",
        );
    }

    #[test]
    fn accepted_field_on_a_blank_line_takes_the_records_first_field_indent() {
        assert_accepts(
            "{\n  name = \"a\",\n|\n}",
            "port",
            "{\n  name = \"a\",\n  port = |,\n}",
        );
    }

    #[test]
    fn accepted_object_field_in_an_indented_record_is_a_block() {
        assert_accepts(
            "{\n  name = \"a\",\n  |\n}",
            "database",
            "{\n  name = \"a\",\n  database = {\n    |\n  },\n}",
        );
    }

    #[test]
    fn accepted_enum_default_is_a_tag_selected_for_typing() {
        let (text, selection) = accept_with(DEFAULTS_SCHEMA, "{\n  |\n}", "retry");
        assert_eq!(text, "{\n  retry = 'fast,\n}");
        assert_eq!(selection, 13..17, "the tag without its quote is selected");
    }

    #[test]
    fn accepted_integer_and_boolean_defaults_are_bare() {
        let (text, selection) = accept_with(DEFAULTS_SCHEMA, "{\n  |\n}", "port");
        assert_eq!(text, "{\n  port = 8080,\n}");
        assert_eq!(selection, 11..15);
        let (text, _) = accept_with(DEFAULTS_SCHEMA, "{\n  |\n}", "enabled");
        assert_eq!(text, "{\n  enabled = true,\n}");
    }

    /// A string field with a choice of values, for a value suggestion.
    const CHOICE_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "key": { "type": "string", "enum": ["certs/privkey.pem", "other"] }
  }
}"#;

    /// Accepting a value with the caret inside the empty quotes of a string
    /// field writes the value between those quotes, not a second pair.
    #[test]
    fn accepted_value_inside_quotes_takes_one_pair_of_quotes() {
        let (text, selection) =
            accept_with(CHOICE_SCHEMA, "{\n  key = \"|\",\n}\n", "certs/privkey.pem");
        assert_eq!(text, "{\n  key = \"certs/privkey.pem\",\n}\n");
        assert_eq!(selection, 29..29, "the caret goes after the closing quote");
    }

    #[test]
    fn accepted_string_default_is_quoted() {
        let (text, selection) = accept_with(DEFAULTS_SCHEMA, "{\n  |\n}", "mode");
        assert_eq!(text, "{\n  mode = \"plain text\",\n}");
        assert_eq!(selection, 12..22);
    }
}
