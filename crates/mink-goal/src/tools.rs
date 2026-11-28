use crate::store::*;
use async_trait::async_trait;
use mink::runtime::ext::{
    ArtifactReader, CompactionCheckpoint, CompactionCheckpointProvider, ReadEvidenceContext,
    ReadObservation, ToolCallObserver, ToolEffectJournal, ToolExecutionMask, TurnPolicyProvider,
    split_read_path_selection,
};
use mink::runtime::{
    AgentTool, ApprovalTier, ToolDefinition, ToolError, ToolExecutionContext, ToolExecutionMode,
    ToolOutput, ToolPtcExposure, ToolResultKind, ToolRoleScope,
};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

pub(crate) struct GoalReviewToolContext<'a> {
    pub(crate) cwd: &'a Path,
    pub(crate) artifacts: &'a ArtifactReader,
}

pub(crate) trait GoalReviewToolSession: Send + Sync {
    fn billing_turn_id(&self) -> String;
    fn snapshot_for_read(&self) -> GoalSnapshot;
    fn mark_inspecting(&self) -> anyhow::Result<()>;
    fn record_rejection(&self, reason: String);
    fn submit_continue(&self, proposal: GoalContinueProposal) -> anyhow::Result<()>;
    fn submit(&self, review: GoalReview, ctx: &GoalReviewToolContext<'_>) -> anyhow::Result<()>;
    fn submit_conclusion(
        &self,
        conclusion: GoalHostConclusion,
        ctx: &GoalReviewToolContext<'_>,
    ) -> anyhow::Result<()>;
    fn finish_read(&self, captured: (String, PathBuf, String)) -> anyhow::Result<()>;
}

#[derive(Clone, Default)]
pub(crate) struct GoalToolBundle {
    goal_store: Arc<OnceLock<Arc<GoalStore>>>,
    artifacts: Arc<OnceLock<ArtifactReader>>,
    review_session: Arc<Mutex<Option<Arc<dyn GoalReviewToolSession>>>>,
    adjudication_session: Arc<Mutex<Option<Arc<dyn GoalReviewToolSession>>>>,
}

impl GoalToolBundle {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn bind_store(&self, goal_store: Arc<GoalStore>, artifacts: ArtifactReader) {
        let _ = self.goal_store.set(goal_store);
        let _ = self.artifacts.set(artifacts);
    }

    pub(crate) fn bind_review_session(&self, review_session: Arc<dyn GoalReviewToolSession>) {
        *self
            .review_session
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(review_session);
    }

    pub(crate) fn checkpoint_provider(&self) -> Arc<dyn CompactionCheckpointProvider> {
        Arc::new(GoalCheckpointProvider {
            goal_store: self.goal_store.clone(),
        })
    }

    pub(crate) fn set_adjudication(&self, session: Option<Arc<dyn GoalReviewToolSession>>) {
        *self
            .adjudication_session
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = session;
    }

    pub(crate) fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        vec![
            Arc::new(GoalReadTool {
                bundle: self.clone(),
            }),
            Arc::new(GoalDraftTool {
                bundle: self.clone(),
            }),
            Arc::new(GoalReviewTool {
                bundle: self.clone(),
            }),
        ]
    }

    pub(crate) fn turn_policy(&self) -> Arc<dyn TurnPolicyProvider> {
        Arc::new(GoalTurnPolicyProvider {
            goal_store: self.goal_store.clone(),
        })
    }

    fn active_review_session(&self) -> Option<Arc<dyn GoalReviewToolSession>> {
        self.review_session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .or_else(|| {
                self.adjudication_session
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
            })
    }

