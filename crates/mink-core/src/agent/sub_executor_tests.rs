use super::*;
use crate::llm::mock::MockLlmBackend;
use crate::protocol::{Event, StopEvent, TextEvent};

#[tokio::test]
async fn child_inherits_resource_session_but_has_own_agent_session() {
    let mut parent = crate::regression::test_context_for_agent("sub-vfs-scope")
        .await
        .unwrap();
    Arc::get_mut(&mut parent)
        .expect("test context should be uniquely owned")
        .vfs_scope
        .resource_session_id = "tenant-knowledge".into();
    parent
        .todo_store
        .apply_structure(
            0,
            crate::session::todo::TodoChanges {
                add: vec![crate::session::todo::TodoAdd {
                    content: "parent-only task".into(),
                }],
                ..Default::default()
            },
        )
        .unwrap();

    let parent_snapshot = parent.capability_snapshot.clone();
    let config = parent.config.clone();
    let child = SubAgentExecutor::new(parent, "sub-agent-session".into(), false, config)
        .await
        .unwrap();
    assert_eq!(
        child.child_ctx.vfs_scope.resource_session_id,
        "tenant-knowledge"
    );
    assert_eq!(
        child.child_ctx.vfs_scope.agent_session_id,
        "sub-agent-session"
    );
    assert!(Arc::ptr_eq(
        &child.child_ctx.capability_snapshot,
        &parent_snapshot
    ));
    assert_eq!(child.child_ctx.session_layout, SessionLayout::Isolated);
    assert!(
        child
            .child_ctx
            .home
            .ends_with("subagents/sub-agent-session")
    );
    assert!(child.child_store.lines().await.unwrap().is_empty());
    assert!(child.child_ctx.todo_store.snapshot().items.is_empty());
}

#[tokio::test]
async fn child_rejects_session_id_path_escape() {
    let parent = crate::regression::test_context_for_agent("sub-invalid-id")
        .await
        .unwrap();
    let config = parent.config.clone();
    let error = SubAgentExecutor::new(parent, "../escape".into(), false, config)
        .await
        .err()
        .expect("path-like session id must fail");
    assert!(error.to_string().contains("invalid sub-agent session id"));
}

#[tokio::test]
async fn concurrent_children_idempotently_create_shared_parent() -> anyhow::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "mink-subagent-parent-race-{}-{}",
        std::process::id(),
        NEXT_DIR.fetch_add(1, Ordering::Relaxed)
    ));
    let parent = root.join("parent-session");
    tokio::fs::create_dir_all(&parent).await?;
    let barrier = Arc::new(tokio::sync::Barrier::new(8));
    let mut tasks = tokio::task::JoinSet::new();

    for index in 0..8 {
        let parent = parent.clone();
        let barrier = barrier.clone();
        tasks.spawn(async move {
            let child = parent.join("subagents").join(format!("child-{index}"));
            barrier.wait().await;
            prepare_child_home(&parent, &child, false).await?;
            anyhow::Ok(child)
        });
    }

    while let Some(result) = tasks.join_next().await {
        let child = result??;
        assert!(child.is_dir());
    }
    let metadata = tokio::fs::symlink_metadata(parent.join("subagents")).await?;
    assert!(metadata.is_dir());
    assert!(!metadata.file_type().is_symlink());
    tokio::fs::remove_dir_all(root).await?;
    Ok(())
}

