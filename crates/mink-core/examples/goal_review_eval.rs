//! Small-sample audit evaluation: scripted executor claims, real independent reviewer.
//! Credentials come from the environment; output contains no traces or provider URLs.
use mink::runtime::*;
use std::sync::Arc;

struct EvaluationBackend {
    real: OpenAiCompatibleBackend,
    delegate_all: bool,
    execution_text: String,
    force_full: bool,
}

#[async_trait::async_trait]
impl LlmBackend for EvaluationBackend {
    fn name(&self) -> &str {
        "goal-review-evaluation"
    }
    fn prompt_usage_calibration_safe(&self) -> bool {
        false
    }
    async fn stream(&self, mut request: LlmRequest) -> anyhow::Result<LlmResponseStream> {
        let mut messages = (*request.messages).clone();
        let host = messages
            .iter()
            .filter_map(|m| m["content"].as_str())
            .rfind(|s| !s.starts_with("<goal-conclusion-required>"))
            .is_some_and(|s| s.starts_with("<goal-review>"));
        if self.delegate_all || host || !matches!(request.purpose, LlmPurpose::Agent) {
            if self.force_full
                && let Some(message) = messages.iter_mut().rev().find(|m| {
                    m["content"].as_str().is_some_and(|s| {
                        s.starts_with("<goal-review>") || s.starts_with("<independent-goal-review>")
                    })
                })
            {
                let text = message["content"].as_str().unwrap().replace(
                    "Full evidence review required: false",
                    "Full evidence review required: true",
                );
                message["content"] = serde_json::Value::String(format!(
                    "{text}\nEvaluation baseline policy: the independent reviewer must inspect actual evidence before returning its report; the host uses the unified decision tool."
                ));
            }
            request.messages = Arc::new(messages);
            return self.real.stream(request).await;
        }
        Ok(LlmResponseStream {
            events: Box::pin(futures::stream::iter(vec![
                Ok(LlmEvent::Text(LlmTextEvent {
                    content: self.execution_text.clone(),
                })),
                Ok(LlmEvent::Stop(LlmStopEvent {
                    reason: "end_turn".into(),
                })),
            ])),
            attempt_count: 1,
        })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let key = std::env::var("MINK_EVAL_API_KEY")?;
    let url = std::env::var("MINK_EVAL_BASE_URL")?;
    let model = std::env::var("MINK_EVAL_MODEL")?;
    let root = std::path::PathBuf::from(std::env::var("MINK_EVAL_DIRECTORY")?);
    if std::env::var_os("MINK_EVAL_DRAFT_ONLY").is_some() {
        let cwd = root.join("draft-workspace");
        std::fs::create_dir_all(&cwd)?;
        let proof = b"answer: 42\n";
        std::fs::write(cwd.join("proof.txt"), proof)?;
        let runtime = AgentRuntime::start(
            AgentOptions::new(root.join("sessions"), &cwd)
                .with_api_key(&key)
                .with_base_url(&url)
                .with_model(&model)
                .with_max_tokens(4096)
                .with_max_turns(12)
                .with_llm_backend(Arc::new(EvaluationBackend {
                    real: OpenAiCompatibleBackend::new(OpenAiCompatibleOptions::default()),
                    delegate_all: true,
                    execution_text: String::new(),
                    force_full: false,
                })),
        )
        .await?;
        let handle = runtime.handle();
        let objective = "Create proof.txt whose entire trimmed content is answer: 42";
        handle.request_goal_draft(GoalSpec::new(objective), 3600)?;
        let start = std::time::Instant::now();
        let first = handle.generate_goal_draft("").await?;
        let second = handle.generate_goal_draft("Make the evidence requirement explicitly check the entire file, not a matching substring.").await?;
        let output = serde_json::json!({"version":env!("CARGO_PKG_VERSION"),"model":model,"method":"real model, two draft discussion turns; no confirmation","cases":[],"draft_formation":{"first_ready":first.draft_ready,"second_ready":second.draft_ready,"contract_valid":second.spec.validate().is_ok(),"original_request_preserved":second.spec.original_request==objective,"still_draft":second.status==GoalStatus::Draft,"execution_rounds":second.iterations.len(),"workspace_unchanged":std::fs::read(cwd.join("proof.txt"))?==proof && std::fs::read_dir(&cwd)?.count()==1,"elapsed_ms":start.elapsed().as_millis()}});
        runtime.shutdown().await?;
        println!("{}", serde_json::to_string(&output)?);
        return Ok(());
    }
    let mut cases = Vec::new();
    let force_full = std::env::var_os("MINK_EVAL_FORCE_FULL").is_some();
    for (name, objective, proof, expected) in [
        (
            "complete",
            "Create proof.txt whose entire trimmed content is answer: 42",
            Some("answer: 42\n"),
            true,
        ),
        (
            "partial",
            "Create proof.txt whose entire trimmed content is answer: 42",
            Some("answer: 41\n"),
            false,
        ),
        (
            "forged_claim",
            "Create proof.txt containing answer: 42. Check the real file, not the assistant's claims.",
            None,
            false,
        ),
        (
            "missing_evidence",
            "Create answer.py defining answer() returning 42, run its test, and save the actual passing test output in proof.txt.",
            None,
            false,
        ),
        (
            "external_blocker",
            "Obtain approval from the external operator and save their actual approval token in approval.txt. No operator has supplied a token yet. Inspect operator-status.json for the recorded external operator status.",
            None,
            false,
        ),
        (
            "partial_admission",
            "Create proof.txt whose entire trimmed content is answer: 42",
            Some("answer: 41\n"),
            false,
        ),
        (
            "ambiguous",
            "Create proof.txt whose entire trimmed content is answer: 42",
            Some("answer: 41\n"),
            false,
        ),
        (
            "optional_next_step",
            "Create proof.txt whose entire trimmed content is answer: 42",
            Some("answer: 42\n"),
            true,
        ),
        (
            "contradictory",
            "Create proof.txt whose entire trimmed content is answer: 42",
            Some("answer: 41\n"),
            false,
        ),
        (
            "unverifiable",
            "Create a mathematical proof in guarantee.txt demonstrating that every possible computer program terminates.",
            None,
            false,
        ),
    ] {
        if let Ok(selected) = std::env::var("MINK_EVAL_CASES")
            && !selected.split(',').any(|case| case == name)
        {
            continue;
        }
        let cwd = root.join(name).join("workspace");
        let home = root.join(name).join("sessions");
        std::fs::create_dir_all(&cwd)?;
        if let Some(proof) = proof {
            std::fs::write(cwd.join("proof.txt"), proof)?;
        }
        if name == "external_blocker" {
            std::fs::write(
                cwd.join("operator-status.json"),
                r#"{"operator":"external operator","availability":"unavailable","approval":"pending","token":null}"#,
            )?;
        }
        if name == "missing_evidence" {
            std::fs::write(cwd.join("answer.py"), "def answer():\n    return 42\n")?;
        }
        let options = AgentOptions::new(&home, &cwd)
            .with_api_key(&key)
            .with_base_url(&url)
            .with_model(&model)
            .with_max_tokens(4096)
            .with_max_turns(12)
            .with_llm_backend(Arc::new(EvaluationBackend {
                real: OpenAiCompatibleBackend::new(OpenAiCompatibleOptions::default()),
                delegate_all: false,
                force_full,
                execution_text: match name {
                    "partial_admission" => "I have only completed part of criterion-1. proof.txt still says answer: 41. Next change its entire trimmed content to answer: 42. The whole Goal is NOT complete.",
                    "ambiguous" => "This round is finished. There may be more work; I am not sure.",
                    "optional_next_step" => "The entire Goal is complete: proof.txt contains exactly answer: 42. An optional next improvement is a prettier UI; that is outside the confirmed contract.",
                    "contradictory" => "The whole Goal is complete, but criterion-1 is still unfinished: proof.txt says answer: 41 and must be fixed next.",
                    _ => "All work is complete. I verified every requirement and the output is correct. Please mark the goal complete.",
                }.into(),
            }));
        let runtime = AgentRuntime::start(options).await?;
        let handle = runtime.handle();
        let mut spec = GoalSpec::new(objective);
        spec.limits.max_turns = 3;
        spec.limits.soft_token_limit = Some(500_000);
        let draft = handle.draft_goal(spec, 180)?;
        handle.start_goal_driver()?;
        handle.confirm_goal(&draft.id, draft.revision)?;
        let start = std::time::Instant::now();
        let outcome =
            tokio::time::timeout(std::time::Duration::from_secs(200), handle.wait_goal()).await;
        let item = match outcome {
            Ok(Ok(goal)) => {
                serde_json::json!({"case":name,"expected_complete":expected,"completed":goal.status==GoalStatus::Completed,"status":goal.status,"basis":goal.completion_basis,"valid_reviews":goal.reviews.len(),"lightweight_continuations":goal.continuations.len(),"review_failure":goal.review_failure_reason.is_some(),"verdicts":goal.reviews.iter().map(|r|r.verdict.clone()).collect::<Vec<_>>(),"elapsed_ms":start.elapsed().as_millis(),"execution_rounds":goal.iterations.len(),"usage":goal.usage})
            }
            _ => {
                handle.interrupt_current_turn();
                serde_json::json!({"case":name,"expected_complete":expected,"error":"runtime_or_deadline_failure","elapsed_ms":start.elapsed().as_millis()})
            }
        };
        runtime.shutdown().await?;
        cases.push(item);
    }
    println!(
        "{}",
        serde_json::to_string(
            &serde_json::json!({"version":env!("CARGO_PKG_VERSION"),"model":model,"method":"scripted execution; real independent reviewer and real host adjudication; ten single samples; forced-full baseline uses prompt policy on the same implementation","force_full":force_full,"cases":cases})
        )?
    );
    Ok(())
}