    fn is_review_agent(&self) -> bool {
        self.review_session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    fn adjudication(&self) -> Option<Arc<dyn GoalReviewToolSession>> {
        self.adjudication_session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

pub struct GoalReadTool {
    bundle: GoalToolBundle,
}

impl GoalReadTool {
    pub(crate) fn for_review_session(
        session: Arc<dyn GoalReviewToolSession>,
    ) -> Arc<dyn AgentTool> {
        let bundle = GoalToolBundle::new();
        bundle.bind_review_session(session);
        Arc::new(Self { bundle })
    }
}

fn decode_tool_args<T: serde::de::DeserializeOwned>(
    input: serde_json::Value,
) -> Result<T, ToolError> {
    serde_json::from_value(input)
        .map_err(|error| ToolError::argument(format!("invalid tool arguments: {error}")))
}

#[allow(clippy::double_must_use)]
#[async_trait]
impl AgentTool for GoalReadTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "GoalRead",
            "Read the authoritative current goal or unconfirmed draft, full contract, identity, limits and review results. No goal returns null.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }),
        )
        .with_approval(ApprovalTier::Read)
        .with_result_kind(ToolResultKind::Text)
        .with_execution(ToolExecutionMode::ParallelReadOnly)
        .with_role_scope(ToolRoleScope::All)
        .with_ptc_exposure(ToolPtcExposure::DirectOnly)
        .with_side_effect_exempt(true)
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _ctx: ToolExecutionContext,
    ) -> Result<ToolOutput, ToolError> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {}
        let _: Args = decode_tool_args(input)?;
        let goal = self
            .bundle
            .active_review_session()
            .map(|review| review.snapshot_for_read())
            .or_else(|| self.bundle.goal_store.get().and_then(|s| s.snapshot()))
            .filter(|g| !g.hidden);
        let snapshot = goal
            .as_ref()
            .map(|g| -> anyhow::Result<serde_json::Value> {
                let mut value = serde_json::to_value(g)?;
                value["budget"] = serde_json::to_value(g.budget())?;
                value["requirement_ids"] = serde_json::to_value(g.requirement_ids())?;
                value["latest_round_decision"] = serde_json::to_value(g.latest_round_decision())?;
                Ok(value)
            })
            .transpose()
            .map_err(|error| ToolError::new(format!("{error:#}")))?;
        let value = if goal.as_ref().is_some_and(|g| g.status == GoalStatus::Draft) {
            serde_json::json!({"goal":null,"draft":snapshot})
        } else {
            serde_json::json!({"goal":snapshot,"draft":null})
        };
        let encoded =
            serde_json::to_string(&value).map_err(|error| ToolError::new(format!("{error:#}")))?;
        Ok(ToolOutput::text(encoded).with_structured_value(value))
    }
}

pub struct GoalDraftTool {
    bundle: GoalToolBundle,
}

#[allow(clippy::double_must_use)]
#[async_trait]
impl AgentTool for GoalDraftTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "GoalDraft",
            "Replace the current unconfirmed goal proposal. Read GoalRead first and use its exact draft id and revision. Host-owned original request, limits and acceptance cannot be changed. Only the human host can confirm and start.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "draft_id": { "type": "string" },
                    "revision": { "type": "integer", "minimum": 1 },
                    "spec": {
                        "type": "object",
                        "properties": {
                            "objective": { "type": "string" },
                            "scope": { "type": "string" },
                            "constraints": { "type": "array", "items": { "type": "string" } },
                            "subgoals": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "id": { "type": "string" },
                                        "objective": { "type": "string" }
                                    },
                                    "required": ["id", "objective"],
                                    "additionalProperties": false
                                }
                            },
                            "criteria": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "id": { "type": "string" },
                                        "subgoal_id": { "type": "string" },
                                        "outcome": { "type": "string" },
                                        "required_evidence": { "type": "string" }
                                    },
                                    "required": ["id", "subgoal_id", "outcome", "required_evidence"],
                                    "additionalProperties": false
                                }
                            },
                            "suggested_actions": { "type": "array", "items": { "type": "string" } }
                        },
                        "required": ["objective", "scope", "constraints", "subgoals", "criteria", "suggested_actions"],
                        "additionalProperties": false
                    }
                },
                "required": ["draft_id", "revision", "spec"],
                "additionalProperties": false
            }),
        )
        .with_approval(ApprovalTier::Write)
        .with_result_kind(ToolResultKind::Control)
        .with_execution(ToolExecutionMode::Sequential)
        .with_mutating(true)
        .with_storm_exempt(true)
        .with_role_scope(ToolRoleScope::PrimaryOnly)
        .with_ptc_exposure(ToolPtcExposure::DirectOnly)
        .with_side_effect_exempt(true)
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _ctx: ToolExecutionContext,
    ) -> Result<ToolOutput, ToolError> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            draft_id: String,
            revision: u64,
            spec: GoalDraftSpec,
        }
        let args: Args = decode_tool_args(input)?;
        if self.bundle.is_review_agent() {
            return Err(ToolError::new("reviewers cannot edit contracts"));
        }
        let store = self
            .bundle
            .goal_store
            .get()
            .ok_or_else(|| ToolError::new("goal store not initialized"))?;
        let draft = store
            .replace_draft(&args.draft_id, args.revision, args.spec)
            .map_err(|error| ToolError::new(format!("{error:#}")))?;
        let encoded =
            serde_json::to_string(&draft).map_err(|error| ToolError::new(format!("{error:#}")))?;
        Ok(ToolOutput::text(encoded))
    }
}

