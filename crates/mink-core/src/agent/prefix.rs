use crate::context::AgentSharedContext;
use crate::session::prefix::ImmutablePrefix;
use anyhow::Result;
use std::sync::Arc;

#[derive(Clone)]
pub struct PrefixManager {
    ctx: Arc<AgentSharedContext>,
    allow_native_web: bool,
}

impl PrefixManager {
    pub fn new(ctx: Arc<AgentSharedContext>) -> Self {
        Self::with_native_web(ctx, true)
    }

    pub(super) fn with_native_web(ctx: Arc<AgentSharedContext>, allow_native_web: bool) -> Self {
        Self {
            ctx,
            allow_native_web,
        }
    }

    /// Dependency fingerprint of the current session capabilities.
    fn current_dependency_fingerprint(&self, web: &[serde_json::Value]) -> Result<String> {
        let workflows = crate::prompt::workflows::PromptWorkflowResolver::builtin()
            .resolve(&self.ctx.tools.tool_capabilities)?;
        Ok(self.dependency_fingerprint(&workflows, web))
    }

    fn dependency_fingerprint(
        &self,
        workflows: &crate::prompt::workflows::ResolvedPromptWorkflows,
        web: &[serde_json::Value],
    ) -> String {
        let mut fingerprint = format!(
            "mink-prefix-dependencies-v2\0{}\0{}\0{}\0{}\0{}",
            self.ctx.tools.capability_snapshot.dependency_fingerprint,
            self.ctx.tools.tool_surface.fingerprint(),
            self.ctx.tools.tool_capabilities.fingerprint(),
            workflows.fingerprint(),
            self.ctx.tools.model_capabilities.capability_fingerprint,
        );
        for tool in web {
            fingerprint.push('\0');
            fingerprint.push_str(tool["type"].as_str().unwrap_or_default());
        }
        fingerprint
    }

    fn web_tools(&self) -> Vec<serde_json::Value> {
        let options = self.ctx.config.web;
        if !self.allow_native_web
            || (!options.native_search && !options.native_read_url)
            || self.ctx.tools.tool_resolution_context.role()
                == crate::tools::surface::AgentRole::GoalReviewer
            || self
                .ctx
                .config
                .enabled_tools
                .as_ref()
                .is_some_and(Vec::is_empty)
            || self
                .ctx
                .stores
                .goal_store
                .snapshot()
                .is_some_and(|goal| goal.status == crate::session::goal::GoalStatus::Draft)
        {
            return Vec::new();
        }
        let model = self
            .ctx
            .active_model
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !self
            .ctx
            .llm_backend
            .supports_deepseek_web(&self.ctx.paths.api_url, &model.actual)
        {
            return Vec::new();
        }
        crate::llm::deepseek_web::declarations(options)
    }

    pub async fn ensure(&self) -> Result<(String, Vec<serde_json::Value>)> {
        if let Some(source) = &self.ctx.tools.prefix_source
            && let Some(prefix) = source.prefix(&self.ctx.paths.events_path)
        {
            return Ok(prefix);
        }

        // One async build gate: the std guard below is never held across the
        // critical-event await (the future must stay Send).
        let _build = self.ctx.io.prefix_build_lock.lock().await;
        let web = self.web_tools();

        {
            let mut guard = self
                .ctx
                .io
                .immutable_prefix
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(ref prefix) = *guard {
                let current_dependency = self.current_dependency_fingerprint(&web)?;
                if prefix.verify_fingerprint()
                    && prefix.dependency_fingerprint() == current_dependency
                {
                    return Ok((
                        prefix.system_prompt().to_string(),
                        prefix.tools_json().to_vec(),
                    ));
                }
                // Self-consistent but stale dependencies (capability change)
                // must rebuild instead of faking a cache hit.
                *guard = None;
            }
        }

        let system_prompt = self.ctx.build_system_prompt()?;
        // Static image-capability enhancement: when the frozen session
        // capability supports images, the Read description is augmented
        // here and cached inside ImmutablePrefix (v7 §3.4). Unsupported
        // sessions leave the schema byte-for-byte unchanged.
        let mut tools_json = crate::tools::surface::augment_read_schema_for_image(
            self.ctx.tools.tool_surface.schemas(),
            &self.ctx.tools.model_capabilities,
        );
        let workflows = crate::prompt::workflows::PromptWorkflowResolver::builtin()
            .resolve(&self.ctx.tools.tool_capabilities)?;
        self.ctx
            .log_event(crate::events::EventLog::PromptWorkflowResolution {
                active_workflows: workflows
                    .ordered()
                    .iter()
                    .map(|spec| spec.id.to_string())
                    .collect(),
                workflow_fingerprint: workflows.fingerprint().to_string(),
            });
        // The capability fingerprint joins the dependency fingerprint so
        // any capability change forces a prefix rebuild (v7 §3.4).
        let dependency_fingerprint = self.dependency_fingerprint(&workflows, &web);
        tools_json.extend(web);
        let prefix = ImmutablePrefix::new(
            system_prompt.clone(),
            tools_json.clone(),
            dependency_fingerprint.clone(),
        );
        // Contract evidence: only cache the prefix once the writer confirmed
        // the snapshot was actually written; a failure returns to the caller
        // and the next ensure() rebuilds instead of faking a cache hit.
        self.ctx
            .log_critical_event(crate::events::EventLog::PrefixSnapshot {
                version: Some(1),
                fingerprint: prefix.fingerprint().to_string(),
                dependency_fingerprint,
                system_prompt: system_prompt.clone(),
                tools_json: tools_json.clone(),
            })
            .await
            .map_err(|error| {
                // A cancelled or interrupted turn must stay classified as
                // interruption, not as an event-log failure.
                if self.ctx.cancel.is_cancelled() || self.ctx.interrupt.is_set() {
                    anyhow::Error::new(crate::agent::turn::TurnInterrupted)
                } else {
                    error
                }
            })?;
        {
            let mut guard = self
                .ctx
                .io
                .immutable_prefix
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *guard = Some(prefix);
        }
        Ok((system_prompt, tools_json))
    }

    #[cfg(test)]
    pub fn invalidate(&self) {
        *self
            .ctx
            .io
            .immutable_prefix
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
    }
}

#[cfg(test)]
#[path = "prefix_tests.rs"]
mod tests;
