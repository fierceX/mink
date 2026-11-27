use crate::llm::transport;
use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;

/// One final request's immutable projection.
///
/// Built once for each final request; compaction or an active-history change
/// invalidates it and forces a rebuild. Budget checks, compaction pressure and
/// transport conversion share this value; an attempt only clones `Arc`s and
/// replaces its cancel token.
pub(crate) struct RequestProjection {
    /// Runtime-shaped messages after the single-consumption image projection
    /// (consumed images are stable text citations).
    pub(crate) runtime_projected: Arc<Vec<Value>>,
    /// OpenAI-shaped messages converted once from `runtime_projected`.
    /// `tool_attachment` blocks are intentionally not materialized here: the
    /// budget estimate must keep counting their tile cost, not base64 bytes.
    pub(crate) openai_projected: Arc<Vec<Value>>,
    pub(crate) openai_tools: Arc<Vec<Value>>,
    pub(crate) runtime_tools: Arc<Vec<Value>>,
    pub(crate) system_prompt: Arc<str>,
    pub(crate) estimated_tokens: usize,
    /// Estimated cost of the same system/tools with an empty message list;
    /// overflow recovery uses it instead of re-estimating the prefix.
    pub(crate) fixed_tokens: usize,
    pub(crate) fingerprint: String,
    pub(crate) has_images: bool,
}

#[cfg(test)]
thread_local! {
    static BUILD_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_build_count() {
    BUILD_COUNT.with(|count| count.set(0));
}

#[cfg(test)]
pub(crate) fn build_count() -> usize {
    BUILD_COUNT.with(std::cell::Cell::get)
}

impl RequestProjection {
    pub(crate) fn build(messages: &[Value], system_prompt: &str, tools: &[Value]) -> Result<Self> {
        #[cfg(test)]
        BUILD_COUNT.with(|count| count.set(count.get() + 1));
        let mut runtime_projected =
            crate::llm::image_projection::project_consumed_attachments(messages).into_owned();
        super::deepseek_web::project_replay(&mut runtime_projected, tools);
        let openai_projected = Arc::new(transport::convert_messages_to_openai(
            &runtime_projected,
            tools,
        )?);
        let openai_tools = Arc::new(transport::convert_tools_to_openai(tools));
        let estimated_tokens =
            transport::estimate_openai_converted(&openai_projected, &openai_tools, system_prompt)?;
        let fixed_tokens = transport::estimate_openai_converted(&[], &openai_tools, system_prompt)?;
        let fingerprint = crate::session::compaction::prefix_fingerprint(system_prompt, tools);
        let has_images = runtime_projected.iter().any(|message| {
            message
                .get("content")
                .and_then(Value::as_array)
                .is_some_and(|blocks| {
                    blocks.iter().any(|block| {
                        block.get("type").and_then(Value::as_str) == Some("tool_attachment")
                    })
                })
        });
        Ok(Self {
            runtime_projected: Arc::new(runtime_projected),
            openai_projected,
            openai_tools,
            runtime_tools: Arc::new(tools.to_vec()),
            system_prompt: Arc::from(system_prompt),
            estimated_tokens,
            fixed_tokens,
            fingerprint,
            has_images,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn web_replay_is_excluded_from_the_projection_when_native_tools_are_off() {
        let basic = json!({"role":"assistant","content":[{"type":"text","text":"answer"}]});
        let mut with_replay = basic.clone();
        with_replay["_mink"]["deepseek_web"] = json!({
            "calls":[{"calls":[{"signature":"s".repeat(9000)}]}],
            "reasoning_content":""
        });
        let baseline = RequestProjection::build(&[basic], "", &[]).unwrap();
        let off = RequestProjection::build(&[with_replay.clone()], "", &[]).unwrap();
        let on =
            RequestProjection::build(&[with_replay], "", &[json!({"type":"deepseek_read_url"})])
                .unwrap();
        assert_eq!(off.estimated_tokens, baseline.estimated_tokens);
        assert!(off.openai_projected[0].get("server_tool_calls").is_none());
        assert!(off.runtime_projected[0].get("_mink").is_none());
        assert!(on.openai_projected[0]["server_tool_calls"].is_array());
    }

    #[test]
    fn projection_build_counter_is_observable() {
        reset_build_count();
        let _ = RequestProjection::build(&[], "", &[]).unwrap();
        assert_eq!(build_count(), 1);
    }

    #[test]
    fn projection_matches_legacy_budget_for_fresh_images() {
        let messages = vec![
            json!({"role":"user","content":"inspect"}),
            json!({"role":"assistant","content":[{"type":"text","text":"ok"}]}),
            json!({"role":"user","content":[
                {"type":"tool_attachment","tool_use_id":"c","url":"image://sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","format":"png","width":1024,"height":768,"bytes":1024}
            ]}),
        ];
        let tools =
            vec![json!({"name":"Read","description":"read","input_schema":{"type":"object"}})];
        let projection = RequestProjection::build(&messages, "system", &tools).unwrap();
        assert!(projection.has_images);
        assert_eq!(
            projection.estimated_tokens,
            crate::llm::transport::estimate_openai_context_tokens(&messages, &tools, "system")
                .unwrap(),
            "the round projection must not change the budget semantics"
        );
        assert_eq!(
            projection.fixed_tokens,
            crate::llm::transport::estimate_openai_context_tokens(&[], &tools, "system").unwrap()
        );
    }

    #[test]
    fn projection_consumes_historical_images_once() {
        let messages = vec![
            json!({"role":"user","content":[
                {"type":"tool_attachment","tool_use_id":"a","url":"image://sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","format":"png","width":1024,"height":768,"bytes":1024}
            ]}),
            json!({"role":"assistant","content":[{"type":"text","text":"seen"}]}),
            json!({"role":"user","content":"next"}),
        ];
        let projection = RequestProjection::build(&messages, "", &[]).unwrap();
        assert!(
            !projection.has_images,
            "consumed images must not materialize again"
        );
        assert!(
            !serde_json::to_string(&*projection.openai_projected)
                .unwrap()
                .contains("tool_attachment")
        );
    }
}
