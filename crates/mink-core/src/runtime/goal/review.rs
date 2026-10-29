//! Host-owned, read-only reviews of one durable execution boundary.
use super::*;
use crate::resources::selector::split_read_path_selection;
use std::collections::BTreeMap;

type EvidenceVersions = Vec<(String, String)>;
type EvidenceRemapping = BTreeMap<String, String>;
type StageObserver = Arc<dyn Fn(&GoalSnapshot) + Send + Sync>;

pub(crate) struct GoalReviewSession {
    pub(crate) goal: GoalSnapshot,
    parent_goal_store: Arc<crate::session::goal::GoalStore>,
    usage: Arc<crate::session::usage::UsageJournal>,
    /// A host-agent conclusion mailbox, separate from GoalStore. The read-only
    /// reviewer cannot submit to it; the runtime commits only validated results.
    pub(crate) submission: Mutex<Option<GoalReview>>,
    pub(crate) continuation: Mutex<Option<GoalContinueProposal>>,
    observer: Mutex<Option<StageObserver>>,
    last_rejection: Mutex<Option<String>>,
    pub(crate) reads: Mutex<BTreeMap<String, String>>,
    artifact_paths: Mutex<BTreeMap<String, PathBuf>>,
}

impl GoalReviewSession {
    pub(crate) fn new(
        goal: GoalSnapshot,
        parent_goal_store: Arc<crate::session::goal::GoalStore>,
        usage: Arc<crate::session::usage::UsageJournal>,
    ) -> Self {
        Self {
            goal,
            parent_goal_store,
            usage,
            submission: Mutex::new(None),
            continuation: Mutex::new(None),
            observer: Mutex::new(None),
            last_rejection: Mutex::new(None),
            reads: Mutex::new(BTreeMap::new()),
            artifact_paths: Mutex::new(BTreeMap::new()),
        }
    }

    pub(crate) fn has_submission(&self) -> bool {
        self.submission.lock().unwrap().is_some() || self.continuation.lock().unwrap().is_some()
    }
    pub(crate) fn snapshot_for_read(&self) -> GoalSnapshot {
        self.parent_goal_store
            .snapshot()
            .filter(|g| g.review_candidate == self.goal.review_candidate)
            .unwrap_or_else(|| self.goal.clone())
    }
    pub(crate) fn set_stage(&self, stage: GoalReviewStage) -> Result<()> {
        let current = self
            .parent_goal_store
            .snapshot()
            .ok_or_else(|| anyhow::anyhow!("goal missing"))?;
        ensure!(
            current.status == GoalStatus::Active
                && current.review_candidate == self.goal.review_candidate,
            "review candidate is no longer active"
        );
        if current.review_stage != Some(stage) {
            let next = self.parent_goal_store.transition(
                &current.id,
                current.revision,
                GoalTransition::SetReviewStage {
                    expected: self
                        .goal
                        .review_candidate
                        .clone()
                        .ok_or_else(|| anyhow::anyhow!("missing review candidate"))?,
                    stage,
                },
            )?;
            if let Some(observer) = self.observer.lock().unwrap().as_ref() {
                observer(&next);
            }
        }
        Ok(())
    }
    pub(crate) fn submit_continue(&self, proposal: GoalContinueProposal) -> Result<()> {
        let current = self.snapshot_for_read();
        ensure!(
            current.status == GoalStatus::Active
                && current.review_candidate == self.goal.review_candidate,
            "review candidate is no longer active"
        );
        proposal.validate(&current)?;
        ensure!(
            current
                .review_report
                .as_ref()
                .is_some_and(|r| r.candidate == proposal.candidate),
            "a successful independent descriptive report is required"
        );
        ensure!(
            self.submission.lock().unwrap().is_none(),
            "full conclusion already submitted"
        );
        let mut mailbox = self.continuation.lock().unwrap();
        ensure!(
            mailbox.as_ref().is_none_or(|p| p == &proposal),
            "different continuation already submitted"
        );
        *mailbox = Some(proposal);
        Ok(())
    }

