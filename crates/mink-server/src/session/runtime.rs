use anyhow::{Result, anyhow};
use mink::runtime::{AgentEvent, AgentEventStream, AgentOptions, AgentRuntime, SessionInfo};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

const TURN_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);
const TURN_JOIN_GRACE: std::time::Duration = std::time::Duration::from_secs(1);
const OWNED_SHUTDOWN_JOIN_GRACE: std::time::Duration = std::time::Duration::from_secs(12);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimePhase {
    Idle,
    Running,
    Cancelling,
    Closing,
    Closed,
}

enum GuardedTurnResult {
    Finished(Result<()>),
    ForceClose {
        reason: String,
        saw_stop: bool,
        saw_final: bool,
        turn_id: String,
        session: Box<SessionInfo>,
    },
}

struct RuntimeState {
    runtime: Option<AgentRuntime>,
    phase: RuntimePhase,
    turn_task: Option<JoinHandle<Result<()>>>,
    last_idle: Instant,
    forced_terminal: Option<ForcedTerminal>,
}

struct ForcedTerminal {
    reason: String,
    saw_stop: bool,
    saw_final: bool,
    turn_id: String,
    session: Box<SessionInfo>,
}

struct Projection {
    generation: String,
    watermark: u64,
    rows: Vec<serde_json::Value>,
    progress: Vec<serde_json::Value>,
    current_turn: Option<String>,
    phase: &'static str,
    last_final: Option<serde_json::Value>,
    resources: serde_json::Value,
    diagnostics: serde_json::Value,
    activity: serde_json::Value,
}
impl Projection {
    fn trim(&mut self) {
        let boundaries: Vec<_> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                row["role"] == "user"
                    && row["content"].is_string()
                    && row["internal"] != true
                    && row.pointer("/_mink/guidance") != Some(&serde_json::Value::Bool(true))
            })
            .map(|(i, _)| i)
            .collect();
        if boundaries.len() > 20 {
            self.rows.drain(..boundaries[boundaries.len() - 20]);
        }
    }
}

pub struct SessionRuntime {
    projection: Arc<Mutex<Projection>>,
    inbox: Arc<mink::runtime::InputInbox>,
    image_input: mink::runtime::ImageInputCapability,
    attachments: Arc<mink::runtime::AttachmentStore>,
    state: Arc<Mutex<RuntimeState>>,
    event_tx: broadcast::Sender<String>,
    stream_sequence: Arc<AtomicU64>,
}