pub struct GoalReviewTool {
    bundle: GoalToolBundle,
}

impl GoalReviewTool {
    #[cfg(test)]
    pub(crate) fn for_test_adjudication(
        session: Option<Arc<dyn GoalReviewToolSession>>,
        artifacts: ArtifactReader,
    ) -> Self {
        let bundle = GoalToolBundle::new();
        let _ = bundle.artifacts.set(artifacts);
        bundle.set_adjudication(session);
        Self { bundle }
    }
}

#[allow(clippy::double_must_use)]
#[async_trait]
impl AgentTool for GoalReviewTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "GoalReview",
            "Submit the HOST decision based on the independent descriptive report. continue means necessary work remains (including after inspecting evidence); complete means the reviewer verified the whole contract; blocked means an objective external dependency prevents progress. The runtime binds the current candidate, report and inspected evidence. Do not reproduce a review table or candidate. On rejection, correct the diagnostic and call again. Only available during host adjudication.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "decision": {
                        "type": "string",
                        "enum": ["continue", "complete", "blocked"]
                    },
                    "summary": {
                        "type": "string",
                        "description": "Explain the decision using the independent report."
                    },
                    "next_step": {
                        "type": "string",
                        "description": "Required for continue or blocked; concrete contract work or dependency resolution."
                    },
                    "remaining_requirements": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional requirement_ids from GoalRead; omit to conservatively track all requirements."
                    },
                    "blocker": {
                        "type": "string",
                        "description": "Only for blocked: the objective external dependency. Omit when there is no blocker."
                    }
                },
                "required": ["decision", "summary"],
                "additionalProperties": false
            }),
        )
        .with_approval(ApprovalTier::Write)
        .with_result_kind(ToolResultKind::Control)
        .with_execution(ToolExecutionMode::Sequential)
        .with_mutating(true)
        .with_storm_exempt(true)
        .with_role_scope(ToolRoleScope::PrimaryOnly)
        .with_ptc_exposure(ToolPtcExposure::DirectOnly)
        .with_side_effect_exempt(true)
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: ToolExecutionContext,
    ) -> Result<ToolOutput, ToolError> {
        let submission: GoalReviewSubmission = decode_tool_args(input).inspect_err(|error| {
            if let Some(session) = self.bundle.adjudication() {
                session.record_rejection(error.to_string());
            }
        })?;
        let session = self.bundle.adjudication().ok_or_else(|| {
            ToolError::new("GoalReview is only available during the host agent's adjudication turn")
        })?;
        let artifacts = self
            .bundle
            .artifacts
            .get()
            .ok_or_else(|| ToolError::new("artifact manager not initialized"))?;
        let tool_ctx = GoalReviewToolContext {
            cwd: ctx.cwd(),
            artifacts,
        };
        let result = match submission {
            GoalReviewSubmission::Decision(conclusion) => {
                session.submit_conclusion(conclusion, &tool_ctx)
            }
            GoalReviewSubmission::Full(review) => session.submit(review, &tool_ctx),
            GoalReviewSubmission::Continue(proposal) => session.submit_continue(proposal),
        };
        // A decoded but rejected conclusion is protocol feedback, not damaged
        // model output. Keep the failure visible and let the host correct it
        // in the same turn without spending the format-recovery window.
        if let Err(error) = result {
            session.record_rejection(format!("{error:#}"));
            return Ok(ToolOutput {
                content: format!(
                    "GoalReview rejected: {error:#}. No conclusion was recorded. Correct the reported fields and call GoalReview again. Evidence references must come from the independent report's inspected evidence or a successful Read; missing evidence uses [] and unknown findings."
                ),
                conversation_content: None,
                success: false,
                exit_code: None,
                structured_value: None,
            });
        }
        Ok(ToolOutput::text("Host goal conclusion recorded."))
    }
}