#[tokio::test]
async fn fork_inherits_full_history_and_compacted_projection() -> anyhow::Result<()> {
    let summary_backend = Arc::new(MockLlmBackend::new(
            "summary-model",
            vec![vec![
                Ok(Event::Text(TextEvent {
                    content: "Task focus: fork\nLatest request: inherit\nProgress: compacted\nTool evidence: none\nReflections: none".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ]],
        ));
    let parent = crate::regression::test_context_for_agent_with_config_and_backend(
        "sub-fork-context-state",
        |config| {
            config.max_context_tokens = 64_000;
            config.context_reserve_tokens = 12_000;
            config.context_compact_tail_tokens = 4_000;
            config.context_compact_max_output_tokens = 2_048;
        },
        summary_backend,
    )
    .await?;
    for index in 0..3 {
        parent
            .store
            .add_user(&format!("request {index}: {}", "x".repeat(6_000)))
            .await?;
        parent
            .store
            .add_assistant(&format!("progress {index}: {}", "y".repeat(6_000)), "", &[])
            .await?;
    }
    let full_history = parent.store.lines().await?;
    let resolved = crate::config::model_resolver(&parent.config).resolve(&parent.config.model);
    assert!(
        parent
            .compaction
            .evaluate_and_compact(
                "manual",
                0,
                crate::llm::client::LlmModelTarget::new(
                    &resolved.actual,
                    resolved.alias.as_deref(),
                ),
            )
            .await?
            .0
    );
    let projected = parent.compaction.active_messages().await?;
    let parent_dir = parent.store.path().parent().unwrap().to_path_buf();
    let inherited_artifact =
        parent
            .artifacts
            .write_text("Bash", "parent output", "parent artifact content")?;
    tokio::fs::write(parent_dir.join("future-state.bin"), b"preserved").await?;
    tokio::fs::write(
        parent_dir.join("stats.json"),
        r#"{"total_input_tokens":999,"total_output_tokens":777}"#,
    )
    .await?;
    parent.todo_store.apply_structure(
        0,
        crate::session::todo::TodoChanges {
            add: vec![crate::session::todo::TodoAdd {
                content: "inherited task".into(),
            }],
            ..Default::default()
        },
    )?;
    let parent_todos = parent
        .todo_store
        .advance(
            1,
            crate::session::todo::TodoTransitions {
                activate: vec!["T0001".into()],
                ..Default::default()
            },
        )?
        .snapshot;

    let config = parent.config.clone();
    let child = SubAgentExecutor::new(parent, "sub-fork-state".into(), true, config).await?;
    assert_eq!(child.child_store.lines().await?, full_history);
    assert_eq!(
        child.child_ctx.compaction.active_messages().await?,
        projected
    );
    assert_eq!(child.child_ctx.todo_store.snapshot(), parent_todos);
    assert_eq!(
        tokio::fs::read(child.child_ctx.home.join("future-state.bin")).await?,
        b"preserved"
    );
    let usage = child.child_ctx.stats.snapshot().await;
    assert_eq!(usage.total_input_tokens, 0);
    assert_eq!(usage.total_output_tokens, 0);
    assert_eq!(
        child
            .child_ctx
            .artifacts
            .read_text(&inherited_artifact.id)?,
        "parent artifact content"
    );
    let child_artifact =
        child
            .child_ctx
            .artifacts
            .write_text("Bash", "child output", "child artifact content")?;
    assert_ne!(child_artifact.id, inherited_artifact.id);
    assert_eq!(
        child
            .child_ctx
            .artifacts
            .read_text(&inherited_artifact.id)?,
        "parent artifact content"
    );
    Ok(())
}

#[tokio::test]
async fn sub_agent_interrupt_is_shared_and_child_cancel_is_linked() -> anyhow::Result<()> {
    use std::sync::atomic::Ordering;

    let parent = crate::regression::test_context_for_agent("sub-interrupt-sharing").await?;
    let config = parent.config.clone();
    let child =
        SubAgentExecutor::new(parent.clone(), "sub-interrupt-child".into(), false, config).await?;

    // Same task tree: interrupt is one shared flag (no "isolation" allowed).
    assert!(Arc::ptr_eq(&parent.interrupt, &child.child_ctx.interrupt));
    parent.interrupt.store(true, Ordering::SeqCst);
    assert!(child.child_ctx.interrupt.load(Ordering::SeqCst));

    // Cancel is a linked child token: the child's own cancel must not leak
    // upward, while a parent cancel propagates downward.
    assert!(!parent.cancel.is_cancelled());
    child.child_ctx.cancel.cancel();
    assert!(
        !parent.cancel.is_cancelled(),
        "child-local cancel must not affect the parent"
    );
    parent.cancel.cancel();
    assert!(
        child.child_ctx.cancel.is_cancelled(),
        "parent cancel must propagate to the child"
    );
    Ok(())
}

#[test]
fn sub_agent_status_maps_to_strings_and_tool_status() {
    use crate::tools::metadata::{ToolFailureKind, ToolStatus};

    let cases = [
        (SubAgentStatus::Succeeded, "ok", ToolStatus::Succeeded),
        (
            SubAgentStatus::Failed,
            "failed",
            ToolStatus::Failed(ToolFailureKind::Unknown),
        ),
        (
            SubAgentStatus::Interrupted,
            "cancelled",
            ToolStatus::Interrupted,
        ),
        (
            SubAgentStatus::TimedOut,
            "timed_out",
            ToolStatus::Failed(ToolFailureKind::Timeout),
        ),
    ];
    for (status, text, tool_status) in cases {
        assert_eq!(status.as_str(), text);
        assert_eq!(status.to_tool_status(), tool_status);
    }
}

#[tokio::test]
async fn plan_store_is_stable_per_session_and_isolated_for_child() -> anyhow::Result<()> {
    let parent = crate::regression::test_context_for_agent("plan-store-identity").await?;
    let first = crate::context::ToolContext::from(parent.as_ref());
    let second = crate::context::ToolContext::from(parent.as_ref());
    assert!(
        Arc::ptr_eq(&first.plan_store, &second.plan_store),
        "same session must share one PlanStore/transition lock"
    );

    let child = SubAgentExecutor::new(
        parent.clone(),
        "plan-store-child".into(),
        false,
        parent.config.clone(),
    )
    .await?;
    let child_tools = crate::context::ToolContext::from(child.child_ctx.as_ref());
    assert!(
        !Arc::ptr_eq(&first.plan_store, &child_tools.plan_store),
        "sub-agent must own its PlanStore"
    );
    Ok(())
}

/// A child that forks the parent's history must keep the parent's
/// `<environment>` heading, otherwise the child's request head differs from the
/// parent's and the inherited history cannot reuse the provider's cached
/// prefix (the head is compared from the first token).
#[tokio::test]
async fn child_environment_heading_matches_the_parent_for_prefix_reuse() -> anyhow::Result<()> {
    let backend = Arc::new(MockLlmBackend::new("env-model", vec![]));
    let parent = crate::regression::test_context_for_agent_with_config_and_backend(
        "sub-environment-home",
        |_| {},
        backend,
    )
    .await?;
    parent.store.add_user("parent turn").await?;

    let parent_prompt = parent.build_system_prompt()?;
    let child = SubAgentExecutor::new(
        parent.clone(),
        "sub-env-head".into(),
        true,
        parent.config.clone(),
    )
    .await?;
    let child_prompt = child.child_ctx.build_system_prompt()?;

    // The child really does run in its own isolated home...
    assert!(
        child.child_ctx.home.ends_with("subagents/sub-env-head"),
        "{}",
        child.child_ctx.home.display()
    );
    // ...but the model-visible environment line must be the parent's.
    let parent_env = parent_prompt
        .split_once("<environment>\n")
        .and_then(|(_, rest)| rest.split_once("\n</environment>"))
        .map(|(block, _)| block.to_string())
        .expect("parent environment block");
    let child_env = child_prompt
        .split_once("<environment>\n")
        .and_then(|(_, rest)| rest.split_once("\n</environment>"))
        .map(|(block, _)| block.to_string())
        .expect("child environment block");
    assert_eq!(claim_home(&parent_env), claim_home(&child_env));
    assert_eq!(parent_env, child_env);

    // The shared head therefore extends past the environment section.
    let common = parent_prompt
        .chars()
        .zip(child_prompt.chars())
        .take_while(|(left, right)| left == right)
        .count();
    let environment_end = parent_prompt
        .find("</environment>")
        .expect("environment section ends");
    assert!(
        common > environment_end,
        "shared head {common} must pass the environment section ({environment_end})"
    );
    Ok(())
}

fn claim_home(block: &str) -> String {
    block
        .lines()
        .find_map(|line| line.strip_prefix("home: "))
        .unwrap_or_default()
        .to_string()
}

#[test]
fn terminal_diagnostic_is_bounded_and_control_free() {
    let setup = format!("sub_agent_setup_error: {} \u{0007}", "x".repeat(5000));
    let result = SubAgentResult::terminal(
        SubAgentStatus::Failed,
        SubAgentStopReason::Error,
        String::new(),
        Some(setup),
    );
    let diagnostic = result.diagnostic.expect("setup diagnostic");
    assert!(diagnostic.starts_with("sub_agent_setup_error:"));
    assert!(diagnostic.len() <= 4096, "len={}", diagnostic.len());
    assert!(!diagnostic.contains('\u{0007}'));
    assert!(diagnostic.ends_with("… [truncated]"));

    let panic = format!("sub_agent_error: task panicked: {}", "é".repeat(3000));
    let result = SubAgentResult::terminal(
        SubAgentStatus::Failed,
        SubAgentStopReason::Error,
        String::new(),
        Some(panic),
    );
    let diagnostic = result.diagnostic.expect("panic diagnostic");
    assert!(diagnostic.starts_with("sub_agent_error: task panicked:"));
    assert!(diagnostic.len() <= 4096);
    assert!(diagnostic.is_char_boundary(diagnostic.len()));
}

#[test]
fn display_output_is_bounded_with_head_and_tail() {
    let long = format!("{}{}", "A".repeat(20_000), "B".repeat(20_000));
    let bounded = bounded_display(&long);
    assert!(bounded.len() <= 32 * 1024, "len={}", bounded.len());
    assert!(bounded.contains("… [truncated]"));
    assert!(bounded.starts_with('A'));
    assert!(bounded.ends_with('B'));
    assert!(bounded.is_char_boundary(bounded.len()));

    let multibyte = "é".repeat(30_000);
    let bounded = bounded_display(&multibyte);
    assert!(bounded.len() <= 32 * 1024);
    assert!(!bounded.contains('\u{FFFD}'));
    assert!(bounded.is_char_boundary(bounded.len()));
}

#[test]
fn legacy_string_assistant_content_is_extracted() {
    let (thinking, text) = extract_assistant_content(&serde_json::json!("legacy answer"));
    assert!(thinking.is_empty());
    assert_eq!(text, "legacy answer");
}

#[test]
fn bounded_diagnostic_strips_controls_and_truncates_on_char_boundaries() {
    let cleaned = bounded_diagnostic("a\u{0007}b\r\nc\td");
    assert_eq!(cleaned, "ab\nc\td");

    let long = "é".repeat(4000);
    let bounded = bounded_diagnostic(&long);
    assert!(bounded.len() <= 4096, "len={}", bounded.len());
    assert!(bounded.ends_with("… [truncated]"));
    assert!(bounded.is_char_boundary(bounded.len()));
    assert!(!bounded.contains('\u{FFFD}'));

    // The marker itself counts toward the 4096-byte limit.
    let exact = "x".repeat(4096);
    assert_eq!(bounded_diagnostic(&exact).len(), 4096);
    let over = "x".repeat(4097);
    let truncated = bounded_diagnostic(&over);
    assert_eq!(truncated.len(), 4096);
    assert!(truncated.ends_with("… [truncated]"));
}

#[test]
fn assistant_text_keeps_every_text_block_and_ignores_empty_text() {
    let content = vec![
        serde_json::json!({"type":"thinking","thinking":"plan"}),
        serde_json::json!({"type":"text","text":"one"}),
        serde_json::json!({"type":"text","text":""}),
        serde_json::json!({"type":"tool_use","id":"t1"}),
        serde_json::json!({"type":"text","text":"two"}),
    ];
    let (thinking, text) = extract_assistant_text(&content);
    assert_eq!(thinking, "plan");
    assert_eq!(text, "onetwo");
}

#[test]
fn blank_success_text_falls_back_to_streamed_text_then_no_output() {
    assert_eq!(
        resolve_success_text(String::new(), "streamed".into()),
        "streamed"
    );
    assert_eq!(
        resolve_success_text("  ".into(), "  ".into()),
        "(no output)"
    );
    assert_eq!(
        resolve_success_text("answer".into(), "streamed".into()),
        "answer"
    );
}

#[tokio::test]
async fn setup_timeout_removes_the_partial_child_home() -> anyhow::Result<()> {
    let parent = crate::regression::test_context_for_agent("sub-setup-timeout").await?;
    let child_home = parent
        .store
        .path()
        .parent()
        .expect("parent session dir")
        .join("subagents")
        .join("slow_setup_child");
    let cancel = parent.cancel.linked_child_token();
    let outcome = tokio::time::timeout(
        tokio::time::Duration::from_millis(300),
        SubAgentExecutor::new_child(
            parent.clone(),
            "slow_setup_child".into(),
            false,
            parent.config.clone(),
            cancel,
            None,
            None,
            SubAgentOrigin::SubAgent,
        ),
    )
    .await;
    assert!(outcome.is_err(), "the injected slow setup must time out");
    assert!(
        !child_home.exists(),
        "a timed-out setup must remove its partial child home: {}",
        child_home.display()
    );
    Ok(())
}

#[tokio::test]
async fn child_without_label_uses_colon_id_title_fallback() -> anyhow::Result<()> {
    let parent = crate::regression::test_context_for_agent("sub-title-fallback").await?;
    let config = parent.config.clone();
    let cancel = parent.cancel.linked_child_token();
    let child = SubAgentExecutor::new_child(
        parent,
        "sub-title-fallback".into(),
        false,
        config,
        cancel,
        None,
        None,
        SubAgentOrigin::SubAgent,
    )
    .await?;
    let raw = tokio::fs::read_to_string(child.child_ctx.home.join("session.json")).await?;
    let metadata: serde_json::Value = serde_json::from_str(&raw)?;
    assert_eq!(metadata["title"], "Sub-agent: sub-title-fallback");
    Ok(())
}

#[tokio::test]
async fn child_metadata_records_parent_origin_label_and_depth() -> anyhow::Result<()> {
    let parent = crate::regression::test_context_for_agent("sub-metadata").await?;
    let config = parent.config.clone();
    let parent_id = parent.config.session_id.clone();
    let cancel = parent.cancel.linked_child_token();
    let child = SubAgentExecutor::new_child(
        parent,
        "sub-metadata-child".into(),
        false,
        config,
        cancel,
        Some("review diff".into()),
        None,
        SubAgentOrigin::SubAgent,
    )
    .await?;
    let raw = tokio::fs::read_to_string(child.child_ctx.home.join("session.json")).await?;
    let metadata: serde_json::Value = serde_json::from_str(&raw)?;
    assert_eq!(metadata["parent"], parent_id);
    assert_eq!(metadata["subagent"]["origin"], "sub_agent");
    assert_eq!(metadata["subagent"]["mode"], "fresh");
    assert_eq!(metadata["subagent"]["label"], "review diff");
    assert_eq!(metadata["subagent"]["depth"], 1);
    assert!(
        metadata["title"]
            .as_str()
            .is_some_and(|title| title.contains("review diff"))
    );
    Ok(())
}

#[tokio::test]
async fn internal_origins_are_distinct_and_do_not_require_delegation_depth() -> anyhow::Result<()> {
    assert_eq!(SubAgentOrigin::SubAgent.as_str(), "sub_agent");
    assert_eq!(SubAgentOrigin::GoalReview.as_str(), "goal_review");
    assert_eq!(SubAgentOrigin::Replan.as_str(), "replan");

    let parent =
        crate::regression::test_context_for_agent_with_config("sub-replan-metadata", |config| {
            config.sub_agent_max_depth = 0
        })
        .await?;
    let config = parent.config.clone();
    let cancel = parent.cancel.linked_child_token();
    let child = SubAgentExecutor::new_child(
        parent.clone(),
        "replan-metadata-child".into(),
        false,
        config,
        cancel,
        None,
        None,
        SubAgentOrigin::Replan,
    )
    .await?;
    let raw = tokio::fs::read_to_string(child.child_ctx.home.join("session.json")).await?;
    let metadata: serde_json::Value = serde_json::from_str(&raw)?;
    assert_eq!(metadata["subagent"]["origin"], "replan");

    let review = SubAgentExecutor::new_child(
        parent.clone(),
        "goal-review-metadata-child".into(),
        true,
        parent.config.clone(),
        parent.cancel.linked_child_token(),
        Some("goal review".into()),
        None,
        SubAgentOrigin::GoalReview,
    )
    .await?;
    let raw = tokio::fs::read_to_string(review.child_ctx.home.join("session.json")).await?;
    let metadata: serde_json::Value = serde_json::from_str(&raw)?;
    assert_eq!(metadata["subagent"]["origin"], "goal_review");
    assert_eq!(metadata["subagent"]["mode"], "fork");
    Ok(())
}

#[tokio::test]
async fn fork_commit_window_drop_removes_destination_without_returning_ok() -> anyhow::Result<()> {
    let parent = crate::regression::test_context_for_agent("sub-commit-drop").await?;
    let child_home = parent
        .store
        .path()
        .parent()
        .expect("parent session dir")
        .join("subagents")
        .join("slow_commit_drop");
    let cancel = parent.cancel.linked_child_token();
    let task = tokio::spawn({
        let parent = parent.clone();
        async move {
            SubAgentExecutor::new_child(
                parent.clone(),
                "slow_commit_drop".into(),
                true,
                parent.config.clone(),
                cancel,
                None,
                None,
                SubAgentOrigin::SubAgent,
            )
            .await
        }
    });
    for _ in 0..100 {
        if child_home.exists() {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
    }
    assert!(child_home.exists(), "fork clone did not start");
    task.abort();
    let _ = task.await;
    for _ in 0..100 {
        if !child_home.exists() {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }
    assert!(
        !child_home.exists(),
        "abandoned commit must remove the child home"
    );
    Ok(())
}

#[tokio::test]
async fn fork_commit_success_keeps_the_directory() -> anyhow::Result<()> {
    let parent = crate::regression::test_context_for_agent("sub-commit-success").await?;
    let child_home = parent
        .store
        .path()
        .parent()
        .expect("parent session dir")
        .join("subagents")
        .join("slow_commit_success");
    let cancel = parent.cancel.linked_child_token();
    let child = SubAgentExecutor::new_child(
        parent.clone(),
        "slow_commit_success".into(),
        true,
        parent.config.clone(),
        cancel,
        None,
        None,
        SubAgentOrigin::SubAgent,
    )
    .await?;
    assert!(child_home.exists(), "successful commit must keep the home");
    assert_eq!(child.context().sub_agent_depth, 1);
    Ok(())
}

#[tokio::test]
async fn clone_cancel_mid_copy_removes_the_destination() -> anyhow::Result<()> {
    let root = std::env::temp_dir().join(format!(
        "mink-subagent-clone-mid-cancel-{}",
        std::process::id()
    ));
    let source = root.join("source");
    let destination = root.join("slow_clone_destination");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&source)?;
    std::fs::write(source.join("conversation.jsonl"), b"{}")?;
    let cancel = crate::cancel::CancellationToken::new();
    let copy_cancel = cancel.clone();
    let task = tokio::task::spawn_blocking(move || {
        clone_session_dir(&source, &destination, Some(&copy_cancel))
    });
    let destination = root.join("slow_clone_destination");
    for _ in 0..50 {
        if destination.exists() {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
    }
    assert!(destination.exists(), "clone did not start");
    cancel.cancel();
    let result = task.await?;
    assert!(result.is_err(), "cancelled clone must fail");
    assert!(
        !destination.exists(),
        "blocking clone guard must remove the destination"
    );
    let _ = std::fs::remove_dir_all(root);
    Ok(())
}

#[test]
fn clone_session_dir_removes_a_cancelled_destination() -> anyhow::Result<()> {
    let root =
        std::env::temp_dir().join(format!("mink-subagent-clone-cancel-{}", std::process::id()));
    let source = root.join("source");
    let destination = root.join("destination");
    std::fs::create_dir_all(&source)?;
    std::fs::write(source.join("conversation.jsonl"), b"{}")?;
    let cancel = crate::cancel::CancellationToken::new();
    cancel.cancel();
    let error = clone_session_dir(&source, &destination, Some(&cancel))
        .expect_err("cancelled clone must fail");
    assert!(error.to_string().contains("cancelled"));
    assert!(!destination.exists(), "partial destination must be removed");
    let _ = std::fs::remove_dir_all(root);
    Ok(())
}
