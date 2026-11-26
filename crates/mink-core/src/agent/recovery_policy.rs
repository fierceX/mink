use crate::tools::catalog::ToolCatalog;
use crate::tools::runtime_guidance::RenderedRuntimeGuidance;
use crate::tools::semantic_capabilities::{
    CapabilityCallContext, CapabilityProvider, ResolvedToolCapabilities, ToolSemanticCapability,
    call_satisfies_capability,
};
use std::collections::BTreeSet;

#[derive(Debug, Clone)]
pub struct InspectionAction {
    pub capability: ToolSemanticCapability,
    pub provider: CapabilityProvider,
}

#[derive(Debug, Clone)]
pub struct RecoveryPolicy {
    inspection_actions: Vec<InspectionAction>,
    allow_run_code_inspection: bool,
}

pub enum RecoveryFirstCallDecision {
    Allowed,
    Blocked(RenderedRuntimeGuidance),
}

impl RecoveryPolicy {
    #[cfg(test)]
    pub fn from_resolved(resolved: &ResolvedToolCapabilities, catalog: &ToolCatalog) -> Self {
        Self::build(resolved, catalog, None)
    }

    pub fn from_resolved_for_surface(
        resolved: &ResolvedToolCapabilities,
        catalog: &ToolCatalog,
        surface: &crate::tools::surface::ModelToolSurface,
    ) -> Self {
        Self::build(resolved, catalog, Some(surface))
    }

    fn build(
        resolved: &ResolvedToolCapabilities,
        catalog: &ToolCatalog,
        surface: Option<&crate::tools::surface::ModelToolSurface>,
    ) -> Self {
        use ToolSemanticCapability::*;
        let mut seen = BTreeSet::new();
        let mut inspection_actions = Vec::new();
        let mut allow_run_code_inspection = false;
        let run_code_on_wire = surface.is_some_and(|s| s.has("RunCode"));
        for capability in [
            PathRead,
            ContentSearch,
            PathDiscovery,
            ResourceRead,
            ResourceSearch,
            FocusedVerificationExec,
        ] {
            let Some(provider) = resolved.primary_provider(capability).cloned() else {
                continue;
            };
            let Some(tool) = catalog.get(&provider.tool) else {
                continue;
            };
            let metadata = match &tool.availability {
                crate::tools::catalog::ToolBuildAvailability::Compiled { metadata } => metadata,
                crate::tools::catalog::ToolBuildAvailability::FeatureUnavailable { .. } => continue,
            };
            if metadata.mutating && capability != FocusedVerificationExec {
                continue;
            }
            if let Some(surface) = surface
                && !surface.has(&provider.tool)
            {
                if run_code_on_wire && surface.has_programmatic(&provider.tool) {
                    allow_run_code_inspection = true;
                }
                continue;
            }
            if seen.insert((provider.tool.clone(), capability)) {
                inspection_actions.push(InspectionAction {
                    capability,
                    provider,
                });
            }
        }
        Self {
            inspection_actions,
            allow_run_code_inspection,
        }
    }

    pub fn classify_first_call(
        &self,
        call: &CapabilityCallContext<'_>,
    ) -> RecoveryFirstCallDecision {
        if self.allow_run_code_inspection && call.tool_name == "RunCode" {
            return RecoveryFirstCallDecision::Allowed;
        }
        if self
            .inspection_actions
            .iter()
            .any(|action| call_satisfies_capability(action.capability, &action.provider, call))
        {
            return RecoveryFirstCallDecision::Allowed;
        }
        let mut referenced_tools: BTreeSet<_> = self
            .inspection_actions
            .iter()
            .map(|action| action.provider.tool.clone())
            .collect();
        if self.allow_run_code_inspection {
            referenced_tools.insert("RunCode".to_string());
        }
        let content = if referenced_tools.is_empty() {
            "SIGNAL_RECOVERY guard: no tool call is allowed because no active inspection provider is available. Report the limitation and stop the turn.".to_string()
        } else {
            format!(
                "SIGNAL_RECOVERY guard: this call was not executed because it does not satisfy an approved first-step inspection. Use one of: {}.",
                referenced_tools
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        RecoveryFirstCallDecision::Blocked(RenderedRuntimeGuidance {
            content,
            referenced_tools,
        })
    }
}

#[cfg(test)]
#[path = "recovery_policy_tests.rs"]
mod tests;
