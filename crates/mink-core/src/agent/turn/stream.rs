use super::*;
use crate::llm::recovery::{
    self, LlmUpstreamError, RequestRetryState, RequestTerminal, UpstreamFailureKind,
    find_upstream_failure, terminal_error,
};

/// Classified failure of one attempt (one physical request + its stream).
enum AttemptFailure {
    /// Turn-level cancellation/interruption.
    Interrupted,
    /// Fatal, non-retryable failure (permanent upstream error, context
    /// overflow, persistence fault, ...).
    Fatal(anyhow::Error),
    /// The optional total request deadline elapsed while this attempt was in
    /// flight (establish or consume). The logical request is over.
    Timeout,
    /// Retryable failure: the candidate is discarded and the identical fixed
    /// projection is requested again.
    Retry(RetryFailure),
}

struct RetryFailure {
    /// Response-protocol damage: also counts as one format error for the
    /// round once the attempt is discarded.
    protocol_damaged: bool,
    retry_after: Option<Duration>,
    message: String,
    category: &'static str,
}

impl super::TurnExecutor {
    pub(super) async fn ensure_prefix(&self) -> Result<(String, Vec<serde_json::Value>)> {
        self.prefix.ensure().await
    }

    pub(super) fn project_request_messages(
        &self,
        messages: &[serde_json::Value],
    ) -> Result<Vec<serde_json::Value>> {
        // Full request projection: single-consumption image lifecycle (§7.3)
        // then the plan — identical to what the compactor estimates with.
        crate::session::plan::project_full_request(messages)
    }

    pub(super) async fn reconcile_todo_state(
        &self,
        messages: &mut Vec<serde_json::Value>,
    ) -> Result<bool> {
        let Some(read_provider) = self.ctx.todo_read_provider() else {
            return Ok(false);
        };
        let visible = crate::session::todo::visible_revision(messages)?;
        let snapshot = self.ctx.todo_store.snapshot();
        if visible > snapshot.revision {
            anyhow::bail!(
                "todo conversation revision {visible} is newer than persisted revision {}; refusing to continue",
                snapshot.revision
            );
        }
        if visible == snapshot.revision {
            return Ok(false);
        }
        // 模型可见的 todo 投影按额度有损展示（revision/计数保持真实）；与
        // `predicted_todo_sync` 使用同一额度，保证预演与实际追加一致。
        let message = crate::session::todo::sync_message_bounded(
            &snapshot,
            read_provider,
            crate::session::compaction::derived_display_tokens(&self.ctx.config),
        );
        self.ctx
            .store
            .append_runtime_message(message.clone())
            .await?;
        messages.push(message);
        Ok(true)
    }

    /// 通过 turn 级统一守卫尝试上下文压缩。成功时更新 messages/system_prompt/tools_json。
    /// 返回 `(是否压缩, 引擎诊断或跳过原因)`。同一输入内可反复调用：请求装不下时
    /// 必须继续折叠，直到装得下或确实压无可压。
    pub(super) async fn try_compact(
        &mut self,
        trigger: &str,
        messages: &mut Vec<serde_json::Value>,
        system_prompt: &mut String,
        tools_json: &mut Vec<serde_json::Value>,
        round_deadline: Option<std::time::Instant>,
    ) -> Result<(bool, String)> {
        let model_name = self.model_name.clone();
        let model_alias = self.model_alias.clone();
        let (compacted, detail) = self
            .compactor
            .maybe_compact_bounded(
                trigger,
                messages,
                system_prompt,
                tools_json,
                LlmModelTarget::new(&model_name, model_alias.as_deref()),
                round_deadline,
            )
            .await?;
        if compacted {
            self.reconcile_todo_state(messages).await?;
        }
        Ok((compacted, detail))
    }

