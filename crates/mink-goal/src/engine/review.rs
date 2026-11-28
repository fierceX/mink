//! Host-owned, read-only reviews of one durable execution boundary.
use super::*;
use mink::runtime::ext::{
    BudgetExhaustedError, EphemeralChildOptions, EphemeralChildSession, EphemeralChildStatus,
    InternalTurnRequest, TurnPermit,
};
use mink::runtime::{SessionReader, UsageRecord};
use std::collections::BTreeMap;

type EvidenceVersions = Vec<(String, String)>;
type EvidenceRemapping = BTreeMap<String, String>;
type StageObserver = Arc<dyn Fn(&GoalSnapshot) + Send + Sync>;

#[derive(Debug, Clone)]
enum ReviewMailbox {
    Conclusion(GoalReview),
    Continuation(GoalContinueProposal),
}

pub(crate) struct GoalReviewSession {
    pub(crate) goal: GoalSnapshot,
    parent_goal_store: Arc<GoalStore>,
    usage_records: Arc<dyn Fn() -> Result<Vec<UsageRecord>> + Send + Sync>,
    /// Single host-agent conclusion/continuation mailbox, separate from
    /// GoalStore. Holding both mutually exclusive states behind one lock
    /// eliminates check-then-lock races between `submit_continue` and
    /// `submit_conclusion`/`submit`.
    mailbox: Mutex<Option<ReviewMailbox>>,
    observer: Mutex<Option<StageObserver>>,
    last_rejection: Mutex<Option<String>>,
    pub(crate) reads: Mutex<BTreeMap<String, String>>,
    artifact_paths: Mutex<BTreeMap<String, PathBuf>>,
}

impl GoalReviewSession {
    pub(crate) fn new(
        goal: GoalSnapshot,
        parent_goal_store: Arc<GoalStore>,
        usage_records: Arc<dyn Fn() -> Result<Vec<UsageRecord>> + Send + Sync>,
    ) -> Self {
        Self {
            goal,
            parent_goal_store,
            usage_records,
            mailbox: Mutex::new(None),
            observer: Mutex::new(None),
            last_rejection: Mutex::new(None),
            reads: Mutex::new(BTreeMap::new()),
            artifact_paths: Mutex::new(BTreeMap::new()),
        }
    }

    pub(crate) fn has_submission(&self) -> bool {
        self.mailbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    pub(crate) fn submitted_conclusion(&self) -> Option<GoalReview> {
        match self
            .mailbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            Some(ReviewMailbox::Conclusion(review)) => Some(review.clone()),
            _ => None,
        }
    }

