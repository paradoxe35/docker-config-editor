# Assessment of the `leptos-config-editor` 0.2.4 API

This file answers two questions from the demo work: is the crate's API bad, or
was the problem in how this project used it? It lists what works, what is a
real limitation of the API, what was my mistake, and what would make the API
easier to use.

## Short answer

Both. The crate's core design is sound: a language is a trait with a parser
and optional refinements, and a JSON Schema drives hover docs and completion.
That made five languages possible in one demo. But a few API decisions cause
real trouble, and I made several mistakes on top of them.

## Status against leptos-config-editor 0.2.5

The demo now uses 0.2.5 (`Cargo.toml`, `=0.2.5`). Checked against its source
and in the browser:

- Point 1 (value docs not public): fixed. `HoverDocs` has a public `value`
  field.
- Point 8 (broken line above the caret): fixed. `line_to_blank` now blanks it.
  RON offers only unset fields in the browser.
- The 0.2.5 README (section "Completion and hover") documents three rules that
  our modules now follow: `ParseError` must carry its `line` (every module
  does); a value's docs come from `value_span` (added to the `.env` module,
  which lacked it); a keyword that needs its own docs is a property name or a
  documented value of its key (the Dockerfile model is keyed this way).
- Points 3, 5, 9, 10, 11: not changed in 0.2.5 (`format_value` has the same
  signature; no warning hook; no per-language trigger).
- `.env`, typing without Ctrl+Space: the suggestion list does not open while
  typing. Ctrl+Space works. Not changed in 0.2.5; cause not yet found.

## What the browser showed

Checked with real keystrokes and screenshots, after the fixes:

- Hover cards show each key's own docs for all five languages, and the
  enum value cards show their entry text.
- Ctrl+Space at the start of a document, and typing a prefix (`RU` for
  Dockerfile, `rev` for Caddyfile, `networking.` for NixOS), open the
  right suggestions. Records in RON and Nickel offer their unset fields,
  and a record whose fields are all set offers nothing, as the crate
  documents.
- Errors name the problem and its line, once the parsers report them
  specifically. Before the fixes several were generic or wrong.

Applied since then: NixOS options the demo's schema does not list are shown as
Info notes through the crate's `host_diagnostics` prop (`HostDiagnostic` with
`Severity::Info`), so a rule that is not a syntax error can be added without
a language hook. Caddy repeated directives are kept, not warned about: Caddy
allows them.

Still open, and a crate change is needed: a broken RON or Nickel line directly
above the caret still lets completion offer fields that are already written
(see `around.rs`, `line_to_blank`). The workaround is to keep text below the
caret; this helps for other fields, but the broken line's own field is still
offered.

## What works well

- **Schema keywords carry the docs.** `description`, `title`, `x-enum-descriptions`,
  `const` with `title`/`description`, `deprecated`, `deprecationMessage` and
  `x-docs-url` (http/https only) give per-key docs with no extra code. The
  Dockerfile, NixOS, Caddyfile, RON and Nickel schemas all use them.
- **A test API exists.** `Schema::compile`, `Schema::suggestions` and
  `Schema::hover` let each language be tested without a browser. Every
  module's tests use them, which is why hover and completion were checked
  before the browser pass.
- **Tokens and spans.** `key_span`, `value_span` and `completion_context`
  let a language work on text that does not parse yet, which matters while
  typing.
- **Optional refinement traits.** A language that only needs parsing,
  serialization and a completion context does not have to implement the
  rest.

## Real limitations of the API

1. **Enum-value docs are not in the public test API.** The editor's hover
   card does show an enum value's description (checked in the browser for
   Caddyfile `gzip`, NixOS `"no"`, RON `Info` and Nickel `'info`). But the
   value is held in `KeyHover.value`, which is `pub(crate)`, and
   `Schema::hover` returns `HoverDocs` without it. So a host's tests cannot
   assert the value description, and the modules' tests check only the key.
   Suggested fix: add the value docs to `HoverDocs`, or a `hover_value`
   method.

2. **Hover only works on property names.** Hover resolves through the
   property (`Key`) segment of the path. A path that ends in an array index
   returns no hover. A data model where the thing under the caret is a
   value, such as a Dockerfile instruction keyword stored as
   `"instruction": "FROM"`, gets one generic description. The Dockerfile
   module had to change to `{"FROM": {...}}`, so the keyword becomes the
   property name. This constraint is not stated in the docs. Suggested fix:
   document it, and let a language return a value-level docs target.

3. **No registry for custom languages.** `Language` implements `FromStr`
   only for the built-in formats. The demo's picker first called
   `value.parse().unwrap()`, which would panic for any custom language. The
   fix is to keep a list of `Language` constants in the host. Suggested fix:
   a registration call, or a `Language::from_name` that accepts custom
   languages the host registers.

