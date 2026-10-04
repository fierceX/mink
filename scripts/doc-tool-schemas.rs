//! Export actual runtime schemas for documentation checks without an LLM call.
use mink::runtime::{AgentOptions, AgentRuntime, EditMode, PostInitContext, PostInitHook};
use std::path::PathBuf;
use std::sync::Arc;

struct Capture(PathBuf);
impl PostInitHook for Capture {
    fn run(&self, context: &PostInitContext<'_>) -> anyhow::Result<()> {
        std::fs::write(&self.0, serde_json::to_vec(context.tools())?)?;
        Ok(())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let root = PathBuf::from(std::env::args_os().nth(1).expect("output directory"));
    for (name, mode) in [
        ("hashline", EditMode::Hashline),
        ("replace", EditMode::Replace),
    ] {
        let runtime = AgentRuntime::start(
            AgentOptions::new(root.join(name), &root)
                .with_api_key("fixture-key")
                .with_edit_mode(mode)
                .with_post_init_hook(Arc::new(Capture(root.join(format!("{name}.json"))))),
        )
        .await?;
        runtime.shutdown().await?;
    }
    Ok(())
}