impl SessionRuntime {
    pub async fn open(options: AgentOptions) -> Result<Self> {
        let runtime = AgentRuntime::start(options).await?;
        let info = runtime.session_info();
        let directory = info
            .conversation_path
            .parent()
            .expect("session directory")
            .to_path_buf();
        let read_dir = directory.clone();
        let read_result = tokio::task::spawn_blocking(move || -> Result<_> {
            let reader = mink::runtime::session::SessionReader::new(read_dir);
            Ok((
                reader.conversation()?,
                serde_json::json!({"plan":reader.plan()?,"todo":reader.todo()?,"artifacts":reader.artifacts()?}),
            ))
        })
        .await.map_err(anyhow::Error::from).and_then(|result| result);
        let (rows, resources) = match read_result {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return match runtime.shutdown().await {
                    Ok(()) => Err(error.context("cannot initialize session projection")),
                    Err(shutdown) => Err(anyhow!(
                        "cannot initialize session projection: {error:#}; cleanup failed: {shutdown}"
                    )),
                };
            }
        };
        let (model, stats) = runtime.presentation_snapshot().await;
        let mut projection = Projection {
            generation: format!(
                "{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_nanos()
            ),
            watermark: 0,
            rows,
            progress: vec![],
            current_turn: None,
            phase: "idle",
            last_final: None,
            resources,
            diagnostics: serde_json::json!({"type":"title_update","model":model,"stats":stats}),
            activity: serde_json::json!({"work_state":"idle","wait_elapsed_secs":null,"active_sub_agents":[]}),
        };
        projection.trim();
        let inbox = runtime.handle().input_inbox().clone();
        let image_input = runtime.handle().image_input().clone();
        let attachments = Arc::new(mink::runtime::AttachmentStore::new(
            directory.join("attachments"),
        ));
        let (event_tx, _) = broadcast::channel(1024);
        Ok(Self {
            projection: Arc::new(Mutex::new(projection)),
            inbox,
            image_input,
            attachments,
            state: Arc::new(Mutex::new(RuntimeState {
                runtime: Some(runtime),
                phase: RuntimePhase::Idle,
                turn_task: None,
                last_idle: Instant::now(),
                forced_terminal: None,
            })),
            event_tx,
            stream_sequence: Arc::new(AtomicU64::new(1)),
        })
    }

    pub fn phase(&self) -> RuntimePhase {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).phase
    }
    pub fn running(&self) -> bool {
        matches!(
            self.phase(),
            RuntimePhase::Running | RuntimePhase::Cancelling | RuntimePhase::Closing
        )
    }
    pub fn closed(&self) -> bool {
        self.phase() == RuntimePhase::Closed
    }
    pub fn idle_for(&self) -> Option<std::time::Duration> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (state.phase == RuntimePhase::Idle).then(|| state.last_idle.elapsed())
    }
    pub fn event_receiver(&self) -> broadcast::Receiver<String> {
        self.event_tx.subscribe()
    }

    pub fn inbox(&self) -> &Arc<mink::runtime::InputInbox> {
        &self.inbox
    }
    pub fn image_input(&self) -> &mink::runtime::ImageInputCapability {
        &self.image_input
    }
    pub fn attachments(&self) -> &Arc<mink::runtime::AttachmentStore> {
        &self.attachments
    }
    pub fn detail(&self) -> serde_json::Value {
        let p = self.projection.lock().unwrap_or_else(|e| e.into_inner());
        serde_json::json!({"generation":p.generation,"current_turn":p.current_turn,"phase":p.phase,"capabilities":{"image_input":self.image_input},"last_final":p.last_final})
    }
    pub fn snapshot_subscription(&self) -> (serde_json::Value, broadcast::Receiver<String>) {
        let p = self.projection.lock().unwrap_or_else(|e| e.into_inner());
        let rx = self.event_tx.subscribe();
        (
            serde_json::json!({"type":"session_snapshot","generation":p.generation,"stream_sequence":p.watermark,"conversation":p.rows,"progress":p.progress,"current_turn":p.current_turn,"phase":p.phase,"running":matches!(p.phase,"running"|"cancelling"|"closing"),"inputs":self.inbox.outstanding(),"capabilities":{"image_input":self.image_input},"last_final":p.last_final,"resources":p.resources,"diagnostics":p.diagnostics,"activity":p.activity}),
            rx,
        )
    }
    pub fn publish_inputs(&self) {
        publish(
            &self.event_tx,
            &self.stream_sequence,
            &self.projection,
            serde_json::json!({"type":"inputs_updated","inputs":self.inbox.outstanding()}),
        );
    }
    pub fn start_turn(&self, input: String) -> Result<()> {
        self.submit(
            mink::runtime::HumanInput {
                request_id: format!(
                    "legacy-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)?
                        .as_nanos()
                ),
                text: input,
                target_turn_id: None,
                attachment_ids: vec![],
            },
            None,
        )
        .map(|_| ())
    }
    pub fn submit(
        &self,
        input: mink::runtime::HumanInput,
        resume: Option<(&str, u64)>,
    ) -> Result<mink::runtime::InputReceipt> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if resume.is_none()
            && let Some(receipt) = self.inbox.existing(&input)?
        {
            return Ok(receipt);
        }
        if input.target_turn_id.is_some() && resume.is_none() {
            if state.phase != RuntimePhase::Running {
                anyhow::bail!("turn no longer accepts guidance");
            }
            let runtime = state
                .runtime
                .as_ref()
                .ok_or_else(|| anyhow!("session runtime is closed"))?;
            let (receipt, _) = runtime.handle().stream_input(input)?;
            drop(state);
            self.publish_inputs();
            return Ok(receipt);
        }
        if state.phase != RuntimePhase::Idle {
            anyhow::bail!("session runtime is {:?}", state.phase);
        }
        if state
            .turn_task
            .as_ref()
            .is_some_and(|task| !task.is_finished())
        {
            anyhow::bail!("session turn cleanup is still running");
        }
        state.turn_task.take();
        let runtime = state
            .runtime
            .as_ref()
            .ok_or_else(|| anyhow!("session runtime is closed"))?;
        let session = runtime.session_info().clone();
        let (receipt, stream) = match resume {
            Some((id, revision)) => runtime.handle().resume_input(id, revision)?,
            None => runtime.handle().stream_input(input)?,
        };
        let Some(stream) = stream else {
            return Ok(receipt);
        };
        {
            let mut p = self.projection.lock().unwrap_or_else(|e| e.into_inner());
            p.current_turn = Some(receipt.turn_id.clone());
            p.phase = "running";
            p.progress.clear();
        }
        state.phase = RuntimePhase::Running;
        let shared = self.state.clone();
        let tx = self.event_tx.clone();
        let stream_sequence = self.stream_sequence.clone();
        let projection = self.projection.clone();
        let inbox = self.inbox.clone();
        state.turn_task = Some(tokio::spawn(async move {
            let guarded =
                run_turn_guarded(&tx, &stream_sequence, &projection, &shared, stream, session)
                    .await;
            let result = match guarded {
                GuardedTurnResult::Finished(result) => {
                    let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
                    if !matches!(state.phase, RuntimePhase::Closing | RuntimePhase::Closed) {
                        state.phase = RuntimePhase::Idle;
                        state.last_idle = Instant::now();
                    }
                    drop(state);
                    publish(
                        &tx,
                        &stream_sequence,
                        &projection,
                        serde_json::json!({"type":"inputs_updated","inputs":inbox.outstanding()}),
                    );
                    result
                }
                GuardedTurnResult::ForceClose {
                    reason,
                    saw_stop,
                    saw_final,
                    turn_id,
                    session,
                } => {
                    let runtime = {
                        let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
                        state.forced_terminal = Some(ForcedTerminal {
                            reason: reason.clone(),
                            saw_stop,
                            saw_final,
                            turn_id,
                            session,
                        });
                        state.phase = RuntimePhase::Closing;
                        publish(
                            &tx,
                            &stream_sequence,
                            &projection,
                            serde_json::json!({"type":"phase_updated","phase":"closing"}),
                        );
                        state.runtime.take()
                    };
                    let owns_runtime_shutdown = runtime.is_some();
                    let shutdown_error = if let Some(runtime) = runtime {
                        runtime
                            .shutdown()
                            .await
                            .err()
                            .map(|error| error.to_string())
                    } else {
                        None
                    };
                    if owns_runtime_shutdown {
                        finalize_shutdown(
                            &shared,
                            &tx,
                            &stream_sequence,
                            Some(&projection),
                            shutdown_error.as_deref(),
                        );
                    }
                    match shutdown_error {
                        Some(error) => {
                            Err(anyhow!("{reason}; forced runtime shutdown failed: {error}"))
                        }
                        None => Err(anyhow!(reason)),
                    }
                }
            };
            if let Err(error) = &result {
                eprintln!("[mink-server] turn task ended abnormally: {error:#}");
            }
            result
        }));
        drop(state);
        self.publish_inputs();
        Ok(receipt)
    }

    pub fn session_id(&self) -> String {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .runtime
            .as_ref()
            .map(|runtime| runtime.session_info().session_id.clone())
            .unwrap_or_default()
    }

    pub fn interrupt(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(
            state.phase,
            RuntimePhase::Running | RuntimePhase::Cancelling
        ) {
            state.phase = RuntimePhase::Cancelling;
            publish(
                &self.event_tx,
                &self.stream_sequence,
                &self.projection,
                serde_json::json!({"type":"phase_updated","phase":"cancelling"}),
            );
            if let Some(runtime) = &state.runtime {
                runtime.interrupt_current_turn();
            }
        }
    }

    pub async fn shutdown(&self) -> Result<()> {
        let (mut task, runtime) = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.phase == RuntimePhase::Closed {
                return Ok(());
            }
            state.phase = RuntimePhase::Closing;
            publish(
                &self.event_tx,
                &self.stream_sequence,
                &self.projection,
                serde_json::json!({"type":"phase_updated","phase":"closing"}),
            );
            if let Some(runtime) = &state.runtime {
                runtime.interrupt_current_turn();
            }
            (state.turn_task.take(), state.runtime.take())
        };
        let turn_join_grace = if runtime.is_none() {
            // The timeout cleanup task may already own AgentRuntime::shutdown,
            // whose own bounded turn/actor cleanup can exceed five seconds.
            OWNED_SHUTDOWN_JOIN_GRACE
        } else {
            TURN_SHUTDOWN_GRACE
        };
        let mut errors = Vec::new();
        if let Some(handle) = task.as_mut() {
            match tokio::time::timeout(turn_join_grace, handle).await {
                Ok(result) => {
                    task = None;
                    record_turn_join(result, &mut errors);
                }
                Err(_) => {
                    errors.push(format!(
                        "turn did not stop within {}s",
                        turn_join_grace.as_secs()
                    ));
                }
            }
        }
        let shutdown_error = if let Some(runtime) = runtime {
            runtime
                .shutdown()
                .await
                .err()
                .map(|error| error.to_string())
        } else {
            None
        };
        if let Some(error) = &shutdown_error {
            errors.push(format!("runtime shutdown failed: {error}"));
        }
        if let Some(mut handle) = task {
            match tokio::time::timeout(TURN_JOIN_GRACE, &mut handle).await {
                Ok(result) => record_turn_join(result, &mut errors),
                Err(_) => {
                    handle.abort();
                    match handle.await {
                        Err(error) if error.is_cancelled() => {}
                        result => record_turn_join(result, &mut errors),
                    }
                    errors.push("turn task required forced abort after runtime shutdown".into());
                }
            }
        }
        finalize_shutdown(
            &self.state,
            &self.event_tx,
            &self.stream_sequence,
            Some(&self.projection),
            shutdown_error.as_deref(),
        );
        if errors.is_empty() {
            Ok(())
        } else {
            Err(anyhow!(errors.join("; ")))
        }
    }
}

