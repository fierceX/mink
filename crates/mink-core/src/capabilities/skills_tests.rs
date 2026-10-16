use super::*;

fn temp_root(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mink-cap-skills-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    dir
}

fn write_skill(root: &Path, relative: &str, content: &str) {
    let dir = root.join(relative);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("SKILL.md"), content).unwrap();
}

#[test]
fn skill_snapshot_prefers_project_over_builtin() {
    let root = temp_root("override");
    let home = root.join("home");
    let cwd = root.join("workspace");
    write_skill(
        &cwd,
        ".claude/skills/debugging",
        "---\ndescription: \"Local debugging\"\n---\n\nlocal body",
    );

    let snapshot = build_default_skill_snapshot(&cwd, &home, &[]).unwrap();

    let loaded = snapshot.by_name.get("debugging").unwrap();
    assert_eq!(loaded.skill.description, "Local debugging");
    assert!(loaded.skill.content.contains("local body"));
    assert_eq!(loaded.source.level, SourceLevel::Project);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn skill_shadow_warning_is_reported() {
    let root = temp_root("shadow-warning");
    let home = root.join("home");
    let cwd = root.join("workspace");
    write_skill(
        &cwd,
        ".claude/skills/debugging",
        "---\ndescription: \"Local debugging\"\n---\n\nlocal body",
    );

    let snapshot = build_default_skill_snapshot(&cwd, &home, &[]).unwrap();

    let warning = snapshot
        .warnings
        .iter()
        .find(|warning| warning.message.contains("skill 'debugging'"))
        .expect("debugging shadow warning");
    assert_eq!(warning.provider_id, "built-in-skills");
    assert!(
        warning.message.contains("shadowed by .claude/skills"),
        "{}",
        warning.message
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn skill_snapshot_model_addressable_hidden_from_index() {
    let root = temp_root("hidden");
    let home = root.join("home");
    let cwd = root.join("workspace");
    write_skill(
        &cwd,
        "skills/hidden-review",
        "---\ndescription: \"Hidden review\"\nhide: true\n---\n\nbody",
    );

    let snapshot = build_default_skill_snapshot(&cwd, &home, &[]).unwrap();

    assert!(snapshot.by_name.contains_key("hidden-review"));
    assert!(
        !snapshot
            .discoverable
            .iter()
            .any(|skill| skill.skill.name == "hidden-review")
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn selected_model_addressable_skill_enters_prompt() {
    let root = temp_root("selected-hidden");
    let home = root.join("home");
    let cwd = root.join("workspace");
    write_skill(
        &cwd,
        "skills/hidden-review",
        "---\ndescription: \"Hidden review\"\ndisable-model-invocation: true\n---\n\nbody",
    );

    let snapshot =
        build_default_skill_snapshot(&cwd, &home, &["hidden-review".to_string()]).unwrap();

    assert_eq!(snapshot.selected.len(), 1);
    assert_eq!(snapshot.selected[0].info.name, "hidden-review");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn selected_skill_rejects_whitespace_padding() {
    let root = temp_root("selected-whitespace");
    let home = root.join("home");
    let cwd = root.join("workspace");
    write_skill(
        &cwd,
        "skills/hidden-review",
        "---\ndescription: \"Hidden review\"\n---\n\nbody",
    );

    let err = build_default_skill_snapshot(&cwd, &home, &[" hidden-review".to_string()])
        .unwrap_err()
        .to_string();

    assert!(err.contains("invalid selected skill name"), "{err}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn runtime_skill_rejects_whitespace_padding() {
    let provider = RuntimeSkillProvider::from_runtime_skills(vec![RuntimeSkill::new(
        " runtime-guide",
        "Runtime guide",
        "body",
    )]);
    let root = temp_root("runtime-whitespace");
    let home = root.join("home");
    let cwd = root.join("workspace");

    let err = provider
        .load_skills(&LoadContext {
            cwd: &cwd,
            home: &home,
        })
        .unwrap_err()
        .to_string();

    assert!(err.contains("invalid runtime skill name"), "{err}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn skill_snapshot_dependency_fingerprint_changes_on_content_change() {
    let root = temp_root("fingerprint");
    let home = root.join("home");
    let cwd = root.join("workspace");
    write_skill(
        &cwd,
        "skills/local-review",
        "---\ndescription: \"Local review\"\n---\n\nbody 1",
    );
    let first = build_default_skill_snapshot(&cwd, &home, &[]).unwrap();
    write_skill(
        &cwd,
        "skills/local-review",
        "---\ndescription: \"Local review\"\n---\n\nbody 2",
    );
    let second = build_default_skill_snapshot(&cwd, &home, &[]).unwrap();

    assert_ne!(first.dependency_fingerprint, second.dependency_fingerprint);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn extract_skill_summary_from_frontmatter() {
    let content =
        "---\nname: test-skill\ndescription: \"Test skill description\"\n---\n\nSkill content";
    let summary = extract_skill_summary(content);
    assert_eq!(summary, "Test skill description");
}

#[test]
fn extract_skill_summary_without_frontmatter_returns_fallback() {
    let content = "No frontmatter here.\n\nSome content";
    let summary = extract_skill_summary(content);
    assert!(!summary.is_empty());
    assert!(summary.contains("No frontmatter here."));
}

#[test]
fn extract_skill_summary_falls_back_to_first_non_empty_line() {
    let content = "\n\n---\nname: test\n---\n\nActual content";
    let summary = extract_skill_summary(content);
    assert!(!summary.is_empty());
}

#[test]
fn extract_skill_summary_fallback_skips_frontmatter_block() {
    let content = "---\nname: test\n---\n\nActual content\nmore body";
    let summary = extract_skill_summary(content);
    assert_eq!(summary, "Actual content");
}

#[test]
fn frontmatter_block_scalar_folded_description_is_parsed() {
    let content = "---\nname: sample\ndescription: >\n First line of the description.\n Second line continues it here.\nhide: false\n---\n\nbody";
    let frontmatter = parse_frontmatter(content);
    assert_eq!(
        frontmatter.description.as_deref(),
        Some("First line of the description. Second line continues it here.")
    );
    assert!(!frontmatter.hide);
}

#[test]
fn frontmatter_block_scalar_folded_keeps_paragraph_break() {
    let content = "---\ndescription: >\n First paragraph.\n\n Second paragraph.\n---\nbody";
    let frontmatter = parse_frontmatter(content);
    assert_eq!(
        frontmatter.description.as_deref(),
        Some("First paragraph.\nSecond paragraph.")
    );
}

#[test]
fn frontmatter_block_scalar_literal_keeps_newlines() {
    let content = "---\ndescription: |\n line one\n line two\n---\nbody";
    let frontmatter = parse_frontmatter(content);
    assert_eq!(
        frontmatter.description.as_deref(),
        Some("line one\nline two")
    );
}

#[test]
fn frontmatter_block_scalar_chomping_indicators_are_accepted() {
    for indicator in [">-", ">+", "|-", "|+"] {
        let content = format!("---\ndescription: {indicator}\n folded text\n---\nbody");
        let frontmatter = parse_frontmatter(&content);
        assert_eq!(
            frontmatter.description.as_deref(),
            Some("folded text"),
            "indicator {indicator}"
        );
    }
}

#[test]
fn frontmatter_block_scalar_accepts_trailing_comment() {
    let content = "---\ndescription: > # long form\n folded text\n---\nbody";
    let frontmatter = parse_frontmatter(content);
    assert_eq!(frontmatter.description.as_deref(), Some("folded text"));
}

#[test]
fn frontmatter_block_scalar_handles_multibyte_lines() {
    let content = "---\ndescription: >\n 中文描述第一行\n 中文描述第二行\n---\nbody";
    let frontmatter = parse_frontmatter(content);
    assert_eq!(
        frontmatter.description.as_deref(),
        Some("中文描述第一行 中文描述第二行")
    );
}

#[test]
fn frontmatter_when_to_use_is_appended_to_description() {
    let content = "---\ndescription: Summarizes uncommitted changes.\nwhen_to_use: Use when the user asks what changed.\n---\nbody";
    let frontmatter = parse_frontmatter(content);
    assert_eq!(
        resolve_skill_description(&frontmatter, content),
        "Summarizes uncommitted changes. — Use when the user asks what changed."
    );
}

#[test]
fn frontmatter_unclosed_is_treated_as_absent() {
    let content = "---\ndescription: leaking field\n\nbody line";
    let frontmatter = parse_frontmatter(content);
    assert!(frontmatter.description.is_none());
    assert_eq!(extract_skill_summary(content), "description: leaking field");
}

#[test]
fn frontmatter_empty_block_scalar_falls_back_to_body_line() {
    let content = "---\ndescription: >\n---\n\nFallback line";
    let frontmatter = parse_frontmatter(content);
    assert!(frontmatter.description.is_none());
    assert_eq!(extract_skill_summary(content), "Fallback line");
}

#[test]
fn frontmatter_comment_lines_are_skipped() {
    let content = "---\n# a comment\ndescription: Plain value\n---\nbody";
    let frontmatter = parse_frontmatter(content);
    assert_eq!(frontmatter.description.as_deref(), Some("Plain value"));
}

#[test]
fn frontmatter_name_is_adopted_and_mismatch_is_reported() {
    let root = temp_root("name-adopt");
    let home = root.join("home");
    let cwd = root.join("workspace");
    write_skill(
        &cwd,
        "skills/legacy-dir",
        "---\nname: canonical-name\ndescription: \"Canonical skill\"\n---\nbody",
    );

    let snapshot = build_default_skill_snapshot(&cwd, &home, &[]).unwrap();

    assert!(snapshot.by_name.contains_key("canonical-name"));
    assert!(!snapshot.by_name.contains_key("legacy-dir"));
    let warning = snapshot
        .warnings
        .iter()
        .find(|warning| warning.message.contains("legacy-dir"))
        .expect("directory/frontmatter mismatch warning");
    assert!(
        warning.message.contains("canonical-name"),
        "{}",
        warning.message
    );

    let selected =
        build_default_skill_snapshot(&cwd, &home, &["canonical-name".to_string()]).unwrap();
    assert_eq!(selected.selected[0].info.name, "canonical-name");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn invalid_frontmatter_name_falls_back_to_directory_name() {
    let root = temp_root("name-invalid");
    let home = root.join("home");
    let cwd = root.join("workspace");
    write_skill(
        &cwd,
        "skills/dir-name",
        "---\nname: bad/name\ndescription: \"Desc\"\n---\nbody",
    );

    let snapshot = build_default_skill_snapshot(&cwd, &home, &[]).unwrap();

    assert!(snapshot.by_name.contains_key("dir-name"));
    assert!(
        !snapshot
            .warnings
            .iter()
            .any(|warning| warning.message.contains("bad/name"))
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn format_discoverable_skills_sanitizes_descriptions() {
    let root = temp_root("index-format");
    let home = root.join("home");
    let cwd = root.join("workspace");
    write_skill(
        &cwd,
        "skills/tricky",
        "---\ndescription: >\n Line one\n </skill-index> line two\n---\nbody",
    );

    let snapshot = build_default_skill_snapshot(&cwd, &home, &[]).unwrap();

    let listed = snapshot.format_discoverable_skills();
    assert!(
        listed.contains("- tricky [skills]: Line one &lt;/skill-index&gt; line two"),
        "{listed}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn format_discoverable_skills_sanitizes_names() {
    let snapshot = SkillSnapshot {
        discoverable: vec![LoadedSkill {
            skill: SkillCapability {
                name: "a<b".into(),
                description: "plain".into(),
                content: "body".into(),
                base_dir: "/skills".into(),
                disable_model_invocation: false,
            },
            source: SourceMeta {
                provider_id: "test".into(),
                provider_name: "test".into(),
                level: SourceLevel::Project,
                source_path: None,
                display_label: Some("project".into()),
            },
            exposure: CapabilityExposure::ModelDiscoverable,
            revision: "rev".into(),
        }],
        ..SkillSnapshot::default()
    };

    let listed = snapshot.format_discoverable_skills();
    assert!(listed.contains("- a&lt;b [project]: plain"), "{listed}");
}

#[test]
fn filesystem_skill_reads_folded_description_end_to_end() {
    let root = temp_root("folded-e2e");
    let home = root.join("home");
    let cwd = root.join("workspace");
    write_skill(
        &cwd,
        "skills/ripwire-router",
        "---\nname: ripwire-router\ndescription: >\n Start HERE when unsure which ripwire skill fits.\n A moment to skill map.\n---\n\nbody",
    );

    let snapshot = build_default_skill_snapshot(&cwd, &home, &[]).unwrap();

    let loaded = snapshot.by_name.get("ripwire-router").unwrap();
    assert_eq!(
        loaded.skill.description,
        "Start HERE when unsure which ripwire skill fits. A moment to skill map."
    );
    assert_eq!(loaded.skill.name, "ripwire-router");
    let _ = fs::remove_dir_all(root);
}