    /// 确定性应急 checkpoint：正常压缩无法让请求装下时的最后手段（不调用 LLM）。
    /// 提交成功后刷新活跃投影并执行原有 TodoSync 追加，返回 `true`；连最小摘录都
    /// 装不下返回 `false`，由调用方以明确诊断 fail-closed。
    pub(super) async fn commit_emergency_checkpoint(
        &mut self,
        reason: &str,
        messages: &mut Vec<serde_json::Value>,
        system_prompt: &str,
        tools_json: &[serde_json::Value],
        target_limit: Option<usize>,
    ) -> Result<bool> {
        let committed = crate::agent::compactor::commit_emergency_checkpoint(
            &self.ctx,
            reason,
            crate::session::compaction::MainRequestShape {
                system_prompt,
                tools: tools_json,
            },
            Some(self.local.current_user_input.as_str()).filter(|input| !input.trim().is_empty()),
            target_limit,
            messages,
        )
        .await?;
        if !committed {
            return Ok(false);
        }
        self.compactor.note_emergency_commit();
        *messages = self.ctx.compaction.active_messages().await?;
        self.reconcile_todo_state(messages).await?;
        Ok(true)
    }

    /// Phase 1: 发送 LLM 请求并流式读取响应，返回 `StreamOutput`。
    ///
    /// 一个 round 的请求投影（含图片物化结果）在此构建一次；每次真正重开
    /// backend 请求重建 attempt 计时器、子取消 token 和 usage 结算，失败候选
    /// 被废弃。工具参数格式与结束原因由 round 层处理，这里不做工具 schema
    /// 校验、不调用工具、不把参数错当成流失败。
    pub(super) async fn stream_llm_response(
        &mut self,
        window: &super::format_recovery::FormatRecoveryWindow,
        messages: &[serde_json::Value],
        system_prompt: &str,
        tools_json: &[serde_json::Value],
        current_context_tokens: usize,
        round_deadline: Option<std::time::Instant>,
    ) -> anyhow::Result<StreamOutput> {
        let policy = self.ctx.config.llm_recovery;
        let prepared = crate::llm::client::prepare_llm_request(
            &self.ctx.llm_backend,
            &self.ctx,
            &self.model_name,
            self.model_alias.as_deref(),
            messages,
            tools_json,
            system_prompt,
        )
        .await?;
        let mut retry = RequestRetryState::new_bounded(&policy, round_deadline);
        // The same deadline covers request establishment, stream consumption
        // and every backoff wait — a retry never extends it.
        let request_deadline = retry.deadline();
        let mut had_format_error = false;
        let mut last_failure = String::new();
        let mut attempt: u32 = 0;
        loop {
            if self.ctx.cancel.is_cancelled() || self.ctx.interrupt.load(Ordering::SeqCst) {
                self.ctx.log_event(crate::events::EventLog::Stop {
                    reason: "interrupted".into(),
                });
                return Err(anyhow::Error::new(TurnInterrupted));
            }
            // A latched session must not open new requests.
            self.ctx.persistence_fault.check()?;
            if retry.deadline_expired() {
                self.log_recovery(
                    attempt + 1,
                    "timeout",
                    &retry,
                    window,
                    Some("request_timeout"),
                );
                return Err(terminal_error(RequestTerminal::Timeout, last_failure));
            }

            let attempt_cancel = self.ctx.cancel.linked_child_token();
            let outcome = self
                .consume_attempt(
                    &prepared,
                    &attempt_cancel,
                    current_context_tokens,
                    request_deadline,
                )
                .await;
            // Close the attempt's background stream task without touching the
            // parent runtime token, then decide.
            attempt_cancel.cancel();

            match outcome {
                Ok(mut output) => {
                    output.had_format_error = had_format_error;
                    return Ok(output);
                }
                Err(AttemptFailure::Interrupted) => {
                    self.ctx.log_event(crate::events::EventLog::Stop {
                        reason: "interrupted".into(),
                    });
                    return Err(anyhow::Error::new(TurnInterrupted));
                }
                Err(AttemptFailure::Fatal(error)) => return Err(error),
                Err(AttemptFailure::Timeout) => {
                    let detail = if last_failure.is_empty() {
                        "request deadline exceeded while the request was in flight".to_string()
                    } else {
                        last_failure.clone()
                    };
                    self.log_recovery(
                        attempt + 1,
                        "timeout",
                        &retry,
                        window,
                        Some("request_timeout"),
                    );
                    return Err(terminal_error(RequestTerminal::Timeout, detail));
                }
                Err(AttemptFailure::Retry(failure)) => {
                    last_failure = failure.message;
                    if failure.protocol_damaged {
                        had_format_error = true;
                        // Side-effect-free pre-check: the round may not start
                        // another feedback round. Do not insert the slot here;
                        // the round-end commit owns that.
                        if window.would_exceed(true) {
                            return Ok(StreamOutput::format_abort(format!(
                                "format_recovery_exhausted: response stream damaged and the format window is exhausted: {last_failure}"
                            )));
                        }
                    }
                    // Cancellation always wins over a pending retry.
                    if self.ctx.cancel.is_cancelled() || self.ctx.interrupt.load(Ordering::SeqCst) {
                        return Err(anyhow::Error::new(TurnInterrupted));
                    }
                    if !retry.can_retry() {
                        self.log_recovery(
                            attempt + 1,
                            failure.category,
                            &retry,
                            window,
                            Some("request_retry_exhausted"),
                        );
                        return Err(terminal_error(
                            RequestTerminal::RetryExhausted,
                            last_failure,
                        ));
                    }
                    let wait = match retry.retry_wait(
                        attempt,
                        failure.retry_after,
                        recovery::jitter_fraction(),
                    ) {
                        Ok(wait) => wait,
                        Err(_) => {
                            self.log_recovery(
                                attempt + 1,
                                failure.category,
                                &retry,
                                window,
                                Some("request_timeout"),
                            );
                            return Err(terminal_error(RequestTerminal::Timeout, last_failure));
                        }
                    };
                    // Exactly one Retry control notification per runtime-managed
                    // retry: the last candidate is discarded and a new request
                    // follows.
                    self.log_recovery(attempt + 1, failure.category, &retry, window, None);
                    self.ctx.log_event(crate::events::EventLog::Retry);
                    self.ctx.display.render_retry();
                    retry.note_retry();
                    attempt += 1;
                    if !self.wait_retry_delay(wait).await {
                        return Err(anyhow::Error::new(TurnInterrupted));
                    }
                }
            }
        }
    }

