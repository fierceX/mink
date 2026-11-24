use super::*;

pub(super) fn make_broadcast_event(value: &serde_json::Value) -> Arc<BroadcastEvent> {
    let kind = if matches!(
        value.get("type").and_then(serde_json::Value::as_str),
        Some("inputs_updated" | "conversation_committed" | "phase_updated")
    ) {
        BroadcastEventKind::Control
    } else {
        BroadcastEventKind::Standard
    };
    Arc::new(BroadcastEvent {
        kind,
        payload: Arc::from(value.to_string()),
    })
}

pub(super) fn agent_event_to_json(event: &AgentEvent, stream_sequence: u64) -> serde_json::Value {
    let mut value = serde_json::to_value(event)
        .unwrap_or_else(|_| serde_json::json!({"type":"serialization_error"}));
    value["stream_sequence"] = stream_sequence.into();
    if value.get("type").and_then(serde_json::Value::as_str) == Some("final") {
        value["type"] = "turn_final".into();
    }
    // Core emits its own GoalUpdated/GoalFinished events during a turn; the
    // server also mirrors the goal watch channel. Both carry the full contract,
    // which can be hundreds of kilobytes of reviews and usage records. The
    // stream carries the same summary on both paths, and the detail tab asks
    // for the full snapshot explicitly.
    if matches!(
        value.get("type").and_then(serde_json::Value::as_str),
        Some("goal_updated" | "goal_finished")
    ) && let Some(goal) = value.get("goal").cloned()
    {
        value["goal"] = goal_summary_value(&goal);
    }
    let kind = value_type(&value).map(str::to_string);
    if let Some(object) = value.as_object_mut() {
        match kind.as_deref() {
            Some("tool_result") => {
                // The workbench renders the complete tool row from the formal
                // history; the live event only needs the content field.
                object.remove("content_preview");
            }
            Some("sub_agent_output") => {
                // A sub-agent's final text is displayed as a summary card; the
                // full body already lives in the child session. Bound both
                // streams so one child cannot dominate the live SSE payload.
                for key in ["text", "thinking"] {
                    if let Some(text) = object.get(key).and_then(serde_json::Value::as_str) {
                        object.insert(key.into(), bounded_text(text, 32 * 1024).into());
                    }
                }
            }
            _ => {}
        }
    }
    value
}

pub(super) fn value_type(value: &serde_json::Value) -> Option<&str> {
    value.get("type").and_then(serde_json::Value::as_str)
}

pub(super) fn bounded_text(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated {} bytes]", &text[..end], text.len() - end)
}

/// Goal fields the live bar and status wording consume. Heavy history stays on
/// the explicit detail read; the stream and the session snapshot remain small.
pub(super) fn goal_summary(goal: &GoalSnapshot) -> serde_json::Value {
    serde_json::to_value(goal)
        .map(|value| goal_summary_value(&value))
        .unwrap_or(serde_json::Value::Null)
}

pub(crate) fn goal_summary_value(goal: &serde_json::Value) -> serde_json::Value {
    let Some(mut object) = goal.as_object().cloned() else {
        return goal.clone();
    };
    let iterations = object
        .remove("iterations")
        .and_then(|value| value.as_array().map(Vec::len))
        .unwrap_or(0);
    object.insert("iterations_count".into(), iterations.into());
    for key in [
        "usage_records",
        "checker_records",
        "checker_evidence_versions",
        "evidence_versions",
        "checker_passed_candidate",
        "review_report",
        "report",
        "environment",
    ] {
        object.remove(key);
    }
    for key in ["reviews", "continuations"] {
        if let Some(array) = object
            .get_mut(key)
            .and_then(serde_json::Value::as_array_mut)
        {
            let last = array.last().cloned();
            array.clear();
            if let Some(last) = last {
                array.push(last);
            }
        }
    }
    serde_json::Value::Object(object)
}

pub(super) fn publish(
    tx: &broadcast::Sender<Arc<BroadcastEvent>>,
    sequence: &AtomicU64,
    projection: &Arc<Mutex<Projection>>,
    value: serde_json::Value,
) {
    let event = {
        let mut p = projection.lock().unwrap_or_else(|e| e.into_inner());
        publish_locked(sequence, &mut p, value)
    };
    let _ = tx.send(make_broadcast_event(&event));
}