pub(crate) fn goal_draft_mask() -> ToolExecutionMask {
    ToolExecutionMask::allow_only(["Read", "Glob", "Grep", "GoalRead", "GoalDraft"])
        .with_exclusive_ptc(true)
        .with_block_custom_resource_schemes(true)
        .with_disable_native_web(true)
        .with_disable_replan(true)
        .with_block_reason(
            "Goal draft is awaiting human confirmation; only read-only exploration and GoalDraft are allowed",
        )
}

pub(crate) fn goal_review_mask() -> ToolExecutionMask {
    ToolExecutionMask::allow_only(["Read", "Glob", "Grep", "GoalRead"])
        .with_disable_native_web(true)
        .with_disable_replan(true)
        .with_block_reason("read-only goal reviewer")
}

pub(crate) fn goal_adjudication_mask() -> ToolExecutionMask {
    ToolExecutionMask::allow_only(["Read", "Glob", "Grep", "GoalRead", "GoalReview"])
        .with_exclusive_ptc(true)
        .with_block_custom_resource_schemes(true)
        .with_disable_native_web(true)
        .with_disable_replan(true)
        .with_block_reason("host adjudication only permits read-only inspection and GoalReview")
}

struct GoalCheckpointProvider {
    goal_store: Arc<OnceLock<Arc<GoalStore>>>,
}

impl CompactionCheckpointProvider for GoalCheckpointProvider {
    fn render_checkpoint(&self, max_chars: usize) -> anyhow::Result<Option<CompactionCheckpoint>> {
        let Some(store) = self.goal_store.get() else {
            return Ok(None);
        };
        let Some(value) = store.checkpoint(max_chars)? else {
            return Ok(None);
        };
        let content = value["content"].as_str().unwrap_or_default().to_string();
        let metadata = value["_mink"].clone();
        Ok(Some(CompactionCheckpoint { content, metadata }))
    }
}

struct GoalTurnPolicyProvider {
    goal_store: Arc<OnceLock<Arc<GoalStore>>>,
}

impl TurnPolicyProvider for GoalTurnPolicyProvider {
    fn ordinary_turn_mask(&self) -> Option<ToolExecutionMask> {
        let store = self.goal_store.get()?;
        if store
            .snapshot()
            .is_some_and(|g| g.status == GoalStatus::Draft)
        {
            Some(goal_draft_mask())
        } else {
            None
        }
    }

    fn ordinary_effect_journal(&self) -> Option<Arc<dyn ToolEffectJournal>> {
        let store = self.goal_store.get()?.clone();
        Some(Arc::new(GoalExecutionObserver { goal_store: store }))
    }
}

struct GoalExecutionObserver {
    goal_store: Arc<GoalStore>,
}

impl ToolEffectJournal for GoalExecutionObserver {
    fn tracks_durable_effects(&self) -> bool {
        self.goal_store
            .snapshot()
            .is_some_and(|g| g.phase == GoalPhase::Executing)
    }

    fn before_side_effect(&self, call_id: &str, _tool_name: &str) -> anyhow::Result<()> {
        self.goal_store.mark_effect(call_id)
    }

