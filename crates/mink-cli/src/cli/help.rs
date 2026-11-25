pub(super) fn truncate_display(s: &str, max_width: usize) -> String {
    crate::util::truncate_display(s, max_width)
}

pub(super) fn pad_display(s: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    let w = UnicodeWidthStr::width(s);
    if w >= width {
        s.to_string()
    } else {
        let mut result = s.to_string();
        result.push_str(&" ".repeat(width - w));
        result
    }
}

pub(super) fn print_usage() {
    let program = std::env::args()
        .next()
        .and_then(|arg| {
            std::path::Path::new(&arg)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "mink".to_string());
    println!("{}", version_line());
    println!();
    println!("Usage: {program} [options] [prompt]");
    println!("Options:");
    println!("  -m, --model MODEL       Model name or alias: flash | pro | any backend model");
    println!("  --api-key KEY           API key (default from env)");
    println!("  --base-url URL          Override API base URL");
    println!("  --mission PATH          Load custom system prompt from MISSION.md file");
    println!("  --session NAME          Use named session");
    println!("  --continue              Continue most recent session");
    println!("  --list-sessions         List saved sessions");
    println!("  --list-skills           List available skills");
    println!("  -v, --verbose           Verbose mode");
    println!("  -i, --interactive       Interactive mode (REPL)");
    #[cfg(feature = "tui")]
    println!("  --tui[=full|inline]     Full TUI (default) or inline TUI");
    println!("  --print                 Stream JSON events to stdout");
    println!("  --agent-jsonl           Agent JSONL protocol");
    println!("  --enabled-tools <list>  Comma-separated tools to enable, or 'none'");
    println!(
        "  --ptc MODE              Programmatic tool calling: off (default) | tool | exclusive"
    );
    println!("  --edit-mode MODE        Edit protocol: hashline (default) or replace");
    println!("  --edit-fuzzy-match BOOL Enable Replace progressive/fuzzy matching");
    println!("  --edit-fuzzy-threshold N Replace fuzzy threshold, 0.0..=1.0 (default: 0.95)");
    println!("  --edit-enforce-seen-lines BOOL Require displayed Hashline anchors");
    println!("  --skill NAME            Select a skill (repeatable); same as tools.skills");
    println!("  --config <toml>         Set config via TOML string (grouped sections)");
    println!(
        "                          Example: --config \"[generation]\\nmax_tokens=4096\\n[tools]\\ntool_timeout=300\""
    );
    println!("                          Sections: [provider] [generation] [context] [tools]");
    println!("                          [provider] image_input: \"on\" | \"off\" (explicit");
    println!("                          multi-modal override; vision_models: model id list;");
    println!("                          [provider.image]: max_images_per_request etc.)");
    println!("                          [tools.edit] [signal] [sandbox] [sandbox_python] [ptc]");
    println!("  -h, --help              Show this help");
    println!();
    println!("Config via TOML (--config or .minkrc):");
    println!("  See .minkrc.example for a complete reference.");
    println!(
        "  Priority: CLI flags > --config > project .minkrc > user ~/.minkrc > env > defaults."
    );
    println!();
    println!("Environment:");
    println!("  DEEPSEEK_API_KEY        DeepSeek API key");
    println!("  DEEPSEEK_BASE_URL       DeepSeek base URL");
    println!("  LOG_EVENTS              Enable event logging (default: true)");
    println!("  TOOL_RESULT_MAX_BYTES   Max tool result bytes");
    println!("  FILE_WRITE_MAX_BYTES    Max file write bytes");
    println!("  MAX_SEARCH_FILES        Max files scanned by Glob/Grep");
    println!("  MAX_SEARCH_RESULTS      Max Glob/Grep results");
    println!("  MINK_LIMITS             Sandbox limits as JSON [sandbox] override");
    println!(
        "  MINK_SIGNAL_POLICY      Signal policy: off | evidence | state_ops | restart | full"
    );
    println!("  MINK_PTC_MODE           Programmatic tool calling: off | tool | exclusive");
    println!("  MINK_EDIT_MODE          Edit protocol: hashline | replace");
    println!("  MINK_EDIT_FUZZY_MATCH   Replace fuzzy matching: true | false");
    println!("  MINK_EDIT_FUZZY_THRESHOLD Replace fuzzy threshold, 0.0..=1.0");
    println!("  MINK_EDIT_ENFORCE_SEEN_LINES Hashline seen-line guard: true | false");
    println!("  MINK_IMAGE_INPUT        Image capability override: on | off");
    println!("  MINK_VISION_MODELS      Comma-separated vision model ids (replaces default)");
}
pub(super) fn version_line() -> String {
    let git_hash = env!("MINK_GIT_HASH");
    if git_hash.is_empty() {
        format!("mink {}", env!("CARGO_PKG_VERSION"))
    } else {
        format!("mink {} ({git_hash})", env!("CARGO_PKG_VERSION"))
    }
}