/// Publish a goal update and advance the mirrored goal in the same critical
/// section, so `snapshot_subscription` and `detail` can render the goal
/// without touching session state.
///
/// A cleared (`hidden`) goal is normalized to null here: the mirror, the
/// session snapshot/detail projections and the `goal_updated` event must all
/// agree that a cleared goal is gone, matching the REST read.
pub(super) fn publish_goal(
    tx: &broadcast::Sender<Arc<BroadcastEvent>>,
    sequence: &AtomicU64,
    projection: &Arc<Mutex<Projection>>,
    goal: Option<mink::runtime::GoalSnapshot>,
) {
    let goal = goal.filter(|goal| !goal.hidden);
    let event = {
        let mut p = projection.lock().unwrap_or_else(|e| e.into_inner());
        p.goal = goal.clone();
        publish_locked(
            sequence,
            &mut p,
            serde_json::json!({"type":"goal_updated","goal":goal.as_ref().map(goal_summary)}),
        )
    };
    let _ = tx.send(make_broadcast_event(&event));
}

pub(super) fn push_bounded_progress(
    progress: &mut Vec<serde_json::Value>,
    item: serde_json::Value,
) {
    if progress.len() >= MAX_PROGRESS_ITEMS {
        progress.remove(0);
    }
    progress.push(item);
}

pub(super) fn publish_locked(
    sequence: &AtomicU64,
    p: &mut Projection,
    mut value: serde_json::Value,
) -> serde_json::Value {
    value["generation"] = p.generation.clone().into();
    let seq = sequence.fetch_add(1, Ordering::Relaxed);
    value["stream_sequence"] = seq.into();
    p.watermark = seq;
    value["after_conversation_seq"] = p
        .rows
        .last()
        .and_then(|row| row.get("seq"))
        .cloned()
        .unwrap_or(serde_json::json!(0));
    update_activity(&mut p.activity, &value);
    match value["type"].as_str().unwrap_or_default() {
        "title_update" => {
            p.diagnostics = value.clone();
        }
        "conversation_committed" => {
            let mut row = value["message"].clone();
            row["seq"] = value["conversation_seq"].clone();
            if row["role"] == "assistant" {
                p.progress
                    .retain(|e| !matches!(e["type"].as_str(), Some("text" | "thinking")));
            }
            if let Some(blocks) = row["content"].as_array() {
                for block in blocks {
                    if let Some(artifacts) = block
                        .pointer("/_mink/artifacts")
                        .and_then(serde_json::Value::as_array)
                    {
                        let existing = p.resources["artifacts"]
                            .as_array_mut()
                            .expect("artifact snapshot is an array");
                        for artifact in artifacts {
                            if !existing.iter().any(|row| row["id"] == artifact["id"]) {
                                existing.push(artifact.clone());
                            }
                        }
                    }
                    if let Some(presentation) = block.pointer("/_mink/presentation") {
                        apply_presentation(&mut p.resources, presentation);
                    }
                }
            }
            let row_bytes = serde_json::to_vec(&row)
                .map(|bytes| bytes.len())
                .unwrap_or(0);
            p.rows.push(row);
            p.row_bytes.push(row_bytes);
            p.trim();
        }
        "phase_updated" => {
            p.phase = match value["phase"].as_str() {
                Some("cancelling") => "cancelling",
                Some("closing") => "closing",
                Some("closed") => "closed",
                Some("running") => "running",
                Some("idle") => "idle",
                _ => p.phase,
            };
        }
        "turn_started" => {
            p.phase = "running";
            p.current_turn = value["turn_id"].as_str().map(str::to_string);
        }
        "turn_final" => {
            if !matches!(p.phase, "closing" | "closed") {
                p.phase = "idle";
            }
            p.current_turn = None;
            p.progress.clear();
            p.last_final = Some(value.clone());
        }
        "text" | "thinking" => {
            let delta = value["content"].as_str().unwrap_or_default();
            if let Some(last) = p.progress.last_mut().filter(|e| e["type"] == value["type"]) {
                if let Some(serde_json::Value::String(buf)) = last.get_mut("content") {
                    buf.push_str(delta);
                } else {
                    last["content"] = delta.into();
                }
            } else {
                push_bounded_progress(&mut p.progress, value.clone());
            }
        }
        "signal" | "error" | "sub_agent_status" | "sub_agent_output" | "retry"
        | "context_compact" => {
            push_bounded_progress(&mut p.progress, value.clone());
        }
        _ => {}
    }
    value
}

