use crate::agent::prefix::PrefixManager;
use crate::context::AgentSharedContext;
use crate::llm::client::{LlmModelTarget, LlmPurpose, LlmRequest};
use anyhow::Result;
use std::sync::Arc;

pub struct TurnCompactor {
    ctx: Arc<AgentSharedContext>,
    prefix: PrefixManager,
    /// Compactions committed for the current user input. There is no per-input
    /// attempt cap: a request that does not fit must keep folding while a
    /// legal cut exists, and the caller stops only when compaction can no
    /// longer reduce the projection.
    compactions_this_turn: usize,
}

impl TurnCompactor {
    pub fn new(ctx: Arc<AgentSharedContext>, prefix: PrefixManager) -> Self {
        Self {
            ctx,
            prefix,
            compactions_this_turn: 0,
        }
    }

    pub fn reset(&mut self) {
        self.compactions_this_turn = 0;
    }

    pub fn compactions_this_turn(&self) -> usize {
        self.compactions_this_turn
    }

    /// One compaction attempt. Returns `(compacted, detail)` where `detail` is
    /// the engine's success summary or its skip reason, so callers can report
    /// why a request could not be reduced.
    pub async fn maybe_compact(
        &mut self,
        trigger: &str,
        messages: &mut Vec<serde_json::Value>,
        system_prompt: &mut String,
        tools_json: &mut Vec<serde_json::Value>,
        target: LlmModelTarget<'_>,
    ) -> Result<(bool, String)> {
        // Same projection as the real request: consumed image references
        // become text citations FIRST, so the compaction estimate counts
        // visual tokens only for the unconsumed batch — otherwise history
        // pictures would trigger premature compaction (they are never
        // re-expanded after consumption).
        let request_messages = crate::session::plan::project_full_request(messages)?;
        let local_tokens = crate::llm::transport::estimate_openai_context_tokens(
            &request_messages,
            tools_json,
            system_prompt,
        )?;
        let source_fingerprint =
            crate::session::compaction::prefix_fingerprint(system_prompt, tools_json);
        let projection_request = LlmRequest {
            purpose: LlmPurpose::Agent,
            model: target.model.to_string(),
            model_alias: target.alias.map(str::to_string),
            api_url: self.ctx.api_url.clone(),
            api_key: self.ctx.api_key().to_string(),
            system_prompt: system_prompt.clone(),
            messages: request_messages,
            tools: tools_json.clone(),
            max_tokens: crate::session::compaction::effective_max_tokens(&self.ctx.config),
            cancel: self.ctx.cancel.clone(),
            verbose: self.ctx.verbose(),
            display: self.ctx.display.clone(),
        };
        let current_projection = self
            .ctx
            .llm_backend
            .cache_projection(&projection_request, projection_request.messages.len());
        let (did_compact, detail) = self
            .ctx
            .compaction
            .evaluate_and_compact_with_prefix(
                trigger,
                local_tokens,
                target,
                Some(&source_fingerprint),
                current_projection.as_ref(),
                Some(self.compactions_this_turn + 1),
            )
            .await?;
        if did_compact {
            self.compactions_this_turn += 1;
            (*system_prompt, *tools_json) = self.prefix.ensure().await?;
            *messages = self.ctx.compaction.active_messages().await?;
            return Ok((true, detail));
        }
        Ok((false, detail))
    }
}

#[cfg(test)]
#[path = "compactor_tests.rs"]
mod tests;
