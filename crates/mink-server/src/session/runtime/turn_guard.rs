use super::events::agent_event_to_json;
use super::*;

pub(super) const DEFAULT_TURN_TIMEOUT_SECS: u64 = 7_200;

pub(super) fn turn_timeout_secs(value: Option<&str>) -> u64 {
    value
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_TURN_TIMEOUT_SECS)
}

pub(crate) fn draft_generation_timeout_secs() -> u64 {
    turn_timeout_secs(std::env::var("MINK_SERVER_TURN_TIMEOUT").ok().as_deref())
}

pub(super) async fn run_turn_guarded(
    tx: &broadcast::Sender<Arc<BroadcastEvent>>,
    stream_sequence: &AtomicU64,
    projection: &Arc<Mutex<Projection>>,
    state: &Arc<Mutex<RuntimeState>>,
    mut stream: AgentEventStream,
    session: SessionInfo,
) -> GuardedTurnResult {
    let timeout_secs = turn_timeout_secs(std::env::var("MINK_SERVER_TURN_TIMEOUT").ok().as_deref());
    let deadline = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs));
    tokio::pin!(deadline);
    let mut saw_stop = false;
    let mut saw_final = false;
    let turn_id = stream.turn_id().to_string();
    loop {
        tokio::select! {
            event = stream.recv() => match event {
                Some(event) => {
                    saw_stop |= matches!(&event.kind, mink::runtime::AgentEventKind::Stop { .. });
                    saw_final |= matches!(&event.kind, mink::runtime::AgentEventKind::Final { .. });
                    publish(tx,stream_sequence,projection,agent_event_to_json(&event,0));
                }
                None => {
                    return GuardedTurnResult::Finished(stream.outcome().await.map(|_| ()).map_err(Into::into));
                },
            },
            _ = &mut deadline => {
                {
                    let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                    if state.phase == RuntimePhase::Running {
                        state.phase = RuntimePhase::Cancelling;
                        publish(tx,stream_sequence,projection,serde_json::json!({"type":"phase_updated","phase":"cancelling"}));
                    }
                }
                stream.cancel();
                publish(tx,stream_sequence,projection,serde_json::json!({
                    "type":"turn_error",
                    "error":"turn timed out"
                }));
                break;
            }
        }
    }

    let grace = async {
        while let Some(event) = stream.recv().await {
            saw_stop |= matches!(&event.kind, mink::runtime::AgentEventKind::Stop { .. });
            saw_final |= matches!(&event.kind, mink::runtime::AgentEventKind::Final { .. });
            publish(
                tx,
                stream_sequence,
                projection,
                agent_event_to_json(&event, 0),
            );
        }
        stream.outcome().await
    };
    match tokio::time::timeout(TURN_SHUTDOWN_GRACE, grace).await {
        Ok(Ok(outcome)) => GuardedTurnResult::Finished(Err(anyhow!(
            "turn timed out after {timeout_secs}s (ended as {:?})",
            outcome.status
        ))),
        Ok(Err(error)) => GuardedTurnResult::Finished(Err(anyhow!(
            "turn timed out after {timeout_secs}s and cancellation failed: {error}"
        ))),
        Err(_) => GuardedTurnResult::ForceClose {
            reason: format!(
                "turn timed out after {timeout_secs}s and did not stop within {}s",
                TURN_SHUTDOWN_GRACE.as_secs()
            ),
            saw_stop,
            saw_final,
            turn_id,
            session: Box::new(session),
        },
    }
}

pub(super) fn publish_forced_timeout_final(
    tx: &broadcast::Sender<Arc<BroadcastEvent>>,
    stream_sequence: &AtomicU64,
    turn_id: &str,
    session: &SessionInfo,
    error: &str,
    observed: (bool, bool),
    projection: Option<&Arc<Mutex<Projection>>>,
) {
    let (saw_stop, saw_final) = observed;
    let deliver = |mut value: serde_json::Value| {
        if let Some(projection) = projection {
            publish(tx, stream_sequence, projection, value);
        } else {
            value["stream_sequence"] = stream_sequence.fetch_add(1, Ordering::Relaxed).into();
            let _ = tx.send(make_broadcast_event(&value));
        }
    };
    if !saw_stop {
        let sequence = 0;
        deliver(serde_json::json!({
            "type": "stop",
            "turn_id": turn_id,
            "reason": "timeout",
            "stream_sequence": sequence,
        }));
    }
    if !saw_final {
        let sequence = 0;
        deliver(serde_json::json!({
        "type": "turn_final",
        "turn_id": turn_id,
        "stream_sequence": sequence,
        "outcome": {
            "turn_id": turn_id,
            "billing_turn_id": turn_id,
            "status": "failed",
            "session": session,
            "text": "",
            "thinking": "",
            "tool_call_count": 0,
            "tool_error_count": 1,
            "error": error,
            "usage_records": [],
            "usage": {
                "request_count": 0,
                "reported_request_count": 0,
                "unreported_request_count": 0,
                "attempt_count": 0,
                "tokens": {
                    "input_tokens": 0,
                    "cache_read_tokens": 0,
                    "cache_creation_tokens": 0,
                    "output_tokens": 0
                }
            }
        }
        }));
    }
}