fn finalize_shutdown(
    state: &Arc<Mutex<RuntimeState>>,
    tx: &broadcast::Sender<String>,
    stream_sequence: &AtomicU64,
    projection: Option<&Arc<Mutex<Projection>>>,
    shutdown_error: Option<&str>,
) {
    let terminal = {
        let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
        state.phase = RuntimePhase::Closed;
        state.forced_terminal.take()
    };
    let Some(terminal) = terminal else {
        if let Some(projection) = projection {
            publish(
                tx,
                stream_sequence,
                projection,
                serde_json::json!({"type":"phase_updated","phase":"closed"}),
            );
        }
        return;
    };
    let error = match shutdown_error {
        Some(shutdown_error) => format!(
            "{}; forced runtime shutdown failed: {shutdown_error}",
            terminal.reason
        ),
        None => terminal.reason,
    };
    publish_forced_timeout_final(
        tx,
        stream_sequence,
        &terminal.turn_id,
        &terminal.session,
        &error,
        (terminal.saw_stop, terminal.saw_final),
        projection,
    );
    if let Some(projection) = projection {
        publish(
            tx,
            stream_sequence,
            projection,
            serde_json::json!({"type":"phase_updated","phase":"closed"}),
        );
    }
}

fn record_turn_join(
    result: std::result::Result<Result<()>, tokio::task::JoinError>,
    errors: &mut Vec<String>,
) {
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => errors.push(format!("turn task failed: {error:#}")),
        Err(error) => errors.push(format!("turn task panicked: {error}")),
    }
}

