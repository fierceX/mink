use anyhow::{Result, anyhow};
use mink::runtime::{
    AgentOptions, AgentRuntime, LlmBackend, LlmEvent, LlmRequest, LlmResponseStream, LlmStopEvent,
    LlmTextEvent, SessionPolicy,
};
use mink_goal::{GoalEngine, GoalPlugin, GoalSpec, GoalStatus};
use std::path::PathBuf;
use std::sync::Arc;

struct EmptyBackend;

#[async_trait::async_trait]
impl LlmBackend for EmptyBackend {
    fn name(&self) -> &str {
        "empty"
    }

    async fn stream(&self, _request: LlmRequest) -> Result<LlmResponseStream> {
        Ok(LlmResponseStream {
            events: Box::pin(futures::stream::iter(vec![
                Ok(LlmEvent::Text(LlmTextEvent {
                    content: "ok".into(),
                })),
                Ok(LlmEvent::Stop(LlmStopEvent {
                    reason: "end_turn".into(),
                })),
            ])),
            attempt_count: 1,
        })
    }
}

fn temp_pair(name: &str) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("mink-goal-inv-{}-{name}", std::process::id()));
    let home = root.join("home");
    let cwd = root.join("cwd");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    (home, cwd)
}

/// Goal 契约是持久快照上的 compare-and-set：陈旧 revision 不能确认契约，重开
/// 会话后恢复的是同一契约身份与 revision，而不是一份新草案。
#[tokio::test]
async fn goal_contract_revision_is_a_cas_and_survives_a_reopen() -> Result<()> {
    let (home, cwd) = temp_pair("goal-recovery");
    let start_runtime = |policy: Option<SessionPolicy>| async {
        let plugin = GoalPlugin::new();
        let mut options =
            AgentOptions::new(home.clone(), cwd.clone()).with_llm_backend(Arc::new(EmptyBackend));
        if let Some(policy) = policy {
            options = options.with_session(policy);
        }
        plugin.install_into(&mut options);
        let runtime = AgentRuntime::start(options)
            .await
            .map_err(|e| anyhow!("start: {e}"))?;
        let goals =
            GoalEngine::attach(runtime.handle(), plugin).map_err(|e| anyhow!("attach: {e}"))?;
        Ok::<_, anyhow::Error>((runtime, goals))
    };
    let (runtime, goals) = start_runtime(None).await?;
    let draft = goals
        .draft(GoalSpec::new("ship the goal"), 3_600)
        .map_err(|e| anyhow!("draft: {e}"))?;
    assert!(
        goals.confirm(&draft.id, draft.revision + 1).is_err(),
        "a stale revision must not confirm the contract"
    );
    let confirmed = goals
        .confirm(&draft.id, draft.revision)
        .map_err(|e| anyhow!("confirm: {e}"))?;
    assert_eq!(confirmed.status, GoalStatus::Active);
    let sid = runtime.session_info().session_id.clone();
    runtime
        .shutdown()
        .await
        .map_err(|e| anyhow!("shutdown: {e}"))?;

    let (runtime, goals) = start_runtime(Some(SessionPolicy::Resume(sid))).await?;
    let restored = goals
        .snapshot()
        .ok_or_else(|| anyhow!("goal missing after reopen"))?;
    assert_eq!(restored.id, confirmed.id);
    assert_eq!(restored.contract_revision, confirmed.contract_revision);
    assert_eq!(restored.spec, confirmed.spec);
    assert_eq!(restored.status, GoalStatus::Active);
    // The recovery pass republishes the snapshot, so the CAS token advances: a
    // caller still holding the pre-reopen revision is refused instead of acting
    // on a state it no longer describes, and the restored revision works.
    assert!(
        goals
            .update_limits(
                &restored.id,
                confirmed.revision,
                restored.spec.limits.clone()
            )
            .is_err(),
        "a revision from before the reopen must be stale"
    );
    let adjusted = goals
        .update_limits(
            &restored.id,
            restored.revision,
            restored.spec.limits.clone(),
        )
        .map_err(|e| anyhow!("limits: {e}"))?;
    assert_eq!(adjusted.status, GoalStatus::Active);
    runtime
        .shutdown()
        .await
        .map_err(|e| anyhow!("shutdown: {e}"))?;
    let _ = std::fs::remove_dir_all(home.parent().unwrap());
    Ok(())
}
