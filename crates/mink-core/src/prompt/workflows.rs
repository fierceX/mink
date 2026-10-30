use super::PromptBuildContext;
use crate::tools::semantic_capabilities::{
    ProviderTier, ResolvedToolCapabilities, ToolSemanticCapability,
};
use anyhow::{Result, ensure};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PromptFact {
    ToolCapability(ToolSemanticCapability),
    SpecializedWithFallback(ToolSemanticCapability),
}

#[derive(Debug, Clone, Copy)]
pub enum WorkflowRequirement {
    All(&'static [PromptFact]),
    Any(&'static [PromptFact]),
    AllWithAny {
        all: &'static [PromptFact],
        any: &'static [PromptFact],
    },
}

impl WorkflowRequirement {
    pub fn satisfied_by(&self, facts: &BTreeSet<PromptFact>) -> bool {
        match self {
            Self::All(required) => required.iter().all(|fact| facts.contains(fact)),
            Self::Any(required) => required.iter().any(|fact| facts.contains(fact)),
            Self::AllWithAny { all, any } => {
                all.iter().all(|fact| facts.contains(fact))
                    && any.iter().any(|fact| facts.contains(fact))
            }
        }
    }
}

#[derive(Debug)]
pub struct PromptWorkflowSpec {
    pub id: &'static str,
    pub requires: WorkflowRequirement,
    pub exclusive_group: Option<&'static str>,
    pub priority: u16,
    pub render: fn(&PromptBuildContext, &ResolvedToolCapabilities) -> Result<RenderedPromptPack>,
}

pub struct RenderedPromptPack {
    pub content: String,
    pub referenced_tools: BTreeSet<String>,
    pub consumed_facts: BTreeSet<PromptFact>,
}

#[derive(Debug, Clone)]
pub struct ResolvedPromptWorkflows {
    ordered: Vec<&'static PromptWorkflowSpec>,
    facts: BTreeSet<PromptFact>,
    fingerprint: String,
}

pub struct PromptWorkflowResolver {
    specs: &'static [PromptWorkflowSpec],
}

use PromptFact::{SpecializedWithFallback, ToolCapability};
use ToolSemanticCapability::*;

const SEARCH_FACTS: &[PromptFact] = &[ToolCapability(ContentSearch), ToolCapability(PathRead)];
const HASHLINE_EDIT_FACTS: &[PromptFact] = &[
    ToolCapability(EditableSnapshotRead),
    ToolCapability(HashlineEdit),
];
const REPLACE_EDIT_FACTS: &[PromptFact] =
    &[ToolCapability(PathRead), ToolCapability(ContentReplaceEdit)];
const ROUTING_FACTS: &[PromptFact] = &[
    SpecializedWithFallback(PathRead),
    SpecializedWithFallback(ContentSearch),
    SpecializedWithFallback(PathDiscovery),
];
const MUTATION_ROUTING_ALL: &[PromptFact] = &[ToolCapability(ShellExec)];
const MUTATION_ROUTING_ANY: &[PromptFact] = &[
    ToolCapability(FileCreate),
    ToolCapability(FileOverwrite),
    ToolCapability(FileEdit),
];
const PYTHON_FACTS: &[PromptFact] = &[
    ToolCapability(HostPythonExec),
    ToolCapability(SandboxedPythonExec),
];
const PLAN_ALL: &[PromptFact] = &[
    ToolCapability(PlanDraft),
    ToolCapability(PlanConfirm),
    ToolCapability(PlanClear),
];
const TODO_INSPECT: &[PromptFact] = &[ToolCapability(TodoInspect)];
const TODO_STRUCTURE: &[PromptFact] = &[
    ToolCapability(TodoInspect),
    ToolCapability(TodoStructureMutation),
];
const TODO_PROGRESS: &[PromptFact] = &[
    ToolCapability(TodoInspect),
    ToolCapability(TodoProgressTransition),
];

static WORKFLOWS: &[PromptWorkflowSpec] = &[
    PromptWorkflowSpec {
        id: "search-then-inspect",
        requires: WorkflowRequirement::All(SEARCH_FACTS),
        exclusive_group: None,
        priority: 100,
        render: render_search_then_inspect,
    },
    PromptWorkflowSpec {
        id: "hashline-edit",
        requires: WorkflowRequirement::All(HASHLINE_EDIT_FACTS),
        exclusive_group: Some("edit-protocol"),
        priority: 100,
        render: render_hashline_edit,
    },
    PromptWorkflowSpec {
        id: "replace-edit",
        requires: WorkflowRequirement::All(REPLACE_EDIT_FACTS),
        exclusive_group: Some("edit-protocol"),
        priority: 100,
        render: render_replace_edit,
    },
    PromptWorkflowSpec {
        id: "specialized-provider-routing",
        requires: WorkflowRequirement::Any(ROUTING_FACTS),
        exclusive_group: None,
        priority: 100,
        render: render_specialized_routing,
    },
    PromptWorkflowSpec {
        id: "specialized-mutation-routing",
        requires: WorkflowRequirement::AllWithAny {
            all: MUTATION_ROUTING_ALL,
            any: MUTATION_ROUTING_ANY,
        },
        exclusive_group: None,
        priority: 100,
        render: render_specialized_mutation_routing,
    },
    PromptWorkflowSpec {
        id: "python-execution-routing",
        requires: WorkflowRequirement::Any(PYTHON_FACTS),
        exclusive_group: None,
        priority: 100,
        render: render_python_routing,
    },
    PromptWorkflowSpec {
        id: "todo-inspection",
        requires: WorkflowRequirement::All(TODO_INSPECT),
        exclusive_group: None,
        priority: 100,
        render: render_todo_inspection,
    },
    PromptWorkflowSpec {
        id: "todo-structure",
        requires: WorkflowRequirement::All(TODO_STRUCTURE),
        exclusive_group: None,
        priority: 100,
        render: render_todo_structure,
    },
    PromptWorkflowSpec {
        id: "todo-progress",
        requires: WorkflowRequirement::All(TODO_PROGRESS),
        exclusive_group: None,
        priority: 100,
        render: render_todo_progress,
    },
    PromptWorkflowSpec {
        id: "plan-lifecycle",
        requires: WorkflowRequirement::All(PLAN_ALL),
        exclusive_group: None,
        priority: 100,
        render: render_plan_lifecycle,
    },
    PromptWorkflowSpec {
        id: "memory-recall",
        requires: WorkflowRequirement::All(SEARCH_FACTS),
        exclusive_group: None,
        priority: 90,
        render: render_memory_recall,
    },
];

static BUILTIN: LazyLock<PromptWorkflowResolver> =
    LazyLock::new(|| PromptWorkflowResolver { specs: WORKFLOWS });

impl PromptWorkflowResolver {
    pub fn builtin() -> &'static Self {
        &BUILTIN
    }

    pub fn resolve(&self, tools: &ResolvedToolCapabilities) -> Result<ResolvedPromptWorkflows> {
        self.validate()?;
        let facts = facts_from_capabilities(tools);
        let active = self.select_specs(&facts);
        let ordered = self
            .specs
            .iter()
            .filter(|spec| active.contains(spec.id))
            .collect::<Vec<_>>();
        let fingerprint = workflow_fingerprint(&ordered, &facts);
        Ok(ResolvedPromptWorkflows {
            ordered,
            facts,
            fingerprint,
        })
    }

    /// Select the active specs in one pass.
    ///
    /// Facts are a fixed set computed from the resolved capabilities; no spec
    /// adds facts, so a spec rejected for its requirement can never become
    /// eligible later. An exclusive group claims at most one spec, and any spec
    /// rejected by a claim stays rejected. One pass therefore reaches the same
    /// fixed point the former fixed-point loop computed.
    fn select_specs(&self, facts: &BTreeSet<PromptFact>) -> BTreeSet<&'static str> {
        let mut active = BTreeSet::new();
        let mut group_winners: BTreeMap<&'static str, (usize, &'static PromptWorkflowSpec)> =
            BTreeMap::new();
        for (index, spec) in self.specs.iter().enumerate() {
            if !spec.requires.satisfied_by(facts) {
                continue;
            }
            let Some(group) = spec.exclusive_group else {
                active.insert(spec.id);
                continue;
            };
            let candidate = (index, spec);
            // Higher priority wins; equal priority keeps the spec declared
            // first, which is the order the former loop's `min_by_key`
            // produced.
            let better = group_winners
                .get(group)
                .is_none_or(|(winner_index, winner)| {
                    (spec.priority, std::cmp::Reverse(index))
                        > (winner.priority, std::cmp::Reverse(*winner_index))
                });
            if better {
                group_winners.insert(group, candidate);
            }
        }
        for (_, (_, spec)) in group_winners {
            active.insert(spec.id);
        }
        active
    }

    /// The fixed-point selection this resolver used to run, kept only as a
    /// test oracle: new behaviour must match it on every compiled surface
    /// combination.
    #[cfg(test)]
    pub(crate) fn resolve_legacy(
        &self,
        tools: &ResolvedToolCapabilities,
    ) -> Result<ResolvedPromptWorkflows> {
        self.validate()?;
        let facts = facts_from_capabilities(tools);
        let active = self.select_specs_legacy(&facts);
        let ordered = self
            .specs
            .iter()
            .filter(|spec| active.contains(spec.id))
            .collect::<Vec<_>>();
        let fingerprint = workflow_fingerprint(&ordered, &facts);
        Ok(ResolvedPromptWorkflows {
            ordered,
            facts,
            fingerprint,
        })
    }

    /// The former fixed-point loop, verbatim.
    #[cfg(test)]
    fn select_specs_legacy(&self, facts: &BTreeSet<PromptFact>) -> BTreeSet<&'static str> {
        let mut active = BTreeSet::new();
        let mut claimed_exclusive_groups = BTreeSet::new();
        loop {
            let eligible: Vec<(usize, &PromptWorkflowSpec)> = self
                .specs
                .iter()
                .enumerate()
                .filter(|(_, spec)| !active.contains(spec.id))
                .filter(|(_, spec)| {
                    spec.exclusive_group
                        .is_none_or(|group| !claimed_exclusive_groups.contains(group))
                })
                .filter(|(_, spec)| spec.requires.satisfied_by(facts))
                .collect();
            if eligible.is_empty() {
                break;
            }
            let mut grouped: BTreeMap<Option<&str>, Vec<(usize, &PromptWorkflowSpec)>> =
                BTreeMap::new();
            for candidate in eligible {
                grouped
                    .entry(candidate.1.exclusive_group)
                    .or_default()
                    .push(candidate);
            }
            let mut winners = Vec::new();
            for (group, candidates) in grouped {
                if group.is_none() {
                    winners.extend(candidates);
                } else {
                    let winner = candidates
                        .into_iter()
                        .min_by_key(|(index, spec)| (std::cmp::Reverse(spec.priority), *index))
                        .expect("exclusive group was nonempty");
                    winners.push(winner);
                }
            }
            winners.sort_by_key(|(index, _)| *index);
            let mut changed = false;
            for (_, spec) in winners {
                if active.insert(spec.id) {
                    if let Some(group) = spec.exclusive_group {
                        claimed_exclusive_groups.insert(group);
                    }
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        active
    }

    fn validate(&self) -> Result<()> {
        let mut ids = BTreeSet::new();
        for spec in self.specs {
            ensure!(ids.insert(spec.id), "duplicate workflow id '{}'", spec.id);
            match spec.requires {
                WorkflowRequirement::All(facts) | WorkflowRequirement::Any(facts) => {
                    ensure!(!facts.is_empty(), "workflow requirement cannot be empty");
                }
                WorkflowRequirement::AllWithAny { all, any } => {
                    ensure!(!all.is_empty() && !any.is_empty());
                }
            }
        }
        Ok(())
    }
}

impl ResolvedPromptWorkflows {
    pub fn has_fact(&self, fact: PromptFact) -> bool {
        self.facts.contains(&fact)
    }

    pub fn ordered(&self) -> &[&'static PromptWorkflowSpec] {
        &self.ordered
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

fn facts_from_capabilities(tools: &ResolvedToolCapabilities) -> BTreeSet<PromptFact> {
    let mut facts = BTreeSet::new();
    for (capability, binding) in tools.iter() {
        facts.insert(ToolCapability(*capability));
        if binding.primary.tier == ProviderTier::Specialized
            && binding
                .alternatives
                .iter()
                .any(|provider| provider.tier == ProviderTier::Fallback)
        {
            facts.insert(SpecializedWithFallback(*capability));
        }
    }
    facts
}

fn render_search_then_inspect(
    _: &PromptBuildContext,
    tools: &ResolvedToolCapabilities,
) -> Result<RenderedPromptPack> {
    let search = tools.primary_provider(ContentSearch).unwrap();
    let read = tools.primary_provider(PathRead).unwrap();
    Ok(RenderedPromptPack {
        content: include_str!("../assets/prompts/workflows/search_then_inspect.md")
            .replace("{{SEARCH_PROVIDER}}", search.tool.as_str())
            .replace("{{READ_PROVIDER}}", read.tool.as_str())
            .trim()
            .into(),
        referenced_tools: [search.tool.clone(), read.tool.clone()]
            .into_iter()
            .collect(),
        consumed_facts: SEARCH_FACTS.iter().copied().collect(),
    })
}

fn render_memory_recall(
    _: &PromptBuildContext,
    tools: &ResolvedToolCapabilities,
) -> Result<RenderedPromptPack> {
    let search = tools.primary_provider(ContentSearch).unwrap();
    let read = tools.primary_provider(PathRead).unwrap();
    Ok(RenderedPromptPack {
        content: include_str!("../assets/prompts/workflows/memory_recall.md")
            .replace("{{SEARCH_PROVIDER}}", search.tool.as_str())
            .replace("{{READ_PROVIDER}}", read.tool.as_str())
            .trim()
            .into(),
        referenced_tools: [search.tool.clone(), read.tool.clone()]
            .into_iter()
            .collect(),
        consumed_facts: SEARCH_FACTS.iter().copied().collect(),
    })
}

fn render_hashline_edit(
    context: &PromptBuildContext,
    tools: &ResolvedToolCapabilities,
) -> Result<RenderedPromptPack> {
    ensure!(
        context.edit_mode == crate::config::EditMode::Hashline,
        "hashline workflow/config mismatch"
    );
    let read = tools.primary_provider(EditableSnapshotRead).unwrap();
    let edit = tools.primary_provider(HashlineEdit).unwrap();
    let mut content = include_str!("../assets/prompts/workflows/hashline_edit.md")
        .replace("{{READ_PROVIDER}}", read.tool.as_str())
        .replace("{{EDIT_PROVIDER}}", edit.tool.as_str())
        .replace(
            "{{SEEN_LINE_MODE}}",
            if context.edit_enforce_seen_lines {
                "enabled"
            } else {
                "disabled"
            },
        );
    let mut referenced_tools: BTreeSet<_> =
        [read.tool.clone(), edit.tool.clone()].into_iter().collect();
    let mut consumed_facts: BTreeSet<_> = HASHLINE_EDIT_FACTS.iter().copied().collect();
    if let Some(search) = tools.primary_provider(ContentSearch) {
        content.push_str(&format!("\n{} results may also provide per-file hashline headers and mark only displayed match/context lines as seen.", search.tool));
        referenced_tools.insert(search.tool.clone());
        consumed_facts.insert(ToolCapability(ContentSearch));
    }
    if let Some(write) = tools.primary_provider(FileOverwrite) {
        content = content.replace(
            "{{LARGE_FILE_GUIDANCE}}",
            &format!(
                "- Prefer one {} rewrite for many large-file changes.",
                write.tool
            ),
        );
        referenced_tools.insert(write.tool.clone());
        consumed_facts.insert(ToolCapability(FileOverwrite));
    } else {
        content = content.replace(
            "{{LARGE_FILE_GUIDANCE}}",
            &format!("- Batch related changes into one {} call.", edit.tool),
        );
    }
    Ok(RenderedPromptPack {
        content: content.trim().into(),
        referenced_tools,
        consumed_facts,
    })
}

fn render_replace_edit(
    context: &PromptBuildContext,
    tools: &ResolvedToolCapabilities,
) -> Result<RenderedPromptPack> {
    ensure!(
        context.edit_mode == crate::config::EditMode::Replace,
        "replace workflow/config mismatch"
    );
    let read = tools.primary_provider(PathRead).unwrap();
    let edit = tools.primary_provider(ContentReplaceEdit).unwrap();
    Ok(RenderedPromptPack {
        content: include_str!("../assets/prompts/workflows/replace_edit.md")
            .replace("{{READ_PROVIDER}}", read.tool.as_str())
            .replace("{{EDIT_PROVIDER}}", edit.tool.as_str())
            .replace(
                "{{FUZZY_MODE}}",
                if context.edit_fuzzy_match {
                    "enabled"
                } else {
                    "disabled"
                },
            )
            .replace(
                "{{FUZZY_THRESHOLD}}",
                &format!("{:.3}", context.edit_fuzzy_threshold),
            )
            .trim()
            .into(),
        referenced_tools: [read.tool.clone(), edit.tool.clone()].into_iter().collect(),
        consumed_facts: REPLACE_EDIT_FACTS.iter().copied().collect(),
    })
}

fn render_specialized_routing(
    _: &PromptBuildContext,
    tools: &ResolvedToolCapabilities,
) -> Result<RenderedPromptPack> {
    let mut lines = Vec::new();
    let mut referenced_tools = BTreeSet::new();
    let mut consumed_facts = BTreeSet::new();
    for (capability, label) in [
        (PathRead, "path reading"),
        (ContentSearch, "content search"),
        (PathDiscovery, "path discovery"),
    ] {
        let Some(binding) = tools.binding(capability) else {
            continue;
        };
        let fallbacks: Vec<_> = binding
            .alternatives
            .iter()
            .filter(|provider| provider.tier == ProviderTier::Fallback)
            .collect();
        if binding.primary.tier != ProviderTier::Specialized || fallbacks.is_empty() {
            continue;
        }
        referenced_tools.insert(binding.primary.tool.clone());
        let names = fallbacks
            .iter()
            .map(|provider| {
                referenced_tools.insert(provider.tool.clone());
                provider.tool.as_str()
            })
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!(
            "- For {label}, prefer {}. Use {names} only when shell semantics are genuinely required.",
            binding.primary.tool
        ));
        consumed_facts.insert(SpecializedWithFallback(capability));
    }
    ensure!(
        !lines.is_empty(),
        "routing workflow activated without a routable binding"
    );
    Ok(RenderedPromptPack {
        content: format!(
            "{}\n{}",
            include_str!("../assets/prompts/workflows/specialized_provider_routing.md").trim(),
            lines.join("\n")
        ),
        referenced_tools,
        consumed_facts,
    })
}

fn render_specialized_mutation_routing(
    _: &PromptBuildContext,
    tools: &ResolvedToolCapabilities,
) -> Result<RenderedPromptPack> {
    let shell = tools.primary_provider(ShellExec).unwrap();
    let mut lines = Vec::new();
    let mut referenced_tools = BTreeSet::from([shell.tool.clone()]);
    let mut consumed_facts = BTreeSet::from([ToolCapability(ShellExec)]);

    if let Some(create) = tools.primary_provider(FileCreate)
        && create.tool != shell.tool
    {
        lines.push(format!(
            "- For file creation, use {} rather than {} redirection or heredocs.",
            create.tool, shell.tool
        ));
        referenced_tools.insert(create.tool.clone());
        consumed_facts.insert(ToolCapability(FileCreate));
    }
    if let Some(overwrite) = tools.primary_provider(FileOverwrite)
        && overwrite.tool != shell.tool
    {
        lines.push(format!(
            "- For full-file replacement, use {} rather than {} redirection or heredocs.",
            overwrite.tool, shell.tool
        ));
        referenced_tools.insert(overwrite.tool.clone());
        consumed_facts.insert(ToolCapability(FileOverwrite));
    }
    if let Some(edit) = tools.primary_provider(FileEdit)
        && edit.tool != shell.tool
    {
        lines.push(format!(
            "- For changes to existing file content, use {} with its resolved edit protocol rather than mutation commands through {} such as sed or awk.",
            edit.tool, shell.tool
        ));
        referenced_tools.insert(edit.tool.clone());
        consumed_facts.insert(ToolCapability(FileEdit));
    }
    ensure!(
        !lines.is_empty(),
        "mutation routing workflow activated without distinct specialized providers"
    );
    Ok(RenderedPromptPack {
        content: format!(
            "{}\n{}",
            include_str!("../assets/prompts/workflows/specialized_mutation_routing.md").trim(),
            lines.join("\n")
        ),
        referenced_tools,
        consumed_facts,
    })
}

fn render_python_routing(
    _: &PromptBuildContext,
    tools: &ResolvedToolCapabilities,
) -> Result<RenderedPromptPack> {
    let mut providers = Vec::new();
    let mut referenced_tools = BTreeSet::new();
    let mut consumed_facts = BTreeSet::new();
    for (capability, label) in [
        (HostPythonExec, "host execution"),
        (SandboxedPythonExec, "sandboxed execution"),
    ] {
        if let Some(provider) = tools.primary_provider(capability) {
            providers.push(format!("- {} provides {label}.", provider.tool));
            referenced_tools.insert(provider.tool.clone());
            consumed_facts.insert(ToolCapability(capability));
        }
    }
    Ok(RenderedPromptPack {
        content: format!(
            "{}\n{}",
            include_str!("../assets/prompts/workflows/python_execution_routing.md").trim(),
            providers.join("\n")
        ),
        referenced_tools,
        consumed_facts,
    })
}

fn render_plan_lifecycle(
    _context: &PromptBuildContext,
    tools: &ResolvedToolCapabilities,
) -> Result<RenderedPromptPack> {
    let draft = tools.primary_provider(PlanDraft).unwrap();
    let confirm = tools.primary_provider(PlanConfirm).unwrap();
    let clear = tools.primary_provider(PlanClear).unwrap();
    let referenced_tools: BTreeSet<_> =
        [draft.tool.clone(), confirm.tool.clone(), clear.tool.clone()]
            .into_iter()
            .collect();
    let consumed_facts: BTreeSet<_> = [
        ToolCapability(PlanDraft),
        ToolCapability(PlanConfirm),
        ToolCapability(PlanClear),
    ]
    .into_iter()
    .collect();
    let content = include_str!("../assets/prompts/workflows/plan_lifecycle.md")
        .replace("{{DRAFT_PROVIDER}}", draft.tool.as_str())
        .replace("{{CONFIRM_PROVIDER}}", confirm.tool.as_str())
        .replace("{{CLEAR_PROVIDER}}", clear.tool.as_str());
    Ok(RenderedPromptPack {
        content: content.trim().into(),
        referenced_tools,
        consumed_facts,
    })
}

fn render_todo_inspection(
    _: &PromptBuildContext,
    tools: &ResolvedToolCapabilities,
) -> Result<RenderedPromptPack> {
    let read = tools.primary_provider(TodoInspect).unwrap();
    Ok(RenderedPromptPack {
        content: include_str!("../assets/prompts/workflows/todo_inspection.md")
            .replace("{{TODO_READ_PROVIDER}}", read.tool.as_str())
            .trim()
            .into(),
        referenced_tools: [read.tool.clone()].into_iter().collect(),
        consumed_facts: TODO_INSPECT.iter().copied().collect(),
    })
}

fn render_todo_structure(
    _: &PromptBuildContext,
    tools: &ResolvedToolCapabilities,
) -> Result<RenderedPromptPack> {
    let read = tools.primary_provider(TodoInspect).unwrap();
    let write = tools.primary_provider(TodoStructureMutation).unwrap();
    Ok(RenderedPromptPack {
        content: include_str!("../assets/prompts/workflows/todo_structure.md")
            .replace("{{TODO_READ_PROVIDER}}", read.tool.as_str())
            .replace("{{TODO_WRITE_PROVIDER}}", write.tool.as_str())
            .trim()
            .into(),
        referenced_tools: [read.tool.clone(), write.tool.clone()]
            .into_iter()
            .collect(),
        consumed_facts: TODO_STRUCTURE.iter().copied().collect(),
    })
}

fn render_todo_progress(
    _: &PromptBuildContext,
    tools: &ResolvedToolCapabilities,
) -> Result<RenderedPromptPack> {
    let read = tools.primary_provider(TodoInspect).unwrap();
    let advance = tools.primary_provider(TodoProgressTransition).unwrap();
    Ok(RenderedPromptPack {
        content: include_str!("../assets/prompts/workflows/todo_progress.md")
            .replace("{{TODO_READ_PROVIDER}}", read.tool.as_str())
            .replace("{{TODO_ADVANCE_PROVIDER}}", advance.tool.as_str())
            .trim()
            .into(),
        referenced_tools: [read.tool.clone(), advance.tool.clone()]
            .into_iter()
            .collect(),
        consumed_facts: TODO_PROGRESS.iter().copied().collect(),
    })
}

fn workflow_fingerprint(ordered: &[&PromptWorkflowSpec], facts: &BTreeSet<PromptFact>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"mink-prompt-workflows-v1\0");
    for spec in ordered {
        hasher.update(spec.id.as_bytes());
        hasher.update(b"\0");
    }
    for fact in facts {
        hasher.update(format!("{fact:?}\0").as_bytes());
    }
    crate::capabilities::fingerprint::hex_lower(hasher.finalize())
}

pub(super) fn workflow_section_ids() -> impl Iterator<Item = &'static str> {
    WORKFLOWS.iter().map(|spec| spec.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FACT_A: &[PromptFact] = &[ToolCapability(ContentSearch)];
    const FACT_B: &[PromptFact] = &[ToolCapability(PathRead)];

    fn spec(
        id: &'static str,
        requires: WorkflowRequirement,
        exclusive_group: Option<&'static str>,
        priority: u16,
    ) -> PromptWorkflowSpec {
        PromptWorkflowSpec {
            id,
            requires,
            exclusive_group,
            priority,
            render: render_memory_recall,
        }
    }

    fn facts(items: &[PromptFact]) -> BTreeSet<PromptFact> {
        items.iter().copied().collect()
    }

    /// Test-only tables are leaked to satisfy the resolver's `'static` spec
    /// slice; the amount is a few structs per test.
    fn leak(specs: Vec<PromptWorkflowSpec>) -> &'static [PromptWorkflowSpec] {
        Box::leak(specs.into_boxed_slice())
    }

    #[test]
    fn single_pass_matches_the_fixed_point_selection() {
        let specs = leak(vec![
            spec(
                "group-first",
                WorkflowRequirement::All(FACT_A),
                Some("edit"),
                100,
            ),
            spec(
                "group-tie",
                WorkflowRequirement::All(FACT_A),
                Some("edit"),
                100,
            ),
            spec(
                "group-high",
                WorkflowRequirement::All(FACT_B),
                Some("edit"),
                120,
            ),
            spec(
                "group-low",
                WorkflowRequirement::All(FACT_A),
                Some("edit"),
                90,
            ),
            spec("plain", WorkflowRequirement::All(FACT_A), None, 100),
        ]);
        let resolver = PromptWorkflowResolver { specs };
        for available in [
            facts(&[]),
            facts(FACT_A),
            facts(FACT_B),
            facts(&[FACT_A[0], FACT_B[0]]),
        ] {
            assert_eq!(
                resolver.select_specs(&available),
                resolver.select_specs_legacy(&available),
                "selection differs for {available:?}"
            );
        }
        // A tie keeps the spec declared first; a higher priority wins outright.
        assert_eq!(
            resolver.select_specs(&facts(FACT_A)),
            BTreeSet::from(["group-first", "plain"])
        );
        assert_eq!(
            resolver.select_specs(&facts(&[FACT_A[0], FACT_B[0]])),
            BTreeSet::from(["group-high", "plain"])
        );
    }

    #[test]
    fn requirements_that_no_fact_satisfies_select_nothing() {
        let specs = leak(vec![spec(
            "needs",
            WorkflowRequirement::Any(FACT_A),
            None,
            100,
        )]);
        let resolver = PromptWorkflowResolver { specs };
        assert!(resolver.select_specs(&facts(FACT_B)).is_empty());
        // Any() and All() are both handled: one matching fact is enough here.
        assert_eq!(
            resolver.select_specs(&facts(FACT_A)),
            BTreeSet::from(["needs"])
        );
    }

    #[test]
    fn invalid_tables_fail_validation() {
        let duplicate = leak(vec![
            spec("same", WorkflowRequirement::All(FACT_A), None, 100),
            spec("same", WorkflowRequirement::All(FACT_B), None, 100),
        ]);
        assert!(
            PromptWorkflowResolver { specs: duplicate }
                .validate()
                .is_err()
        );
        let empty = leak(vec![spec(
            "empty",
            WorkflowRequirement::All(&[]),
            None,
            100,
        )]);
        assert!(PromptWorkflowResolver { specs: empty }.validate().is_err());
    }
}