fn agent_event_to_json(event: &AgentEvent, stream_sequence: u64) -> serde_json::Value {
    let mut value = serde_json::to_value(event)
        .unwrap_or_else(|_| serde_json::json!({"type":"serialization_error"}));
    value["stream_sequence"] = stream_sequence.into();
    if value.get("type").and_then(serde_json::Value::as_str) == Some("final") {
        value["type"] = "turn_final".into();
    }
    value
}

fn publish(
    tx: &broadcast::Sender<String>,
    sequence: &AtomicU64,
    projection: &Arc<Mutex<Projection>>,
    mut value: serde_json::Value,
) {
    let mut p = projection.lock().unwrap_or_else(|e| e.into_inner());
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
            p.rows.push(row);
            p.trim();
        }
        "phase_updated" => {
            p.phase = match value["phase"].as_str() {
                Some("cancelling") => "cancelling",
                Some("closing") => "closing",
                Some("closed") => "closed",
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
            if let Some(last) = p.progress.last_mut().filter(|e| e["type"] == value["type"]) {
                let text = format!(
                    "{}{}",
                    last["content"].as_str().unwrap_or_default(),
                    value["content"].as_str().unwrap_or_default()
                );
                last["content"] = text.into();
            } else {
                p.progress.push(value.clone());
            }
        }
        "signal" | "error" | "sub_agent_status" | "sub_agent_output" | "retry"
        | "context_compact" => {
            p.progress.push(value.clone());
        }
        _ => {}
    }
    let _ = tx.send(value.to_string());
}

