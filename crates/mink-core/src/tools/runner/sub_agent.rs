use super::*;

pub struct SubAgentTool;

impl ToolExec for SubAgentTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::new("SubAgent", ApprovalTier::Exec, ToolResultKind::SubAgent)
            .storm_exempt()
            .spawns_sub_agent()
    }

    fn execute(
        &self,
        input: &serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolOutcome> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            prompt: String,
            description: Option<String>,
            fork: Option<bool>,
            model: Option<String>,
        }
        let args: Args = crate::tools::args::decode_args(input)?;
        if args.prompt.trim().is_empty() {
            bail!("Error: sub-agent prompt is required");
        }
        // Description/fork are declared in the schema; accept them so valid
        // calls are not rejected by the runtime contract.
        let _ = args.description;
        let _ = args.fork;
        let _ = args.model;
        Ok(ToolOutcome::text(String::new()))
    }
}
