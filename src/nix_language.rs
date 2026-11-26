//! Nix expression language, NixOS flavour.
//!
//! A NixOS module is read as its top-level attribute set. Strings (`"..."`
//! and `''...''`), integers, floats, booleans, `null`, paths (kept as their
//! text), lists and nested sets are read; dotted keys are merged into nested
//! objects. Not read: a `{ config, pkgs, ... }:` header, `let ... in` blocks,
//! `with x;` scopes, `inherit`, and any binding whose value is an expression
//! (`pkgs.git`, `mkIf ...`, `a + b`, interpolated strings). Those bindings are
//! left out of the JSON.
//!
//! Completion, hover and key/value spans read the text itself, so they keep
//! working while the file does not parse yet.

use std::collections::HashMap;
use std::ops::Range;

use leptos_config_editor::language::{
    CompletionContext, ConfigLanguage, EntryPosition, Hover, InsertContext, InsertPlan, LanguageConfig,
    LanguageInfo, Markup, MultilineConstruct, ParseError, Planned, SchemaPathSegment, Span, Text,
    Token as Highlight, TokenKind, Tokens, utf16_to_byte_index,
};
use leptos_config_editor::{FieldPath, Language};
use serde_json::{Map, Number, Value};

/// The NixOS options subset: networking, services, users, environment, time,
/// i18n, boot loaders and system.stateVersion. Descriptions follow the NixOS
/// option declarations in nixpkgs; the enum value descriptions are written
/// from sshd_config(5).
pub const NIXOS_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "networking": {
      "type": "object",
      "properties": {
        "hostName": {
          "type": "string",
          "description": "The name of the machine. Leave it empty if you want to obtain it from a DHCP server (if using DHCP).",
          "default": "nixos",
          "x-docs-url": "https://search.nixos.org/options?show=networking.hostName"
        },
        "useDHCP": {
          "type": "boolean",
          "description": "Whether to use DHCP to obtain an IP address and other configuration for all network interfaces that do not have any manually configured IPv4 addresses.",
          "default": true,
          "x-docs-url": "https://search.nixos.org/options?show=networking.useDHCP"
        },
        "nameservers": {
          "type": "array",
          "items": { "type": "string" },
          "description": "The list of nameservers.  It can be left empty if it is auto-detected through DHCP.",
          "default": [],
          "x-docs-url": "https://search.nixos.org/options?show=networking.nameservers"
        },
        "firewall": {
          "type": "object",
          "properties": {
            "enable": {
              "type": "boolean",
              "description": "Whether to enable the firewall. This is a simple stateful firewall that blocks connection attempts to unauthorised TCP or UDP ports on this machine.",
              "default": true,
              "x-docs-url": "https://search.nixos.org/options?show=networking.firewall.enable"
            },
            "allowedTCPPorts": {
              "type": "array",
              "items": { "type": "integer", "minimum": 0, "maximum": 65535 },
              "description": "List of TCP ports on which incoming connections are accepted.",
              "default": [],
              "x-docs-url": "https://search.nixos.org/options?show=networking.firewall.allowedTCPPorts"
            }
          }
        }
      }
    },
    "services": {
      "type": "object",
      "properties": {
        "openssh": {
          "type": "object",
          "properties": {
            "enable": {
              "type": "boolean",
              "description": "Whether to enable the OpenSSH secure shell daemon, which allows secure remote logins.",
              "default": false,
              "x-docs-url": "https://search.nixos.org/options?show=services.openssh.enable"
            },
            "settings": {
              "type": "object",
              "properties": {
                "PermitRootLogin": {
                  "type": "string",
                  "enum": ["yes", "without-password", "prohibit-password", "forced-commands-only", "no"],
                  "x-enum-descriptions": [
                    "Root may log in with any authentication method the server allows.",
                    "Older spelling of prohibit-password: root may log in only with a key or another non-password method.",
                    "Root may log in only with a key or another non-password method.",
                    "Root may log in with a public key only, and only when that key's authorized_keys entry specifies a command.",
                    "Root may not log in."
                  ],
                  "description": "Whether the root user can login using ssh.",
                  "default": "prohibit-password",
                  "x-docs-url": "https://search.nixos.org/options?show=services.openssh.settings.PermitRootLogin"
                }
              }
            }
          }
        },
        "nginx": {
          "type": "object",
          "properties": {
            "enable": {
              "type": "boolean",
              "description": "Whether to enable Nginx Web Server.",
              "default": false,
              "x-docs-url": "https://search.nixos.org/options?show=services.nginx.enable"
            },
            "virtualHosts": {
              "type": "object",
              "description": "Declarative vhost config",
              "additionalProperties": {
                "type": "object",
                "properties": {
                  "serverName": {
                    "type": "string",
                    "description": "Name of this virtual host. Defaults to attribute name in virtualHosts."
                  }
                }
              }
            }
          }
        },
        "postgresql": {
          "type": "object",
          "properties": {
            "enable": {
              "type": "boolean",
              "description": "Whether to enable PostgreSQL Server.",
              "default": false,
              "x-docs-url": "https://search.nixos.org/options?show=services.postgresql.enable"
            },
            "package": {
              "type": "string",
              "description": "The package being used by postgresql.",
              "x-docs-url": "https://search.nixos.org/options?show=services.postgresql.package"
            },
            "ensureDatabases": {
              "type": "array",
              "items": { "type": "string" },
              "description": "Ensures that the specified databases exist. This option will never delete existing databases, especially not when the value of this option is changed. This means that databases created once through this option or otherwise have to be removed manually.",
              "default": [],
              "x-docs-url": "https://search.nixos.org/options?show=services.postgresql.ensureDatabases"
            }
          }
        }
      }
    },
    "users": {
      "type": "object",
      "properties": {
        "users": {
          "type": "object",
          "additionalProperties": {
            "type": "object",
            "properties": {
              "isNormalUser": {
                "type": "boolean",
                "description": "Indicates whether this is an account for a “real” user. This automatically sets group to users, createHome to true, home to /home/«username», useDefaultShell to true, and isSystemUser to false. Exactly one of isNormalUser and isSystemUser must be true.",
                "default": false
              },
              "extraGroups": {
                "type": "array",
                "items": { "type": "string" },
                "description": "The user's auxiliary groups.",
                "default": []
              },
              "shell": {
                "type": "string",
                "description": "The path to the user's shell. Can use shell derivations, like pkgs.bashInteractive. Don’t forget to enable your shell in programs if necessary, like programs.zsh.enable = true;."
              }
            }
          }
        }
      }
    },
    "environment": {
      "type": "object",
      "properties": {
        "systemPackages": {
          "type": "array",
          "items": { "type": "string" },
          "description": "The set of packages that appear in /run/current-system/sw. These packages are automatically available to all users, and are automatically updated every time you rebuild the system configuration. (The latter is the main difference with installing them in the default profile, /nix/var/nix/profiles/default.)",
          "default": [],
          "x-docs-url": "https://search.nixos.org/options?show=environment.systemPackages"
        }
      }
    },
    "time": {
      "type": "object",
      "properties": {
        "timeZone": {
          "type": ["string", "null"],
          "description": "The time zone used when displaying times and dates. See https://en.wikipedia.org/wiki/List_of_tz_database_time_zones for a comprehensive list of possible values for this setting. If null, the timezone will default to UTC and can be set imperatively using timedatectl.",
          "default": null,
          "x-docs-url": "https://search.nixos.org/options?show=time.timeZone"
        }
      }
    },
    "i18n": {
      "type": "object",
      "properties": {
        "defaultLocale": {
          "type": "string",
          "description": "The default locale. It determines the language for program messages, the format for dates and times, sort order, and so on. Setting the default character set is done via i18n.defaultCharset.",
          "default": "en_US.UTF-8",
          "x-docs-url": "https://search.nixos.org/options?show=i18n.defaultLocale"
        }
      }
    },
    "boot": {
      "type": "object",
      "properties": {
        "loader": {
          "type": "object",
          "properties": {
            "systemd-boot": {
              "type": "object",
              "properties": {
                "enable": {
                  "type": "boolean",
                  "description": "Whether to enable the systemd-boot (formerly gummiboot) EFI boot manager.",
                  "default": false,
                  "x-docs-url": "https://search.nixos.org/options?show=boot.loader.systemd-boot.enable"
                }
              }
            },
            "grub": {
              "type": "object",
              "properties": {
                "enable": {
                  "type": "boolean",
                  "description": "Whether to enable the GNU GRUB boot loader.",
                  "x-docs-url": "https://search.nixos.org/options?show=boot.loader.grub.enable"
                }
              }
            }
          }
        }
      }
    },
    "system": {
      "type": "object",
      "properties": {
        "stateVersion": {
          "type": "string",
          "description": "This option defines the first version of NixOS you have installed on this particular machine, and is used to maintain compatibility with application data (e.g. databases) created on older NixOS versions. For example, if NixOS version XX.YY ships with AwesomeDB version N by default, and is then upgraded to version XX.YY+1, which ships AwesomeDB version N+1, the existing databases may no longer be compatible, causing applications to fail, or even leading to data loss. The stateVersion mechanism avoids this situation by making the default version of such packages conditional on the first version of NixOS you've installed (encoded in stateVersion), instead of simply always using the latest one. Note that this generally only affects applications that can't upgrade their data automatically - applications and services supporting automatic migrations will remain on latest versions when you upgrade. Most users should never change this value after the initial install, for any reason, even if you've upgraded your system to a new NixOS release. This value does not affect the Nixpkgs version your packages and OS are pulled from, so changing it will not upgrade your system. This value being lower than the current NixOS release does not mean your system is out of date, out of support, or vulnerable. Do not change this value unless you have manually inspected all the changes it would make to your configuration, and migrated your data accordingly.",
          "x-docs-url": "https://search.nixos.org/options?show=system.stateVersion"
        }
      }
    }
  }
}"#;

/// The Nix language, NixOS module files.
pub struct Nix;

static NIX_LANGUAGE: Nix = Nix;

/// The Nix language for `Language`-driven editors.
pub const NIX: Language = Language::new(&NIX_LANGUAGE);

// ---------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Kind {
    /// An identifier or keyword (`networking`, `let`, `true`).
    Ident(String),
    /// An integer or float as written (`80`, `1.5`).
    Num(String),
    /// A `"..."` string: its decoded text, whether it has `${...}`, whether it is closed.
    Str { text: String, interpolated: bool, closed: bool },
    /// A `''...''` string, decoded and dedented.
    Indented { text: String, interpolated: bool, closed: bool },
    /// A path literal as written (`./hardware.nix`).
    Path(String),
    /// Structural punctuation: `= ; . { } [ ] ( ) : , @ ? ...`.
    Punct(&'static str),
    /// Operators other than the punctuation above (`==`, `//`, `-`, `+`).
    Op(String),
    /// A `# line` or `/* block */` comment.
    Comment { block: bool, closed: bool },
    /// Any other character.
    Other(char),
}

#[derive(Clone, Debug, PartialEq)]
struct Token {
    kind: Kind,
    /// Byte offsets into the source.
    start: usize,
    end: usize,
}

impl Token {
    fn is_punct(&self, punct: &str) -> bool {
        matches!(&self.kind, Kind::Punct(p) if *p == punct)
    }

    fn is_comment(&self) -> bool {
        matches!(self.kind, Kind::Comment { .. })
    }
}

fn count_while(text: &str, keep: impl Fn(u8) -> bool) -> usize {
    text.bytes().take_while(|b| keep(*b)).count()
}

fn unescape(next: char) -> char {
    match next {
        'n' => '\n',
        't' => '\t',
        'r' => '\r',
        other => other,
    }
}