fn update_activity(activity: &mut serde_json::Value, event: &serde_json::Value) {
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

fn apply_presentation(resources: &mut serde_json::Value, presentation: &serde_json::Value) {
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

async fn run_turn_guarded(
    tx: &broadcast::Sender<String>,
    stream_sequence: &AtomicU64,
    projection: &Arc<Mutex<Projection>>,
    state: &Arc<Mutex<RuntimeState>>,
    mut stream: AgentEventStream,
    session: SessionInfo,
) -> GuardedTurnResult {
    let timeout_secs = std::env::var("MINK_SERVER_TURN_TIMEOUT")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(1200);
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

fn publish_forced_timeout_final(
    tx: &broadcast::Sender<String>,
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
            let _ = tx.send(value.to_string());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_tracks_reliable_progress_and_clears_wait_at_boundaries() {
        let mut activity = serde_json::json!({"work_state":"idle","wait_elapsed_secs":null});
        update_activity(&mut activity, &serde_json::json!({"type":"turn_started"}));
        update_activity(
            &mut activity,
            &serde_json::json!({"type":"info","message":"Waiting for model response... elapsed=23s idle=5s"}),
        );
        assert_eq!(activity["work_state"], "waiting");
        assert_eq!(activity["wait_elapsed_secs"], 23);
        // Formal history commits and stats must not erase current activity.
        update_activity(
            &mut activity,
            &serde_json::json!({"type":"conversation_committed"}),
        );
        assert_eq!(activity["wait_elapsed_secs"], 23);
        for (event, work) in [
            (serde_json::json!({"type":"thinking"}), "thinking"),
            (serde_json::json!({"type":"text"}), "generating"),
            (serde_json::json!({"type":"tool_call"}), "tool"),
            (serde_json::json!({"type":"tool_result"}), "waiting"),
            (
                serde_json::json!({"type":"sub_agent_status","session_id":"child","status":"running"}),
                "sub-agent",
            ),
            (
                serde_json::json!({"type":"sub_agent_output","session_id":"child"}),
                "waiting",
            ),
            (
                serde_json::json!({"type":"info","message":"Compressing..."}),
                "compacting",
            ),
            (
                serde_json::json!({"type":"turn_final","outcome":{"error":"cancelled"}}),
                "error",
            ),
            (
                serde_json::json!({"type":"turn_final","outcome":{"error":null}}),
                "idle",
            ),
        ] {
            update_activity(&mut activity, &event);
            assert_eq!(activity["work_state"], work);
            assert!(activity["wait_elapsed_secs"].is_null());
        }
    }

    #[test]
    fn activity_keeps_remaining_children_running_and_interruption_idle() {
        let mut activity = serde_json::json!({"work_state":"idle","wait_elapsed_secs":null});
        for id in ["first", "second"] {
            update_activity(
                &mut activity,
                &serde_json::json!({"type":"sub_agent_status","session_id":id,"status":"launched"}),
            );
        }
        update_activity(
            &mut activity,
            &serde_json::json!({"type":"sub_agent_output","session_id":"first","status":"ok"}),
        );
        assert_eq!(activity["work_state"], "sub-agent");
        assert_eq!(activity["active_sub_agents"], serde_json::json!(["second"]));
        update_activity(
            &mut activity,
            &serde_json::json!({"type":"turn_final","outcome":{"status":"interrupted","error":"interrupted"}}),
        );
        assert_eq!(activity["work_state"], "idle");
        assert_eq!(activity["active_sub_agents"], serde_json::json!([]));
    }

    fn session_info() -> SessionInfo {
        let path = std::path::PathBuf::from("/tmp/mink-server-runtime-test");
        SessionInfo {
            session_id: "session".into(),
            session_ref: "session".into(),
            is_new: false,
            home: path.clone(),
            cwd: path.clone(),
            events_path: path.join("events.jsonl"),
            conversation_path: path.join("conversation.jsonl"),
            artifacts_dir: path.join("artifacts"),
            summary_path: path.join("summary.md"),
            usage_path: path.join("usage.jsonl"),
            plan_path: path.join("plan.md"),
            plan_draft_path: path.join("plan.draft"),
            todos_path: path.join("todos.json"),
        }
    }

    #[test]
    fn forced_terminal_is_published_once_after_closed() {
        let (tx, mut rx) = broadcast::channel(8);
        let sequence = AtomicU64::new(1);
        let state = Arc::new(Mutex::new(RuntimeState {
            runtime: None,
            phase: RuntimePhase::Closing,
            turn_task: None,
            last_idle: Instant::now(),
            forced_terminal: Some(ForcedTerminal {
                reason: "turn timed out".into(),
                saw_stop: false,
                saw_final: false,
                turn_id: "turn".into(),
                session: Box::new(session_info()),
            }),
        }));

        finalize_shutdown(&state, &tx, &sequence, None, Some("shutdown error"));
        assert_eq!(state.lock().unwrap().phase, RuntimePhase::Closed);
        let stop: serde_json::Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
        let final_event: serde_json::Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(stop["type"], "stop");
        assert_eq!(final_event["type"], "turn_final");
        assert_eq!(final_event["outcome"]["status"], "failed");
        assert!(
            final_event["outcome"]["error"]
                .as_str()
                .unwrap()
                .contains("shutdown error")
        );

        finalize_shutdown(&state, &tx, &sequence, None, None);
        assert!(rx.try_recv().is_err());
    }
    #[test]
    fn resource_projection_applies_todo_deltas_without_losing_completed_items() {
        let mut resources = serde_json::json!({"plan":{"plan":null,"draft":null},"todo":{"revision":1,"items":[{"id":"done","content":"old","status":"completed"},{"id":"work","content":"new","status":"in_progress"}]}});
        apply_presentation(
            &mut resources,
            &serde_json::json!({"kind":"todo","data":{"revision":2,"items":[],"changes":[{"change":"completed","id":"work"}]}}),
        );
        assert_eq!(resources["todo"]["items"].as_array().unwrap().len(), 2);
        assert_eq!(resources["todo"]["items"][1]["status"], "completed");
        apply_presentation(
            &mut resources,
            &serde_json::json!({"kind":"todo","data":{"revision":1,"items":[],"changes":[{"change":"removed","id":"done"}]}}),
        );
        assert_eq!(resources["todo"]["items"].as_array().unwrap().len(), 2);
        apply_presentation(
            &mut resources,
            &serde_json::json!({"kind":"plan","data":{"transition":"confirmed","content":"plan"}}),
        );
        assert_eq!(resources["plan"]["plan"], "plan");
        assert!(resources["plan"]["draft"].is_null());
    }
}
