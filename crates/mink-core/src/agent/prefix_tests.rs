use super::*;
use crate::session::prefix::ImmutablePrefix;

#[tokio::test]
async fn ensure_reuses_valid_cached_prefix() -> anyhow::Result<()> {
    let ctx = crate::regression::test_context_for_agent("prefix-cache-hit").await?;
    let manager = PrefixManager::new(ctx.clone());
    let (first_prompt, first_tools) = manager.ensure().await?;
    let first_names: Vec<_> = first_tools
        .iter()
        .filter_map(|schema| schema.get("name").and_then(serde_json::Value::as_str))
        .collect();
    assert_eq!(
        first_names,
        ctx.tool_surface.names().collect::<Vec<_>>(),
        "request schemas must come from the resolved surface"
    );
    let cached_fingerprint = ctx
        .immutable_prefix
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .fingerprint()
        .to_string();

    let (second_prompt, second_tools) = manager.ensure().await?;

    assert_eq!(first_prompt, second_prompt);
    assert_eq!(first_tools, second_tools);
    assert_eq!(
        ctx.immutable_prefix
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .fingerprint(),
        cached_fingerprint
    );
    Ok(())
}

#[tokio::test]
async fn ensure_logs_prefix_snapshot_once_and_rebuild_replaces_it() -> anyhow::Result<()> {
    let ctx = crate::regression::test_context_for_agent("prefix-snapshot-event").await?;
    let manager = PrefixManager::new(ctx.clone());
    let (system_prompt, tools) = manager.ensure().await?;

    let snapshot = |events: &str| -> Vec<serde_json::Value> {
        events
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|evt| {
                evt.get("type").and_then(serde_json::Value::as_str) == Some("prefix_snapshot")
            })
            .collect()
    };

    ctx.flush_event_log().await?;
    let events = tokio::fs::read_to_string(&ctx.events_path).await?;
    let snapshots = snapshot(&events);
    assert_eq!(
        snapshots.len(),
        1,
        "first build writes exactly one snapshot"
    );
    let evt = &snapshots[0];
    assert_eq!(evt["version"], 1);
    assert_eq!(
        evt["fingerprint"].as_str().unwrap(),
        ctx.immutable_prefix
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .fingerprint()
    );
    assert_eq!(evt["system_prompt"].as_str().unwrap(), system_prompt);
    assert_eq!(evt["tools_json"], serde_json::Value::Array(tools.clone()));

    // Cache hit must not duplicate the snapshot.
    manager.ensure().await?;
    ctx.flush_event_log().await?;
    let events = tokio::fs::read_to_string(&ctx.events_path).await?;
    assert_eq!(snapshot(&events).len(), 1);

    // Invalidation rebuild replaces the snapshot with the new fingerprint.
    manager.invalidate();
    manager.ensure().await?;
    ctx.flush_event_log().await?;
    let events = tokio::fs::read_to_string(&ctx.events_path).await?;
    let snapshots = snapshot(&events);
    assert_eq!(snapshots.len(), 2, "rebuild appends one more snapshot");
    assert_eq!(
        snapshots[1]["fingerprint"].as_str().unwrap(),
        ctx.immutable_prefix
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .fingerprint()
    );
    Ok(())
}

#[tokio::test]
async fn ensure_drops_corrupt_cached_prefix_and_rebuilds() -> anyhow::Result<()> {
    let ctx = crate::regression::test_context_for_agent("prefix-cache-invalid").await?;
    *ctx.immutable_prefix.lock().unwrap() = Some(ImmutablePrefix::new_with_fingerprint(
        "stale".into(),
        vec![serde_json::json!({"name":"Bash"})],
        String::new(),
        "bad-fingerprint".into(),
    ));
    let manager = PrefixManager::new(ctx.clone());

    let (system_prompt, tools) = manager.ensure().await?;

    assert_ne!(system_prompt, "stale");
    assert!(!tools.is_empty());
    assert!(
        ctx.immutable_prefix
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .verify_fingerprint()
    );
    Ok(())
}

#[tokio::test]
async fn prefix_waits_for_real_writer_result_and_retries_after_recovery() -> anyhow::Result<()> {
    // Real writer failure (no fault-injection flag): the events path lives in
    // a missing directory, so every append fails to open. The prefix commit
    // must surface that error and must not populate the cache.
    let dir = std::env::temp_dir().join(format!(
        "mink-prefix-writer-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let events_path = dir.join("missing").join("events.jsonl");
    let writer = crate::session::event_log::EventLogWriter::start(events_path.clone());
    let ctx =
        crate::regression::context_with_event_log("prefix-writer-failure", writer.clone()).await?;
    let manager = PrefixManager::new(ctx.clone());

    let error = manager
        .ensure()
        .await
        .expect_err("a real write failure must reach the prefix caller");
    assert!(error.to_string().contains("failed to open"), "{error:#}");
    assert!(
        ctx.immutable_prefix
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_none(),
        "cache must not advance when the writer rejected the snapshot"
    );

    // Recovery: once the writer can open its file, the next ensure() rebuilds
    // and commits exactly one snapshot.
    std::fs::create_dir_all(events_path.parent().unwrap())?;
    manager.ensure().await?;
    // Startup diagnostics also failed against the broken path earlier; the
    // flush reports those losses, which is expected here. Drain and continue.
    let _ = writer.flush().await;
    let contents = std::fs::read_to_string(&events_path)?;
    let snapshots = contents
        .lines()
        .filter(|line| line.contains("\"prefix_snapshot\""))
        .count();
    assert_eq!(snapshots, 1, "retry must write exactly one snapshot");
    let _ = std::fs::remove_dir_all(dir);
    Ok(())
}
