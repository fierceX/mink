//! Cross-crate `.minkrc` schema parity guard.
//!
//! `mink-server` cannot depend on the `publish = false` CLI crate, so
//! `crates/mink-server/src/session/agent_config.rs` replicates the CLI's
//! `MinkConfigFile` structs by hand. That replication silently drifted once:
//! the multimodal `[provider] image_input` / `vision_models` /
//! `[provider.image]` keys were added to the CLI but not to the server, and
//! because both files use `deny_unknown_fields`, every `.minkrc` using those
//! keys was rejected as a whole by the server.
//!
//! To make that class of drift fail loudly, this test parses both source
//! files and requires the config-file structs to expose exactly the same
//! field names. When the CLI gains a field, add it here (`agent_config.rs`)
//! and to the CLI in the same change.

use std::collections::BTreeSet;

const CLI_SOURCE: &str = include_str!("../../mink-cli/src/config.rs");
const SERVER_SOURCE: &str = include_str!("../src/session/agent_config.rs");

/// Config-file structs replicated between CLI and server.
const REPLICATED_STRUCTS: [&str; 11] = [
    "MinkConfigFile",
    "ProviderConfigFile",
    "ImageConfigFile",
    "GenerationConfigFile",
    "ContextConfigFile",
    "EditConfigFile",
    "ToolsConfigFile",
    "SignalPolicyFile",
    "RecoveryConfigFile",
    "SandboxConfigFile",
    "SandboxPythonConfigFile",
];

/// Field names declared by `struct <name> { ... }` in one source file.
fn struct_fields(source: &str, name: &str) -> BTreeSet<String> {
    let marker = format!("struct {name} {{");
    let start = source
        .find(&marker)
        .unwrap_or_else(|| panic!("struct {name} not found in source"));
    let body_start = start + marker.len();
    let body_end = body_start
        + source[body_start..]
            .find("\n}")
            .unwrap_or_else(|| panic!("struct {name} body is not terminated"));
    source[body_start..body_end]
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let rest = line.strip_prefix("pub ")?;
            rest.split_once(':')
                .map(|(field, _)| field.trim().to_string())
        })
        .collect()
}

#[test]
fn server_config_file_structs_match_the_cli_field_by_field() {
    for name in REPLICATED_STRUCTS {
        let cli_fields = struct_fields(CLI_SOURCE, name);
        let server_fields = struct_fields(SERVER_SOURCE, name);
        assert!(
            !cli_fields.is_empty(),
            "no public fields found for {name} in mink-cli/src/config.rs"
        );
        assert_eq!(
            server_fields, cli_fields,
            "{name} drifted from mink-cli/src/config.rs; update \
             crates/mink-server/src/session/agent_config.rs to mirror the CLI schema"
        );
    }
}
