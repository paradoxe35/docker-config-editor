//! The service-configuration domains the generic formats (YAML, JSON, TOML,
//! HCL, XML, RON, Nickel) are edited against. Each domain is a JSON Schema
//! and an example configuration in JSON; the editor converts the example into
//! the chosen format with `Language::convert`.

use leptos_config_editor::Language;

/// One domain: its name, its JSON Schema and its example configuration.
pub struct Domain {
    pub label: &'static str,
    pub schema: &'static str,
    /// An example configuration in JSON that validates against `schema`.
    pub example: &'static str,
}

/// The burrow tunnel domains: client, server and visitor configuration.
pub const DOMAINS: [Domain; 3] = [
    Domain {
        label: "Client",
        schema: include_str!("../client-schema.json"),
        example: include_str!("examples/client.json"),
    },
    Domain {
        label: "Server",
        schema: include_str!("../server-schema.json"),
        example: include_str!("examples/server.json"),
    },
    Domain {
        label: "Visitor",
        schema: include_str!("../visitor-schema.json"),
        example: include_str!("examples/visitor.json"),
    },
];

/// The example of `domain` written in `target`, with what the conversion
/// could not keep. `Err` names the reason the format cannot show it.
pub fn example_in(domain: &Domain, target: Language) -> Result<(String, Vec<String>), String> {
    let converted = Language::JSON
        .convert(domain.example, target)
        .map_err(|error| error.to_string())?;
    Ok((converted.text, converted.losses))
}

#[cfg(test)]
mod tests {
    use super::*;
    use leptos_config_editor::Schema;

    #[test]
    fn every_example_validates_against_its_schema() {
        let mut failures = Vec::new();
        for domain in &DOMAINS {
            let schema = Schema::compile(domain.schema).expect("schema compiles");
            let problems = schema.validate(Language::JSON, domain.example);
            if !problems.is_empty() {
                failures.push(format!(
                    "{}: {:?}",
                    domain.label,
                    problems.iter().map(|p| p.message.clone()).collect::<Vec<_>>()
                ));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn every_example_converts_to_yaml_and_json_without_losses() {
        for domain in &DOMAINS {
            for target in [Language::YAML, Language::JSON, Language::TOML] {
                let (_, losses) = example_in(domain, target)
                    .unwrap_or_else(|error| panic!("{} to {target:?}: {error}", domain.label));
                assert!(losses.is_empty(), "{} to {target:?}: {losses:?}", domain.label);
            }
        }
    }
}
