//! Run an explicitly bounded goal. Requires a configured provider key.
use mink::runtime::{AgentOptions, AgentRuntime, GoalSpec};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let runtime = AgentRuntime::start(
        AgentOptions::new("/tmp/mink-goal-example", ".")
            .with_api_key(std::env::var("DEEPSEEK_API_KEY")?),
    )
    .await?;
    let handle = runtime.handle();
    let goals = handle.goals();
    let mut changes = goals.subscribe();
    let initial = changes.borrow_and_update().clone();
    println!("Initial goal: {initial:?}");
    // Enable scheduling; only explicit confirmation starts the run.
    goals.start_driver()?;
    let _draft = goals.request_draft(
        GoalSpec::new("Inspect this project and save findings to findings.md"),
        3600,
    )?;
    let draft = goals.generate_draft("").await?;
    println!(
        "Confirm draft {} revision {}:\n{}",
        draft.id,
        draft.revision,
        serde_json::to_string_pretty(&draft.spec)?
    );
    println!("Type the exact revision to confirm (anything else cancels):");
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if answer.trim() != draft.revision.to_string() || !draft.draft_ready {
        goals.cancel_draft(&draft.id, draft.revision)?;
        runtime.shutdown().await?;
        return Ok(());
    }
    goals.confirm(&draft.id, draft.revision)?;
    let outcome = goals.wait().await?;
    println!(
        "Goal {:?}; completion basis {:?}",
        outcome.status, outcome.completion_basis
    );
    runtime.shutdown().await?;
    Ok(())
}