    pub(crate) fn record_rejection(&self, reason: String) {
        *self
            .last_rejection
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(reason);
    }

    /// The review has its own request loop, but spends against the authoritative
    /// parent goal budget. Reconcile every completed attempt before admitting
    /// another request so continuations, retries and summaries see the same
    /// usage as the host scheduler.
    pub(crate) fn check_budget(&self) -> Result<()> {
        self.parent_goal_store
            .reconcile_usage(&self.usage.all_records()?)?;
        let reason = self
            .parent_goal_store
            .snapshot()
            .filter(|current| current.id == self.goal.id)
            .and_then(|current| current.review_budget_reason().map(str::to_string));
        if let Some(reason) = reason {
            return Err(anyhow::Error::new(GoalReviewBudgetExhausted { reason }));
        }
        Ok(())
    }

    pub(crate) fn capture_read(
        &self,
        call: &crate::protocol::ToolCallEvent,
        ctx: &crate::goal_review::GoalReviewToolContext<'_>,
    ) -> Result<Option<(String, PathBuf, String)>> {
        if call.name != "Read" {
            return Ok(None);
        }
        let Some(raw) = call.input_json["path"].as_str() else {
            return Ok(None);
        };
        let selection = split_read_path_selection(raw)?;
        let (reference, path) = if let Some(id) = selection.path.strip_prefix("artifact://") {
            ensure!(
                !id.contains('/') && !id.contains(".."),
                "invalid artifact reference"
            );
            (
                selection.path.clone(),
                ctx.artifact_dir.join(format!("{id}.txt")),
            )
        } else if selection.path.contains("://") {
            return Ok(None);
        } else {
            let path = Path::new(&selection.path);
            let path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                ctx.cwd.join(path)
            };
            let Ok(path) = path.canonicalize() else {
                return Ok(None);
            };
            if !path.starts_with(ctx.cwd.canonicalize()?) {
                return Ok(None);
            }
            (path.to_string_lossy().into_owned(), path)
        };
        let Ok(hash) = hash_file(&path) else {
            return Ok(None);
        };
        Ok(Some((reference, path, hash)))
    }
    pub(crate) fn finish_read(
        &self,
        (reference, path, hash): (String, PathBuf, String),
    ) -> Result<()> {
        ensure!(hash_file(&path)? == hash, "evidence changed while reading");
        let mut reads = self.reads.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(old) = reads.get(&reference) {
            ensure!(old == &hash, "evidence changed between reads");
        }
        if reference.starts_with("artifact://") {
            self.artifact_paths
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(reference.clone(), path);
        }
        reads.insert(reference, hash);
        Ok(())
    }
    /// Import the independent reader's evidence before the host interprets its report.
    pub(crate) fn import_evidence(
        &self,
        parent: &AgentRuntimeHandle,
    ) -> Result<(EvidenceVersions, EvidenceRemapping)> {
        let mut reads = self.reads.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let paths = self
            .artifact_paths
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut replacements = BTreeMap::new();
        for (reference, path) in paths.iter() {
            let hash = reads
                .get(reference)
                .ok_or_else(|| anyhow::anyhow!("missing artifact read version"))?;
            let content = std::fs::read_to_string(path)?;
            ensure!(
                hash_bytes(content.as_bytes()) == *hash && hash_file(path)? == *hash,
                "reviewer artifact changed after inspection"
            );
            let id = reference.strip_prefix("artifact://").unwrap();
            let parent_path = parent.session.artifacts_dir.join(format!("{id}.txt"));
            match parent.goal_ctx.artifacts.find(id)? {
                Some(_) => {
                    let original = parent.goal_ctx.artifacts.read_text(id)?;
                    ensure!(
                        hash_bytes(original.as_bytes()) == *hash,
                        "inherited artifact changed after inspection"
                    );
                }
                None => {
                    ensure!(
                        !parent_path.try_exists()?,
                        "unindexed parent artifact identity"
                    );
                    let record = parent.goal_ctx.artifacts.write_text(
                        "GoalReviewEvidence",
                        "read-only reviewer evidence",
                        &content,
                    )?;
                    let replacement = format!("artifact://{}", record.id);
                    ensure!(
                        parent.evidence_version(&replacement)? == *hash,
                        "imported reviewer artifact does not match inspected bytes"
                    );
                    replacements.insert(reference.clone(), replacement);
                }
            }
        }
        for (old, new) in &replacements {
            let hash = reads
                .remove(old)
                .ok_or_else(|| anyhow::anyhow!("missing imported read version"))?;
            reads.insert(new.clone(), hash);
        }
        drop(paths);
        *self.reads.lock().unwrap_or_else(|e| e.into_inner()) = reads.clone();
        let mut paths = self
            .artifact_paths
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for (old, new) in &replacements {
            paths.remove(old);
            let id = new
                .strip_prefix("artifact://")
                .expect("imported artifact reference");
            paths.insert(
                new.clone(),
                parent.session.artifacts_dir.join(format!("{id}.txt")),
            );
        }
        Ok((reads.into_iter().collect(), replacements))
    }

    /// Publish child-only read evidence through the parent's artifact store.
    /// Forked parent artifacts retain their identity and must still match the
    /// inspected bytes; fresh reviewer outputs receive a parent-owned identity.
    pub(crate) fn export(
        &self,
        parent: &AgentRuntimeHandle,
    ) -> Result<(GoalReview, Vec<(String, String)>)> {
        let mut feedback = self
            .submission
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| anyhow::anyhow!("missing host goal conclusion"))?;
        let (reads, replacements) = self.import_evidence(parent)?;
        let remap = |refs: &mut Vec<String>| {
            for reference in refs {
                if let Some(replacement) = replacements.get(reference) {
                    *reference = replacement.clone();
                }
            }
        };
        if let Some(original) = &mut feedback.original_request {
            remap(&mut original.evidence);
        }
        for check in &mut feedback.criteria {
            remap(&mut check.evidence);
        }
        for check in &mut feedback.constraints {
            remap(&mut check.evidence);
        }
        if let Some(blocker) = &mut feedback.blocker {
            remap(&mut blocker.evidence);
        }
        Ok((feedback, reads))
    }
    pub(crate) fn submit_conclusion(
        &self,
        conclusion: GoalHostConclusion,
        ctx: &crate::goal_review::GoalReviewToolContext<'_>,
    ) -> Result<()> {
        let current = self.snapshot_for_read();
        ensure!(
            current.status == GoalStatus::Active
                && current.review_candidate == self.goal.review_candidate,
            "review candidate is no longer active"
        );
        ensure!(!conclusion.summary.trim().is_empty(), "summary is required");
        ensure!(
            serde_json::to_vec(&conclusion)?.len() <= 64 * 1024,
            "conclusion too large"
        );
        ensure!(
            conclusion.decision == GoalHostDecision::Blocked || conclusion.blocker.is_none(),
            "no external blocker: omit blocker for continue/complete"
        );
        let report = current
            .review_report
            .clone()
            .filter(|r| Some(&r.candidate) == current.review_candidate.as_ref())
            .ok_or_else(|| anyhow::anyhow!("a successful independent report is required"))?;
        let artifact = report
            .artifact
            .strip_prefix("artifact://")
            .ok_or_else(|| anyhow::anyhow!("invalid report reference"))?;
        ensure!(
            hash_bytes(ctx.artifacts.read_text(artifact)?.as_bytes()) == report.content_hash,
            "independent report changed"
        );
        let remaining = if conclusion.remaining_requirements.is_empty() {
            current.requirement_ids()
        } else {
            conclusion.remaining_requirements.clone()
        };
        let valid: std::collections::BTreeSet<_> = current.requirement_ids().into_iter().collect();
        let actual: std::collections::BTreeSet<_> = remaining.iter().cloned().collect();
        ensure!(
            actual.len() == remaining.len() && actual.is_subset(&valid),
            "remaining_requirements must contain unique GoalRead requirement_ids"
        );
        if conclusion.decision == GoalHostDecision::Continue && !current.review_requires_full {
            return self.submit_continue(GoalContinueProposal {
                candidate: report.candidate,
                summary: conclusion.summary,
                remaining_requirements: remaining,
                next_step: conclusion.next_step,
            });
        }
        ensure!(
            self.continuation.lock().unwrap().is_none(),
            "continuation already submitted"
        );
        let verdict = match conclusion.decision {
            GoalHostDecision::Continue => GoalReviewVerdict::NeedsWork,
            GoalHostDecision::Complete => GoalReviewVerdict::Satisfied,
            GoalHostDecision::Blocked => GoalReviewVerdict::Blocked,
        };
        let blocker = if verdict == GoalReviewVerdict::Blocked {
            let mut evidence = Vec::new();
            for (reference, _) in &report.evidence_versions {
                if let Some(id) = reference.strip_prefix("artifact://") {
                    let record = ctx.artifacts.get(id)?;
                    if matches!(
                        record.tool.as_str(),
                        "GoalExecutionConclusion" | "IndependentGoalReview"
                    ) {
                        continue;
                    }
                }
                evidence.push(reference.clone());
            }
            // Prefer inspectable dependency files to incidental per-round
            // artifacts. Claims and review narration are not dependency facts.
            if evidence.iter().any(|r| !r.starts_with("artifact://")) {
                evidence.retain(|r| !r.starts_with("artifact://"));
            }
            let mut ids: Vec<_> = current
                .spec
                .criteria
                .iter()
                .filter(|c| remaining.contains(&c.id))
                .map(|c| c.id.clone())
                .collect();
            if ids.is_empty() {
                ids = current.spec.criteria.iter().map(|c| c.id.clone()).collect();
            }
            Some(GoalBlocker {
                category: "external".into(),
                criterion_ids: ids,
                dependency: conclusion.blocker.unwrap_or_default(),
                evidence,
            })
        } else {
            None
        };
        let review = GoalReview {
            candidate: report.candidate.clone(),
            verdict,
            report: Some(Box::new(report)),
            original_request: None,
            criteria: vec![],
            constraints: vec![],
            summary: conclusion.summary,
            next_step: conclusion.next_step,
            blocker,
        };
        review.validate(&current)?;
        let mut mailbox = self.submission.lock().unwrap();
        ensure!(
            mailbox.as_ref().is_none_or(|r| r == &review),
            "different conclusion already submitted"
        );
        *mailbox = Some(review);
        Ok(())
    }

    pub(crate) fn submit(
        &self,
        feedback: GoalReview,
        ctx: &crate::goal_review::GoalReviewToolContext<'_>,
    ) -> Result<()> {
        ensure!(
            self.continuation.lock().unwrap().is_none(),
            "continuation already submitted"
        );
        ensure!(
            feedback.report.is_none(),
            "report binding is runtime-owned; submit decision and summary instead"
        );
        feedback.validate(&self.goal)?;
        let expected = feedback.feedback().into_review(&self.goal)?;
        ensure!(
            feedback.verdict == expected.verdict,
            "verdict does not match the requirement findings"
        );
        ensure!(
            self.parent_goal_store
                .snapshot()
                .is_some_and(|current| current.status == GoalStatus::Active
                    && current.review_candidate == self.goal.review_candidate),
            "review candidate is no longer active"
        );
        let reads = self.reads.lock().unwrap_or_else(|e| e.into_inner());
        for reference in feedback.evidence() {
            let (key, path) = if let Some(id) = reference.strip_prefix("artifact://") {
                ensure!(
                    !id.contains('/') && !id.contains(".."),
                    "invalid artifact reference"
                );
                (
                    reference.clone(),
                    ctx.artifact_dir.join(format!("{id}.txt")),
                )
            } else {
                let path = Path::new(reference.strip_prefix("file://").unwrap_or(&reference));
                let path = if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    ctx.cwd.join(path)
                }
                .canonicalize()?;
                ensure!(
                    path.starts_with(ctx.cwd.canonicalize()?),
                    "evidence outside working directory"
                );
                (path.to_string_lossy().into_owned(), path)
            };
            let hash = hash_file(&path)?;
            ensure!(
                reads.get(&key) == Some(&hash),
                "review evidence must be inspected with Read and remain unchanged: {reference}"
            );
        }
        let mut result = self.submission.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            result.as_ref().is_none_or(|old| old == &feedback),
            "host conclusion already submitted with different facts"
        );
        *result = Some(feedback);
        Ok(())
    }
}