/// The end of a `${...}` interpolation whose body starts at `from`.
fn skip_interpolation(src: &str, from: usize) -> usize {
    let mut depth = 1;
    let mut i = from;
    while let Some(ch) = src[i..].chars().next() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            '"' => {
                i = lex_quoted(src, i + 1).3;
                continue;
            }
            _ => {}
        }
        i += ch.len_utf8();
    }
    src.len()
}

/// A `"..."` string whose body starts at `from`: (text, interpolated, closed, end).
fn lex_quoted(src: &str, from: usize) -> (String, bool, bool, usize) {
    let mut text = String::new();
    let mut interpolated = false;
    let mut i = from;
    while let Some(ch) = src[i..].chars().next() {
        match ch {
            '"' => return (text, interpolated, true, i + 1),
            '\\' => match src[i + 1..].chars().next() {
                Some(next) => {
                    text.push(unescape(next));
                    i += 1 + next.len_utf8();
                }
                None => i = src.len(),
            },
            '$' if src[i + 1..].starts_with('{') => {
                interpolated = true;
                i = skip_interpolation(src, i + 2);
            }
            _ => {
                text.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    (text, interpolated, false, src.len())
}

/// Removes the common indentation of an indented string's lines, as Nix does.
fn dedent(text: &str) -> String {
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.first().is_some_and(|line| line.trim().is_empty()) {
        lines.remove(0);
    }
    if lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    let indent = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start_matches([' ', '\t']).len())
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .map(|line| if line.trim().is_empty() { "" } else { &line[indent..] })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A `''...''` string whose body starts at `from`: (text, interpolated, closed, end).
fn lex_indented(src: &str, from: usize) -> (String, bool, bool, usize) {
    let mut raw = String::new();
    let mut interpolated = false;
    let mut i = from;
    loop {
        if src[i..].starts_with("''") {
            let after = &src[i + 2..];
            if after.starts_with('\'') {
                raw.push_str("''");
                i += 3;
            } else if after.starts_with('$') {
                raw.push('$');
                i += 3;
            } else if let Some(next) = after.strip_prefix('\\').and_then(|rest| rest.chars().next()) {
                raw.push(unescape(next));
                i += 3 + next.len_utf8();
            } else {
                return (dedent(&raw), interpolated, true, i + 2);
            }
        } else if src[i..].starts_with("${") {
            interpolated = true;
            i = skip_interpolation(src, i + 2);
        } else {
            match src[i..].chars().next() {
                Some(ch) => {
                    raw.push(ch);
                    i += ch.len_utf8();
                }
                None => return (dedent(&raw), interpolated, false, src.len()),
            }
        }
    }
}

/// Splits `src` into tokens, comments included. Tolerant: an unclosed string
/// or comment runs to the end of the text, so a prefix of a file lexes too.
fn lex(src: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < src.len() {
        let rest = &src[i..];
        let c = rest.as_bytes()[0];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        let (kind, end) = if c == b'#' {
            let end = rest.find('\n').map_or(src.len(), |n| i + n);
            (Kind::Comment { block: false, closed: true }, end)
        } else if rest.starts_with("/*") {
            match src[i + 2..].find("*/") {
                Some(n) => (Kind::Comment { block: true, closed: true }, i + 2 + n + 2),
                None => (Kind::Comment { block: true, closed: false }, src.len()),
            }
        } else if c == b'"' {
            let (text, interpolated, closed, end) = lex_quoted(src, i + 1);
            (Kind::Str { text, interpolated, closed }, end)
        } else if rest.starts_with("''") {
            let (text, interpolated, closed, end) = lex_indented(src, i + 2);
            (Kind::Indented { text, interpolated, closed }, end)
        } else if c.is_ascii_alphabetic() || c == b'_' {
            let end = i + count_while(rest, |b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'\'' | b'-'));
            (Kind::Ident(src[i..end].to_string()), end)
        } else if c.is_ascii_digit() {
            let mut end = i + count_while(rest, |b| b.is_ascii_digit());
            if src[end..].starts_with('.') && src[end + 1..].bytes().next().is_some_and(|b| b.is_ascii_digit()) {
                end += 1 + count_while(&src[end + 1..], |b| b.is_ascii_digit());
            }
            (Kind::Num(src[i..end].to_string()), end)
        } else if rest.starts_with("./")
            || rest.starts_with("../")
            || rest.starts_with("~/")
            || (c == b'/' && rest[1..].bytes().next().is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.'))
        {
            let end = i + count_while(rest, |b| b.is_ascii_alphanumeric() || b"._-+/~".contains(&b));
            (Kind::Path(src[i..end].to_string()), end)
        } else if rest.starts_with("...") {
            (Kind::Punct("..."), i + 3)
        } else if let Some(op) = ["==", "!=", "<=", ">=", "&&", "||", "->", "//", "++"]
            .into_iter()
            .find(|op| rest.starts_with(op))
        {
            (Kind::Op(op.to_string()), i + 2)
        } else {
            let punct = match c {
                b';' => Some(";"),
                b'=' => Some("="),
                b'.' => Some("."),
                b'{' => Some("{"),
                b'}' => Some("}"),
                b'[' => Some("["),
                b']' => Some("]"),
                b'(' => Some("("),
                b')' => Some(")"),
                b':' => Some(":"),
                b',' => Some(","),
                b'@' => Some("@"),
                b'?' => Some("?"),
                _ => None,
            };
            match punct {
                Some(p) => (Kind::Punct(p), i + 1),
                None if b"+-*/<>!|&".contains(&c) => (Kind::Op((c as char).to_string()), i + 1),
                None => {
                    let ch = rest.chars().next().expect("a character");
                    (Kind::Other(ch), i + ch.len_utf8())
                }
            }
        };
        out.push(Token { kind, start, end });
        i = end;
    }
    out
}

/// The tokens of `src` without comments.
fn code_tokens(src: &str) -> Vec<Token> {
    lex(src).into_iter().filter(|token| !token.is_comment()).collect()
}

/// The name a token gives an attribute path segment, if it can be one.
fn name_of(token: &Token) -> Option<String> {
    match &token.kind {
        Kind::Ident(name) => Some(name.clone()),
        Kind::Num(num) if !num.contains('.') => Some(num.clone()),
        Kind::Str { text, interpolated: false, .. } => Some(text.clone()),
        _ => None,
    }
}

/// The names of an attribute path written `a."b".c`; `None` if the tokens are
/// not one. An empty run is the empty path.
fn attrpath_names(tokens: &[Token]) -> Option<Vec<String>> {
    if tokens.len() % 2 == 0 && !tokens.is_empty() {
        return None;
    }
    let mut names = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        if index % 2 == 1 {
            if !token.is_punct(".") {
                return None;
            }
        } else {
            names.push(name_of(token)?);
        }
    }
    Some(names)
}

fn path_segments(names: &[String]) -> Vec<SchemaPathSegment> {
    names.iter().map(|name| SchemaPathSegment::Key(name.clone())).collect()
}

fn is_identifier(key: &str) -> bool {
    const KEYWORDS: [&str; 13] = [
        "assert", "else", "false", "if", "in", "inherit", "let", "null", "or", "rec", "then", "true", "with",
    ];
    if KEYWORDS.contains(&key) {
        return false;
    }
    let mut chars = key.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '\'' | '-'))
}

// ---------------------------------------------------------------------------
// Scope scan, for completion and hover on text that may not parse
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scope {
    /// An attribute set; its bindings sit at `Level::path`.
    Set,
    List,
    Paren,
    /// Between `let` and `in`.
    Let,
    /// A function body, a `with` body or anything else no key is offered in.
    Other,
}

#[derive(Clone, Debug)]
struct Level {
    scope: Scope,
    path: Vec<String>,
}

struct Scanned {
    levels: Vec<Level>,
    /// The tokens of the statement (binding) the scan is in.
    stmt: Vec<Token>,
}

impl Scanned {
    fn top(&self) -> &Level {
        self.levels.last().expect("the root level")
    }

    /// The level a `{` opens, given the statement before it.
    fn open_brace(&self) -> Level {
        let top = self.top();
        let base = top.path.clone();
        if top.scope != Scope::Set {
            return Level { scope: Scope::Other, path: base };
        }
        match self.stmt.split_last() {
            None => Level { scope: Scope::Set, path: base },
            Some((last, _)) if matches!(&last.kind, Kind::Ident(name) if name.as_str() == "rec") => {
                Level { scope: Scope::Set, path: base }
            }
            Some((last, _)) if last.is_punct(":") || last.is_punct("@") => {
                Level { scope: Scope::Set, path: base }
            }
            Some((last, attr)) if last.is_punct("=") => match attrpath_names(attr) {
                Some(names) if !names.is_empty() => {
                    let mut path = base;
                    path.extend(names);
                    Level { scope: Scope::Set, path }
                }
                _ => Level { scope: Scope::Other, path: base },
            },
            Some(_) => Level { scope: Scope::Other, path: base },
        }
    }
}

fn scan(tokens: &[Token]) -> Scanned {
    let mut state = Scanned {
        levels: vec![Level { scope: Scope::Set, path: Vec::new() }],
        stmt: Vec::new(),
    };
    for token in tokens {
        if token.is_comment() {
            continue;
        }
        match &token.kind {
            Kind::Punct("{") => {
                let level = state.open_brace();
                state.levels.push(level);
                state.stmt.clear();
            }
            Kind::Punct("[") | Kind::Punct("(") => {
                let scope = if token.is_punct("[") { Scope::List } else { Scope::Paren };
                let path = state.top().path.clone();
                state.levels.push(Level { scope, path });
                state.stmt.clear();
            }
            Kind::Punct("}") | Kind::Punct("]") | Kind::Punct(")") => {
                while state.levels.len() > 1 && state.top().scope == Scope::Let {
                    state.levels.pop();
                }
                if state.levels.len() > 1 {
                    state.levels.pop();
                }
                state.stmt.clear();
            }
            Kind::Punct(";") => state.stmt.clear(),
            Kind::Ident(name) if name.as_str() == "let" => {
                let path = state.top().path.clone();
                state.levels.push(Level { scope: Scope::Let, path });
                state.stmt.clear();
            }
            Kind::Ident(name) if name.as_str() == "in" => {
                if state.levels.len() > 1 && state.top().scope == Scope::Let {
                    state.levels.pop();
                }
                state.stmt.clear();
            }
            _ => state.stmt.push(token.clone()),
        }
    }
    state
}