4. **Lexing is fixed to `"` strings and `#`-style comments in the editor.**
   Raw strings (RON `r#"..."#`, Nickel `m%"..."%`) and nested block comments
   are not highlighted or folded correctly. Suggested fix: a hook for the
   language's own tokens, or the `highlight` method documented as the place
   for this.

5. **`InsertPlan` is `#[non_exhaustive]`.** A test cannot build one, so
   `write_insert` can only be tested through `accept`. Suggested fix: a
   constructor, or a test helper.

6. **Serialization defaults to an error.** A language that does not override
   `serialize` gets no round trip, and the docs do not say which methods are
   needed for Edit-as. Suggested fix: document the set of methods needed for
   each feature.

7. **Many optional methods.** A small language needs `info`, `parse`, and in
   practice `config`, `completion_context`, `hover` and `serialize`. The INI
   example that shows the pattern is published with the crate (`tests/ini`,
   `tests/custom_language.rs`), but the README does not point a new author
   there first. Suggested fix: a minimal template in the README.

8. **Completion hides written keys only when the text parses.** Keys already
   written are read from the document around the caret. A broken line directly
   above the caret is not blanked (`around.rs`, `line_to_blank`: only lines
   above the last non-empty line are), so the rest of the document fails to
   parse and the written keys are lost. The RON and Nickel modules hit this.
   The workaround (keep text below the caret) is partial, see above. Suggested
   fix: blank a broken line next to the caret too.

9. **No language-level warnings.** `ConfigLanguage::parse` returns a
   `ParseError` with a fixed error severity and stops the parse. A rule that is
   not a syntax error (a warning, an info note) has no hook in the language.
   The demo uses the host's `host_diagnostics` prop for its one info rule
   (NixOS unknown options). Suggested fix: let a language return diagnostics
   with a severity.

10. **`format_value` gets no schema.** It is called with a value only, so a
    string that looks like an identifier cannot be written differently from
    an enum tag. The Nickel module writes such strings as tags and documents it.
    Suggested fix: pass the schema, or the property, to `format_value`.

11. **Suggest-while-typing is one global switch.** `auto_trigger_completions`
    applies to every language. A language cannot say which characters open the
    list, so, for example, NixOS could not open it after `.` without also
    opening it for every other language. It is also not a per-language choice
    in the docs. Suggested fix: per-language trigger characters.

12. **Positions are one-based in both directions.** `TextPosition` is one-based
    for line and column, while byte offsets and UTF-16 offsets are zero-based.
    This is easy to get wrong, and the docs say it only in passing. Suggested
    fix: a note next to each type, or distinct types.

13. **Descriptions for tests come from `Suggestion::detail()`.** Enum
    descriptions reach completion through `detail()`, and the hover value
    through no public accessor (point 1). A host test has to use completion to
    check an enum description. This works, but it is an indirect route.

## What was my mistake

- **The Dockerfile model.** I first modeled each instruction as a value
  under an `instruction` key. Hover could not show per-keyword docs because
  of point 2 above, and I reported the hover as working before I had tested
  it. The fix was to key items by keyword.
- **Picker crash.** I wrote `value.parse().unwrap()` in the picker (point 3).
  That was a bug in the demo, not an API design error, and it is fixed.
- **Wrong crate name at first.** I searched for `config-editor-leptos`. The
  published name is `leptos-config-editor`.
- **Claims before checks.** Earlier I said hover and completion worked
  before a browser test. Browser checks found they did not.
- **Subagent model.** I launched some agents on the wrong model, which you
  corrected. Subagent output needed review before I trusted it; the review
  found real parse and schema bugs in several modules.

## Crate or our side?

- **Crate:** 1 (value docs not public), 3 (no way to register a custom
  language), 5 (`InsertPlan` cannot be built), 8 (`around.rs` keeps the broken
  line above the caret), 9 (no warning hook), 10 (`format_value` gets no
  schema), 11 (one global suggest-while-typing switch), 13 (hover value only
  through completion).
- **Crate docs, not code:** 2 (the property-name rule is not documented), 6
  (which methods Edit-as needs is not documented), 7 (the INI example is
  published as `tests/ini` and `tests/custom_language.rs`, so the claim that
  it lives in a git checkout was wrong), 12 (one-based positions are said only
  in passing).
- **Our side:** the Dockerfile instruction-as-value model (it led to point 2),
  the picker `unwrap` (point 3's crash), the missing `serialize` in the Caddy
  module, and the missing `highlight` implementation (point 4). The crate has a
  `highlight` hook (`language.rs`: `highlight` and `highlight_continued`) that
  the modules do not implement. Raw strings and nested comments can be
  highlighted through that hook, so point 4 is not only a crate limitation.
  Done: all six modules now implement `highlight`, and each has `serialize` for Edit-as.

## Summary

The crate is built on sound ideas and most limitations are its own: value docs
in the public API, a rule for custom-language registration, the broken-line
guard in completion, and the missing language-level warnings. Several more are
documentation gaps. Some of what looked like crate limits was our own model or
our own code, and those are listed above.
