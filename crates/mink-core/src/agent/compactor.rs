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

/// 候选投影的 todo 状态（快照 + 读 provider + 展示额度）。压缩候选会折走最新
/// revision，因此同步需求必须按**压缩后的候选**判断，而不是压缩前历史。
pub(crate) fn todo_candidate_state<'a>(
    ctx: &'a AgentSharedContext,
    snapshot: &'a crate::session::todo::TodoSnapshot,
) -> Option<crate::session::compaction::TodoCandidateState<'a>> {
    let read_provider = ctx.todo_read_provider()?;
    Some(crate::session::compaction::TodoCandidateState {
        snapshot,
        read_provider,
        allowance_tokens: crate::session::compaction::derived_display_tokens(&ctx.config),
    })
}

/// 折叠历史里真实出现、且仍存在于 artifact 索引中的引用（上限 8 条）。
/// 只列出可验证的 id：不创建虚构 URL，也不猜测被删除的 artifact。
fn existing_artifact_refs(ctx: &AgentSharedContext, messages: &[serde_json::Value]) -> Vec<String> {
    const MAX_REFS: usize = 8;
    let mut refs: Vec<String> = Vec::new();
    for message in messages {
        let text = serde_json::to_string(message).unwrap_or_default();
        let mut rest = text.as_str();
        while let Some(index) = rest.find("artifact://") {
            rest = &rest[index + "artifact://".len()..];
            let id: String = rest
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_')
                .collect();
            if id.is_empty() {
                continue;
            }
            if !refs.contains(&id) && ctx.artifacts.get(&id).is_ok() {
                refs.push(id);
                if refs.len() >= MAX_REFS {
                    return refs;
                }
            }
        }
    }
    refs
}

/// 应急 checkpoint 的共用入口（turn 与 manual 两个入口共用同一实现）：
/// 不调用 LLM，提交确定性有损摘录。返回是否提交。
pub(crate) async fn commit_emergency_checkpoint(
    ctx: &AgentSharedContext,
    reason: &str,
    main_request: crate::session::compaction::MainRequestShape<'_>,
    current_user_input: Option<&str>,
    target_limit: Option<usize>,
    messages: &[serde_json::Value],
) -> Result<bool> {
    let snapshot = ctx.todo_store.snapshot();
    let context = crate::session::compaction::EmergencyCheckpointContext {
        main_request,
        target_limit,
        current_user_input,
        todo: todo_candidate_state(ctx, &snapshot),
        artifact_refs: existing_artifact_refs(ctx, messages),
    };
    ctx.compaction
        .commit_emergency_checkpoint(reason, &context)
        .await
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
                // 主请求的真实形状：候选验收与真实发送使用同一套 system/tools。
                Some(crate::session::compaction::MainRequestShape {
                    system_prompt,
                    tools: tools_json,
                }),
                todo_candidate_state(&self.ctx, &self.ctx.todo_store.snapshot()),
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