/// The completion context at a UTF-16 `cursor` of `source`.
fn completion_at(source: &str, cursor16: usize) -> CompletionContext {
    let cursor = utf16_to_byte_index(source, cursor16);
    let prefix = &source[..cursor];
    let tokens = lex(prefix);
    let line_start = prefix.rfind('\n').map_or(0, |at| at + 1);
    let base_indent: String = prefix[line_start..]
        .chars()
        .take_while(|ch| *ch == ' ' || *ch == '\t')
        .collect();
    let utf16 = |byte: usize| source[..byte].encode_utf16().count();

    let state = scan(&tokens);
    let top = state.top().clone();
    let mut context = CompletionContext {
        path: path_segments(&top.path),
        base_indent,
        ..Default::default()
    };
    let in_comment = tokens
        .last()
        .is_some_and(|t| t.is_comment() && t.end == cursor && !matches!(t.kind, Kind::Comment { block: true, closed: true }));
    if in_comment || top.scope != Scope::Set {
        context.no_properties = true;
        return context;
    }
    let stmt = &state.stmt;
    match stmt.iter().position(|t| t.is_punct("=")) {
        Some(eq) => {
            // After `=`: the caret is in the value of the key before it.
            let inert = |mut context: CompletionContext| {
                context.no_properties = true;
                context
            };
            let Some(names) = attrpath_names(&stmt[..eq]) else { return inert(context) };
            let Some((key, head)) = names.split_last() else { return inert(context) };
            let mut path = top.path.clone();
            path.extend(head.iter().cloned());
            context.path = path_segments(&path);
            context.value_key = Some(key.clone());
            context.replace_start = utf16(cursor);
            context.replace_end = cursor16;
            match &stmt[eq + 1..] {
                [] => {}
                [token] if token.end == cursor => match name_of(token) {
                    Some(text) => {
                        context.typed = text;
                        context.replace_start = utf16(token.start);
                        // A value is written with its quotes, so the range
                        // covers the whole literal, closing quote included;
                        // else the quote after the caret would be left behind.
                        if matches!(token.kind, Kind::Str { .. }) {
                            let (_, interpolated, closed, end) = lex_quoted(source, token.start + 1);
                            if closed && !interpolated {
                                context.replace_end = utf16(end);
                            }
                        }
                    }
                    None => return inert(context),
                },
                _ => return inert(context),
            }
            context
        }
        None => {
            // A key: the caret starts a binding, or types the next segment of one.
            if stmt.is_empty() {
                context.replace_start = utf16(cursor);
                context.replace_end = cursor16;
                return context;
            }
            let inert = |mut context: CompletionContext| {
                context.no_properties = true;
                context
            };
            let last = stmt.last().expect("a non-empty statement");
            let (head_tokens, typed) = if last.end == cursor && name_of(last).is_some() {
                (&stmt[..stmt.len() - 1], Some(last))
            } else if last.is_punct(".") && last.end == cursor {
                (&stmt[..stmt.len() - 1], None)
            } else {
                return inert(context);
            };
            // The head is the path before the caret, up to its last dot.
            let head_tokens = match typed {
                _ if head_tokens.is_empty() => head_tokens,
                Some(_) => match head_tokens.split_last() {
                    Some((dot, rest)) if dot.is_punct(".") => rest,
                    _ => return inert(context),
                },
                None => head_tokens,
            };
            let Some(head) = attrpath_names(head_tokens) else { return inert(context) };
            if typed.is_none() && head.is_empty() {
                return inert(context);
            }
            let dotted = !head.is_empty();
            let mut path = top.path.clone();
            path.extend(head);
            context.path = path_segments(&path);
            match typed {
                Some(token) => {
                    context.typed = name_of(token).unwrap_or_default();
                    context.replace_start = utf16(token.start);
                }
                None => {
                    // Typing a dot opens the children of the path before it
                    // without a typed word; the crate hides an empty query
                    // unless the entry start is marked explicit.
                    context.replace_start = utf16(cursor);
                    context.explicit_entry_start = true;
                }
            }
            // A dotted path continues on the line: an object child is
            // written as `name.`, not as a block.
            if dotted {
                context.position = EntryPosition::Inline;
            }
            context.replace_end = cursor16;
            context
        }
    }
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

struct Document {
    value: Value,
    keys: HashMap<Vec<String>, Span>,
    values: HashMap<Vec<String>, Span>,
    /// What the value leaves out (a `let`, a `with`, an expression), in order.
    lost: Vec<Loss>,
}

/// A construct the data model skips, so converting the file would drop it.
struct Loss {
    line: usize,
    /// What was skipped, as the message names it (`` `let` block ``).
    what: String,
    /// The JSON pointer of the skipped binding's place in the set.
    pointer: String,
}

/// Why a value could not be read: a construct to skip, or a syntax error.
enum Failure {
    /// A construct the value cannot hold: the binding is left out.
    Unsupported,
    Error(ParseError),
}

impl From<ParseError> for Failure {
    fn from(error: ParseError) -> Self {
        Failure::Error(error)
    }
}

struct Parser<'a> {
    src: &'a str,
    toks: Vec<Token>,
    at: usize,
    line_starts: Vec<usize>,
    keys: HashMap<Vec<String>, Span>,
    values: HashMap<Vec<String>, Span>,
    /// The paths of the spans recorded so far, in order: a skipped binding
    /// removes the ones its partial parse recorded.
    span_log: Vec<Vec<String>>,
    /// What the skipped constructs so far leave out.
    lost: Vec<Loss>,
}

/// The index of the `}` closing the `{` at `open`.
fn matching_brace(toks: &[Token], open: usize) -> Option<usize> {
    let mut depth = 0;
    for (index, token) in toks.iter().enumerate().skip(open) {
        if token.is_punct("{") {
            depth += 1;
        } else if token.is_punct("}") {
            depth -= 1;
            if depth == 0 {
                return Some(index);
            }
        }
    }
    None
}

/// Adds `value` at `names` in `map`, merging into existing sets.
fn insert_value(map: &mut Map<String, Value>, names: &[String], value: Value) -> Result<(), String> {
    let Some((last, init)) = names.split_last() else {
        return Err("empty attribute path".to_string());
    };
    let mut current = map;
    for name in init {
        let entry = current
            .entry(name.clone())
            .or_insert_with(|| Value::Object(Map::new()));
        match entry {
            Value::Object(inner) => current = inner,
            _ => return Err(format!("attribute `{name}` is already defined as a value")),
        }
    }
    if !current.contains_key(last) {
        current.insert(last.clone(), value);
        return Ok(());
    }
    let existing = current.get_mut(last).expect("the key is present");
    merge(existing, value, last)
}

fn merge(existing: &mut Value, value: Value, name: &str) -> Result<(), String> {
    match (existing, value) {
        (Value::Object(into), Value::Object(from)) => {
            for (key, value) in from {
                if into.contains_key(&key) {
                    let slot = into.get_mut(&key).expect("the key is present");
                    merge(slot, value, &key)?;
                } else {
                    into.insert(key, value);
                }
            }
            Ok(())
        }
        _ => Err(format!("attribute `{name}` is already defined")),
    }
}


impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self {
        let mut line_starts = vec![0];
        line_starts.extend(src.match_indices('\n').map(|(at, _)| at + 1));
        Parser {
            src,
            toks: code_tokens(src),
            at: 0,
            line_starts,
            keys: HashMap::new(),
            values: HashMap::new(),
            span_log: Vec::new(),
            lost: Vec::new(),
        }
    }

    /// Notes a construct skipped at token `at`, inside the set at `path`.
    fn note_lost(&mut self, at: usize, what: String, path: Option<&[String]>) {
        let line = self.toks.get(at).map_or(1, |token| self.locate(token.start).0);
        let mut pointer: String = path.unwrap_or_default().iter().map(|name| format!("/{name}")).collect();
        if pointer.is_empty() {
            pointer.push('/');
        }
        self.lost.push(Loss { line, what, pointer });
    }

    fn peek(&self) -> Option<&Token> {
        self.toks.get(self.at)
    }

    fn peek_is(&self, punct: &str) -> bool {
        self.peek().is_some_and(|t| t.is_punct(punct))
    }

    /// One-based (line, character column) of a byte offset.
    fn locate(&self, byte: usize) -> (usize, usize) {
        let index = self.line_starts.partition_point(|&start| start <= byte) - 1;
        let column = self.src[self.line_starts[index]..byte].chars().count() + 1;
        (index + 1, column)
    }

    fn line_end_column(&self, line: usize) -> usize {
        let start = self.line_starts[line - 1];
        self.src[start..].split('\n').next().unwrap_or("").chars().count() + 1
    }

    /// A one-line span; a range over lines is cut at the end of its first line.
    fn span(&self, start: usize, end: usize) -> Span {
        let (line, start_column) = self.locate(start);
        let (end_line, end_column) = self.locate(end);
        let end_column = if end_line == line { end_column } else { self.line_end_column(line) };
        Span { line, start_column, end_column }
    }

    fn error(&self, token: Option<&Token>, message: impl Into<String>) -> ParseError {
        let byte = token.map_or(self.src.len(), |t| t.start);
        let (line, column) = self.locate(byte);
        ParseError::new(message, Some(line), Some(column))
    }

    /// The error at the end of the text: names the outermost bracket still open.
    fn eof_error(&self) -> ParseError {
        let mut open: Vec<usize> = Vec::new();
        for (index, token) in self.toks.iter().enumerate() {
            match &token.kind {
                Kind::Punct("{") | Kind::Punct("[") | Kind::Punct("(") => open.push(index),
                Kind::Punct("}") | Kind::Punct("]") | Kind::Punct(")") => {
                    open.pop();
                }
                _ => {}
            }
        }
        match open.first() {
            Some(&index) => {
                let token = &self.toks[index];
                let (opener, closer) = match &token.kind {
                    Kind::Punct("[") => ("[", "]"),
                    Kind::Punct("(") => ("(", ")"),
                    _ => ("{", "}"),
                };
                let line = self.locate(token.start).0;
                self.error(Some(token), format!("missing `{closer}` to close the `{opener}` on line {line}"))
            }
            None => self.error(None, "missing `;` at the end of the file"),
        }
    }

    fn expect(&mut self, punct: &str) -> Result<(), ParseError> {
        if self.peek_is(punct) {
            self.at += 1;
            Ok(())
        } else {
            Err(self.error(self.peek(), format!("expected `{punct}`")))
        }
    }

    /// Skips to the `;` ending the current statement (consumed), or to a
    /// closing bracket that ends the enclosing set or list (not consumed).
    fn skip_statement(&mut self) {
        // Open `let` and `with` scopes, each with the bracket depth it opened
        // at: the `;` of `with pkgs;` and of a `let` binding end inside the
        // statement, not the statement itself.
        let mut depth = 0i32;
        let mut scopes: Vec<(bool, i32)> = Vec::new();
        while let Some(token) = self.toks.get(self.at) {
            match &token.kind {
                Kind::Punct("{") | Kind::Punct("[") | Kind::Punct("(") => depth += 1,
                Kind::Punct("}") | Kind::Punct("]") | Kind::Punct(")") => {
                    if depth == 0 {
                        return;
                    }
                    depth -= 1;
                    scopes.retain(|(_, opened)| *opened <= depth);
                }
                Kind::Punct(";") => match scopes.last() {
                    Some((is_let, opened)) if *opened == depth => {
                        if !is_let {
                            scopes.pop();
                        }
                    }
                    _ if depth == 0 => {
                        self.at += 1;
                        return;
                    }
                    _ => {}
                },
                Kind::Ident(name) if name.as_str() == "let" => scopes.push((true, depth)),
                Kind::Ident(name) if name.as_str() == "with" => scopes.push((false, depth)),
                Kind::Ident(name) if name.as_str() == "in" => {
                    if let Some(at) = scopes.iter().rposition(|(is_let, _)| *is_let) {
                        scopes.truncate(at);
                    }
                }
                _ => {}
            }
            self.at += 1;
        }
    }

    fn skip_let(&mut self) -> Result<(), ParseError> {
        let let_token = self.toks[self.at].clone();
        self.at += 1;
        let mut depth = 0i32;
        let mut nested_lets = 0;
        while let Some(token) = self.toks.get(self.at) {
            match &token.kind {
                Kind::Punct("{") | Kind::Punct("[") | Kind::Punct("(") => depth += 1,
                Kind::Punct("}") | Kind::Punct("]") | Kind::Punct(")") => depth -= 1,
                Kind::Ident(name) if name.as_str() == "let" => nested_lets += 1,
                Kind::Ident(name) if name.as_str() == "in" => {
                    if nested_lets == 0 && depth == 0 {
                        self.at += 1;
                        return Ok(());
                    }
                    nested_lets -= 1;
                }
                _ => {}
            }
            self.at += 1;
        }
        Err(self.error(Some(&let_token), "`let` without `in`"))
    }

    fn skip_with(&mut self) {
        self.at += 1;
        self.skip_statement();
    }

    /// Whether the file starts with a `{ ... }:` or `x:` function header.
    fn header_ahead(&self) -> bool {
        match self.toks.first() {
            Some(first) if first.is_punct("{") => matching_brace(&self.toks, 0)
                .and_then(|close| self.toks.get(close + 1))
                .is_some_and(|next| next.is_punct(":") || next.is_punct("@")),
            Some(Token { kind: Kind::Ident(_), .. }) => self.toks.get(1).is_some_and(|t| t.is_punct(":")),
            _ => false,
        }
    }

    fn skip_header(&mut self) {
        if self.peek_is("{") {
            if let Some(close) = matching_brace(&self.toks, self.at) {
                self.at = close + 1;
            }
            if self.peek_is("@") {
                self.at += 2;
            }
        } else {
            self.at += 1;
        }
        if self.peek_is(":") {
            self.at += 1;
        }
    }

    fn document(&mut self) -> Result<Value, ParseError> {
        if self.toks.is_empty() {
            return Ok(Value::Object(Map::new()));
        }
        if self.header_ahead() {
            self.note_lost(self.at, "function header".to_string(), None);
            self.skip_header();
        }
        loop {
            let keyword = match self.peek().map(|t| &t.kind) {
                Some(Kind::Ident(name)) => name.clone(),
                _ => break,
            };
            match keyword.as_str() {
                "let" => {
                    self.note_lost(self.at, "`let` block".to_string(), None);
                    self.skip_let()?;
                }
                "with" => {
                    self.note_lost(self.at, "`with` scope".to_string(), None);
                    self.skip_with();
                }
                _ => break,
            }
        }
        if matches!(self.peek().map(|t| &t.kind), Some(Kind::Ident(name)) if name.as_str() == "rec") {
            self.at += 1;
        }
        match self.peek() {
            Some(token) if token.is_punct("{") => {}
            Some(token) => {
                return Err(self.error(
                    Some(token),
                    "the file must be one attribute set, `{ ... }`: this editor reads a module's top-level attribute set",
                ));
            }
            None => return Ok(Value::Object(Map::new())),
        }
        let root: Vec<String> = Vec::new();
        let map = self.attrset(Some(&root))?;
        if let Some(token) = self.peek() {
            return Err(self.error(Some(token), "unexpected text after the attribute set"));
        }
        Ok(Value::Object(map))
    }

    /// `{ bindings }`, read into a map. `path` is the set's own path, for spans.
    fn attrset(&mut self, path: Option<&[String]>) -> Result<Map<String, Value>, ParseError> {
        self.expect("{")?;
        let mut map = Map::new();
        loop {
            let next = self.peek().map(|t| {
                (
                    t.is_punct("}"),
                    matches!(&t.kind, Kind::Ident(name) if name.as_str() == "inherit"),
                )
            });
            match next {
                None => return Err(self.eof_error()),
                Some((true, _)) => {
                    self.at += 1;
                    return Ok(map);
                }
                Some((_, true)) => {
                    self.note_lost(self.at, "`inherit`".to_string(), path);
                    self.skip_statement();
                }
                Some(_) => self.binding(&mut map, path)?,
            }
        }
    }

    /// `a.b."c" = value;` as an attribute path with each segment's token range.
    fn attrpath(&mut self) -> Result<Vec<(String, usize, usize)>, Failure> {
        let mut names = Vec::new();
        loop {
            let Some(token) = self.peek().cloned() else {
                return Err(self.error(None, "expected an attribute name").into());
            };
            let name = match &token.kind {
                Kind::Ident(name) => name.clone(),
                Kind::Num(num) if !num.contains('.') => num.clone(),
                Kind::Str { text, interpolated: false, closed: true } => text.clone(),
                Kind::Str { interpolated: true, .. } | Kind::Indented { interpolated: true, .. } => {
                    return Err(Failure::Unsupported);
                }
                Kind::Str { closed: false, .. } => {
                    return Err(self.error(Some(&token), "unterminated string").into());
                }
                _ => return Err(self.error(Some(&token), "expected an attribute name").into()),
            };
            self.at += 1;
            names.push((name, token.start, token.end));
            if self.peek_is(".") {
                self.at += 1;
            } else {
                return Ok(names);
            }
        }
    }

    fn binding(&mut self, map: &mut Map<String, Value>, path: Option<&[String]>) -> Result<(), ParseError> {
        let key_at = self.at;
        let segments = match self.attrpath() {
            Ok(segments) => segments,
            Err(Failure::Unsupported) => {
                self.at = key_at;
                self.note_lost(key_at, "attribute".to_string(), path);
                self.skip_statement();
                return Ok(());
            }
            Err(Failure::Error(error)) => return Err(error),
        };
        let names: Vec<String> = segments.iter().map(|(name, _, _)| name.clone()).collect();
        if !self.peek_is("=") {
            return Err(self.error(self.peek(), "expected `=` after the attribute name"));
        }
        self.at += 1;
        if self.peek_is(";") {
            return Err(self.error(self.peek(), "expected a value after `=`"));
        }
        let full: Option<Vec<String>> = path.map(|p| p.iter().cloned().chain(names.iter().cloned()).collect());
        let value_at = self.at;
        let spans_before = self.span_log.len();
        let key_start = segments[0].1;
        let key_end = segments[segments.len() - 1].2;

        // What the binding's partial parse noted is dropped with its spans;
        // the binding itself is noted once, by its own name.
        let lost_before = self.lost.len();
        let lost_at = full.clone();
        let skip = |this: &mut Self| {
            for path in this.span_log.split_off(spans_before) {
                this.keys.remove(&path);
                this.values.remove(&path);
            }
            this.lost.truncate(lost_before);
            this.note_lost(key_at, "attribute".to_string(), lost_at.as_deref().or(path));
            this.at = key_at;
            this.skip_statement();
        };

        match self.value(full.as_deref()) {
            Ok(value) => {
                if !self.peek_is(";") {
                    if matches!(self.peek().map(|t| &t.kind), Some(Kind::Op(_))) {
                        skip(self);
                        return Ok(());
                    }
                    if self.peek().is_none() {
                        return Err(self.eof_error());
                    }
                    let last = self.toks[self.at - 1].clone();
                    let line = self.locate(last.start).0;
                    let text = &self.src[last.start..last.end];
                    return Err(self.error(Some(&last), format!("missing `;` after `{text}` on line {line}")));
                }
                let value_start = self.toks[value_at].start;
                let value_end = self.toks[self.at - 1].end;
                self.at += 1;
                if let Err(message) = insert_value(map, &names, value) {
                    return Err(self.error(self.toks.get(key_at), message));
                }
                if let Some(full) = full {
                    let key_span = self.span(key_start, key_end);
                    let value_span = self.span(value_start, value_end);
                    self.keys.insert(full.clone(), key_span);
                    self.values.insert(full.clone(), value_span);
                    self.span_log.push(full);
                }
                Ok(())
            }
            Err(Failure::Unsupported) => {
                skip(self);
                Ok(())
            }
            Err(Failure::Error(error)) => Err(error),
        }
    }

    /// A value: a literal, a list, or a set (read with `path` for its own bindings).
    fn value(&mut self, path: Option<&[String]>) -> Result<Value, Failure> {
        let Some(token) = self.peek().cloned() else {
            return Err(self.error(None, "expected a value").into());
        };
        match &token.kind {
            Kind::Punct("{") => Ok(Value::Object(self.attrset(path)?)),
            Kind::Ident(name) if name.as_str() == "rec" => {
                self.at += 1;
                Ok(Value::Object(self.attrset(path)?))
            }
            Kind::Punct("[") => {
                self.at += 1;
                let mut items = Vec::new();
                loop {
                    match self.peek().map(|t| t.is_punct("]")) {
                        Some(true) => {
                            self.at += 1;
                            break;
                        }
                        Some(false) => items.push(self.value(None)?),
                        None => return Err(self.error(None, "missing `]`: the list is not closed").into()),
                    }
                }
                Ok(Value::Array(items))
            }
            Kind::Str { text, interpolated, closed } => {
                self.at += 1;
                if *interpolated {
                    return Err(Failure::Unsupported);
                }
                if !closed {
                    return Err(self.error(Some(&token), "unterminated string").into());
                }
                Ok(Value::String(text.clone()))
            }
            Kind::Indented { text, interpolated, closed } => {
                self.at += 1;
                if *interpolated {
                    return Err(Failure::Unsupported);
                }
                if !closed {
                    return Err(self.error(Some(&token), "unterminated string").into());
                }
                Ok(Value::String(text.clone()))
            }
            Kind::Num(text) => {
                self.at += 1;
                Ok(self.number(text, &token)?)
            }
            Kind::Op(op) if op.as_str() == "-" && matches!(self.toks.get(self.at + 1).map(|t| &t.kind), Some(Kind::Num(_))) => {
                let number = self.toks[self.at + 1].clone();
                self.at += 2;
                let Kind::Num(text) = &number.kind else { unreachable!("checked above") };
                Ok(self.number(&format!("-{text}"), &number)?)
            }
            Kind::Ident(name) if name.as_str() == "true" => {
                self.at += 1;
                Ok(Value::Bool(true))
            }
            Kind::Ident(name) if name.as_str() == "false" => {
                self.at += 1;
                Ok(Value::Bool(false))
            }
            Kind::Ident(name) if name.as_str() == "null" => {
                self.at += 1;
                Ok(Value::Null)
            }
            Kind::Path(text) => {
                self.at += 1;
                Ok(Value::String(text.clone()))
            }
            Kind::Punct(";") | Kind::Punct("}") | Kind::Punct("]") | Kind::Punct(")") | Kind::Punct(",") | Kind::Punct("=") => {
                Err(self.error(Some(&token), "expected a value").into())
            }
            Kind::Ident(_) => Err(Failure::Unsupported),
            Kind::Punct("(") => match self.parenthesised_number() {
                Some((len, text)) => {
                    let token = self.toks[self.at + len - 2].clone();
                    self.at += len;
                    Ok(self.number(&text, &token)?)
                }
                None => Err(Failure::Unsupported),
            },
            _ => Err(Failure::Unsupported),
        }
    }

    /// `(N)` or `(-N)` at the cursor: the token count and the signed text.
    fn parenthesised_number(&self) -> Option<(usize, String)> {
        let negative = matches!(self.toks.get(self.at + 1).map(|t| &t.kind), Some(Kind::Op(op)) if op.as_str() == "-");
        let num_at = self.at + 1 + usize::from(negative);
        let Kind::Num(text) = &self.toks.get(num_at)?.kind else {
            return None;
        };
        if !self.toks.get(num_at + 1)?.is_punct(")") {
            return None;
        }
        let sign = if negative { "-" } else { "" };
        Some((num_at + 2 - self.at, format!("{sign}{text}")))
    }

    fn number(&self, text: &str, token: &Token) -> Result<Value, ParseError> {
        if text.contains('.') {
            let float: f64 = text.parse().map_err(|_| self.error(Some(token), "invalid number"))?;
            return Number::from_f64(float)
                .map(Value::Number)
                .ok_or_else(|| self.error(Some(token), "number out of range"));
        }
        text.parse::<i64>()
            .map(Value::from)
            .map_err(|_| self.error(Some(token), "integer out of range"))
    }
}

/// The parsed document: the value, and where each binding's key and value are.
fn parse_document(source: &str) -> Result<Document, ParseError> {
    let mut parser = Parser::new(source);
    let value = parser.document()?;
    Ok(Document {
        value,
        keys: parser.keys,
        values: parser.values,
        lost: parser.lost,
    })
}


// ---------------------------------------------------------------------------
// Options outside the demo's subset
// ---------------------------------------------------------------------------

/// The schema, parsed once.
fn schema_root() -> &'static Value {
    static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    SCHEMA.get_or_init(|| serde_json::from_str(NIXOS_SCHEMA).expect("the NixOS schema is JSON"))
}

/// Whether `names` is a path in [`NIXOS_SCHEMA`]: a property at each step, or
/// any key where the schema gives `additionalProperties` (`users.users.<name>`).
fn schema_knows(names: &[String]) -> bool {
    let mut node = schema_root();
    for name in names {
        node = match node.get("properties").and_then(|properties| properties.get(name)) {
            Some(child) => child,
            None => match node.get("additionalProperties") {
                Some(extra) if extra.is_object() => extra,
                _ => return false,
            },
        };
    }
    true
}