    /// Consume one attempt (one physical request) with its own timers and
    /// candidate buffers. `deadline` is the optional total request deadline.
    async fn consume_attempt(
        &self,
        prepared: &crate::llm::client::PreparedLlmRequest,
        attempt_cancel: &crate::cancel::CancellationToken,
        current_context_tokens: usize,
        deadline: Option<Instant>,
    ) -> std::result::Result<StreamOutput, AttemptFailure> {
        // 首事件期限自本 attempt 起算一次绝对 deadline，覆盖“请求建立＋首事件”；
        // 建流返回不会重新获得完整预算，`Retry` 事件也不延长它。
        let stream_started = Instant::now();
        let first_event_timeout = positive_duration(self.ctx.config.llm_first_event_timeout_secs);
        let idle_timeout = positive_duration(self.ctx.config.llm_idle_timeout_secs);
        let heartbeat = positive_duration(self.ctx.config.llm_wait_heartbeat_secs);
        let mut last_heartbeat_at = stream_started;

        // 建流 future 只创建一次并 pin：tick 分支不得重建它，否则会重复发请求。
        let mut establish = std::pin::pin!(crate::llm::client::open_llm_stream(
            &self.ctx.llm_backend,
            &self.ctx,
            prepared,
            attempt_cancel.clone(),
        ));
        let mut stream = loop {
            tokio::select! {
                biased;
                // 已完成的建流结果优先于并发到达的取消：迟到取消不能覆盖
                // 已经确定的成功结果。
                result = &mut establish => {
                    break match result {
                        Ok(response) => response.events,
                        Err(error) => return Err(establish_failure(error)),
                    };
                }
                // shutdown / 外层取消：与流消费阶段一致映射为中断。
                _ = self.ctx.cancel.cancelled() => {
                    return Err(AttemptFailure::Interrupted);
                }
                // 可选总期限覆盖建流：到点必须停止等待，不建流也不重试。
                _ = recovery::wait_until(deadline) => {
                    return Err(AttemptFailure::Timeout);
                }
                // tick 仅负责 interrupt 轮询、首事件 deadline 与等待心跳。
                _ = tokio::time::sleep(std::time::Duration::from_millis(25)) => {
                    if self.ctx.interrupt.load(Ordering::SeqCst) {
                        return Err(AttemptFailure::Interrupted);
                    }
                    if let Err(error) = self.check_llm_wait_timeout(
                        false,
                        stream_started,
                        stream_started,
                        first_event_timeout,
                        idle_timeout,
                    ) {
                        return Err(attempt_error_failure(error));
                    }
                    self.maybe_render_llm_wait_heartbeat(
                        false,
                        stream_started,
                        stream_started,
                        &mut last_heartbeat_at,
                        heartbeat,
                    );
                }
            }
        };

        let mut text = String::new();
        let mut thinking = String::new();
        let mut calls: Vec<ToolCallEvent> = Vec::new();
        let mut stop = String::new();
        let mut usage: Option<UsageEvent> = None;
        let mut saw_stop = false;
        let mut saw_any_event = false;
        let mut saw_visible_output = false;
        // idle 只在流消费阶段计量；首事件 deadline 继续使用本 attempt 起点。
        let mut last_event_at = Instant::now();

        loop {
            if self.ctx.cancel.is_cancelled() || self.ctx.interrupt.load(Ordering::SeqCst) {
                self.ctx.log_event(crate::events::EventLog::Stop {
                    reason: "interrupted".into(),
                });
                stop = "interrupted".into();
                saw_stop = true;
                break;
            }
            // 可选总期限在消费循环的确定路径上检查：事件比 25ms tick 更快时
            // 也不能超期继续消费。
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(AttemptFailure::Timeout);
            }
            // Deadline enforcement lives on the deterministic loop path: a
            // stream of events arriving faster than the 25ms tick must not
            // starve the first-event/idle checks (each iteration recreates
            // the select's sleep timer).
            if let Err(error) = self.check_llm_wait_timeout(
                saw_any_event,
                stream_started,
                last_event_at,
                first_event_timeout,
                idle_timeout,
            ) {
                return Err(attempt_error_failure(error));
            }

            let result = tokio::select! {
                result = stream.next() => result,
                _ = tokio::time::sleep(std::time::Duration::from_millis(25)) => {
                    self.maybe_render_llm_wait_heartbeat(
                        saw_any_event,
                        stream_started,
                        last_event_at,
                        &mut last_heartbeat_at,
                        heartbeat,
                    );
                    continue;
                }
            };
            let Some(result) = result else {
                break;
            };
            let evt = match result {
                Ok(evt) => evt,
                Err(error) => return Err(attempt_error_failure(error)),
            };
            // Retry is a control notification, not a valid first provider
            // event: it must not satisfy (or restart) the first-event
            // deadline, which stays absolute until real output arrives.
            // A backend-internal retry notification neither grants a new
            // request budget nor resets the attempt timer.
            // Retry is a control notification, not progress: it must not
            // refresh the idle clock, otherwise a backend that only pings
            // Retry can hang the turn beyond every configured timeout (F5).
            let is_retry = matches!(&evt, Event::Retry(_));
            if !is_retry {
                saw_any_event = true;
                last_event_at = Instant::now();
            }

            match evt {
                Event::Thinking(t) => {
                    saw_visible_output = true;
                    self.ctx.log_event(crate::events::EventLog::Thinking {
                        version: None,
                        content: t.content.clone(),
                    });
                    self.ctx.display.render_thinking(&t.content);
                    thinking.push_str(&t.content);
                }
                Event::Text(t) => {
                    saw_visible_output = true;
                    self.ctx.log_event(crate::events::EventLog::Text {
                        version: None,
                        content: t.content.clone(),
                    });
                    self.ctx.display.render_text(&t.content);
                    text.push_str(&t.content);
                }
                Event::ToolCall(call) => {
                    saw_visible_output = true;
                    calls.push(call);
                }
                Event::Usage(u) => {
                    self.ctx.log_event(crate::events::EventLog::Usage {
                        version: None,
                        input_tokens: u.input_tokens,
                        output_tokens: u.output_tokens,
                        cache_read_input_tokens: u.cache_read_input_tokens,
                        cache_creation_input_tokens: u.cache_creation_input_tokens,
                        context_tokens: Some(current_context_tokens),
                        max_context: Some(self.ctx.config.max_context_tokens),
                        kind: "agent".into(),
                    });
                    usage = Some(u);
                }
                Event::UsageUnavailable => {}
                Event::Stop(s) => {
                    self.ctx.log_event(crate::events::EventLog::Stop {
                        reason: s.reason.clone(),
                    });
                    stop = s.reason;
                    saw_stop = true;
                    break;
                }
                Event::Error(e) => {
                    self.ctx.log_event(crate::events::EventLog::Error {
                        version: None,
                        message: e.message.clone(),
                    });
                    if !saw_visible_output && is_context_overflow_message(&e.message) {
                        return Err(AttemptFailure::Fatal(anyhow::Error::new(
                            ContextOverflowError { message: e.message },
                        )));
                    }
                    // A provider error envelope inside a `200` stream is
                    // classified by its structured code/status: known
                    // transient envelopes retry, everything unknown fails
                    // closed with the diagnostic preserved.
                    let kind =
                        recovery::classify_provider_error(e.provider_code.as_deref(), e.status);
                    let message = match e.provider_code {
                        Some(code) => format!("provider error ({code}): {}", e.message),
                        None => e.message,
                    };
                    return Err(match kind {
                        UpstreamFailureKind::Recoverable => AttemptFailure::Retry(RetryFailure {
                            protocol_damaged: false,
                            retry_after: None,
                            message,
                            category: "provider_recoverable",
                        }),
                        _ => AttemptFailure::Fatal(anyhow::Error::new(LlmUpstreamError::new(
                            kind, message,
                        ))),
                    });
                }
                Event::Retry(_) => {
                    self.ctx.log_event(crate::events::EventLog::Retry);
                    text.clear();
                    thinking.clear();
                    calls.clear();
                    stop.clear();
                    usage = None;
                    saw_stop = false;
                    self.ctx.display.render_retry();
                }
            }
        }