    pub(crate) fn submitted_continuation(&self) -> Option<GoalContinueProposal> {
        match self
            .mailbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            Some(ReviewMailbox::Continuation(proposal)) => Some(proposal.clone()),
            _ => None,
        }
    }

    pub(crate) fn clear_continuation(&self) {
        let mut mailbox = self.mailbox.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(*mailbox, Some(ReviewMailbox::Continuation(_))) {
            *mailbox = None;
        }
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
            // Clone the observer handle and release the lock before invoking it
            // so a re-entrant read or panic cannot deadlock/poison the mutex.
            let observer = self
                .observer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some(observer) = observer {
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
        let mut mailbox = self.mailbox.lock().unwrap_or_else(|e| e.into_inner());
        match mailbox.as_ref() {
            Some(ReviewMailbox::Conclusion(_)) => {
                anyhow::bail!("full conclusion already submitted")
            }
            Some(ReviewMailbox::Continuation(existing)) if existing != &proposal => {
                anyhow::bail!("different continuation already submitted")
            }
            _ => {
                *mailbox = Some(ReviewMailbox::Continuation(proposal));
            }
        }
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
            .reconcile_usage(&(self.usage_records)()?)?;
        let reason = self
            .parent_goal_store
            .snapshot()
            .filter(|current| current.id == self.goal.id)
            .and_then(|current| current.review_budget_reason().map(str::to_string));
        if let Some(reason) = reason {
            return Err(anyhow::Error::new(BudgetExhaustedError::new(format!(
                "goal review budget exhausted: {reason}"
            ))));
        }
        Ok(())
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
        parent: &GoalEngine,
    ) -> Result<(EvidenceVersions, EvidenceRemapping)> {
        let mut reads = self.reads.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let paths = self
            .artifact_paths
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let parent_artifacts_dir = parent
            .handle
            .ext()
            .artifact_reader()
            .artifacts_dir()
            .to_path_buf();
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
            let id = reference
                .strip_prefix("artifact://")
                .ok_or_else(|| anyhow::anyhow!("invalid artifact reference: {reference}"))?;
            let parent_path = parent_artifacts_dir.join(format!("{id}.txt"));
            match parent.handle.ext().find_artifact(id)? {
                Some(existing) => {
                    ensure!(
                        existing.sha256 == *hash,
                        "inherited artifact changed after inspection"
                    );
                }
                None => {
                    ensure!(
                        !parent_path.try_exists()?,
                        "unindexed parent artifact identity"
                    );
                    let record = parent.handle.ext().write_artifact(
                        "GoalReviewEvidence",
                        "read-only reviewer evidence",
                        &content,
                    )?;
                    let replacement = record.url;
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
            paths.insert(new.clone(), parent_artifacts_dir.join(format!("{id}.txt")));
        }
        Ok((reads.into_iter().collect(), replacements))
    }

    /// Publish child-only read evidence through the parent's artifact store.
    /// Forked parent artifacts retain their identity and must still match the
    /// inspected bytes; fresh reviewer outputs receive a parent-owned identity.
    pub(crate) fn export(
        &self,
        parent: &GoalEngine,
    ) -> Result<(GoalReview, Vec<(String, String)>)> {
        let mut feedback = self
            .submitted_conclusion()
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
        ctx: &crate::tools::GoalReviewToolContext<'_>,
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
            self.submitted_continuation().is_none(),
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
        let mut mailbox = self.mailbox.lock().unwrap_or_else(|e| e.into_inner());
        match mailbox.as_ref() {
            Some(ReviewMailbox::Continuation(_)) => {
                anyhow::bail!("continuation already submitted")
            }
            Some(ReviewMailbox::Conclusion(existing)) if existing != &review => {
                anyhow::bail!("different conclusion already submitted")
            }
            _ => {
                *mailbox = Some(ReviewMailbox::Conclusion(review));
            }
        }
        Ok(())
    }

    pub(crate) fn submit(
        &self,
        feedback: GoalReview,
        ctx: &crate::tools::GoalReviewToolContext<'_>,
    ) -> Result<()> {
        ensure!(
            self.submitted_continuation().is_none(),
            "continuation already submitted"
        );
        ensure!(
            feedback.report.is_none(),
            "report binding is runtime-owned; submit decision and summary instead"
        );
        feedback.validate(&self.goal)?;
        let expected = feedback
            .feedback()
            .ok_or_else(|| anyhow::anyhow!("itemized requirement findings are missing"))?
            .into_review(&self.goal)?;
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
                    ctx.artifacts.artifacts_dir().join(format!("{id}.txt")),
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
        let mut mailbox = self.mailbox.lock().unwrap_or_else(|e| e.into_inner());
        match mailbox.as_ref() {
            Some(ReviewMailbox::Continuation(_)) => {
                anyhow::bail!("continuation already submitted")
            }
            Some(ReviewMailbox::Conclusion(old)) if old != &feedback => {
                anyhow::bail!("host conclusion already submitted with different facts")
            }
            _ => {
                *mailbox = Some(ReviewMailbox::Conclusion(feedback));
            }
        }
        Ok(())
    }
}

impl crate::tools::GoalReviewToolSession for GoalReviewSession {
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

    fn record_rejection(&self, reason: String) {
        GoalReviewSession::record_rejection(self, reason)
    }

    fn submit_continue(&self, proposal: GoalContinueProposal) -> Result<()> {
        GoalReviewSession::submit_continue(self, proposal)
    }

    fn submit(
        &self,
        review: GoalReview,
        ctx: &crate::tools::GoalReviewToolContext<'_>,
    ) -> Result<()> {
        GoalReviewSession::submit(self, review, ctx)
    }

    fn submit_conclusion(
        &self,
        conclusion: GoalHostConclusion,
        ctx: &crate::tools::GoalReviewToolContext<'_>,
    ) -> Result<()> {
        GoalReviewSession::submit_conclusion(self, conclusion, ctx)
    }

    fn finish_read(&self, captured: (String, PathBuf, String)) -> Result<()> {
        GoalReviewSession::finish_read(self, captured)
    }
}

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

struct AdjudicationSlotGuard {
    bundle: crate::tools::GoalToolBundle,
}

impl Drop for AdjudicationSlotGuard {
    fn drop(&mut self) {
        self.bundle.set_adjudication(None);
    }
}

impl GoalEngine {
    pub(crate) async fn run_goal_review(
        &self,
        goal: GoalSnapshot,
        session: Arc<GoalReviewSession>,
        permit: &TurnPermit,
    ) -> Result<usize> {
        let candidate = goal
            .review_candidate
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("missing review candidate"))?;
        let session_dir = self.handle.ext().session_dir();
        let (history, committed_sequence) =
            SessionReader::new(session_dir).conversation_with_sequence()?;
        let start = goal.iterations.last().map_or(0, |i| i.conversation_start);
        ensure!(
            candidate.conversation_seq <= committed_sequence && start <= candidate.conversation_seq,
            "execution conclusion boundary is outside committed history"
        );
        let conclusion = execution_conclusion(&history, start, candidate.conversation_seq);
        if conclusion.trim().is_empty() && !session.snapshot_for_read().review_requires_full {
            let current = session.snapshot_for_read();
            let next = self.goal_store.transition(
                &current.id,
                current.revision,
                GoalTransition::MarkReviewAmbiguous,
            )?;
            self.announce_goal(&next);
        }
        let conclusion_artifact = self.handle.ext().write_artifact(
            "GoalExecutionConclusion",
            "final committed execution response",
            &conclusion,
        )?;
        let owner = self.clone();
        *session.observer.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(Arc::new(move |g| owner.announce_goal(g)));
        let mut child: Option<EphemeralChildSession> = None;
        loop {
            session.check_budget()?;
            let mut current = session.snapshot_for_read();
            let cached = current.review_report.clone().filter(|r| {
                r.candidate == *candidate
                    && r.conversation_seq == current.acceptance_conversation_seq
                    && committed_sequence <= r.conversation_seq
                    && self
                        .handle
                        .ext()
                        .read_artifact_text(r.artifact.strip_prefix("artifact://").unwrap_or(""))
                        .is_ok_and(|text| hash_bytes(text.as_bytes()) == r.content_hash)
                    && r.evidence_versions.iter().all(|(reference, hash)| {
                        self.evidence_version(reference)
                            .is_ok_and(|actual| actual == *hash)
                    })
            });
            if current.review_report.is_some() && cached.is_none() {
                let next = self.goal_store.transition(
                    &current.id,
                    current.revision,
                    GoalTransition::InvalidateReviewReport,
                )?;
                self.announce_goal(&next);
                current = session.snapshot_for_read();
            }
            let report = if let Some(report) = cached {
                ensure!(
                    self.handle.ext().committed_seq().await? == report.conversation_seq,
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
                    .unwrap_or_else(|e| e.into_inner())
                    .extend(report.evidence_versions.clone());
                report
            } else {
                if child.is_none() {
                    let allowed: Vec<String> = ["Read", "Glob", "Grep", "GoalRead"]
                        .into_iter()
                        .filter(|name| self.handle.ext().has_reachable_tool(name))
                        .map(str::to_string)
                        .collect();
                    let session_for_budget = session.clone();
                    let mut opts = EphemeralChildOptions::new("goal_review");
                    opts.label = Some("goal review".to_string());
                    opts.allowed_tools = allowed;
                    opts.fork_parent_session = true;
                    opts.isolate_extensions = true;
                    opts.timeout_secs = Some(super::GOAL_REVIEW_TIMEOUT_SECS);
                    opts.billing_turn_id = Some(
                        crate::tools::GoalReviewToolSession::billing_turn_id(session.as_ref()),
                    );
                    opts.tool_mask = Some(crate::tools::goal_review_mask());
                    opts.tool_observer = Some(Arc::new(
                        crate::tools::GoalReviewToolObserver::for_reviewer(session.clone()),
                    ));
                    opts.budget_check = Some(Arc::new(move || session_for_budget.check_budget()));
                    opts.extra_tools = vec![crate::tools::GoalReadTool::for_review_session(
                        session.clone(),
                    )];
                    let spawned = self
                        .handle
                        .ext()
                        .spawn_ephemeral_child(
                            format!(
                                "sub_review_{}",
                                time::OffsetDateTime::now_utc().unix_timestamp_nanos()
                            ),
                            opts,
                        )
                        .await?;
                    child = Some(spawned);
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
                    self.handle
                        .ext()
                        .format_tool_result(&conclusion, self.handle.ext().tool_result_max_bytes()),
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
                    .run_turn(prompt)
                    .await;
                session.check_budget()?;
                ensure!(
                    result.status == EphemeralChildStatus::Succeeded,
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
                let seq = self.handle.ext().committed_seq().await?;
                ensure!(
                    seq == current.acceptance_conversation_seq,
                    "conversation changed before host adjudication"
                );
                let artifact = self.handle.ext().write_artifact(
                    "IndependentGoalReview",
                    "independent descriptive audit report",
                    &result.text,
                )?;
                let report = GoalReviewReport {
                    candidate: candidate.clone(),
                    artifact: artifact.url,
                    content_hash: hash_bytes(result.text.as_bytes()),
                    reference_mapping: mapping.into_iter().collect(),
                    evidence_versions: reads,
                    conversation_seq: seq,
                };
                self.publish_review_report(&session, report.clone())?;
                report
            };
            let report_text = self
                .handle
                .ext()
                .read_artifact_text(report.artifact.strip_prefix("artifact://").unwrap_or(""))?;
            session.set_stage(GoalReviewStage::Adjudicating)?;
            let current = session.snapshot_for_read();
            let prompt = format!(
                "<goal-review>\nCandidate: {}\nContract: {}\nRequirement IDs: {}\nFinal execution conclusion: {}\nIndependent report: {}\nFull report: {}\nInspected evidence versions: {}\nImported reference mapping: {}\nFull evidence review required: {}\nYou are the HOST agent. Use the independent report to call GoalReview with decision=continue, complete or blocked, summary, and next_step for unfinished work. continue is valid whenever necessary contract work remains, even after a full evidence inspection. complete requires the independent reviewer to have checked the entire contract and supporting evidence. blocked requires an objective external dependency; only then include blocker as a short description. Optional remaining_requirements may list GoalRead requirement_ids. The runtime binds the candidate, report and evidence: do not supply candidate, mode, verdict or item tables. Do not repeat the audit. If a call is rejected, correct the specific diagnostic and call again in this turn. Prose alone does not update state. Only inspect and submit; do not execute or modify the task.\n</goal-review>",
                serde_json::to_string(candidate)?,
                serde_json::to_string(&current.spec)?,
                serde_json::to_string(&current.requirement_ids())?,
                self.handle
                    .ext()
                    .format_tool_result(&conclusion, self.handle.ext().tool_result_max_bytes()),
                self.handle
                    .ext()
                    .format_tool_result(&report_text, self.handle.ext().tool_result_max_bytes()),
                report.artifact,
                serde_json::to_string(&report.evidence_versions)?,
                serde_json::to_string(&report.reference_mapping)?,
                current.review_requires_full,
            );
            self.goal_tools.set_adjudication(Some(session.clone()));
            let _slot_guard = AdjudicationSlotGuard {
                bundle: (*self.goal_tools).clone(),
            };
            let session_for_budget = session.clone();
            let session_for_stop = session.clone();
            let mut req = InternalTurnRequest::new("goal_adjudication", prompt);
            req.metadata = serde_json::json!({"candidate": session.goal.review_candidate});
            req.policy.count_as_user_turn = false;
            req.policy.consume_inbox = false;
            req.policy.enable_belief = false;
            req.policy.disable_native_web = true;
            req.billing_turn_id_override =
                Some(format!("goal-review:{}:{}", goal.id, goal.iterations.len()));
            req.policy.tool_mask = Some(crate::tools::goal_adjudication_mask());
            req.policy.tool_observer = Some(Arc::new(
                crate::tools::GoalReviewToolObserver::for_adjudication(session.clone()),
            ));
            req.policy.budget_check = Some(Arc::new(move || session_for_budget.check_budget()));
            req.policy.stop_when = Some(Arc::new(move || session_for_stop.has_submission()));
            req.policy.continue_prompt_when_not_stopped = Some(
                "<goal-conclusion-required>The independent review is available, but no valid host conclusion has been recorded. Submit GoalReview for the bound candidate. If a previous call was rejected, correct its specific tool-result diagnostic and call again. Do not end this adjudication with a prose-only declaration.</goal-conclusion-required>".to_string(),
            );
            let outcome = self
                .handle
                .ext()
                .run_internal_turn_with_permit(permit, req)
                .await;
            drop(_slot_guard);
            self.handle.ext().sync_conversation().await?;
            let seq = self.handle.ext().committed_seq().await?;
            // Bind a reusable report to the host history even when submission failed.
            let mut report = report;
            report.conversation_seq = seq;
            if !self.handle.ext().is_cancelled() && !self.handle.ext().is_interrupted() {
                self.publish_review_report(&session, report)?;
            }
            session.check_budget()?;
            if !session.has_submission() {
                let detail = match outcome {
                    Ok(turn_outcome) => format!(
                        "{:?}: {}",
                        turn_outcome.status,
                        turn_outcome.error.as_deref().unwrap_or("")
                    ),
                    Err(error) => format!("{error:#}"),
                };
                anyhow::bail!(
                    "host agent did not submit a valid goal conclusion: {detail}; last rejection: {}",
                    session
                        .last_rejection
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .as_deref()
                        .unwrap_or("none")
                );
            }
            let proposal = session.submitted_continuation();
            if let Some(proposal) = proposal {
                let current = session.snapshot_for_read();
                let fingerprint = proposal.fingerprint()?;
                let rounds = if current.continuation_fingerprint.as_ref() == Some(&fingerprint) {
                    current.continuation_rounds.saturating_add(1)
                } else {
                    1
                };
                if rounds >= 3 {
                    let next = self.goal_store.transition(
                        &current.id,
                        current.revision,
                        GoalTransition::EscalateRepeatedContinuation {
                            conversation_seq: seq,
                        },
                    )?;
                    self.announce_goal(&next);
                    session.clear_continuation();
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
        let next = self.goal_store.transition(
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
                hash_bytes(self.handle.ext().read_artifact_text(id)?.as_bytes())
                    == report.content_hash,
                "independent report changed"
            );
        }
        let mut versions: BTreeMap<String, String> =
            goal.evidence_versions.iter().cloned().collect();
        let mut refreshed: BTreeMap<String, String> = BTreeMap::new();
        for reference in review.evidence() {
            let key = if reference.starts_with("artifact://") {
                reference.clone()
            } else {
                let path = Path::new(reference.strip_prefix("file://").unwrap_or(&reference));
                let path = if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    self.handle.session_info().cwd.join(path)
                }
                .canonicalize()?;
                path.to_string_lossy().into_owned()
            };
            let actual = if let Some(cached) = refreshed.get(&key) {
                cached.clone()
            } else {
                let hash = self.evidence_version(&reference)?;
                refreshed.insert(key.clone(), hash.clone());
                hash
            };
            ensure!(
                versions.get(&key) == Some(&actual),
                "review evidence changed or was not inspected"
            );
        }
        for (reference, hash) in &mut versions {
            if let Some(actual) = refreshed.get(reference) {
                *hash = actual.clone();
            } else {
                *hash = self.evidence_version(reference)?;
            }
        }
        Ok(versions.into_iter().collect())
    }
}