/// Whether a token can start a binding: the start of the text, or after a
/// brace, `;`, or `in`, `let` or `rec`.
fn starts_binding(previous: Option<&Token>) -> bool {
    match previous {
        None => true,
        Some(token) => {
            token.is_punct("{")
                || token.is_punct(";")
                || token.is_punct("}")
                || matches!(&token.kind, Kind::Ident(name) if matches!(name.as_str(), "in" | "let" | "rec"))
        }
    }
}

/// The attribute paths of `text` that start with a NixOS root of
/// [`NIXOS_SCHEMA`] (`networking`, `services`, ...) but are not in the schema,
/// as byte ranges with the message to show there. Paths with no known root are
/// not reported, and bindings inside `let` are left alone.
pub fn unchecked_options(text: &str) -> Vec<(std::ops::Range<usize>, String)> {
    const MESSAGE: &str = "not in this demo's NixOS subset, so it is not checked";
    let tokens = code_tokens(text);
    let mut found = Vec::new();
    for index in 0..tokens.len() {
        if !starts_binding(index.checked_sub(1).map(|prev| &tokens[prev])) {
            continue;
        }
        let mut names = Vec::new();
        let mut at = index;
        let last = loop {
            let Some(name) = tokens.get(at).and_then(name_of) else { break None };
            names.push(name);
            if tokens.get(at + 1).is_some_and(|token| token.is_punct(".")) {
                at += 2;
            } else {
                break Some(at);
            }
        };
        let Some(last) = last else { continue };
        if !tokens.get(last + 1).is_some_and(|token| token.is_punct("=")) {
            continue;
        }
        let scope = scan(&tokens[..index]);
        let top = scope.top();
        if top.scope != Scope::Set {
            continue;
        }
        let mut path = top.path.clone();
        path.extend(names);
        let known_root = schema_root()
            .get("properties")
            .and_then(|properties| properties.get(&path[0]))
            .is_some();
        if !known_root || schema_knows(&path) {
            continue;
        }
        found.push((tokens[index].start..tokens[last].end, MESSAGE.to_string()));
    }
    found
}

// ---------------------------------------------------------------------------
// Hover path
// ---------------------------------------------------------------------------

fn nth_line_start(source: &str, line_index: usize) -> Option<usize> {
    if line_index == 0 {
        Some(0)
    } else {
        source.match_indices('\n').nth(line_index - 1).map(|(at, _)| at + 1)
    }
}

/// The schema path, key and character column of the attribute segment under
/// `column` of line `line_index`, for a binding (`a.b.c = ...`) in a set.
fn hover_path_at(source: &str, line_index: usize, column: usize) -> Option<(Vec<SchemaPathSegment>, String, usize)> {
    let line_start = nth_line_start(source, line_index)?;
    let line_text = source[line_start..].split('\n').next()?;
    let at = line_start + line_text.char_indices().nth(column)?.0;
    let tokens = code_tokens(source);
    let hit = tokens
        .iter()
        .position(|t| t.start <= at && at < t.end && name_of(t).is_some())?;
    let mut first = hit;
    while first >= 2 && tokens[first - 1].is_punct(".") && name_of(&tokens[first - 2]).is_some() {
        first -= 2;
    }
    let mut last = hit;
    while last + 2 < tokens.len() && tokens[last + 1].is_punct(".") && name_of(&tokens[last + 2]).is_some() {
        last += 2;
    }
    if !tokens.get(last + 1)?.is_punct("=") {
        return None;
    }
    let names: Vec<String> = (first..=last)
        .step_by(2)
        .map(|index| name_of(&tokens[index]).expect("a name"))
        .collect();
    let index = (hit - first) / 2;
    let scope = scan(&tokens[..first]);
    let level = scope.levels.last()?;
    if level.scope != Scope::Set {
        return None;
    }
    let start_byte = tokens[first + 2 * index].start;
    if start_byte < line_start {
        return None;
    }
    let start_column = source[line_start..start_byte].chars().count();
    let mut path = level.path.clone();
    path.extend(names[..=index].iter().cloned());
    Some((path_segments(&path), names[index].clone(), start_column))
}

fn names_of(path: &FieldPath) -> Vec<String> {
    path.segments().iter().map(|segment| segment.as_key().into_owned()).collect()
}

/// The value an accepted option is written with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Template {
    /// `name = true;`
    Bool,
    /// `name = "";`, for a string or an enum.
    Text,
    /// `name = [ ];`
    List,
    /// `name = {` and a line break, the caret on an indented line.
    Set,
    /// `name = ;`, the caret after the `=`.
    Open,
}

/// The schema of option `key` under the container at `path`, from
/// [`NIXOS_SCHEMA`]: a named property, or `additionalProperties` where the
/// schema gives it (`users.users.<name>`).
fn option_schema(path: &[SchemaPathSegment], key: &str) -> Option<&'static Value> {
    let mut node = schema_root();
    for segment in path {
        node = match segment {
            SchemaPathSegment::Key(name) => child_schema(node, name)?,
            SchemaPathSegment::ArrayItem(_) => node.get("items")?,
        };
    }
    child_schema(node, key)
}

fn child_schema<'a>(node: &'a Value, name: &str) -> Option<&'a Value> {
    match node.get("properties").and_then(|properties| properties.get(name)) {
        Some(child) => Some(child),
        None => node.get("additionalProperties").filter(|extra| extra.is_object()),
    }
}

