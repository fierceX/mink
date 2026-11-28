use super::*;

pub(crate) const COMPACTION_INSTRUCTION: &str = "Merge the conversation history above into one checkpoint. Preserve current user goals, constraints, decisions, progress, blockers, file changes, commands, errors, pending work, and exact identifiers. An earlier <compacted-summary>, if present, is established background and must be merged with the newer history. Output these seven non-empty fields: Task focus:, Latest request:, Progress:, Errors:, Decisions:, Tool evidence:, Reflections:. Write (none) for any field without content. Start directly with Task focus:, do not use code fences, do not continue the task, and do not call tools.";

const FALLBACK_SYSTEM_PROMPT: &str = "Summarize coding-agent history for a later model. Preserve user goals, constraints, decisions, progress, blockers, file changes, commands, errors, pending work, and exact identifiers. Do not continue the task.";

impl CompactionEngine {
    pub(super) async fn run_summary_call(
        &self,
        active: &[Value],
        cut: usize,
        previous_summary: Option<&str>,
        history_already_compacted: bool,
        target: LlmModelTarget<'_>,
        round_deadline: Option<std::time::Instant>,
    ) -> Result<(String, SummaryInputMeta)> {
        let request_cancel = self.cancel.linked_child_token();
        let watcher_cancel = request_cancel.clone();
        let cleanup_cancel = request_cancel.clone();
        let interrupt = self.interrupt.clone();
        let watcher = tokio::spawn(async move {
            tokio::select! {
                _ = interrupt.notified() => {
                    watcher_cancel.cancel();
                }
                _ = watcher_cancel.cancelled() => {}
            }
        });
        struct WatcherGuard {
            watcher: tokio::task::JoinHandle<()>,
            cancel: crate::cancel::CancellationToken,
        }
        impl Drop for WatcherGuard {
            fn drop(&mut self) {
                self.watcher.abort();
                self.cancel.cancel();
            }
        }
        let _watcher_guard = WatcherGuard {
            watcher,
            cancel: cleanup_cancel,
        };
        let result = self
            .run_summary_call_with_cancel(
                active,
                cut,
                previous_summary,
                history_already_compacted,
                target,
                request_cancel,
                round_deadline,
            )
            .await;
        drop(_watcher_guard);
        if self.interrupt.is_set() {
            return Err(CompactionInterrupted::error());
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_summary_call_with_cancel(
        &self,
        active: &[Value],
        cut: usize,
        previous_summary: Option<&str>,
        history_already_compacted: bool,
        target: LlmModelTarget<'_>,
        request_cancel: crate::cancel::CancellationToken,
        round_deadline: Option<std::time::Instant>,
    ) -> Result<(String, SummaryInputMeta)> {
        use crate::llm::recovery::{self, RequestRetryState, RequestTerminal, UpstreamFailureKind};

        let input = self.build_summary_input(
            active,
            cut,
            previous_summary,
            history_already_compacted,
            target,
        )?;
        let SummaryRequestInput {
            system_prompt,
            tools,
            mut messages,
            meta,
        } = input;
        crate::llm::deepseek_web::project_replay(&mut messages, &tools);

        // The summary request uses the same request-retry budget and backoff
        // as the main agent, but keeps its own consumption loop: a completed
        // yet unusable summary (empty output, tool call, invalid stop reason)
        // is retried with a purpose correction appended to this temporary
        // request only — never written to the parent conversation.
        let mut retry = RequestRetryState::new_bounded(&self.config.llm_recovery, round_deadline);
        // Same deadline for establishment, consumption and every wait.
        let request_deadline = retry.deadline();
        let mut attempt: u32 = 0;
        let mut correction: Option<String> = None;
        loop {
            if request_cancel.is_cancelled() {
                return Err(CompactionInterrupted::error());
            }
            if self.interrupt.is_set() {
                return Err(CompactionInterrupted::error());
            }
            if let Some(check) = &self.budget_check {
                check()?;
            }
            // Ordinary turns must not be failed by an unrelated extension's budget
            // check; each scoped turn or child session passes its own `budget_check`.
            self.fault.check()?;
            if retry.deadline_expired() {
                return Err(SummaryUnavailable::error(format!(
                    "{}: compaction summary deadline exceeded",
                    RequestTerminal::Timeout.message()
                )));
            }
            let attempt_cancel = request_cancel.linked_child_token();
            let mut attempt_messages = messages.clone();
            if let Some(correction) = &correction {
                attempt_messages.push(json!({
                    "role": "user",
                    "internal": true,
                    "content": correction,
                }));
            }
            // 每次 attempt 都重新估算输入并给出动态输出 cap：纠错文本、图片降级或
            // 路径切换都可能改变输入长度，过期的预算检查会发出装不下的请求。
            let attempt_input_tokens = crate::llm::transport::estimate_openai_context_tokens(
                &attempt_messages,
                &tools,
                &system_prompt,
            )?;
            let summary_output_cap = match summary_output_cap(&self.config, attempt_input_tokens) {
                Some(cap) => cap,
                None => {
                    return Err(SummaryUnavailable::error(format!(
                        "compaction summary leaves no practical output budget: input {attempt_input_tokens} tokens of window {}",
                        self.config.max_context_tokens
                    )));
                }
            };
            // Cache-aligned summaries deliberately retain the Agent tool
            // schemas so the provider can reuse the immutable request prefix.
            // Those tools are alignment-only: compaction never executes them.
            let request = LlmRequest {
                purpose: LlmPurpose::Compaction,
                model: target.model.to_string(),
                model_alias: target.alias.map(str::to_string),
                api_url: self.api_url.clone(),
                api_key: self.api_key.clone(),
                system_prompt: Arc::from(system_prompt.as_str()),
                messages: Arc::new(attempt_messages),
                tools: Arc::new(tools.clone()),
                max_tokens: summary_output_cap,
                cancel: attempt_cancel.clone(),
                verbose: self.config.verbose,
                display: self.display.clone(),
            };
            let capture = self.usage.capture(
                self.usage
                    .scope(UsageKind::Compaction, self.session_id.clone()),
                target.model.to_string(),
            );
            // First-event and idle deadlines mirror the main request path:
            // the first-event budget covers establishment plus the wait for
            // the first event of this attempt, the idle budget covers gaps
            // between real events. Retry notifications are not progress.
            let attempt_started = std::time::Instant::now();
            let first_event_timeout =
                crate::agent::turn::positive_duration(self.config.llm_first_event_timeout_secs);
            let idle_timeout =
                crate::agent::turn::positive_duration(self.config.llm_idle_timeout_secs);
            let opened = tokio::select! {
                opened = crate::llm::client::guarded_open_stream(
                    &self.llm_backend,
                    request,
                    None,
                    capture,
                ) => opened,
                _ = request_cancel.cancelled() => return Err(CompactionInterrupted::error()),
                _ = recovery::wait_until(request_deadline) => {
                    return Err(SummaryUnavailable::error(format!(
                        "{}: compaction summary deadline exceeded before the request was established",
                        RequestTerminal::Timeout.message()
                    )));
                }
                _ = wait_first_event_deadline(attempt_started, first_event_timeout) => {
                    let failure = SummaryRetry {
                        retry_after: None,
                        message: format!(
                            "compaction summary first event timeout after {} seconds",
                            first_event_timeout.map_or(0, |timeout| timeout.as_secs())
                        ),
                    };
                    attempt_cancel.cancel();
                    self.wait_summary_retry(&mut retry, attempt, &failure, &request_cancel)
                        .await?;
                    attempt += 1;
                    continue;
                }
            };
            let response = match opened {
                Ok(response) => response,
                Err(error) => {
                    // Close this failed attempt before classifying/backing off:
                    // a custom backend may still hold the attempt token (F7).
                    attempt_cancel.cancel();
                    if request_cancel.is_cancelled() {
                        return Err(CompactionInterrupted::error());
                    }
                    let failure = classify_summary_failure(&error);
                    if let Some(failure) = failure {
                        self.wait_summary_retry(&mut retry, attempt, &failure, &request_cancel)
                            .await?;
                        attempt += 1;
                        continue;
                    }
                    return Err(SummaryUnavailable::error(format!("{error:#}")));
                }
            };

            let consumption = self
                .consume_summary_stream(
                    response.events,
                    &request_cancel,
                    request_deadline,
                    attempt_started,
                    first_event_timeout,
                    idle_timeout,
                )
                .await?;
            attempt_cancel.cancel();
            match consumption {
                SummaryConsumption::Complete {
                    output,
                    stop_reason,
                    invalid_tool_call,
                } => {
                    let summary = strip_dsml_tags(&output);
                    let issue = if let Some((name, id)) = invalid_tool_call {
                        Some(format!(
                            "compaction attempted invalid tool call {name} ({id})"
                        ))
                    } else if !matches!(stop_reason.as_str(), "stop" | "end_turn") {
                        Some(format!("invalid stop reason {stop_reason:?}"))
                    } else if summary.trim().is_empty() {
                        Some("empty response".to_string())
                    } else {
                        None
                    };
                    match issue {
                        None => return Ok((summary.trim().to_string(), meta)),
                        Some(issue) => {
                            // The correction lives only in this temporary
                            // summary request tail.
                            let failure = SummaryRetry {
                                retry_after: None,
                                message: issue.clone(),
                            };
                            self.wait_summary_retry(&mut retry, attempt, &failure, &request_cancel)
                                .await?;
                            attempt += 1;
                            correction = Some(format!(
                                "<summary-retry>Your previous response was rejected: {issue}. Output only the seven required summary fields now, starting directly with Task focus:.</summary-retry>"
                            ));
                            continue;
                        }
                    }
                }
                SummaryConsumption::ProviderError(error) => {
                    let kind = recovery::classify_provider_error(
                        error.provider_code.as_deref(),
                        error.status,
                    );
                    let message = format!("failed to generate context summary: {}", error.message);
                    if kind != UpstreamFailureKind::Recoverable {
                        return Err(SummaryUnavailable::error(message));
                    }
                    let failure = SummaryRetry {
                        retry_after: None,
                        message,
                    };
                    self.wait_summary_retry(&mut retry, attempt, &failure, &request_cancel)
                        .await?;
                    attempt += 1;
                    continue;
                }
                SummaryConsumption::StreamError(error) => {
                    let failure = classify_summary_failure(&error);
                    if let Some(failure) = failure {
                        self.wait_summary_retry(&mut retry, attempt, &failure, &request_cancel)
                            .await?;
                        attempt += 1;
                        continue;
                    }
                    return Err(SummaryUnavailable::error(format!("{error:#}")));
                }
                SummaryConsumption::Deadline => {
                    return Err(SummaryUnavailable::error(format!(
                        "{}: compaction summary deadline exceeded while streaming",
                        RequestTerminal::Timeout.message()
                    )));
                }
                SummaryConsumption::Interrupted => return Err(CompactionInterrupted::error()),
            }
        }
    }

    /// Consume one compaction attempt without committing anything. `deadline`
    /// is the optional total request deadline shared with establishment and
    /// the retry waits; `attempt_started`/`first_event_timeout`/`idle_timeout`
    /// are the per-attempt first-event and idle deadlines.
    async fn consume_summary_stream(
        &self,
        mut stream: crate::llm::client::LlmEventStream,
        request_cancel: &crate::cancel::CancellationToken,
        deadline: Option<std::time::Instant>,
        attempt_started: std::time::Instant,
        first_event_timeout: Option<std::time::Duration>,
        idle_timeout: Option<std::time::Duration>,
    ) -> Result<SummaryConsumption> {
        let mut output = String::new();
        let mut stop_reason = String::new();
        let mut invalid_tool_call = None;
        // Retry notifications do not count as the first event, nor do they
        // refresh the idle clock (mirrors the main request path).
        let mut saw_any_event = false;
        let mut last_event_at = std::time::Instant::now();
        loop {
            if let Some(consumption) = summary_wait_timeout(
                saw_any_event,
                attempt_started,
                last_event_at,
                first_event_timeout,
                idle_timeout,
            ) {
                return Ok(consumption);
            }
            let wait_timeout_at = summary_next_timeout_instant(
                saw_any_event,
                attempt_started,
                last_event_at,
                first_event_timeout,
                idle_timeout,
            );
            let event = tokio::select! {
                event = futures::StreamExt::next(&mut stream) => event,
                _ = request_cancel.cancelled() => return Ok(SummaryConsumption::Interrupted),
                _ = crate::llm::recovery::wait_until(deadline) => {
                    return Ok(SummaryConsumption::Deadline)
                }
                _ = crate::llm::recovery::wait_until(wait_timeout_at) => continue,
            };
            let Some(event) = event else { break };
            match event {
                Ok(Event::Text(TextEvent { content })) => {
                    saw_any_event = true;
                    last_event_at = std::time::Instant::now();
                    output.push_str(&content);
                }
                Ok(Event::Usage(usage)) => {
                    saw_any_event = true;
                    last_event_at = std::time::Instant::now();
                    self.log_compact_event(&usage);
                    self.stats.record_compact(&usage).await;
                }
                Ok(Event::UsageUnavailable) => {}
                Ok(Event::Error(error)) => return Ok(SummaryConsumption::ProviderError(error)),
                Ok(Event::Stop(StopEvent { reason })) => {
                    stop_reason = reason;
                    break;
                }
                Ok(Event::ToolCall(call)) if invalid_tool_call.is_none() => {
                    saw_any_event = true;
                    last_event_at = std::time::Instant::now();
                    invalid_tool_call = Some((call.name, call.id));
                }
                Ok(Event::Retry(_)) => {
                    // The backend restarted this logical request: partial
                    // output from the aborted attempt must not leak. The
                    // notification is not progress and does not refresh the
                    // idle clock.
                    output.clear();
                    stop_reason.clear();
                    invalid_tool_call = None;
                }
                Ok(_) => {
                    saw_any_event = true;
                    last_event_at = std::time::Instant::now();
                }
                Err(error) => return Ok(SummaryConsumption::StreamError(error)),
            }
        }
        Ok(SummaryConsumption::Complete {
            output,
            stop_reason,
            invalid_tool_call,
        })
    }

    /// Bounded wait before retrying one compaction attempt; fails with the
    /// stable terminal reason when the budget/deadline is exhausted.
    async fn wait_summary_retry(
        &self,
        retry: &mut crate::llm::recovery::RequestRetryState,
        attempt: u32,
        failure: &SummaryRetry,
        request_cancel: &crate::cancel::CancellationToken,
    ) -> Result<()> {
        use crate::llm::recovery::{self, RequestTerminal};
        if !retry.can_retry() {
            return Err(SummaryUnavailable::error(format!(
                "{}: {}",
                RequestTerminal::RetryExhausted.message(),
                failure.message
            )));
        }
        let wait = match retry.retry_wait(attempt, failure.retry_after, recovery::jitter_fraction())
        {
            Ok(wait) => wait,
            Err(_) => {
                return Err(SummaryUnavailable::error(format!(
                    "{}: {}",
                    RequestTerminal::Timeout.message(),
                    failure.message
                )));
            }
        };
        retry.note_retry();
        tokio::select! {
            _ = tokio::time::sleep(wait) => Ok(()),
            _ = request_cancel.cancelled() => Err(CompactionInterrupted::error()),
        }
    }

    pub(super) fn build_summary_input(
        &self,
        active: &[Value],
        cut: usize,
        previous_summary: Option<&str>,
        history_already_compacted: bool,
        target: LlmModelTarget<'_>,
    ) -> Result<SummaryRequestInput> {
        let dropped = &active[..cut];
        let mut source_messages = if history_already_compacted {
            // Reuse `checkpoint_prefix` so the summary request prefix matches
            // `active_messages()` verbatim (summary + active plan + provider checkpoints)
            // and preserves provider prefix-cache alignment across 2nd+ compactions.
            self.checkpoint_prefix(previous_summary.unwrap_or_default())?
        } else {
            let mut messages = Vec::new();
            if let Some(summary) = previous_summary.filter(|summary| !summary.trim().is_empty()) {
                messages.push(compacted_summary_message(summary));
            }
            messages
        };
        // Startup boundary repair removed these messages from the projection;
        // include them in every summary attempt until one actually commits.
        // Taking them here would re-lose them when the request fails.
        let repair_loss = self
            .startup_repair_loss
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        source_messages.extend(repair_loss.iter().cloned());
        let dynamic_prefix_len = source_messages.len();
        source_messages.extend(
            crate::llm::image_projection::project_consumed_attachments(active)
                .iter()
                .cloned(),
        );
        let source_prefix_len = dynamic_prefix_len.saturating_add(cut);

        let latest = self
            .prompt_usage
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .latest_request
            .clone();
        let fallback_reason;
        if let Some(latest) = latest {
            let aligned = (|| -> Result<SummaryRequestInput, String> {
                if latest.model != target.model {
                    return Err("model_changed".into());
                }
                if latest.backend_name != self.llm_backend.name() {
                    return Err("cache_domain_changed".into());
                }
                if latest.projection_generation != self.projection_generation.load(Ordering::SeqCst)
                {
                    return Err("projection_generation_changed".into());
                }
                let Some(recent_projection) = latest.projection.as_ref() else {
                    return Err("backend_projection_unavailable".into());
                };
                let mut projected_source = source_messages.clone();
                crate::llm::deepseek_web::project_replay(
                    &mut projected_source,
                    &latest.source_tools,
                );
                let source_request = LlmRequest {
                    purpose: LlmPurpose::Agent,
                    model: target.model.to_string(),
                    model_alias: target.alias.map(str::to_string),
                    api_url: self.api_url.clone(),
                    api_key: self.api_key.clone(),
                    system_prompt: latest.source_system_prompt.clone(),
                    messages: Arc::new(projected_source),
                    tools: latest.source_tools.clone(),
                    max_tokens: effective_max_tokens(&self.config),
                    cancel: self.cancel.clone(),
                    verbose: self.config.verbose,
                    display: self.display.clone(),
                };
                let Some(candidate) = self
                    .llm_backend
                    .cache_projection(&source_request, source_prefix_len)
                else {
                    return Err("backend_projection_unavailable".into());
                };
                if candidate.model != recent_projection.model {
                    return Err("model_changed".into());
                }
                if candidate.system_prompt != recent_projection.system_prompt
                    || candidate.tools != recent_projection.tools
                {
                    return Err("system_tools_changed".into());
                }
                if candidate.messages.len() != source_prefix_len {
                    return Err("source_boundary_unproven".into());
                }
                let candidate_hashes = message_hashes(&candidate.messages);
                let aligned_messages = candidate_hashes
                    .iter()
                    .zip(recent_projection.message_hashes.iter())
                    .take_while(|(left, right)| left == right)
                    .count();
                let aligned_messages = rollback_incomplete_tool_exchange_boundary(
                    &candidate.messages,
                    aligned_messages,
                );
                if aligned_messages == 0 {
                    return Err("no_safe_message_prefix".into());
                }
                let mut messages = candidate.messages[..aligned_messages].to_vec();
                let suffix = &candidate.messages[aligned_messages..];
                let reduced_suffix_messages = suffix.len();
                if !suffix.is_empty() {
                    if self.config.context_compact_input_reduction {
                        messages.push(json!({
                            "role": "user",
                            "internal": true,
                            "content": format!(
                                "<compaction-uncached-suffix>\n{}\n</compaction-uncached-suffix>",
                                compaction_input::reduce_for_summary(suffix)
                            ),
                        }));
                    } else {
                        let mut raw_suffix = suffix.to_vec();
                        crate::llm::image_projection::degrade_images_for_summary(&mut raw_suffix);
                        messages.extend(raw_suffix);
                    }
                }
                let aligned_estimated_tokens =
                    crate::llm::transport::estimate_openai_context_tokens(
                        &candidate.messages[..aligned_messages],
                        &candidate.tools,
                        &candidate.system_prompt,
                    )
                    .map_err(|error| format!("aligned_estimate_failed:{error}"))?;
                messages.push(compaction_instruction_message());
                // 与专用摘要路径相同的动态 cap 判定：先按实际输入算可用输出预算，
                // 用固定配置 cap 提前淘汰会白白丢掉可缓存的对齐请求。
                let aligned_total_tokens = crate::llm::transport::estimate_openai_context_tokens(
                    &messages,
                    &candidate.tools,
                    &candidate.system_prompt,
                )
                .map_err(|error| format!("aligned_estimate_failed:{error}"))?;
                let Some(reserved_output) = summary_output_cap(&self.config, aligned_total_tokens)
                else {
                    return Err("aligned_no_practical_output".into());
                };
                if super::super::compaction_cut::summary_tokens_over_budget(
                    &self.config,
                    aligned_total_tokens,
                    usize::try_from(reserved_output).unwrap_or(0),
                ) {
                    return Err("aligned_input_over_budget".into());
                }
                Ok(SummaryRequestInput {
                    system_prompt: candidate.system_prompt.to_string(),
                    tools: (*candidate.tools).clone(),
                    messages,
                    meta: SummaryInputMeta {
                        input_mode: if aligned_messages == candidate.messages.len() {
                            "cache_aligned"
                        } else {
                            "partial_aligned"
                        },
                        aligned_messages,
                        aligned_estimated_tokens,
                        reduced_suffix_messages,
                        fallback_reason: None,
                    },
                })
            })();
            match aligned {
                Ok(input) => return Ok(input),
                Err(reason) => fallback_reason = Some(reason),
            }
        } else {
            fallback_reason = Some("no_recent_agent_request".into());
        }

        let mut history = Vec::new();
        if let Some(summary) = previous_summary.filter(|summary| !summary.trim().is_empty()) {
            history.push(compacted_summary_message(summary));
        }
        // Also present in the fallback (no recent agent request): startup
        // repair losses must reach the summary whichever path is taken.
        history.extend(repair_loss);
        history.extend(
            crate::llm::image_projection::project_consumed_attachments(dropped)
                .iter()
                .cloned(),
        );
        crate::llm::image_projection::degrade_images_for_summary(&mut history);
        let input_mode = if self.config.context_compact_input_reduction {
            "reduced"
        } else {
            "raw"
        };
        let mut messages = if self.config.context_compact_input_reduction {
            vec![json!({
                "role": "user",
                "internal": true,
                "content": compaction_input::reduce_for_summary(&history),
            })]
        } else {
            history
        };
        messages.push(compaction_instruction_message());
        if summary_input_over_budget(
            &self.config,
            &messages,
            &[],
            FALLBACK_SYSTEM_PROMPT,
            MIN_PRACTICAL_SUMMARY_TOKENS,
        )? {
            return Err(SummaryUnavailable::error(
                "compaction summary input exceeds configured budget",
            ));
        }
        Ok(SummaryRequestInput {
            system_prompt: FALLBACK_SYSTEM_PROMPT.to_string(),
            tools: Vec::new(),
            messages,
            meta: SummaryInputMeta {
                input_mode,
                aligned_messages: 0,
                aligned_estimated_tokens: 0,
                reduced_suffix_messages: dropped.len(),
                fallback_reason,
            },
        })
    }
}

/// Outcome of consuming one compaction attempt.
enum SummaryConsumption {
    Complete {
        output: String,
        stop_reason: String,
        invalid_tool_call: Option<(String, String)>,
    },
    ProviderError(ErrorEvent),
    StreamError(anyhow::Error),
    /// The optional total request deadline elapsed while the response was in
    /// flight; the logical compaction request is over.
    Deadline,
    Interrupted,
}

/// One retryable compaction failure: a typed upstream failure or an unusable
/// completed summary (empty output, tool call, invalid stop reason).
struct SummaryRetry {
    retry_after: Option<std::time::Duration>,
    message: String,
}

/// Sleep until the attempt's first-event deadline; without a configured
/// timeout this future never resolves (uniform `select!` shape).
async fn wait_first_event_deadline(
    attempt_started: std::time::Instant,
    first_event_timeout: Option<std::time::Duration>,
) {
    match first_event_timeout {
        Some(timeout) => tokio::time::sleep_until((attempt_started + timeout).into()).await,
        None => std::future::pending::<()>().await,
    }
}

fn summary_next_timeout_instant(
    saw_any_event: bool,
    attempt_started: std::time::Instant,
    last_event_at: std::time::Instant,
    first_event_timeout: Option<std::time::Duration>,
    idle_timeout: Option<std::time::Duration>,
) -> Option<std::time::Instant> {
    if !saw_any_event {
        first_event_timeout.map(|timeout| attempt_started + timeout)
    } else {
        idle_timeout.map(|timeout| last_event_at + timeout)
    }
}

/// Per-attempt first-event / idle deadline check shared by the compaction
/// consumption loop. Returns a retryable stream failure once a deadline is
/// exceeded so the existing retry budget applies.
fn summary_wait_timeout(
    saw_any_event: bool,
    attempt_started: std::time::Instant,
    last_event_at: std::time::Instant,
    first_event_timeout: Option<std::time::Duration>,
    idle_timeout: Option<std::time::Duration>,
) -> Option<SummaryConsumption> {
    let now = std::time::Instant::now();
    if !saw_any_event {
        if let Some(timeout) = first_event_timeout
            && now.duration_since(attempt_started) >= timeout
        {
            return Some(SummaryConsumption::StreamError(anyhow::Error::new(
                crate::llm::recovery::LlmUpstreamError::recoverable(format!(
                    "compaction summary first event timeout after {} seconds",
                    timeout.as_secs()
                )),
            )));
        }
        return None;
    }
    if let Some(timeout) = idle_timeout
        && now.duration_since(last_event_at) >= timeout
    {
        return Some(SummaryConsumption::StreamError(anyhow::Error::new(
            crate::llm::recovery::LlmUpstreamError::recoverable(format!(
                "compaction summary idle timeout after {} seconds without events",
                timeout.as_secs()
            )),
        )));
    }
    None
}

/// Classify a compaction attempt failure. `None` means permanent/fatal (the
/// caller returns the original error).
fn classify_summary_failure(error: &anyhow::Error) -> Option<SummaryRetry> {
    use crate::llm::recovery::{UpstreamFailureKind, find_upstream_failure};
    let upstream = find_upstream_failure(error)?;
    match upstream.kind() {
        UpstreamFailureKind::Permanent => None,
        UpstreamFailureKind::Recoverable | UpstreamFailureKind::ProtocolDamaged => {
            Some(SummaryRetry {
                retry_after: upstream.retry_after(),
                message: format!("{error:#}"),
            })
        }
    }
}