impl crate::goal_review::GoalReviewToolSession for GoalReviewSession {
    fn billing_turn_id(&self) -> String {
        format!(
            "goal-review:{}:{}",
            self.goal.id,
            self.goal.iterations.len()
        )
    }

    fn snapshot_for_read(&self) -> GoalSnapshot {
        GoalReviewSession::snapshot_for_read(self)
    }

    fn mark_inspecting(&self) -> anyhow::Result<()> {
        self.set_stage(GoalReviewStage::Inspecting)
    }

    fn check_budget(&self) -> anyhow::Result<()> {
        GoalReviewSession::check_budget(self)
    }

    fn record_rejection(&self, reason: String) {
        GoalReviewSession::record_rejection(self, reason)
    }

    fn submit_continue(&self, proposal: GoalContinueProposal) -> Result<()> {
        GoalReviewSession::submit_continue(self, proposal)
    }

    fn submit(
        &self,
        review: GoalReview,
        ctx: &crate::goal_review::GoalReviewToolContext<'_>,
    ) -> Result<()> {
        GoalReviewSession::submit(self, review, ctx)
    }

    fn submit_conclusion(
        &self,
        conclusion: GoalHostConclusion,
        ctx: &crate::goal_review::GoalReviewToolContext<'_>,
    ) -> Result<()> {
        GoalReviewSession::submit_conclusion(self, conclusion, ctx)
    }