/// The template a property schema writes: `enum` and string types are text,
/// `boolean` is a boolean, `array` a list, `object` (or one with
/// `properties`/`additionalProperties`) a set; other types, a number among
/// them, are left to type.
fn template_of(schema: &Value) -> Template {
    if schema.get("enum").is_some() {
        return Template::Text;
    }
    let mut types: Vec<&str> = match schema.get("type") {
        Some(Value::String(kind)) => vec![kind.as_str()],
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    types.retain(|kind| *kind != "null");
    match types.first() {
        Some(&"boolean") => Template::Bool,
        Some(&"string") => Template::Text,
        Some(&"array") => Template::List,
        Some(&"object") => Template::Set,
        Some(_) => Template::Open,
        None if schema.get("items").is_some() => Template::List,
        None if schema.get("properties").is_some()
            || schema.get("additionalProperties").is_some_and(Value::is_object) =>
        {
            Template::Set
        }
        None => Template::Open,
    }
}

/// The template for a key the schema does not name here: the plan the editor
/// made from its schemas.
fn plan_template(plan: &InsertPlan<'_>) -> Template {
    if plan.list {
        return Template::List;
    }
    if plan.object {
        return Template::Set;
    }
    match plan.value {
        Planned::Value(Value::Bool(_)) => Template::Bool,
        Planned::Value(Value::String(_)) | Planned::Empty { string: true } => Template::Text,
        _ => Template::Open,
    }
}

/// A Nix string literal for `text`.
fn nix_string(text: &str) -> String {
    let escaped = text
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
        .replace("${", "\\${");
    format!("\"{escaped}\"")
}

fn format_nix(value: &Value) -> String {
    match value {
        Value::String(text) => nix_string(text),
        Value::Bool(flag) => flag.to_string(),
        Value::Null => "null".to_string(),
        Value::Number(number) => number.to_string(),
        Value::Array(items) if items.is_empty() => "[ ]".to_string(),
        Value::Array(items) => format!(
            "[ {} ]",
            items
                .iter()
                .map(|item| match item {
                    // `[ 1 -1 ]` is one subtraction in Nix: negatives are parenthesised.
                    Value::Number(number) if number.to_string().starts_with('-') => format!("({number})"),
                    _ => format_nix(item),
                })
                .collect::<Vec<_>>()
                .join(" ")
        ),
        Value::Object(map) if map.is_empty() => "{ }".to_string(),
        Value::Object(map) => format!(
            "{{ {} }}",
            map.iter()
                .map(|(key, value)| {
                    let key = if is_identifier(key) { key.clone() } else { nix_string(key) };
                    format!("{key} = {};", format_nix(value))
                })
                .collect::<Vec<_>>()
                .join(" ")
        ),
    }
}

/// A set written as a multi-line Nix attribute set, nested sets indented.
fn write_set(map: &Map<String, Value>, depth: usize) -> String {
    if map.is_empty() {
        return "{ }".to_string();
    }
    let pad = "  ".repeat(depth + 1);
    let mut out = String::from("{\n");
    for (key, value) in map {
        let key = if is_identifier(key) { key.clone() } else { nix_string(key) };
        let text = match value {
            Value::Object(inner) => write_set(inner, depth + 1),
            other => format_nix(other),
        };
        out.push_str(&format!("{pad}{key} = {text};\n"));
    }
    out.push_str(&"  ".repeat(depth));
    out.push('}');
    out
}

// ---------------------------------------------------------------------------
// Highlighting
// ---------------------------------------------------------------------------

fn is_string(token: &Token) -> bool {
    matches!(token.kind, Kind::Str { .. } | Kind::Indented { .. })
}

/// Colors the body of a string from `from` (offsets into `src`, shifted by
/// `base` in `out`) up to and including its closing delimiter. Returns the
/// offset past that delimiter, or `None` when `src` ends inside the string.
fn string_body(src: &str, from: usize, indented: bool, base: usize, out: &mut Tokens) -> Option<usize> {
    let mut run = from;
    let mut i = from;
    loop {
        let rest = &src[i..];
        if rest.is_empty() {
            out.push(base + run, base + i, TokenKind::String);
            return None;
        }
        if indented && rest.starts_with("''") {
            let after = &rest[2..];
            if after.starts_with(['\'', '$']) {
                i += 3;
            } else if let Some(next) = after.strip_prefix('\\') {
                i += 3 + next.chars().next().map_or(0, char::len_utf8);
            } else {
                out.push(base + run, base + i + 2, TokenKind::String);
                return Some(i + 2);
            }
        } else if !indented && rest.starts_with('"') {
            out.push(base + run, base + i + 1, TokenKind::String);
            return Some(i + 1);
        } else if rest.starts_with("${") {
            out.push(base + run, base + i, TokenKind::String);
            let end = skip_interpolation(src, i + 2);
            out.push(base + i, base + end, TokenKind::Interpolation);
            i = end;
            run = end;
        } else if !indented && rest.starts_with('\\') {
            i += 1 + rest[1..].chars().next().map_or(0, char::len_utf8);
        } else {
            i += rest.chars().next().map_or(1, char::len_utf8);
        }
    }
}

/// Colors the code and comments in `src` (offsets shifted by `base`). Words
/// are keys when an attribute path follows them, else values; strings are
/// colored by [`string_body`].
fn code_runs(src: &str, base: usize, out: &mut Tokens) {
    let tokens = lex(src);
    for (index, token) in tokens.iter().enumerate() {
        let (start, end) = (base + token.start, base + token.end);
        match &token.kind {
            Kind::Ident(name) => {
                let kind = if matches!(name.as_str(), "let" | "in" | "with" | "inherit") {
                    TokenKind::Reference
                } else if matches!(name.as_str(), "true" | "false" | "null") {
                    TokenKind::Literal
                } else if tokens.get(index + 1).is_some_and(|next| next.is_punct(".") || next.is_punct("=")) {
                    TokenKind::Key
                } else {
                    TokenKind::Value
                };
                out.push(start, end, kind);
            }
            Kind::Num(_) => out.push(start, end, TokenKind::Literal),
            Kind::Str { .. } => {
                out.push(start, base + token.start + 1, TokenKind::String);
                string_body(src, token.start + 1, false, base, out);
            }
            Kind::Indented { .. } => {
                out.push(start, base + token.start + 2, TokenKind::String);
                string_body(src, token.start + 2, true, base, out);
            }
            Kind::Path(_) => out.push(start, end, TokenKind::Value),
            Kind::Comment { .. } => out.push(start, end, TokenKind::Comment),
            Kind::Punct(punct) if matches!(*punct, "{" | "}" | "[" | "]" | ";" | "=") => {
                out.push(start, end, TokenKind::Punctuation);
            }
            Kind::Punct(_) | Kind::Op(_) | Kind::Other(_) => {}
        }
    }
}

impl ConfigLanguage for Nix {
    fn info(&self) -> LanguageInfo {
        LanguageInfo {
            label: "Nix",
            icon: "N",
            extensions: &["nix"],
        }
    }

    fn config(&self) -> LanguageConfig {
        LanguageConfig {
            line_comments: &["#"],
            block_comment: Some(("/*", "*/")),
            brackets: &[('{', '}'), ('[', ']'), ('(', ')')],
            separators: &["="],
            quotes: &["''", "\""],
            literal_quotes: &["''"],
            map_brackets: Some(('{', '}')),
            list_brackets: Some(('[', ']')),
            ..LanguageConfig::default()
        }
    }

    fn parse(&self, source: &str) -> Result<Value, ParseError> {
        parse_document(source).map(|document| document.value)
    }

    fn key_span(&self, source: &str, path: &FieldPath) -> Option<Span> {
        parse_document(source).ok()?.keys.get(&names_of(path)).copied()
    }

    fn value_span(&self, source: &str, path: &FieldPath) -> Option<Span> {
        parse_document(source).ok()?.values.get(&names_of(path)).copied()
    }

    fn completion_context(&self, source: &str, cursor: usize) -> CompletionContext {
        completion_at(source, cursor)
    }

    /// The option's documented default (or const) when the schema gives one,
    /// written in Nix form with the caret after it, so the crate selects it
    /// and typing replaces it: `name = false;`, `name = "prohibit-password";`,
    /// `name = [ ];`. Otherwise the option's value template, by its schema
    /// type: `name = true;` with the caret after `true`, `name = "";` inside
    /// the quotes (strings and enums), `name = [ ];` inside the brackets,
    /// `name = {` with the caret on an indented line inside the braces, and
    /// `name = ;` after the `=` for a number or an unknown type.
    fn write_insert(
        &self,
        key: &str,
        plan: &InsertPlan<'_>,
        context: &InsertContext<'_>,
    ) -> Option<(String, usize)> {
        let name = if is_identifier(key) { key.to_string() } else { nix_string(key) };
        let head = format!("{name} = ");
        let template = option_schema(context.path, key).map_or_else(|| plan_template(plan), template_of);
        if template == Template::Set && context.position == EntryPosition::Inline {
            return Some((format!("{name}."), name.encode_utf16().count() + 1));
        }
        if let Planned::Value(value) = plan.value
            && !value.is_object()
        {
            let written = format_nix(value);
            let text = format!("{head}{written};");
            let caret = head.len() + written.len();
            return Some((text.clone(), text[..caret].encode_utf16().count()));
        }
        let (text, caret) = match template {
            Template::Bool => (format!("{head}true;"), head.len() + "true".len()),
            Template::Text => (format!("{head}\"\";"), head.len() + 1),
            Template::List => (format!("{head}[ ];"), head.len() + "[ ".len()),
            Template::Set => {
                let inside = format!("{}{}", context.base_indent, context.unit);
                let text = format!("{head}{{\n{inside}\n{}}};", context.base_indent);
                (text, head.len() + "{\n".len() + inside.len())
            }
            Template::Open => (format!("{head};"), head.len()),
        };
        Some((text.clone(), text[..caret].encode_utf16().count()))
    }

    fn format_value(&self, value: &Value) -> String {
        format_nix(value)
    }

    /// Edit-as writes a module as one attribute set; the previous text is not kept.
    fn serialize(&self, value: &Value, _previous: Option<&str>) -> Result<String, String> {
        match value {
            Value::Object(map) => Ok(format!("{}\n", write_set(map, 0))),
            _ => Err("a Nix module is one attribute set".to_string()),
        }
    }

    fn hover(&self) -> &dyn Hover {
        self
    }

    fn text(&self) -> &dyn Text {
        self
    }

    fn markup(&self) -> Option<&dyn Markup> {
        Some(self)
    }

    fn highlight(&self, line: &str) -> Vec<Highlight> {
        let mut out = Tokens::new();
        code_runs(line, 0, &mut out);
        out.finish()
    }
}

impl Hover for Nix {
    fn hover_path(&self, source: &str, line_index: usize, column: usize) -> Option<(Vec<SchemaPathSegment>, String, usize)> {
        hover_path_at(source, line_index, column)
    }
}

impl Text for Nix {
    /// `#` and `/* */` comments, the block ones over however many lines.
    fn comments(&self, _language: &dyn ConfigLanguage, source: &str) -> Vec<Range<usize>> {
        lex(source)
            .into_iter()
            .filter(Token::is_comment)
            .map(|token| token.start..token.end)
            .collect()
    }

    fn multiline_strings(&self, source: &str) -> Vec<Range<usize>> {
        lex(source)
            .into_iter()
            .filter(|token| is_string(token) && source[token.start..token.end].contains('\n'))
            .map(|token| token.start..token.end)
            .collect()
    }

    /// A `''...''` string over lines is structure: its lines are colored by
    /// [`highlight_continued`](Text::highlight_continued), so `${...}` shows.
    /// A `"..."` string over lines is one colored run.
    fn multiline_constructs(&self, _language: &dyn ConfigLanguage, source: &str) -> Vec<MultilineConstruct> {
        lex(source)
            .into_iter()
            .filter(|token| is_string(token) && source[token.start..token.end].contains('\n'))
            .map(|token| match token.kind {
                Kind::Indented { .. } => MultilineConstruct::structure(token.start..token.end),
                _ => MultilineConstruct::colored(token.start..token.end, TokenKind::String),
            })
            .collect()
    }

    /// The part of a line inside a `''...''` string that opened on an earlier
    /// line: string text and `${...}` up to its closing `''`, then code.
    fn highlight_continued(&self, _language: &dyn ConfigLanguage, text: &str) -> Vec<Highlight> {
        let mut out = Tokens::new();
        if let Some(end) = string_body(text, 0, true, 0, &mut out) {
            code_runs(&text[end..], end, &mut out);
        }
        out.finish()
    }
}

impl Markup for Nix {
    /// Only read for `delimiters` to hold an answer; Nix has no markup.
    fn delimiters(&self) -> (char, char) {
        ('{', '}')
    }

    /// The first skipped construct, as the conversion error: converting would
    /// drop it, since the data model holds literals and attribute sets only.
    fn unconvertible(&self, source: &str) -> Option<String> {
        let document = parse_document(source).ok()?;
        let first = document.lost.first()?;
        let more = match document.lost.len() - 1 {
            0 => String::new(),
            count => format!(", and {count} more"),
        };
        Some(format!(
            "Nix {} on line {} would be lost, since only literal values and attribute sets carry over{more} (at {})",
            first.what, first.line, first.pointer
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leptos_config_editor::{ConvertError, Schema, TextPosition};
    use serde_json::json;

    fn schema() -> Schema {
        Schema::compile(NIXOS_SCHEMA).expect("the NixOS schema compiles")
    }

    /// The UTF-16 offset of a 0-based line and column (ASCII text only).
    fn offset(text: &str, line: usize, column: usize) -> usize {
        let before: usize = text.split('\n').take(line).map(|l| l.len() + 1).sum();
        before + column
    }

    /// The 1-based column of the first `needle` on 1-based `line` of `text`.
    fn column_of(text: &str, line: usize, needle: &str) -> usize {
        let text_line = text.split('\n').nth(line - 1).expect("the line");
        text_line.find(needle).expect("the needle") + 1
    }

    fn labels(schema: &Schema, text: &str, cursor: usize) -> Vec<String> {
        schema
            .suggestions(NIX, text, cursor)
            .iter()
            .map(|s| s.label().to_string())
            .collect()
    }

    #[test]
    fn nix_hover_on_an_option_key_shows_its_description() {
        let text = "{\n  networking.firewall.enable = true;\n}\n";
        let column = column_of(text, 2, "enable");
        let hover = schema()
            .hover(NIX, text, TextPosition::new(2, column + 1))
            .expect("hover on the key");
        assert_eq!(
            hover.description.as_deref(),
            Some(
                "Whether to enable the firewall. This is a simple stateful firewall that blocks connection attempts to unauthorised TCP or UDP ports on this machine."
            )
        );
    }

    #[test]
    /// The value's own docs, from its `x-enum-descriptions` entry, are on the
    /// public `HoverDocs.value`.
    fn nix_hover_on_an_enum_value_shows_the_entry_description() {
        let text = "{\n  services.openssh.settings.PermitRootLogin = \"no\";\n}\n";
        let column = column_of(text, 2, "\"no\"");
        let hover = schema()
            .hover(NIX, text, TextPosition::new(2, column + 1))
            .expect("hover on the value");
        assert_eq!(hover.key, "PermitRootLogin");
        let value = hover.value.expect("the value's docs");
        assert_eq!(value.value, "no");
        assert_eq!(value.description.as_deref(), Some("Root may not log in."));
    }

    #[test]
    fn nix_completion_after_a_dotted_prefix_lists_its_children() {
        let schema = schema();
        let text = "networking.";
        let names = labels(&schema, text, text.encode_utf16().count());
        for child in ["hostName", "firewall", "useDHCP", "nameservers"] {
            assert!(names.iter().any(|n| n == child), "{child} in {names:?}");
        }
        let text = "services.openssh.";
        let names = labels(&schema, text, text.encode_utf16().count());
        for child in ["enable", "settings"] {
            assert!(names.iter().any(|n| n == child), "{child} in {names:?}");
        }
    }

    #[test]
    fn nix_completion_after_a_dot_opens_the_list_without_a_typed_word() {
        for text in ["networking.", "services.openssh."] {
            let context = Nix.completion_context(text, text.encode_utf16().count());
            assert!(context.explicit_entry_start, "{text}: {context:?}");
            assert!(context.typed.is_empty(), "{text}: {context:?}");
        }
    }

    #[test]
    fn nix_completion_at_a_new_binding_lists_the_set_options() {
        let text = "services.openssh = {\n  \n}\n";
        let found = schema().suggestions(NIX, text, offset(text, 1, 2));
        let names: Vec<&str> = found.iter().map(|s| s.label()).collect();
        assert!(names.contains(&"enable"), "{names:?}");
        assert!(names.contains(&"settings"), "{names:?}");
        let enable = found.iter().find(|s| s.label() == "enable").expect("enable");
        assert!(enable.detail().contains("OpenSSH"), "{:?}", enable.detail());
    }

    #[test]
    fn nix_completion_after_equals_on_an_enum_key_lists_the_values() {
        let text = "{\n  services.openssh.settings.PermitRootLogin = \n}\n";
        let cursor = offset(text, 1, "  services.openssh.settings.PermitRootLogin = ".len());
        let names = labels(&schema(), text, cursor);
        for value in ["yes", "without-password", "prohibit-password", "forced-commands-only", "no"] {
            assert!(names.iter().any(|n| n.trim_matches('"') == value), "{value} in {names:?}");
        }
    }

    #[test]
    fn nix_accepting_a_suggestion_writes_a_binding() {
        let schema = schema();
        let text = "services.openssh = {\n  en\n}\n";
        let found = schema.suggestions(NIX, text, offset(text, 1, 4));
        let enable = found.iter().find(|s| s.label() == "enable").expect("enable");
        assert_eq!(enable.accept(text).0, "services.openssh = {\n  enable = false;\n}\n");

        let text = "{\n  networking.fi\n}\n";
        let found = schema.suggestions(NIX, text, offset(text, 1, 15));
        let firewall = found.iter().find(|s| s.label() == "firewall").expect("firewall");
        assert_eq!(firewall.accept(text).0, "{\n  networking.firewall.\n}\n");

        let text = "{\n  services.openssh.settings.PermitRootLogin = \n}\n";
        let found = schema.suggestions(NIX, text, offset(text, 1, 46));
        let no = found.iter().find(|s| s.label().trim_matches('\"') == "no").expect("no");
        assert_eq!(no.accept(text).0, "{\n  services.openssh.settings.PermitRootLogin = \"no\"\n}\n");
    }

    /// The document and the caret's UTF-16 offset after accepting `label`
    /// among the suggestions at the `|` in `marked`.
    fn accepted(schema: &Schema, marked: &str, label: &str) -> (String, usize) {
        let at = marked.find('|').expect("a | caret marker");
        let text = marked.replacen('|', "", 1);
        let found = schema.suggestions(NIX, &text, text[..at].encode_utf16().count());
        let option = found.iter().find(|s| s.label() == label).expect("the option is offered");
        let (inserted, selection) = option.accept(&text);
        (inserted, selection.end)
    }

    #[test]
    fn nix_accepting_a_string_option_writes_its_default_selected() {
        let schema = schema();
        let marked = "networking = {\n  ho|\n}\n";
        let (text, caret) = accepted(&schema, marked, "hostName");
        assert_eq!(text, "networking = {\n  hostName = \"nixos\";\n}\n");
        assert_eq!(caret, offset(&text, 1, "  hostName = \"nixos".len()));
        assert_eq!(selected_after(&schema, marked, "hostName"), "nixos");
    }

    #[test]
    fn nix_accepting_an_array_option_writes_empty_brackets_with_the_caret_inside() {
        let schema = schema();
        let (text, caret) = accepted(&schema, "networking.firewall = {\n  allow|\n}\n", "allowedTCPPorts");
        assert_eq!(text, "networking.firewall = {\n  allowedTCPPorts = [ ];\n}\n");
        assert_eq!(caret, offset(&text, 1, "  allowedTCPPorts = [ ".len()));
    }

    #[test]
    fn nix_accepting_an_object_option_opens_an_indented_block_with_the_caret_inside() {
        let schema = schema();
        let (text, caret) = accepted(&schema, "networking = {\n  fi|\n}\n", "firewall");
        assert_eq!(text, "networking = {\n  firewall = {\n    \n  };\n}\n");
        assert_eq!(caret, offset(&text, 2, "    ".len()));
    }

    #[test]
    fn nix_accepting_an_object_after_a_dotted_prefix_writes_the_dot() {
        let schema = schema();
        let (text, caret) = accepted(&schema, "{\n  networking.fi|\n}\n", "firewall");
        assert_eq!(text, "{\n  networking.firewall.\n}\n");
        assert_eq!(caret, offset(&text, 1, "  networking.firewall.".len()));
    }

    #[test]
    fn nix_accepting_a_string_after_a_dot_writes_the_template() {
        let schema = schema();
        let (text, caret) = accepted(&schema, "{\n  networking.|\n}\n", "hostName");
        assert_eq!(text, "{\n  networking.hostName = \"nixos\";\n}\n");
        assert_eq!(caret, offset(&text, 1, "  networking.hostName = \"nixos".len()));
    }

    #[test]
    fn nix_accepting_an_enum_value_inside_empty_quotes_replaces_the_quoted_text() {
        let schema = schema();
        let text = "{\n  services.openssh.settings.PermitRootLogin = \"\";\n}\n";
        let caret = text.find("\"\";").expect("the quotes") + 1;
        let found = schema.suggestions(NIX, text, text[..caret].encode_utf16().count());
        let no = found
            .iter()
            .find(|s| s.label().trim_matches('"') == "no")
            .expect("the value `no` is offered inside the quotes");
        let (inserted, _) = no.accept(text);
        assert_eq!(inserted, "{\n  services.openssh.settings.PermitRootLogin = \"no\";\n}\n");
    }


    #[test]
    fn nix_accepting_a_string_default_inside_empty_quotes_writes_one_pair() {
        let schema = schema();
        let text = "{\n  networking.hostName = \"\";\n}\n";
        let caret = text.find("\"\";").expect("the quotes") + 1;
        let found = schema.suggestions(NIX, text, text[..caret].encode_utf16().count());
        let nixos = found.iter().find(|s| s.label() == "nixos").expect("the default is offered");
        assert_eq!(nixos.accept(text).0, "{\n  networking.hostName = \"nixos\";\n}\n");
    }

    /// The text an accepted `label` leaves selected (ASCII documents only).
    fn selected_after(schema: &Schema, marked: &str, label: &str) -> String {
        let at = marked.find('|').expect("a | caret marker");
        let text = marked.replacen('|', "", 1);
        let found = schema.suggestions(NIX, &text, text[..at].encode_utf16().count());
        let option = found.iter().find(|s| s.label() == label).expect("the option is offered");
        let (inserted, selection) = option.accept(&text);
        inserted[selection.start..selection.end].to_string()
    }

    #[test]
    fn nix_accepting_a_boolean_option_writes_its_default_selected() {
        let schema = schema();
        let marked = "services.openssh = {\n  en|\n}\n";
        let (text, caret) = accepted(&schema, marked, "enable");
        assert_eq!(text, "services.openssh = {\n  enable = false;\n}\n");
        assert_eq!(caret, offset(&text, 1, "  enable = false".len()));
        assert_eq!(selected_after(&schema, marked, "enable"), "false");
    }

    #[test]
    fn nix_accepting_an_enum_option_writes_its_default_as_a_string_selected() {
        let schema = schema();
        let marked = "services.openssh.settings = {\n  Per|\n}\n";
        let (text, caret) = accepted(&schema, marked, "PermitRootLogin");
        assert_eq!(text, "services.openssh.settings = {\n  PermitRootLogin = \"prohibit-password\";\n}\n");
        assert_eq!(caret, offset(&text, 1, "  PermitRootLogin = \"prohibit-password".len()));
        assert_eq!(selected_after(&schema, marked, "PermitRootLogin"), "prohibit-password");
    }

    #[test]
    fn nix_accepting_a_list_option_writes_its_empty_default_with_the_caret_inside() {
        let schema = schema();
        let (text, caret) = accepted(&schema, "networking.firewall = {\n  allow|\n}\n", "allowedTCPPorts");
        assert_eq!(text, "networking.firewall = {\n  allowedTCPPorts = [ ];\n}\n");
        assert_eq!(caret, offset(&text, 1, "  allowedTCPPorts = [ ".len()));
    }

    #[test]
    fn nix_accepting_an_option_in_an_indented_set_keeps_the_indentation_and_the_closing_line() {
        let schema = schema();
        let marked = "{\n  services.openssh = {\n    en|\n  };\n}\n";
        let (text, caret) = accepted(&schema, marked, "enable");
        assert_eq!(text, "{\n  services.openssh = {\n    enable = false;\n  };\n}\n");
        assert_eq!(caret, offset(&text, 2, "    enable = false".len()));
        assert_eq!(selected_after(&schema, marked, "enable"), "false");
    }

    #[test]
    fn nix_completion_of_top_level_keys_at_the_start_of_the_file() {
        for text in ["{\n  \n}\n", ""] {
            let line = if text.is_empty() { 0 } else { 1 };
            let column = if text.is_empty() { 0 } else { 2 };
            let names = labels(&schema(), text, offset(text, line, column));
            for top in ["networking", "services", "users", "environment", "time", "i18n", "boot", "system"] {
                assert!(names.iter().any(|n| n == top), "{top} for {text:?} in {names:?}");
            }
        }
    }

    #[test]
    fn nix_parses_a_realistic_module_into_json() {
        let text = r#"{ config, pkgs, ... }:

# A small web server.
{
  imports = [ ./hardware-configuration.nix ];
  /* The host. */
  networking.hostName = "web-1";
  networking.firewall.allowedTCPPorts = [ 22 80 443 ];
  networking.firewall.enable = true;
  services.openssh.enable = true;
  services.openssh.settings.PermitRootLogin = "no";
  users.users.alice = {
    isNormalUser = true;
    extraGroups = [ "wheel" ];
  };
  environment.systemPackages = [ pkgs.git ];
  time.timeZone = "Europe/Paris";
  system.stateVersion = ''
    25.11
  '';
}
"#;
        let value = Nix.parse(text).expect("the module parses");
        assert_eq!(
            value,
            json!({
                "imports": ["./hardware-configuration.nix"],
                "networking": {
                    "hostName": "web-1",
                    "firewall": {"allowedTCPPorts": [22, 80, 443], "enable": true}
                },
                "services": {
                    "openssh": {"enable": true, "settings": {"PermitRootLogin": "no"}}
                },
                "users": {"users": {"alice": {"isNormalUser": true, "extraGroups": ["wheel"]}}},
                "time": {"timeZone": "Europe/Paris"},
                "system": {"stateVersion": "25.11"}
            })
        );
        assert!(value.get("environment").is_none(), "an expression binding is left out");
    }

    #[test]
    fn nix_skips_let_and_with_scopes() {
        let text = "let greeting = \"hi\"; in with pkgs; {\n  networking.hostName = greeting;\n  networking.useDHCP = false;\n}\n";
        let value = Nix.parse(text).expect("parses");
        assert_eq!(value, json!({"networking": {"useDHCP": false}}));
    }

    #[test]
    fn nix_reads_a_module_with_header_inherit_and_strings_without_error() {
        let text = r#"{ config, lib, pkgs, ... }@args:

let
  host = "web-1";
in
{
  inherit (lib) mkIf;
  imports = [ ./hardware.nix ];
  networking.hostName = host;
  networking.nameservers = [ "1.1.1.1" "9.9.9.9" ];
  services.nginx.enable = true;
  services.nginx.virtualHosts."example.org" = {
    serverName = "example.org"; # the public name
  };
  services.openssh.settings = rec { PermitRootLogin = "no"; };
  environment.etc."motd".text = ''
    Welcome to ${host}
      ''' and ''$ escapes
  '';
  boot.loader.grub.enable = lib.mkDefault false;
  time.timeZone = null;
}
"#;
        let value = Nix.parse(text).expect("the module parses");
        assert!(value["networking"].get("hostName").is_none(), "a reference to a `let` name is skipped");
        assert_eq!(value["networking"]["nameservers"], json!(["1.1.1.1", "9.9.9.9"]));
        assert_eq!(value["services"]["nginx"]["virtualHosts"]["example.org"]["serverName"], json!("example.org"));
        assert_eq!(value["services"]["openssh"]["settings"]["PermitRootLogin"], json!("no"));
        assert_eq!(value["time"]["timeZone"], Value::Null);
        assert!(value.get("environment").is_none(), "an interpolated string is skipped");
        assert!(value["boot"]["loader"]["grub"].get("enable").is_none(), "an expression binding is left out");
    }

    #[test]
    fn nix_partial_input_never_panics() {
        let schema = schema();
        let full = "{ config, pkgs, ... }:\n{\n  networking.firewall.enable = true;\n  services.openssh.settings.PermitRootLogin = \"no\";\n  environment.systemPackages = with pkgs; [ git ];\n  system.stateVersion = ''\n    25.11\n  '';\n  # done\n}\n";
        for end in 0..=full.len() {
            if !full.is_char_boundary(end) {
                continue;
            }
            let prefix = &full[..end];
            let _ = Nix.parse(prefix);
            let _ = Nix.key_span(prefix, &FieldPath::from_pointer("/networking/firewall/enable"));
            let cursor = prefix.encode_utf16().count();
            let _ = schema.suggestions(NIX, prefix, cursor);
            let _ = schema.hover(NIX, prefix, TextPosition::new(1, 1));
            let _ = schema.hover(NIX, prefix, TextPosition::new(3, 4));
        }
        for text in ["", "{", "{ a = \"", "{ a = ''", "{ a = [", "let", "with", "{ a = { b", "{ a = 1 +", "{ x = \"${", "/*", "{ a.\""] {
            let _ = Nix.parse(text);
            let _ = schema.suggestions(NIX, text, text.encode_utf16().count());
            let _ = schema.hover(NIX, text, TextPosition::new(1, 2));
        }
    }

    #[test]
    fn nix_skips_with_and_let_values_inside_a_set() {
        let text = "{\n  environment.systemPackages = with pkgs; [ git ];\n  networking.hostName = let n = \"x\"; in n;\n  time.timeZone = \"UTC\";\n}\n";
        let value = Nix.parse(text).expect("a `with` or `let` value parses");
        assert_eq!(value, json!({"time": {"timeZone": "UTC"}}));
    }

    #[test]
    fn nix_serialize_parenthesises_negative_list_items() {
        let value = json!({"networking": {"firewall": {"allowedTCPPorts": [1, -1]}}});
        let written = Nix.serialize(&value, None).expect("serializes");
        assert!(written.contains("(-1)"), "{written}");
        assert_eq!(Nix.parse(&written).expect("parses"), value);
    }

    #[test]
    fn nix_quotes_keyword_keys_when_writing() {
        let value = json!({"users": {"users": {"in": {"isNormalUser": true}}}});
        let written = Nix.serialize(&value, None).expect("serializes");
        assert!(written.contains("\"in\" ="), "{written}");
        assert_eq!(Nix.parse(&written).expect("parses"), value);
    }

    #[test]
    fn nix_skipped_binding_leaves_no_spans_behind() {
        let text = "{\n  services.nginx = { enable = true; } // { };\n  services.openssh.enable = true;\n}\n";
        assert!(Nix.key_span(text, &FieldPath::from_pointer("/services/nginx/enable")).is_none());
        assert!(Nix.key_span(text, &FieldPath::from_pointer("/services/openssh/enable")).is_some());
    }

    #[test]
    fn nix_reports_a_syntax_error_with_its_position() {
        let error = Nix.parse("{\n  networking.hostName = \"x\"\n  services.openssh.enable = true;\n}\n");
        assert!(error.is_err());
        let error = Nix.parse("{ a = 1;").expect_err("unclosed set");
        assert_eq!(error.line, Some(1));
    }

    #[test]
    fn nix_reports_an_empty_value_on_its_line() {
        let error = Nix.parse("{\n  services.openssh.enable = ;\n}\n").expect_err("an empty value");
        assert_eq!(error.line, Some(2), "{error:?}");
        assert!(error.message.contains("expected a value"), "{}", error.message);
    }

    #[test]
    fn nix_reports_an_unclosed_set_at_the_brace_that_opened_it() {
        let text = "{\n  services.nginx = {\n  services.openssh.enable = true;\n}\n";
        let error = Nix.parse(text).expect_err("an unclosed set");
        assert_eq!(error.line, Some(1), "{error:?}");
        assert!(error.message.contains("`}`") && error.message.contains("line 1"), "{}", error.message);
    }

    #[test]
    fn nix_reports_a_missing_semicolon_with_its_line() {
        let text = "{\n  services.openssh.enable = true\n  services.nginx.enable = true;\n}\n";
        let error = Nix.parse(text).expect_err("a missing semicolon");
        assert_eq!(error.line, Some(2), "{error:?}");
        assert!(error.message.contains("missing `;`") && error.message.contains("line 2"), "{}", error.message);
    }

    #[test]
    fn nix_serialize_reads_back_to_the_same_value() {
        let text = "{\n  networking.hostName = \"web-1\";\n  networking.firewall.allowedTCPPorts = [ 22 443 ];\n  users.users.\"a.b\" = { isNormalUser = true; };\n  time.timeZone = null;\n}\n";
        let value = Nix.parse(text).expect("parses");
        let written = Nix.serialize(&value, Some(text)).expect("serializes");
        assert_eq!(Nix.parse(&written).expect("the written text parses"), value);
    }

    #[test]
    fn nix_value_span_locates_the_value_text() {
        let text = "{\n  networking.hostName = \"web-1\";\n}\n";
        let path = FieldPath::from_pointer("/networking/hostName");
        let span = Nix.value_span(text, &path).expect("a span");
        assert_eq!((span.line, span.start_column, span.end_column), (2, 25, 32));
    }

    #[test]
    fn nix_hover_on_time_zone_shows_its_verified_description() {
        let text = "{\n  time.timeZone = \"Europe/Paris\";\n}\n";
        let column = column_of(text, 2, "timeZone");
        let hover = schema()
            .hover(NIX, text, TextPosition::new(2, column + 1))
            .expect("hover on timeZone");
        assert_eq!(hover.key, "timeZone");
        assert!(
            hover.description.as_deref().is_some_and(|d| d.starts_with("The time zone used when displaying times and dates.")),
            "{:?}",
            hover.description
        );
    }

    #[test]
    fn nix_known_options_are_not_reported_as_unchecked() {
        let text = "{\n  networking.firewall.enable = true;\n  users.users.alice = { isNormalUser = true; };\n  services.openssh.settings.PermitRootLogin = \"no\";\n}\n";
        assert!(unchecked_options(text).is_empty(), "{:?}", unchecked_options(text));
    }

    #[test]
    fn nix_unknown_option_under_a_known_root_is_reported_with_its_range() {
        let text = "{\n  services.openssh.bogus = true;\n}\n";
        let found = unchecked_options(text);
        let start = text.find("services.openssh.bogus").expect("the path");
        let range = start..start + "services.openssh.bogus".len();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].0, range);
        assert_eq!(found[0].1, "not in this demo's NixOS subset, so it is not checked");
    }

    #[test]
    fn nix_unknown_option_inside_a_known_set_is_reported_at_its_key() {
        let text = "services.openssh = {\n  bogus = true;\n};\n";
        let found = unchecked_options(text);
        let start = text.find("bogus").expect("the key");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].0, start..start + "bogus".len());
    }

    #[test]
    fn nix_paths_without_a_known_root_are_not_reported() {
        let text = "{\n  foo.bar = 1;\n  pkgs.git = 2;\n  environment.nosuch = 3;\n}\n";
        let found = unchecked_options(text);
        assert_eq!(found.len(), 1, "only the known root is checked: {found:?}");
        assert!(text[found[0].0.clone()].starts_with("environment.nosuch"));
        assert!(unchecked_options("{\n  foo.bar = 1;\n}\n").is_empty());
    }

    #[test]
    fn nix_schema_gives_one_description_per_enum_value() {
        let schema: Value = serde_json::from_str(NIXOS_SCHEMA).expect("the schema is JSON");
        let root = &schema["properties"]["services"]["properties"]["openssh"]["properties"]["settings"]["properties"];
        let root_login = &root["PermitRootLogin"];
        let values = root_login["enum"].as_array().expect("an enum");
        let docs = root_login["x-enum-descriptions"].as_array().expect("enum descriptions");
        assert_eq!(values.len(), docs.len());
        assert_eq!(docs.last().and_then(Value::as_str), Some("Root may not log in."));
    }

    const SAMPLE: &str = "{\n  networking.hostName = \"web-01\";\n  networking.firewall.allowedTCPPorts = [ 22 80 443 ];\n  services.openssh.enable = true;\n  services.openssh.settings.PermitRootLogin = \"no\";\n  time.timeZone = \"Europe/Paris\";\n  system.stateVersion = \"25.11\";\n}\n";

    /// The text and kind of each highlighted run of `line`.
    fn runs(line: &str) -> Vec<(&str, TokenKind)> {
        NIX_LANGUAGE.highlight(line).into_iter().map(|token| (&line[token.start..token.end], token.kind)).collect()
    }

    #[test]
    fn nix_highlights_a_key_a_string_a_comment_and_an_interpolation() {
        assert_eq!(
            runs("  name = \"web-${host}\"; # note"),
            [
                ("name", TokenKind::Key),
                ("=", TokenKind::Punctuation),
                ("\"", TokenKind::String),
                ("web-", TokenKind::String),
                ("${host}", TokenKind::Interpolation),
                ("\"", TokenKind::String),
                (";", TokenKind::Punctuation),
                ("# note", TokenKind::Comment),
            ]
        );
    }

    #[test]
    fn nix_highlights_literals_and_scope_keywords() {
        assert_eq!(
            runs("enable = true; port = 443; x = let in null;"),
            [
                ("enable", TokenKind::Key),
                ("=", TokenKind::Punctuation),
                ("true", TokenKind::Literal),
                (";", TokenKind::Punctuation),
                ("port", TokenKind::Key),
                ("=", TokenKind::Punctuation),
                ("443", TokenKind::Literal),
                (";", TokenKind::Punctuation),
                ("x", TokenKind::Key),
                ("=", TokenKind::Punctuation),
                ("let", TokenKind::Reference),
                ("in", TokenKind::Reference),
                ("null", TokenKind::Literal),
                (";", TokenKind::Punctuation),
            ]
        );
    }

    #[test]
    fn nix_highlights_an_indented_string_continued_on_the_next_line() {
        // The opening line: `''` then text, left open.
        assert_eq!(runs("  text = ''hi ${x}"), [
            ("text", TokenKind::Key),
            ("=", TokenKind::Punctuation),
            ("''", TokenKind::String),
            ("hi ", TokenKind::String),
            ("${x}", TokenKind::Interpolation),
        ]);
        // A line inside it: string text, then the closing `''` and code after.
        let continued = NIX_LANGUAGE.highlight_continued(&NIX_LANGUAGE, "  a ${y} b''; z = 1;");
        let kinds: Vec<(&str, TokenKind)> = continued
            .into_iter()
            .map(|token| (&"  a ${y} b''; z = 1;"[token.start..token.end], token.kind))
            .collect();
        assert_eq!(kinds[0], ("  a ", TokenKind::String));
        assert_eq!(kinds[1], ("${y}", TokenKind::Interpolation));
        assert_eq!(kinds[2], (" b''", TokenKind::String));
        assert_eq!(kinds[3], (";", TokenKind::Punctuation));
        assert_eq!(kinds[4], ("z", TokenKind::Key));
    }

    #[test]
    fn nix_converts_the_sample_to_yaml_and_back_to_the_same_module() {
        let yaml = NIX.convert(SAMPLE, Language::YAML).expect("the sample converts to YAML");
        assert!(yaml.text.contains("hostName: web-01"), "{}", yaml.text);
        assert!(yaml.losses.is_empty(), "{:?}", yaml.losses);
        let back = Language::YAML.convert(&yaml.text, NIX).expect("the YAML converts back to Nix");
        assert_eq!(NIX_LANGUAGE.parse(&back.text), NIX_LANGUAGE.parse(SAMPLE));
    }

    #[test]
    fn nix_conversion_names_what_it_would_lose() {
        let with_header = "{ config, pkgs, ... }: {\n  networking.hostName = \"x\";\n  environment.systemPackages = with pkgs; [ git ];\n}\n";
        match NIX.convert(with_header, Language::YAML) {
            Err(ConvertError::Unsupported(message)) => {
                assert!(message.contains("function header on line 1"), "{message}");
                assert!(message.contains("(at /)"), "{message}");
                assert!(message.contains("and 1 more"), "{message}");
            }
            Err(other) => panic!("expected a loss message, got {other:?}"),
            Ok(_) => panic!("converting a module with a header must fail"),
        }
        let expression = "{\n  networking.hostName = \"x\";\n  environment.systemPackages = with pkgs; [ git ];\n}\n";
        match NIX.convert(expression, Language::YAML) {
            Err(ConvertError::Unsupported(message)) => {
                assert!(message.contains("attribute on line 3"), "{message}");
                assert!(message.contains("(at /environment/systemPackages)"), "{message}");
            }
            Err(other) => panic!("expected a loss message, got {other:?}"),
            Ok(_) => panic!("converting an expression must fail"),
        }
    }
}