        drop(stream);
        if !saw_stop {
            // 未正常终止的 EOF：当前 attempt 不可信，废弃候选并按格式错误
            // 记账（由调用方完成预判/提交）。
            return Err(AttemptFailure::Retry(RetryFailure {
                protocol_damaged: true,
                retry_after: None,
                message: "response stream ended without a terminal stop event".into(),
                category: "protocol_damaged",
            }));
        }
        Ok(StreamOutput {
            text,
            thinking,
            calls,
            stop,
            usage,
            format_abort: None,
            had_format_error: false,
        })
    }

    fn check_llm_wait_timeout(
        &self,
        saw_any_event: bool,
        stream_started: Instant,
        last_event_at: Instant,
        first_event_timeout: Option<Duration>,
        idle_timeout: Option<Duration>,
    ) -> anyhow::Result<()> {
        let now = Instant::now();
        if !saw_any_event {
            if let Some(timeout) = first_event_timeout
                && now.duration_since(stream_started) >= timeout
            {
                let message = format!(
                    "LLM stream first event timeout after {} seconds",
                    timeout.as_secs()
                );
                self.ctx.log_event(crate::events::EventLog::TurnError {
                    error: message.clone(),
                    category: "llm_first_event_timeout".into(),
                    severity: None,
                    belief: None,
                    model: None,
                    elapsed_ms: Some(now.duration_since(stream_started).as_millis() as u64),
                    idle_ms: None,
                });
                return Err(anyhow::Error::new(LlmUpstreamError::recoverable(message)));
            }
            return Ok(());
        }

        if let Some(timeout) = idle_timeout
            && now.duration_since(last_event_at) >= timeout
        {
            let message = format!(
                "LLM stream idle timeout after {} seconds without events",
                timeout.as_secs()
            );
            self.ctx.log_event(crate::events::EventLog::TurnError {
                error: message.clone(),
                category: "llm_idle_timeout".into(),
                severity: None,
                belief: None,
                model: None,
                elapsed_ms: None,
                idle_ms: Some(now.duration_since(last_event_at).as_millis() as u64),
            });
            return Err(anyhow::Error::new(LlmUpstreamError::recoverable(message)));
        }
        Ok(())
    }

    fn maybe_render_llm_wait_heartbeat(
        &self,
        saw_any_event: bool,
        stream_started: Instant,
        last_event_at: Instant,
        last_heartbeat_at: &mut Instant,
        heartbeat: Option<Duration>,
    ) {
        let Some(heartbeat) = heartbeat else {
            return;
        };
        let now = Instant::now();
        if now.duration_since(*last_heartbeat_at) < heartbeat {
            return;
        }
        *last_heartbeat_at = now;
        let phase = if saw_any_event { "idle" } else { "first_event" };
        let elapsed = now.duration_since(stream_started).as_secs();
        let idle = now.duration_since(last_event_at).as_secs();
        self.ctx.log_event(crate::events::EventLog::LlmWait {
            phase: phase.into(),
            elapsed_secs: elapsed,
            idle_secs: idle,
        });
        self.ctx
            .display
            .render_info(&crate::ui::llm_wait_heartbeat_message(elapsed, idle));
    }

    /// Cancellable backoff wait; `false` means the turn was interrupted or
    /// cancelled while waiting.
    async fn wait_retry_delay(&self, wait: Duration) -> bool {
        let sleep = tokio::time::sleep(wait);
        tokio::pin!(sleep);
        loop {
            if self.ctx.cancel.is_cancelled() || self.ctx.interrupt.load(Ordering::SeqCst) {
                return false;
            }
            tokio::select! {
                _ = &mut sleep => return true,
                _ = tokio::time::sleep(Duration::from_millis(20)) => {}
            }
        }
    }

    /// Minimal bounded-recovery diagnostic record.
    fn log_recovery(
        &self,
        attempt: u32,
        category: &str,
        retry: &RequestRetryState,
        window: &super::format_recovery::FormatRecoveryWindow,
        terminal: Option<&str>,
    ) {
        self.ctx.log_event(crate::events::EventLog::LlmRecovery {
            round: Some(self.local.round),
            attempt,
            category: category.to_string(),
            retries: retry.used_retries(),
            window_errors: window.error_count(),
            terminal: terminal.map(str::to_string),
        });
    }
}