    fn capture_read(
        &self,
        call: &crate::protocol::ToolCallEvent,
        ctx: &crate::goal_review::GoalReviewToolContext<'_>,
    ) -> Result<Option<(String, PathBuf, String)>> {
        GoalReviewSession::capture_read(self, call, ctx)
    }

    fn finish_read(&self, captured: (String, PathBuf, String)) -> Result<()> {
        GoalReviewSession::finish_read(self, captured)
    }
}

#[derive(Debug)]
pub(crate) struct GoalReviewBudgetExhausted {
    pub(crate) reason: String,
}

impl std::fmt::Display for GoalReviewBudgetExhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "goal review budget exhausted: {}", self.reason)
    }
}

impl std::error::Error for GoalReviewBudgetExhausted {}

pub(super) fn execution_conclusion(
    history: &[serde_json::Value],
    start: usize,
    end: usize,
) -> String {
    history
        .iter()
        .filter(|message| {
            message["seq"]
                .as_u64()
                .is_some_and(|sequence| (start as u64) < sequence && sequence <= end as u64)
        })
        .rfind(|m| m["role"] == "assistant")
        .and_then(|m| m["content"].as_array())
        .filter(|blocks| !blocks.iter().any(|b| b["type"] == "tool_use"))
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

impl AgentRuntimeHandle {
    pub(crate) async fn run_goal_review(
        &self,
        goal: GoalSnapshot,
        session: Arc<GoalReviewSession>,
    ) -> Result<usize> {
        let candidate = goal
            .review_candidate
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("missing review candidate"))?;
        let session_dir = self
            .goal_ctx
            .store
            .path()
            .parent()
            .ok_or_else(|| anyhow::anyhow!("conversation path has no session directory"))?;
        let (history, committed_sequence) =
            crate::runtime::session::SessionReader::new(session_dir.to_path_buf())
                .conversation_with_sequence()?;
        let start = goal.iterations.last().map_or(0, |i| i.conversation_start);
        ensure!(
            candidate.conversation_seq <= committed_sequence && start <= candidate.conversation_seq,
            "execution conclusion boundary is outside committed history"
        );
        let conclusion = execution_conclusion(&history, start, candidate.conversation_seq);
        if conclusion.trim().is_empty() && !session.snapshot_for_read().review_requires_full {
            let current = session.snapshot_for_read();
            let next = self.goal_ctx.goal_store.transition(
                &current.id,
                current.revision,
                GoalTransition::MarkReviewAmbiguous,
            )?;
            self.announce_goal(&next);
        }
        let conclusion_artifact = self.goal_ctx.artifacts.write_text(
            "GoalExecutionConclusion",
            "final committed execution response",
            &conclusion,
        )?;
        let owner = self.clone();
        *session.observer.lock().unwrap() = Some(Arc::new(move |g| owner.announce_goal(g)));
        let mut child = None;
        loop {
            session.check_budget()?;
            let mut current = session.snapshot_for_read();
            let cached = current.review_report.clone().filter(|r| {
                r.candidate == *candidate
                    && r.conversation_seq == current.acceptance_conversation_seq
                    && committed_sequence <= r.conversation_seq
                    && self
                        .goal_ctx
                        .artifacts
                        .read_text(r.artifact.strip_prefix("artifact://").unwrap_or(""))
                        .is_ok_and(|text| hash_bytes(text.as_bytes()) == r.content_hash)
                    && r.evidence_versions.iter().all(|(reference, hash)| {
                        self.evidence_version(reference)
                            .is_ok_and(|actual| actual == *hash)
                    })
            });
            if current.review_report.is_some() && cached.is_none() {
                let next = self.goal_ctx.goal_store.transition(
                    &current.id,
                    current.revision,
                    GoalTransition::InvalidateReviewReport,
                )?;
                self.announce_goal(&next);
                current = session.snapshot_for_read();
            }
            let report = if let Some(report) = cached {
                ensure!(
                    self.goal_ctx.store.committed_len().await? == report.conversation_seq,
                    "conversation changed before host adjudication"
                );
                for (reference, hash) in &report.evidence_versions {
                    ensure!(
                        self.evidence_version(reference)? == *hash,
                        "cached review evidence changed"
                    );
                }
                session
                    .reads
                    .lock()
                    .unwrap()
                    .extend(report.evidence_versions.clone());
                report
            } else {
                if child.is_none() {
                    let executor = crate::agent::sub_executor::SubAgentExecutor::new_with_review(
                        self.goal_ctx.clone(),
                        format!(
                            "sub_review_{}",
                            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
                        ),
                        self.goal_ctx.cancel.linked_child_token(),
                        session.clone(),
                    )
                    .await?;
                    child = Some(crate::agent::sub_executor::ReviewAgentSession::new(
                        executor,
                    ));
                }
                session.set_stage(if current.review_requires_full {
                    GoalReviewStage::Inspecting
                } else {
                    GoalReviewStage::Screening
                })?;
                let prompt = format!(
                    "<independent-goal-review>\nCandidate: {}\nContract: {}\nRequirement IDs: {}\nExecution turn: {}\nFinal execution conclusion: {}\nFull conclusion: artifact://{}\nPrior reviews: {}\nPrior continuation: {}\nFull evidence review required: {}\nEscalation reason: {}\nYou are an independent read-only reviewer. First interpret the host's FINAL conclusion against the confirmed contract. If it explicitly leaves necessary contract work unfinished, identifies specific remaining requirements and a concrete executable next step, and does not claim the whole Goal is complete or contradict itself, you may end with a short descriptive report of those remaining requirements and the next step WITHOUT inspecting workspace files. State that full evidence verification was not performed. Optional improvements outside the contract do not justify continuing. A completed execution round is not a completed Goal. If the conclusion claims the whole goal complete, is empty, ambiguous or contradictory, the remaining work cannot be mapped to the contract, there is an external blocker, or full evidence review is required, inspect files and artifacts with Read and return observations for the original request, EVERY criterion and constraint. Distinguish claims from inspected facts, missing evidence and objective external dependencies. Cite inspected references. Never invent requirements. End naturally with descriptive prose. Do not call GoalReview or control the goal; the host agent submits the conclusion.\n</independent-goal-review>",
                    serde_json::to_string(candidate)?,
                    serde_json::to_string(&current.spec)?,
                    serde_json::to_string(&current.requirement_ids())?,
                    goal.iterations.last().map_or("", |i| i.turn_id.as_str()),
                    crate::tools::runner::format_tool_result(
                        &conclusion,
                        self.goal_ctx.tool_config.tool_result_max_bytes
                    ),
                    conclusion_artifact.id,
                    serde_json::to_string(&current.reviews.last())?,
                    serde_json::to_string(&current.continuations.last())?,
                    current.review_requires_full,
                    current
                        .review_escalation_reason
                        .as_deref()
                        .unwrap_or("none"),
                );
                let result = child
                    .as_mut()
                    .expect("review session initialized")
                    .execute(prompt)
                    .await;
                session.check_budget()?;
                ensure!(
                    result.status == crate::agent::sub_executor::SubAgentStatus::Succeeded,
                    "independent reviewer failed before returning a report: {}",
                    result
                        .diagnostic
                        .as_deref()
                        .filter(|diagnostic| !diagnostic.is_empty())
                        .unwrap_or(&result.text)
                );
                ensure!(
                    !result.text.trim().is_empty(),
                    "independent reviewer returned an empty report"
                );
                let (reads, mapping) = session.import_evidence(self)?;
                let seq = self.goal_ctx.store.committed_len().await?;
                ensure!(
                    seq == current.acceptance_conversation_seq,
                    "conversation changed before host adjudication"
                );
                let artifact = self.goal_ctx.artifacts.write_text(
                    "IndependentGoalReview",
                    "independent descriptive audit report",
                    &result.text,
                )?;
                let report = GoalReviewReport {
                    candidate: candidate.clone(),
                    artifact: format!("artifact://{}", artifact.id),
                    content_hash: hash_bytes(result.text.as_bytes()),
                    reference_mapping: mapping.into_iter().collect(),
                    evidence_versions: reads,
                    conversation_seq: seq,
                };
                self.publish_review_report(&session, report.clone())?;
                report
            };
            let report_text = self
                .goal_ctx
                .artifacts
                .read_text(report.artifact.strip_prefix("artifact://").unwrap_or(""))?;
            session.set_stage(GoalReviewStage::Adjudicating)?;
            let current = session.snapshot_for_read();
            let prompt = format!(
                "<goal-review>\nCandidate: {}\nContract: {}\nRequirement IDs: {}\nFinal execution conclusion: {}\nIndependent report: {}\nFull report: {}\nInspected evidence versions: {}\nImported reference mapping: {}\nFull evidence review required: {}\nYou are the HOST agent. Use the independent report to call GoalReview with decision=continue, complete or blocked, summary, and next_step for unfinished work. continue is valid whenever necessary contract work remains, even after a full evidence inspection. complete requires the independent reviewer to have checked the entire contract and supporting evidence. blocked requires an objective external dependency; only then include blocker as a short description. Optional remaining_requirements may list GoalRead requirement_ids. The runtime binds the candidate, report and evidence: do not supply candidate, mode, verdict or item tables. Do not repeat the audit. If a call is rejected, correct the specific diagnostic and call again in this turn. Prose alone does not update state. Only inspect and submit; do not execute or modify the task.\n</goal-review>",
                serde_json::to_string(candidate)?,
                serde_json::to_string(&current.spec)?,
                serde_json::to_string(&current.requirement_ids())?,
                crate::tools::runner::format_tool_result(
                    &conclusion,
                    self.goal_ctx.tool_config.tool_result_max_bytes
                ),
                crate::tools::runner::format_tool_result(
                    &report_text,
                    self.goal_ctx.tool_config.tool_result_max_bytes
                ),
                report.artifact,
                serde_json::to_string(&report.evidence_versions)?,
                serde_json::to_string(&report.reference_mapping)?,
                current.review_requires_full,
            );
            let mut host = crate::agent::turn::TurnExecutor::new_for_goal_adjudication(
                self.goal_ctx.clone(),
                session.clone(),
            );
            let outcome = self
                .goal_ctx
                .usage
                .with_goal_adjudication(
                    format!("goal-review:{}:{}", goal.id, goal.iterations.len()),
                    host.execute(&prompt, None),
                )
                .await;
            self.goal_ctx.store.sync().await?;
            let seq = self.goal_ctx.store.committed_len().await?;
            // Bind a reusable report to the host history even when submission failed.
            let mut report = report;
            report.conversation_seq = seq;
            if !self.goal_ctx.cancel.is_cancelled()
                && !self.goal_ctx.interrupt.load(Ordering::SeqCst)
            {
                self.publish_review_report(&session, report)?;
            }
            session.check_budget()?;
            if !session.has_submission() {
                let detail = match outcome {
                    Ok((decision, _)) => format!("{decision:?}"),
                    Err(error) => format!("{error:#}"),
                };
                anyhow::bail!(
                    "host agent did not submit a valid goal conclusion: {detail}; last rejection: {}",
                    session
                        .last_rejection
                        .lock()
                        .unwrap()
                        .as_deref()
                        .unwrap_or("none")
                );
            }
            let proposal = session.continuation.lock().unwrap().clone();
            if let Some(proposal) = proposal {
                let current = session.snapshot_for_read();
                let fingerprint = proposal.fingerprint()?;
                let rounds = if current.continuation_fingerprint.as_ref() == Some(&fingerprint) {
                    current.continuation_rounds.saturating_add(1)
                } else {
                    1
                };
                if rounds >= 3 {
                    let next = self.goal_ctx.goal_store.transition(
                        &current.id,
                        current.revision,
                        GoalTransition::EscalateRepeatedContinuation {
                            conversation_seq: seq,
                        },
                    )?;
                    self.announce_goal(&next);
                    *session.continuation.lock().unwrap() = None;
                    continue;
                }
            }
            return Ok(seq);
        }
    }

    fn publish_review_report(
        &self,
        session: &GoalReviewSession,
        report: GoalReviewReport,
    ) -> Result<()> {
        let current = session.snapshot_for_read();
        let next = self.goal_ctx.goal_store.transition(
            &current.id,
            current.revision,
            GoalTransition::PublishReviewReport { report },
        )?;
        self.announce_goal(&next);
        Ok(())
    }

    pub(crate) fn validate_review_evidence(
        &self,
        goal: &GoalSnapshot,
    ) -> Result<Vec<(String, String)>> {
        let review = goal
            .reviews
            .last()
            .ok_or_else(|| anyhow::anyhow!("no independent review"))?;
        review.validate(goal)?;
        if let Some(report) = &review.report {
            let id = report
                .artifact
                .strip_prefix("artifact://")
                .ok_or_else(|| anyhow::anyhow!("invalid report reference"))?;
            ensure!(
                hash_bytes(self.goal_ctx.artifacts.read_text(id)?.as_bytes())
                    == report.content_hash,
                "independent report changed"
            );
        }
        let mut versions: BTreeMap<String, String> =
            goal.evidence_versions.iter().cloned().collect();
        for reference in review.evidence() {
            let key = if reference.starts_with("artifact://") {
                reference.clone()
            } else {
                let path = Path::new(reference.strip_prefix("file://").unwrap_or(&reference));
                let path = if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    self.goal_ctx.cwd.join(path)
                }
                .canonicalize()?;
                path.to_string_lossy().into_owned()
            };
            ensure!(
                versions.get(&key) == Some(&self.evidence_version(&reference)?),
                "review evidence changed or was not inspected"
            );
        }
        for (reference, hash) in &mut versions {
            *hash = self.evidence_version(reference)?;
        }
        Ok(versions.into_iter().collect())
    }
}