    fn has_pending_effect(&self, call_id: &str) -> bool {
        self.goal_store
            .snapshot()
            .is_some_and(|g| g.pending_effects.iter().any(|id| id == call_id))
    }

    fn after_known_effects(&self, known_call_ids: &[String]) -> anyhow::Result<()> {
        self.goal_store.close_effects(known_call_ids)
    }
}

pub(crate) struct GoalReviewToolObserver {
    session: Arc<dyn GoalReviewToolSession>,
    is_reviewer: bool,
}

impl GoalReviewToolObserver {
    pub(crate) fn for_reviewer(session: Arc<dyn GoalReviewToolSession>) -> Self {
        Self {
            session,
            is_reviewer: true,
        }
    }

    pub(crate) fn for_adjudication(session: Arc<dyn GoalReviewToolSession>) -> Self {
        Self {
            session,
            is_reviewer: false,
        }
    }
}

struct CapturedReadObservation {
    session: Arc<dyn GoalReviewToolSession>,
    captured: (String, PathBuf, String),
}

impl ReadObservation for CapturedReadObservation {
    fn verify_bytes(&self, bytes: &[u8]) -> anyhow::Result<()> {
        let actual: String = Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        anyhow::ensure!(
            self.captured.2 == actual,
            "image evidence changed during read"
        );
        Ok(())
    }

    fn on_succeeded(self: Box<Self>) -> anyhow::Result<()> {
        self.session.finish_read(self.captured)
    }
}

impl ToolCallObserver for GoalReviewToolObserver {
    fn on_parse_error(&self, tool_name: &str, error: &str) {
        if !self.is_reviewer && tool_name == "GoalReview" {
            self.session
                .record_rejection(format!("invalid tool arguments: {error}"));
        }
    }

    fn before_prepare_execute(&self, tool_name: &str) -> anyhow::Result<()> {
        if self.is_reviewer && matches!(tool_name, "Read" | "Glob" | "Grep") {
            self.session.mark_inspecting()?;
        }
        Ok(())
    }

    fn observe_read(
        &self,
        tool_name: &str,
        _call_id: &str,
        input: &serde_json::Value,
        ctx: &ReadEvidenceContext<'_>,
    ) -> anyhow::Result<Option<Box<dyn ReadObservation>>> {
        if tool_name != "Read" {
            return Ok(None);
        }
        let Some(raw) = input["path"].as_str() else {
            return Ok(None);
        };
        let captured = capture_read_path(raw, ctx.cwd(), ctx.artifacts_dir())?;
        Ok(captured.map(|captured| {
            Box::new(CapturedReadObservation {
                session: self.session.clone(),
                captured,
            }) as Box<dyn ReadObservation>
        }))
    }
}

pub(crate) fn capture_read_path(
    raw: &str,
    cwd: &Path,
    artifacts_dir: &Path,
) -> anyhow::Result<Option<(String, PathBuf, String)>> {
    let selection = split_read_path_selection(raw)?;
    let (reference, path) = if let Some(id) = selection.path.strip_prefix("artifact://") {
        anyhow::ensure!(
            !id.contains('/') && !id.contains(".."),
            "invalid artifact reference"
        );
        (
            selection.path.clone(),
            artifacts_dir.join(format!("{id}.txt")),
        )
    } else if selection.path.contains("://") {
        return Ok(None);
    } else {
        let path = Path::new(&selection.path);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };
        let Ok(path) = path.canonicalize() else {
            return Ok(None);
        };
        if !path.starts_with(cwd.canonicalize()?) {
            return Ok(None);
        }
        (path.to_string_lossy().into_owned(), path)
    };
    let Ok(hash) = hash_file(&path) else {
        return Ok(None);
    };
    Ok(Some((reference, path, hash)))
}

fn hash_file(path: &Path) -> anyhow::Result<String> {
    use std::io::Read;
    anyhow::ensure!(
        std::fs::metadata(path)?.is_file(),
        "evidence must be a regular file"
    );
    let mut file = std::fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(digest
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}
