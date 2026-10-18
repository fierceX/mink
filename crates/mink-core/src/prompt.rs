mod core;
mod document;
mod mission;
mod modules;
pub mod workflows;

pub use document::{PromptDocument, PromptSection, PromptSectionOrigin};
pub use workflows::{RenderedPromptPack, ResolvedPromptWorkflows};

use crate::capabilities::{ContextFileSnapshot, RuleSnapshot, SkillSnapshot};
use crate::tools::semantic_capabilities::ResolvedToolCapabilities;
use crate::tools::surface::ModelToolSurface;
use anyhow::{Result, ensure};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

pub struct Builder {
    pub cwd: PathBuf,
    pub home: PathBuf,
    /// Scope statement rendered for a delegated child; absent for primaries.
    pub delegation: Option<DelegationScope>,
    pub skill_snapshot: Arc<SkillSnapshot>,
    pub context_file_snapshot: Arc<ContextFileSnapshot>,
    pub rule_snapshot: Arc<RuleSnapshot>,
    pub mission_file: Option<PathBuf>,
    pub mission_content: Option<String>,
    pub tool_surface: Arc<ModelToolSurface>,
    pub tool_capabilities: Arc<ResolvedToolCapabilities>,
    pub edit_mode: crate::config::EditMode,
    pub edit_fuzzy_match: bool,
    pub edit_fuzzy_threshold: f64,
    pub edit_enforce_seen_lines: bool,
    pub signal_policy: crate::config::SignalPolicy,
    /// Character budget for the model-visible skill index (see
    /// [`skill_index_budget_chars`]).
    pub skill_index_budget_chars: usize,
}

pub struct DelegationScope {
    pub depth: u32,
    pub max_depth: u32,
}

pub struct PromptBuildContext {
    pub edit_mode: crate::config::EditMode,
    pub edit_fuzzy_match: bool,
    pub edit_fuzzy_threshold: f64,
    pub edit_enforce_seen_lines: bool,
}

impl Builder {
    pub fn build_system_prompt(&self) -> Result<String> {
        Ok(self.build_document()?.render())
    }

    pub fn build_document(&self) -> Result<PromptDocument> {
        let mut document = PromptDocument::new();
        core::append_core_sections(self, &mut document)?;

        if let Some(scope) = &self.delegation {
            let mut content = String::from(
                "You are a delegated subagent. Your tool scope was fixed when you were started; \
                 operations that are unavailable are not retryable from here. Report the \
                 limitation in your result instead.",
            );
            if scope.depth >= scope.max_depth {
                content.push_str(&format!(
                    "\nDelegation depth {} of {}; further delegation is unavailable.",
                    scope.depth, scope.max_depth
                ));
            }
            document.push(PromptSection {
                id: "delegated-subagent".into(),
                tag: "delegated-subagent".into(),
                origin: PromptSectionOrigin::Core,
                content,
                name: None,
                referenced_tools: Default::default(),
                consumed_facts: Default::default(),
            })?;
        }

        if let Some(content) = self.mission_content()? {
            mission::apply(&mut document, &content, modules::reserved_section_ids())?;
        }

        modules::append_tool_sections(&mut document, &self.tool_surface)?;
        let workflows =
            workflows::PromptWorkflowResolver::builtin().resolve(&self.tool_capabilities)?;
        let build_context = PromptBuildContext {
            edit_mode: self.edit_mode,
            edit_fuzzy_match: self.edit_fuzzy_match,
            edit_fuzzy_threshold: self.edit_fuzzy_threshold,
            edit_enforce_seen_lines: self.edit_enforce_seen_lines,
        };
        for spec in workflows.ordered() {
            let rendered = (spec.render)(&build_context, &self.tool_capabilities)?;
            validate_pack(spec, &rendered, &workflows, &self.tool_surface)?;
            document.push(PromptSection {
                id: spec.id.to_string(),
                tag: spec.id.to_string(),
                origin: PromptSectionOrigin::Workflow,
                content: rendered.content,
                name: None,
                referenced_tools: rendered.referenced_tools,
                consumed_facts: rendered.consumed_facts,
            })?;
        }

        core::append_external_sections(self, &mut document)?;
        document.move_to_end("output-language")?;
        document.validate_generated_references(&self.tool_surface, &workflows)?;
        Ok(document)
    }

    fn mission_content(&self) -> Result<Option<String>> {
        if let Some(content) = &self.mission_content {
            return Ok((!content.trim().is_empty()).then(|| content.clone()));
        }
        let Some(path) = &self.mission_file else {
            return Ok(None);
        };
        let content = fs::read_to_string(path).map_err(|error| {
            anyhow::anyhow!("failed to read mission file {}: {error}", path.display())
        })?;
        Ok((!content.trim().is_empty()).then_some(content))
    }
}

fn validate_pack(
    spec: &workflows::PromptWorkflowSpec,
    rendered: &RenderedPromptPack,
    workflows: &ResolvedPromptWorkflows,
    surface: &ModelToolSurface,
) -> Result<()> {
    for name in &rendered.referenced_tools {
        ensure!(
            surface.has(name),
            "workflow '{}' referenced inactive tool '{}'",
            spec.id,
            name
        );
    }
    for fact in &rendered.consumed_facts {
        ensure!(
            workflows.has_fact(*fact),
            "workflow '{}' consumed unresolved fact '{fact:?}'",
            spec.id
        );
    }
    ensure!(
        spec.requires.satisfied_by(&rendered.consumed_facts),
        "workflow '{}' render contract lacks a satisfying requirement witness",
        spec.id
    );
    Ok(())
}

/// Default character budget for the model-visible skill index when the
/// context window is unknown (unlimited).
pub(crate) const DEFAULT_SKILL_INDEX_BUDGET_CHARS: usize = 8_000;

/// Character budget for the model-visible skill index.
///
/// One percent of a finite context window, clamped to [2_000, 8_000]; the
/// default when the window is unbounded. `MINK_SKILL_INDEX_BUDGET_CHARS`
/// overrides the computed budget (escape hatch for embeddings/tests).
pub(crate) fn skill_index_budget_chars(max_context_tokens: usize) -> usize {
    if let Ok(value) = std::env::var("MINK_SKILL_INDEX_BUDGET_CHARS")
        && let Ok(parsed) = value.trim().parse::<usize>()
    {
        return parsed.max(1);
    }
    skill_index_budget_default(max_context_tokens)
}

fn skill_index_budget_default(max_context_tokens: usize) -> usize {
    if max_context_tokens == 0 {
        DEFAULT_SKILL_INDEX_BUDGET_CHARS
    } else {
        (max_context_tokens / 100).clamp(2_000, 8_000)
    }
}

pub(crate) fn escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
#[path = "prompt_tests.rs"]
mod tests;