pub(super) fn update_activity(activity: &mut serde_json::Value, event: &serde_json::Value) {
    if matches!(event["type"].as_str(), Some("turn_started" | "turn_final")) {
        activity["active_sub_agents"] = serde_json::json!([]);
    }
    let work = match event["type"].as_str().unwrap_or_default() {
        "turn_started" | "retry" | "tool_result" => Some("waiting"),
        "thinking" => Some("thinking"),
        "text" => Some("generating"),
        "tool_call" => Some("tool"),
        "sub_agent_status" | "sub_agent_output" => {
            if !activity["active_sub_agents"].is_array() {
                activity["active_sub_agents"] = serde_json::json!([]);
            }
            let agents = activity["active_sub_agents"].as_array_mut().unwrap();
            let id = &event["session_id"];
            if matches!(event["status"].as_str(), Some("launched" | "running")) {
                if id.is_string() && !agents.contains(id) {
                    agents.push(id.clone());
                }
            } else if event["type"] == "sub_agent_output"
                || matches!(
                    event["status"].as_str(),
                    Some("ok" | "failed" | "timed_out" | "cancelled" | "channel_closed")
                )
            {
                agents.retain(|agent| agent != id);
            }
            Some(if agents.is_empty() {
                "waiting"
            } else {
                "sub-agent"
            })
        }
        "context_compact" => Some("compacting"),
        "turn_final" => Some(
            if event["outcome"]["status"] != "interrupted"
                && event
                    .pointer("/outcome/error")
                    .is_some_and(serde_json::Value::is_string)
            {
                "error"
            } else {
                "idle"
            },
        ),
        "info" => {
            let message = event["message"].as_str().unwrap_or_default();
            if let Some(elapsed) = mink::runtime::parse_llm_wait_heartbeat_elapsed(message) {
                activity["wait_elapsed_secs"] = elapsed.into();
            }
            (message == "Compressing...").then_some("compacting")
        }
        _ => None,
    };
    if let Some(work) = work {
        activity["work_state"] = work.into();
        activity["wait_elapsed_secs"] = serde_json::Value::Null;
    }
}

pub(super) fn apply_presentation(
    resources: &mut serde_json::Value,
    presentation: &serde_json::Value,
) {
    let data = &presentation["data"];
    match presentation["kind"].as_str() {
        Some("plan") => match data["transition"].as_str() {
            Some("confirmed") => {
                resources["plan"] = serde_json::json!({"plan":data["content"],"draft":null})
            }
            Some("cleared") => resources["plan"] = serde_json::json!({"plan":null,"draft":null}),
            Some("draft_saved") => resources["plan"]["draft"] = data["content"].clone(),
            Some("draft_cancelled") => resources["plan"]["draft"] = serde_json::Value::Null,
            _ => {}
        },
        Some("todo") if data["revision"].as_u64() > resources["todo"]["revision"].as_u64() => {
            let mut items = resources["todo"]["items"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            for change in data["changes"].as_array().into_iter().flatten() {
                let id = change["id"]
                    .as_str()
                    .or_else(|| change["item"]["id"].as_str());
                match change["change"].as_str() {
                    Some("removed") => items.retain(|item| item["id"].as_str() != id),
                    Some("added") => items.push(change["item"].clone()),
                    Some(action) => {
                        if let Some(item) = items.iter_mut().find(|item| item["id"].as_str() == id)
                        {
                            match action {
                                "updated" => item["content"] = change["content"].clone(),
                                "completed" => item["status"] = "completed".into(),
                                "activated" => item["status"] = "in_progress".into(),
                                "paused" | "reopened" => item["status"] = "pending".into(),
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            for item in data["items"].as_array().into_iter().flatten() {
                if let Some(existing) = items
                    .iter_mut()
                    .find(|existing| existing["id"] == item["id"])
                {
                    *existing = item.clone();
                } else {
                    items.push(item.clone());
                }
            }
            resources["todo"]["items"] = items.into();
            resources["todo"]["revision"] = data["revision"].clone();
        }
        _ => {}
    }
}

/// Forwards goal-driven turn events into the same ordered SSE projection that
/// ordinary turns use.
///
/// It deliberately owns no lifecycle state: phase and idle time come from the
/// core goal-turn signal through the session's own `RuntimeState`, so a second
/// source of truth cannot drift from execution control.
pub(super) struct GoalTurnSink {
    pub(super) tx: broadcast::Sender<Arc<BroadcastEvent>>,
    pub(super) sequence: Arc<AtomicU64>,
    pub(super) projection: Arc<OnceLock<Arc<Mutex<Projection>>>>,
}

#[async_trait::async_trait]
impl mink::runtime::EventSink for GoalTurnSink {
    async fn on_event(&self, event: mink::runtime::AgentEvent) -> Result<(), String> {
        let value = agent_event_to_json(&event, 0);
        match self.projection.get() {
            Some(projection) => publish(&self.tx, &self.sequence, projection, value),
            None => {
                let mut payload = value;
                payload["stream_sequence"] = self.sequence.fetch_add(1, Ordering::SeqCst).into();
                let _ = self.tx.send(make_broadcast_event(&payload));
            }
        }
        Ok(())
    }
}
