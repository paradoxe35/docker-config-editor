//! Config editor demo on the published `leptos-config-editor` crate.
//!
//! Two kinds of language:
//! - domain-specific languages (Dockerfile, NixOS, Caddyfile) carry their own
//!   schema and sample;
//! - general-purpose formats (YAML, JSON, TOML, HCL, XML, RON, Nickel) take
//!   their schema from a chosen service domain (client, server or visitor),
//!   and show that domain's example configuration in the chosen format.
//!
//! The settings panel exposes the editor's customization options.

mod caddy_language;
mod docker_language;
mod env_language;
mod nickel_language;
mod nix_language;
mod ron_language;
mod schemas;

use caddy_language::{CADDY, CADDY_SCHEMA};
use docker_language::{DOCKERFILE, DOCKERFILE_SCHEMA};
use env_language::ENV;
use leptos::prelude::*;
use leptos_config_editor::{
    ConfigEditor, ConfigEditorToolbar, EditorHandle, EditorSettings, EditorTheme, HostDiagnostic,
    Language, Severity, TextPosition, TextRange,
};
use nickel_language::NICKEL;
use nix_language::{NIX, NIXOS_SCHEMA, unchecked_options};
use ron_language::RON;
use schemas::{DOMAINS, example_in};

/// The one-based line and column of byte `offset` in `text`.
fn position_of(text: &str, offset: usize) -> TextPosition {
    let before = &text[..offset.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().map_or(0, |s| s.chars().count()) + 1;
    TextPosition { line, column }
}

/// NixOS: options under a known root that the demo's schema does not list.
/// Shown as information, since the subset is partial and the option may be valid.
fn nix_host_diagnostics(text: &str) -> Vec<HostDiagnostic> {
    unchecked_options(text)
        .into_iter()
        .map(|(range, message)| {
            HostDiagnostic::at_range(
                TextRange {
                    start: position_of(text, range.start),
                    end: position_of(text, range.end),
                },
                Severity::Info,
                message,
            )
        })
        .collect()
}

/// One language in the picker. A domain-specific language has its own schema
/// and sample (`fixed`); a general-purpose format has none and takes its
/// schema from the selected domain.
struct Entry {
    name: &'static str,
    language: Language,
    fixed: Option<(&'static str, &'static str)>,
}

fn entries() -> Vec<Entry> {
    vec![
        Entry {
            name: "Dockerfile",
            language: DOCKERFILE,
            fixed: Some((
                DOCKERFILE_SCHEMA,
                "FROM alpine:latest\nENV PORT=8080\nEXPOSE 8080\nCMD [\"server\"]\n",
            )),
        },
        Entry {
            name: "NixOS",
            language: NIX,
            fixed: Some((
                NIXOS_SCHEMA,
                "{\n  networking.hostName = \"web-01\";\n  networking.firewall.allowedTCPPorts = [ 22 80 443 ];\n  services.openssh.enable = true;\n  services.openssh.settings.PermitRootLogin = \"no\";\n  time.timeZone = \"Europe/Paris\";\n  system.stateVersion = \"25.11\";\n}\n",
            )),
        },
        Entry {
            name: "Caddyfile",
            language: CADDY,
            fixed: Some((
                CADDY_SCHEMA,
                "{\n    email admin@example.com\n}\n\nexample.com {\n    encode gzip\n    reverse_proxy localhost:8080\n}\n",
            )),
        },
        Entry { name: "YAML", language: Language::YAML, fixed: None },
        Entry { name: "JSON", language: Language::JSON, fixed: None },
        Entry { name: "TOML", language: Language::TOML, fixed: None },
        Entry { name: "HCL", language: Language::HCL, fixed: None },
        Entry { name: "XML", language: Language::XML, fixed: None },
        Entry { name: "RON", language: RON, fixed: None },
        Entry { name: ".env", language: ENV, fixed: None },
        Entry { name: "Nickel", language: NICKEL, fixed: None },
    ]
}

fn is_fixed(index: usize) -> bool {
    entries()[index].fixed.is_some()
}

/// The sample text of entry `index` and the notes on it (what a conversion
/// could not keep, or why it failed). A general-purpose format shows the
/// selected domain's example, converted into it.
fn starting_text(index: usize, domain: usize) -> (String, String) {
    let entry = entries().swap_remove(index);
    match entry.fixed {
        Some((_, sample)) => (sample.to_string(), String::new()),
        None => match example_in(&DOMAINS[domain], entry.language) {
            Ok((text, losses)) if losses.is_empty() => (text, String::new()),
            Ok((text, losses)) => (text, format!("Not kept in {}: {}", entry.name, losses.join("; "))),
            Err(reason) => (String::new(), format!("{} cannot show this example: {reason}", entry.name)),
        },
    }
}

/// The formats the demo can view the document in (Edit as). Native languages
/// are edited as themselves; these are the built-in formats.
const EDIT_AS: [(&str, &str); 6] = [
    ("", "Native"),
    ("YAML", "YAML"),
    ("JSON", "JSON"),
    ("TOML", "TOML"),
    ("HCL", "HCL"),
    ("XML", "XML"),
];

/// The editor's built-in themes, by name.
fn themes() -> Vec<EditorTheme> {
    vec![
        EditorTheme::Nord,
        EditorTheme::VsCodeDark,
        EditorTheme::VsCodeLight,
        EditorTheme::Obsidian,
        EditorTheme::ObsidianLight,
        EditorTheme::Dracula,
        EditorTheme::AtomOneDark,
        EditorTheme::GitHubLight,
        EditorTheme::GitHubDark,
        EditorTheme::SolarizedLight,
        EditorTheme::ViraInspired,
    ]
}

fn theme_named(name: &str) -> Option<EditorTheme> {
    themes().into_iter().find(|theme| format!("{theme:?}") == name)
}

/// One boolean setting in the panel: its label and how to read and write it.
struct Toggle {
    label: &'static str,
    get: fn(&EditorSettings) -> bool,
    set: fn(&mut EditorSettings, bool),
}

fn toggles() -> Vec<Toggle> {
    vec![
        Toggle { label: "Footer", get: |s| s.show_footer, set: |s, v| s.show_footer = v },
        Toggle { label: "Diagnostics", get: |s| s.show_diagnostics, set: |s, v| s.show_diagnostics = v },
        Toggle { label: "Diff against saved", get: |s| s.show_diff, set: |s, v| s.show_diff = v },
        Toggle { label: "Completions", get: |s| s.completions, set: |s, v| s.completions = v },
        Toggle { label: "Suggest while typing", get: |s| s.auto_trigger_completions, set: |s, v| s.auto_trigger_completions = v },
        Toggle { label: "Hover docs", get: |s| s.hover_docs, set: |s, v| s.hover_docs = v },
        Toggle { label: "Syntax highlighting", get: |s| s.highlighting, set: |s, v| s.highlighting = v },
        Toggle { label: "Auto-closing brackets", get: |s| s.auto_closing, set: |s, v| s.auto_closing = v },
        Toggle { label: "Search", get: |s| s.search, set: |s, v| s.search = v },
        Toggle { label: "Go to line", get: |s| s.go_to_line, set: |s, v| s.go_to_line = v },
        Toggle { label: "Folding", get: |s| s.folding, set: |s, v| s.folding = v },
        Toggle { label: "Sticky scroll", get: |s| s.sticky_scroll, set: |s, v| s.sticky_scroll = v },
        Toggle { label: "Ctrl+S saves", get: |s| s.save_shortcut, set: |s, v| s.save_shortcut = v },
    ]
}

/// The settings the demo starts with.
fn default_settings() -> EditorSettings {
    EditorSettings {
        theme: EditorTheme::Nord,
        show_toolbar: false,
        show_footer: true,
        show_diagnostics: true,
        search: true,
        show_diff: true,
        completions: true,
        auto_trigger_completions: true,
        ..Default::default()
    }
}

#[component]
fn Demo() -> impl IntoView {
    let (start, start_notes) = starting_text(0, 0);
    let index = RwSignal::new(0usize);
    let domain = RwSignal::new(0usize);
    let text = RwSignal::new(start.clone());
    let baseline = RwSignal::new(start);
    let notes = RwSignal::new(start_notes);

    let settings = RwSignal::new(default_settings());
    let read_only = RwSignal::new(false);
    let edit_as_language = RwSignal::new(None::<Language>);

    // Host checks shown in the editor: the NixOS information notes, recomputed
    // on each edit while NixOS is the native language, and cleared otherwise.
    let host_problems = RwSignal::new(Vec::<HostDiagnostic>::new());
    Effect::new(move |_| {
        let current = text.get();
        let is_nix = entries()[index.get()].name == "NixOS";
        host_problems.set(if is_nix { nix_host_diagnostics(&current) } else { Vec::new() });
    });

    // Loads the starting text of the chosen language and domain. The editor's
    // language and schema are fixed at mount, so it remounts on the change.
    let load = move |language: usize, service: usize| {
        let (sample, message) = starting_text(language, service);
        text.set(sample.clone());
        baseline.set(sample);
        notes.set(message);
    };

    let reset = move |_| {
        settings.set(default_settings());
        read_only.set(false);
        edit_as_language.set(None);
        load(index.get(), domain.get());
    };

    view! {
        <div class="config-editor-theme demo">
            <ConfigEditorToolbar class="demo__toolbar" theme=move || settings.get().theme>
                <span class="demo__file">{move || entries()[index.get()].name}</span>
                <label class="demo__field">
                    <span>"Language"</span>
                    <select
                        class="demo__select"
                        prop:value=move || index.get().to_string()
                        on:change=move |ev| {
                            if let Ok(language) = event_target_value(&ev).parse::<usize>() {
                                index.set(language);
                                load(language, domain.get());
                            }
                        }
                    >
                        {entries()
                            .into_iter()
                            .enumerate()
                            .map(|(i, entry)| view! { <option value=i.to_string()>{entry.name}</option> })
                            .collect_view()}
                    </select>
                </label>
                <label class="demo__field" hidden=move || is_fixed(index.get())>
                    <span>"Schema"</span>
                    <select
                        class="demo__select"
                        prop:value=move || domain.get().to_string()
                        on:change=move |ev| {
                            if let Ok(service) = event_target_value(&ev).parse::<usize>() {
                                domain.set(service);
                                load(index.get(), service);
                            }
                        }
                    >
                        {DOMAINS
                            .iter()
                            .enumerate()
                            .map(|(i, service)| view! { <option value=i.to_string()>{service.label}</option> })
                            .collect_view()}
                    </select>
                </label>
                <label class="demo__field">
                    <span>"Edit as"</span>
                    <select
                        class="demo__select"
                        prop:value=move || edit_as_language.get().map_or(String::new(), |l| l.to_string())
                        on:change=move |ev| {
                            let value = event_target_value(&ev);
                            edit_as_language.set(value.parse::<Language>().ok());
                        }
                    >
                        {EDIT_AS
                            .into_iter()
                            .map(|(value, label)| view! { <option value=value>{label}</option> })
                            .collect_view()}
                    </select>
                </label>
                <div class="demo__actions">
                    <button
                        class="demo__button"
                        on:click=move |_| settings.update(|s| s.theme = {
                            let all = themes();
                            let at = all.iter().position(|t| *t == s.theme).unwrap_or(0);
                            all[(at + 1) % all.len()].clone()
                        })
                    >
                        {move || format!("Theme: {:?}", settings.get().theme)}
                    </button>
                    <button
                        class="demo__button"
                        on:click=move |_| settings.update(|s| s.show_diagnostics = !s.show_diagnostics)
                    >
                        {move || if settings.get().show_diagnostics { "Diagnostics: On" } else { "Diagnostics: Off" }}
                    </button>
                    <button
                        class="demo__button"
                        on:click=move |_| settings.update(|s| s.show_diff = !s.show_diff)
                    >
                        {move || if settings.get().show_diff { "Diff: On" } else { "Diff: Off" }}
                    </button>
                </div>
            </ConfigEditorToolbar>

            <p class="demo__notes" hidden=move || notes.get().is_empty()>
                {move || notes.get()}
            </p>

            <details class="demo__settings">
                <summary>"Settings"</summary>
                <div class="demo__settings-grid">
                    <label class="demo__field">
                        <span>"Theme"</span>
                        <select
                            class="demo__select"
                            prop:value=move || format!("{:?}", settings.get().theme)
                            on:change=move |ev| {
                                if let Some(theme) = theme_named(&event_target_value(&ev)) {
                                    settings.update(|s| s.theme = theme);
                                }
                            }
                        >
                            {themes()
                                .into_iter()
                                .map(|theme| {
                                    let name = format!("{theme:?}");
                                    let value = name.clone();
                                    view! { <option value=value>{name}</option> }
                                })
                                .collect_view()}
                        </select>
                    </label>
                    <label class="demo__field">
                        <span>"Hover delay (ms)"</span>
                        <input
                            class="demo__select"
                            type="number"
                            min="0"
                            step="50"
                            prop:value=move || settings.get().hover_delay.to_string()
                            on:change=move |ev| {
                                if let Ok(ms) = event_target_value(&ev).parse::<u32>() {
                                    settings.update(|s| s.hover_delay = ms);
                                }
                            }
                        />
                    </label>
                    <label class="demo__check">
                        <input
                            type="checkbox"
                            prop:checked=move || read_only.get()
                            on:change=move |ev| read_only.set(event_target_checked(&ev))
                        />
                        "Read only"
                    </label>
                    {toggles()
                        .into_iter()
                        .map(|toggle| {
                            let Toggle { label, get, set } = toggle;
                            view! {
                                <label class="demo__check">
                                    <input
                                        type="checkbox"
                                        prop:checked=move || get(&settings.get())
                                        on:change=move |ev| {
                                            let on = event_target_checked(&ev);
                                            settings.update(|s| set(s, on));
                                        }
                                    />
                                    {label}
                                </label>
                            }
                        })
                        .collect_view()}
                    <button class="demo__button" on:click=reset>"Reset to defaults"</button>
                </div>
            </details>

            {move || {
                let i = index.get();
                let service = domain.get();
                let entry = entries().swap_remove(i);
                let schema = match entry.fixed {
                    Some((schema, _)) => schema,
                    None => DOMAINS[service].schema,
                };
                let handle = EditorHandle::new();
                view! {
                    <ConfigEditor
                        class="demo__editor"
                        language=entry.language
                        edit_as=edit_as_language
                        value=text
                        on_change=move |updated| text.set(updated)
                        schema=schema
                        settings=settings
                        read_only=read_only
                        host_diagnostics=host_problems
                        handle=handle
                        baseline=baseline
                    />
                }
            }}
        </div>
    }
}

fn main() {
    console_error_panic_hook::set_once();

    mount_to_body(|| {
        view! { <Demo /> }
    })
}
