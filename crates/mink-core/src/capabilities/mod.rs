pub mod context_files;
pub(crate) mod fingerprint;
pub mod model_capabilities;
pub mod rules;
pub mod skills;
pub mod source;

use sha2::{Digest, Sha256};
use std::sync::Arc;

use crate::capabilities::fingerprint::hex_lower;

pub use context_files::{ContextFileSnapshot, build_default_context_file_snapshot};
pub use rules::{RuleSnapshot, build_default_rule_snapshot};
pub use skills::{
    LoadContext as SkillLoadContext, LoadedSkill, RuntimeSkill, SkillCapability,
    SkillDiscoveryPolicy, SkillProvider, SkillSnapshot, build_default_skill_snapshot,
    skill_providers_for_policy,
};
#[cfg(test)]
pub use skills::{ResolvedSkill, SkillInfo};
pub use source::{CapabilityExposure, SourceLevel, SourceMeta};

#[derive(Debug, Clone, Default)]
pub struct CapabilitySnapshot {
    pub skills: SkillSnapshot,
    pub context_files: ContextFileSnapshot,
    pub rules: RuleSnapshot,
    pub warnings: Vec<CapabilityWarning>,
    pub dependency_fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityWarning {
    pub provider_id: String,
    pub message: String,
}

impl CapabilitySnapshot {
    pub fn load_default(
        cwd: &std::path::Path,
        home: &std::path::Path,
        selected_skills: &[String],
    ) -> anyhow::Result<Self> {
        let skills = build_default_skill_snapshot(cwd, home, selected_skills)?;
        let context_files = build_default_context_file_snapshot(cwd, home)?;
        let rules = build_default_rule_snapshot(cwd, home)?;
        let warnings = collect_warnings(&skills, &context_files, &rules);
        let dependency_fingerprint = compute_dependency_fingerprint(&[
            &skills.dependency_fingerprint,
            &context_files.dependency_fingerprint,
            &rules.dependency_fingerprint,
        ]);
        Ok(Self {
            skills,
            context_files,
            rules,
            warnings,
            dependency_fingerprint,
        })
    }

    pub fn load_from_skill_providers(
        providers: &[Arc<dyn SkillProvider>],
        cwd: &std::path::Path,
        home: &std::path::Path,
        selected_skills: &[String],
    ) -> anyhow::Result<Self> {
        let skills = SkillSnapshot::load(
            providers,
            &skills::LoadContext { cwd, home },
            selected_skills,
        )?;
        let context_files = build_default_context_file_snapshot(cwd, home)?;
        let rules = build_default_rule_snapshot(cwd, home)?;
        let warnings = collect_warnings(&skills, &context_files, &rules);
        let dependency_fingerprint = compute_dependency_fingerprint(&[
            &skills.dependency_fingerprint,
            &context_files.dependency_fingerprint,
            &rules.dependency_fingerprint,
        ]);
        Ok(Self {
            skills,
            context_files,
            rules,
            warnings,
            dependency_fingerprint,
        })
    }
}

fn collect_warnings(
    skills: &SkillSnapshot,
    context_files: &ContextFileSnapshot,
    rules: &RuleSnapshot,
) -> Vec<CapabilityWarning> {
    let mut warnings = Vec::new();
    warnings.extend(skills.warnings.clone());
    warnings.extend(context_files.warnings.clone());
    warnings.extend(rules.warnings.clone());
    warnings
}

fn compute_dependency_fingerprint(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(b"\0");
    }
    hex_lower(hasher.finalize())
}

/// Maximum characters of a single capability-index description rendered into
/// the model-visible prompt, matching the Agent Skills listing cap.
pub(crate) const INDEX_DESC_MAX_CHARS: usize = 1_536;

/// Normalize one index entry's descriptive text for the model-visible prompt:
/// strip control characters, fold whitespace runs (including newlines) into a
/// single space, and escape angle brackets so the text cannot imitate prompt
/// section markup.
pub(crate) fn sanitize_index_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for ch in text.chars() {
        if ch.is_control() || ch.is_whitespace() {
            if !out.is_empty() {
                pending_space = true;
            }
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        match ch {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Sanitized and length-capped description for one model-visible index line.
pub(crate) fn index_description(description: &str) -> String {
    truncate_chars(&sanitize_index_text(description), INDEX_DESC_MAX_CHARS)
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod index_tests {
    use super::{INDEX_DESC_MAX_CHARS, index_description, sanitize_index_text};

    #[test]
    fn sanitize_folds_control_characters_and_whitespace() {
        let text = "line one\n\nline two\tcontinues";
        assert_eq!(sanitize_index_text(text), "line one line two continues");
    }

    #[test]
    fn sanitize_escapes_angle_brackets() {
        let text = "close </skill-index> and open <b>";
        assert_eq!(
            sanitize_index_text(text),
            "close &lt;/skill-index&gt; and open &lt;b&gt;"
        );
    }

    #[test]
    fn index_description_truncates_at_the_cap() {
        let long = "x".repeat(INDEX_DESC_MAX_CHARS + 10);
        let capped = index_description(&long);
        assert_eq!(capped.chars().count(), INDEX_DESC_MAX_CHARS);
        assert!(capped.ends_with('…'));

        let at_limit = "y".repeat(INDEX_DESC_MAX_CHARS);
        assert_eq!(index_description(&at_limit), at_limit);
    }
}