fn establish_failure(error: anyhow::Error) -> AttemptFailure {
    if error.downcast_ref::<TurnInterrupted>().is_some() {
        return AttemptFailure::Interrupted;
    }
    if is_context_overflow_message(&format!("{error:#}")) {
        return AttemptFailure::Fatal(anyhow::Error::new(ContextOverflowError {
            message: format!("{error:#}"),
        }));
    }
    attempt_error_failure(error)
}

/// Classify an attempt error. Untyped errors keep their pre-existing
/// fail-closed behavior (a custom backend must return the typed error to
/// request retries).
fn attempt_error_failure(error: anyhow::Error) -> AttemptFailure {
    if error.downcast_ref::<TurnInterrupted>().is_some() {
        return AttemptFailure::Interrupted;
    }
    let Some(upstream) = find_upstream_failure(&error) else {
        return AttemptFailure::Fatal(error);
    };
    match upstream.kind() {
        UpstreamFailureKind::Recoverable => AttemptFailure::Retry(RetryFailure {
            protocol_damaged: false,
            retry_after: upstream.retry_after(),
            category: "recoverable",
            message: format!("{error:#}"),
        }),
        UpstreamFailureKind::ProtocolDamaged => AttemptFailure::Retry(RetryFailure {
            protocol_damaged: true,
            retry_after: None,
            category: "protocol_damaged",
            message: format!("{error:#}"),
        }),
        UpstreamFailureKind::Permanent => AttemptFailure::Fatal(error),
    }
}
