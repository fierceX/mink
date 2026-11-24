use super::*;

#[derive(Debug, Clone)]
pub(super) struct LatestAgentRequest {
    pub(super) model: String,
    pub(super) source_fingerprint: String,
    pub(super) local_tokens: usize,
    pub(super) projection_generation: u64,
    pub(super) backend_name: String,
    pub(super) source_system_prompt: Arc<str>,
    pub(super) source_tools: Arc<Vec<Value>>,
    pub(super) projection: Option<CacheProjectionSnapshot>,
}

#[derive(Debug, Clone)]
pub(super) struct PromptUsageBaseline {
    pub(super) request: Arc<LatestAgentRequest>,
    pub(super) provider_prompt_tokens: usize,
}

#[derive(Debug, Default)]
pub(super) struct PromptUsageState {
    pub(super) latest_request: Option<Arc<LatestAgentRequest>>,
    pub(super) baseline: Option<PromptUsageBaseline>,
}

/// Long-lived provider projection metadata. Message bodies are represented by
/// hashes so request-time image data URLs can never survive across requests.
#[derive(Debug, Clone)]
pub(super) struct CacheProjectionSnapshot {
    pub(super) model: String,
    pub(super) system_prompt: Arc<str>,
    pub(super) tools: Arc<Vec<Value>>,
    pub(super) message_hashes: Arc<Vec<[u8; 32]>>,
}

impl CompactionEngine {
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record_agent_request(
        &self,
        model: &str,
        source_fingerprint: &str,
        local_tokens: usize,
        backend_name: String,
        source_system_prompt: Arc<str>,
        source_tools: Arc<Vec<Value>>,
        projection: Option<LlmCacheProjection>,
    ) {
        let hashes = projection
            .as_ref()
            .map(|projection| Arc::new(message_hashes(&projection.messages)))
            .unwrap_or_else(|| Arc::new(Vec::new()));
        self.record_agent_request_with_hashes(
            model,
            source_fingerprint,
            local_tokens,
            backend_name,
            source_system_prompt,
            source_tools,
            hashes,
            projection,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record_agent_request_with_hashes(
        &self,
        model: &str,
        source_fingerprint: &str,
        local_tokens: usize,
        backend_name: String,
        source_system_prompt: Arc<str>,
        source_tools: Arc<Vec<Value>>,
        message_hashes: Arc<Vec<[u8; 32]>>,
        projection: Option<LlmCacheProjection>,
    ) {
        let request = Arc::new(LatestAgentRequest {
            model: model.to_string(),
            source_fingerprint: source_fingerprint.to_string(),
            local_tokens,
            projection_generation: self.projection_generation.load(Ordering::SeqCst),
            backend_name,
            source_system_prompt,
            source_tools,
            projection: projection.map(|projection| {
                CacheProjectionSnapshot::from_projection(projection, message_hashes)
            }),
        });
        self.prompt_usage
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .latest_request = Some(request);
    }

    pub(crate) fn record_agent_usage(&self, usage: &UsageEvent) {
        if usage.input_tokens < 0
            || usage.cache_read_input_tokens < 0
            || usage.cache_creation_input_tokens < 0
        {
            return;
        }
        let Some(provider_prompt_tokens) = usage
            .input_tokens
            .checked_add(usage.cache_read_input_tokens)
            .and_then(|value| value.checked_add(usage.cache_creation_input_tokens))
            .and_then(|value| usize::try_from(value).ok())
        else {
            return;
        };
        let mut state = self
            .prompt_usage
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(request) = state.latest_request.clone() else {
            return;
        };
        if request.projection_generation != self.projection_generation.load(Ordering::SeqCst) {
            return;
        }
        state.baseline = Some(PromptUsageBaseline {
            request,
            provider_prompt_tokens,
        });
    }

    pub(crate) fn clear_prompt_usage(&self) {
        *self
            .prompt_usage
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = PromptUsageState::default();
    }
}

pub(crate) fn prefix_fingerprint(system_prompt: &str, tools: &[Value]) -> String {
    crate::session::prefix::ImmutablePrefix::compute_fingerprint(system_prompt, tools, None)
}

pub(super) fn provider_projection_extends(
    previous: &CacheProjectionSnapshot,
    current: &LlmCacheProjection,
) -> bool {
    previous.model == current.model
        && previous.system_prompt == current.system_prompt
        && previous.tools == current.tools
        && previous.message_hashes.len() <= current.messages.len()
        && previous
            .message_hashes
            .iter()
            .zip(message_hashes(&current.messages))
            .all(|(left, right)| left == &right)
}

impl CacheProjectionSnapshot {
    pub(super) fn from_projection(
        projection: LlmCacheProjection,
        message_hashes: Arc<Vec<[u8; 32]>>,
    ) -> Self {
        Self {
            model: projection.model,
            system_prompt: projection.system_prompt,
            tools: projection.tools,
            message_hashes,
        }
    }
}
