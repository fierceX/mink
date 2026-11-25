use super::format::{format_dispatched_result, format_tool_result_with_artifact};
use super::*;

impl ToolRunner {
    pub(crate) async fn execute_all(
        &self,
        calls: Vec<ToolCallEvent>,
    ) -> std::result::Result<Vec<ToolExecution>, FatalBatchError> {
        let mut results = Vec::new();
        let mut read_batch = Vec::new();
        // One budget per execute_all: sequential tools flush the read batch
        // mid-way, but the counters must NOT reset there or a Read→Bash→Read
        // group would slip two 10MiB images into the same request.
        let mut image_budget = ImageBatchBudget::default();
        // A latched session must not dispatch further tools: stop before the
        // next call and report every remaining call as explicitly not
        // executed so the tool_call/result protocol stays complete.
        let mut halted: Option<String> = None;

        for call in calls {
            if halted.is_none()
                && let Err(error) = self.ctx.persistence_fault.check()
            {
                halted = Some(format!(
                    "not executed: session state persistence fault: {error:#}"
                ));
            }
            if let Some(reason) = &halted {
                for pending in std::mem::take(&mut read_batch) {
                    results.push(not_executed_tool_result(pending, reason));
                }
                results.push(not_executed_tool_result(call, reason));
                continue;
            }
            let metadata = self.metadata_for(&call.name);
            let custom_sequential = self.find_custom_tool(&call.name).is_some_and(|tool| {
                tool.definition.execution == crate::tools::extension::ToolExecutionMode::Sequential
            });
            if custom_sequential
                || (self
                    .ctx
                    .goal_store
                    .snapshot()
                    .is_some_and(|g| g.phase == crate::session::goal::GoalPhase::Executing)
                    && self.find_custom_tool(&call.name).is_some())
                || requires_sequential_execution(metadata.as_ref())
            {
                match self
                    .execute_read_batch(std::mem::take(&mut read_batch), &mut image_budget)
                    .await
                {
                    Ok(mut batch) => results.append(&mut batch),
                    Err(FatalBatchError { error, completed }) => {
                        results.extend(completed);
                        return Err(FatalBatchError {
                            error,
                            completed: results,
                        });
                    }
                }
                let call = match self.prepare_call(call) {
                    PreparedCall::Immediate(result) => {
                        results.push(result);
                        continue;
                    }
                    PreparedCall::Execute(call) => call,
                };
                if records_goal_effect(
                    &call.name,
                    metadata.as_ref(),
                    self.find_custom_tool(&call.name).is_some(),
                ) && let Err(error) = self.ctx.goal_store.mark_effect(&call.id)
                {
                    results.push(not_executed_tool_result(
                        call,
                        &format!("not executed: {error:#}"),
                    ));
                    return Err(FatalBatchError {
                        error,
                        completed: results,
                    });
                }
                match self.dispatch_sequential(call.clone()).await {
                    Ok(result) => results.push(result),
                    Err(error) => {
                        results.push(unknown_tool_result(call, &error));
                        return Err(FatalBatchError {
                            error,
                            completed: results,
                        });
                    }
                }
            } else {
                read_batch.push(call);
            }
        }

        match self.execute_read_batch(read_batch, &mut image_budget).await {
            Ok(mut batch) => results.append(&mut batch),
            Err(FatalBatchError { error, completed }) => {
                results.extend(completed);
                return Err(FatalBatchError {
                    error,
                    completed: results,
                });
            }
        }
        Ok(results)
    }

    pub fn finalize_deferred_results(&self, results: &mut [ToolExecution]) {
        for result in results {
            if !result.needs_finalization {
                continue;
            }
            let formatted = format_tool_result_with_artifact(
                &result.tool_name,
                &result.content,
                self.ctx.tool_config.tool_result_max_bytes,
                &self.ctx,
            );
            result.content = formatted.content;
            result.artifacts.extend(formatted.artifacts);
            result.needs_finalization = false;
        }
    }

    async fn execute_read_batch(
        &self,
        calls: Vec<ToolCallEvent>,
        budget: &mut ImageBatchBudget,
    ) -> std::result::Result<Vec<ToolExecution>, FatalBatchError> {
        if calls.is_empty() {
            return Ok(Vec::new());
        }
        // Every call passes the policy gate (surface membership, StormBreaker,
        // approval/blocking) BEFORE any classification or file/VFS access:
        // a crafted image call must never execute when Read is not on the
        // model surface (v7 §7.3 / review fix #1). Slots preserve the original
        // call order so mixed batches return results in dispatch order.
        #[allow(clippy::large_enum_variant)]
        enum PreparedSlot {
            Immediate(ToolExecution),
            Text(ToolCallEvent),
            Image {
                call: ToolCallEvent,
                kind: crate::tools::file::ImageReadKind,
            },
            ClassifyError {
                call: ToolCallEvent,
                error: String,
            },
        }
        let mut slots = Vec::with_capacity(calls.len());
        for call in calls {
            match self.prepare_call(call) {
                PreparedCall::Immediate(result) => slots.push(PreparedSlot::Immediate(result)),
                PreparedCall::Execute(call) => {
                    let slot = if call.name == "Read" {
                        let path = call
                            .input_json
                            .get("path")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        // Classification may touch the VFS (slow) — run it on
                        // the blocking pool so async workers are not stalled.
                        let ctx = self.ctx.clone();
                        let path_for_task = path.clone();
                        let classified = tokio::task::spawn_blocking(move || {
                            // Invalid path selection keeps the text-path error
                            // surface (e.g. serde unknown-field diagnostics).
                            let selection =
                                match crate::resources::selector::split_read_path_selection(
                                    &path_for_task,
                                ) {
                                    Ok(selection) => selection,
                                    Err(_) => {
                                        return Ok(crate::tools::file::ImageReadKind::NotImage);
                                    }
                                };
                            crate::tools::file::classify_image_read(
                                &ctx,
                                &path_for_task,
                                &selection,
                            )
                        })
                        .await;
                        match classified {
                            Ok(Ok(kind))
                                if !matches!(kind, crate::tools::file::ImageReadKind::NotImage) =>
                            {
                                PreparedSlot::Image { call, kind }
                            }
                            Ok(Ok(_)) => PreparedSlot::Text(call),
                            // Classification failures must surface as failed
                            // ToolExecutions, never a silent fallback to the
                            // text path.
                            Ok(Err(error)) => PreparedSlot::ClassifyError {
                                call,
                                error: format!("{error:#}"),
                            },
                            Err(error) => PreparedSlot::ClassifyError {
                                call,
                                error: format!("{error:#}"),
                            },
                        }
                    } else {
                        PreparedSlot::Text(call)
                    };
                    slots.push(slot);
                }
            }
        }

        // Phase one, text family: concurrent dispatch (policy gate already
        // passed above, so no second prepare_call).
        let mut text_handles = Vec::new();
        for slot in &slots {
            if let PreparedSlot::Text(call) = slot {
                if let Some(tool) = self.find_custom_tool(&call.name).cloned() {
                    let ctx = self.ctx.clone();
                    let call = call.clone();
                    let storm = self.storm.clone();
                    text_handles.push(tokio::spawn(async move {
                        let result = execute_custom(&ctx, &call, tool).await?;
                        mark_format_confirmed(&storm, &call.name, &call.storm_identity(), &result);
                        Ok::<_, anyhow::Error>(result)
                    }));
                } else {
                    let ctx = self.ctx.clone();
                    let tool_name = call.name.clone();
                    let call = call.clone();
                    let name = call.name.clone();
                    let identity = call.storm_identity();
                    let storm = self.storm.clone();
                    text_handles.push(tokio::spawn(async move {
                        let result = tokio::task::spawn_blocking(move || {
                            let tool = tool_registry()
                                .iter()
                                .find(|t| t.metadata().name == tool_name);
                            Self::execute_one_sync(&ctx, &call, tool.map(|t| t.as_ref()))
                        })
                        .await??;
                        mark_format_confirmed(&storm, &name, &identity, &result);
                        Ok::<_, anyhow::Error>(result)
                    }));
                }
            }
        }
        // Join each dispatched slot, including siblings that finished before
        // the failure was observed. An aborted handle proves no outcome.
        let mut text_results = Vec::with_capacity(text_handles.len());
        let mut handles = text_handles.into_iter();
        let mut fatal = None;
        for slot in &slots {
            let PreparedSlot::Text(call) = slot else {
                continue;
            };
            let handle = handles.next().expect("one handle per dispatched text slot");
            match handle
                .await
                .map_err(|e| anyhow::anyhow!("tool task join failed: {e}"))
                .and_then(|r| r)
            {
                Ok(result) => text_results.push(result),
                Err(error) => {
                    text_results.push(unknown_tool_result(call.clone(), &error));
                    if fatal.is_none() {
                        fatal = Some(error);
                        for pending in handles.as_slice() {
                            pending.abort();
                        }
                    }
                }
            }
        }

        // Image family runs strictly in call order: prepare one image at a
        // time (bounded memory — never hold every image's bytes at once),
        // then admit it against the per-`execute_all` image budget shared
        // across flushes (sequential tools do not reset it).
        let mut text_iter = text_results.into_iter();
        let mut results = Vec::with_capacity(slots.len());
        for slot in slots {
            match slot {
                PreparedSlot::Immediate(result) => results.push(result),
                PreparedSlot::Text(_) => results.push(text_iter.next().expect("text slot")),
                PreparedSlot::Image { call, kind } => {
                    if let Some(error) = &fatal {
                        results.push(not_executed_tool_result(
                            call,
                            &format!("not executed: {error:#}"),
                        ));
                        continue;
                    }
                    let ctx = self.ctx.clone();
                    match kind {
                        crate::tools::file::ImageReadKind::VfsCandidate {
                            display_path,
                            has_selector,
                        } => {
                            // Read the VFS image on demand (one at a time,
                            // bounded memory). NotImage falls back to the
                            // ordinary text read for this call (selector
                            // honored).
                            let ctx_for_prepare = ctx.clone();
                            let outcome = match tokio::task::spawn_blocking(move || {
                                crate::tools::file::prepare_vfs_candidate(
                                    &ctx_for_prepare,
                                    display_path,
                                    has_selector,
                                )
                            })
                            .await
                            {
                                Ok(result) => result,
                                Err(error) => {
                                    Err(anyhow::anyhow!("image prepare task failed: {error}"))
                                }
                            };
                            match outcome {
                                Ok(crate::tools::file::VfsImageOutcome::Image(prepared)) => {
                                    results.push(
                                        self.commit_image_result(call, Ok(prepared), budget).await,
                                    );
                                }
                                Ok(crate::tools::file::VfsImageOutcome::NotImage) => {
                                    results.push(self.execute_text_fallback(call).await);
                                }
                                Err(error) => results.push(failed_tool_result(
                                    call.id,
                                    call.name,
                                    call.fields,
                                    format!("{error:#}"),
                                )),
                            }
                        }
                        _ => {
                            let prepared = match tokio::task::spawn_blocking(move || {
                                crate::tools::file::prepare_image_read(&ctx, kind)
                            })
                            .await
                            {
                                Ok(result) => result,
                                Err(error) => {
                                    Err(anyhow::anyhow!("image prepare task failed: {error}"))
                                }
                            };
                            results.push(self.commit_image_result(call, prepared, budget).await);
                        }
                    }
                }
                PreparedSlot::ClassifyError { call, error } => {
                    results.push(failed_tool_result(call.id, call.name, call.fields, error))
                }
            }
        }
        if let Some(error) = fatal {
            return Err(FatalBatchError {
                error,
                completed: results,
            });
        }
        Ok(results)
    }

    /// Reserve quota in call order and publish the prepared image object.
    /// Quota failures and commit failures are ordinary failed
    /// ToolExecutions; a failed commit never consumes the reservation. The
    /// fsync-heavy cache commit runs on the blocking pool.
    async fn commit_image_result(
        &self,
        call: ToolCallEvent,
        prepared: anyhow::Result<crate::tools::file::PreparedImage>,
        batch: &mut ImageBatchBudget,
    ) -> ToolExecution {
        let prepare_error = |error: anyhow::Error| {
            failed_tool_result(
                call.id.clone(),
                call.name.clone(),
                call.fields.clone(),
                format!("{error:#}"),
            )
        };
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => return prepare_error(error),
        };
        let review_capture = if let Some(review) = self.ctx.review_evidence() {
            let evidence_context = match self.ctx.goal_review_tool_context() {
                Ok(context) => context,
                Err(error) => return prepare_error(error),
            };
            match review.capture_read(&call, &evidence_context) {
                Ok(capture) => capture,
                Err(error) => return prepare_error(error),
            }
        } else {
            None
        };
        if let Some((_, _, hash)) = &review_capture {
            use sha2::{Digest, Sha256};
            let actual = Sha256::digest(&prepared.bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>();
            if hash != &actual {
                return prepare_error(anyhow::anyhow!("image evidence changed during read"));
            }
        }
        let bytes = prepared.bytes.len() as u64;
        // Per-`execute_all` admission (v7 §7.3): the budget starts at zero
        // for every `execute_all` call (shared across sequential-tool
        // flushes), so history never locks out new reads. A rejected read
        // returns an ordinary failed ToolExecution and is never published —
        // no promise of a later attach that materialization could not keep.
        if let Some(limits) = crate::tools::file::image_limits(&self.ctx)
            && (batch.used_images >= limits.max_images_per_request
                || batch.used_bytes.saturating_add(bytes) > limits.max_image_bytes_per_request)
        {
            return prepare_error(anyhow::anyhow!(
                "Error: image attachment batch limit exceeded: usage {}/{} images and {}/{} bytes; use fewer Read calls in this batch",
                batch.used_images,
                limits.max_images_per_request,
                batch.used_bytes,
                limits.max_image_bytes_per_request
            ));
        }
        // Phase B: commit on the blocking pool (no guard held across await).
        let image_id = match &prepared.existing_id {
            Some(id) => id.clone(),
            None => {
                let cache = self.ctx.image_cache.clone();
                let bytes = prepared.bytes.clone();
                match tokio::task::spawn_blocking(move || cache.commit(&bytes)).await {
                    Ok(Ok(id)) => id,
                    Ok(Err(error)) => {
                        return prepare_error(anyhow::anyhow!(
                            "Error: failed to persist image: {error:#}"
                        ));
                    }
                    Err(error) => {
                        return prepare_error(anyhow::anyhow!(
                            "Error: failed to persist image: {error:#}"
                        ));
                    }
                }
            }
        };
        // Commit succeeded — consume the batch budget.
        batch.used_images = batch.used_images.saturating_add(1);
        batch.used_bytes = batch.used_bytes.saturating_add(bytes);
        // Register as a this-turn capture so a materialization failure of a
        // freshly produced attachment fails the request instead of silently
        // degrading (v7 §9.5).
        self.ctx
            .this_turn_image_ids
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(image_id.clone());

        if let (Some(review), Some(captured)) = (self.ctx.review_evidence(), review_capture)
            && let Err(error) = review.finish_read(captured)
        {
            return prepare_error(error);
        }
        let attachment = crate::tools::image::ImageAttachment {
            image_id,
            format: prepared.info.format,
            width: prepared.info.width,
            height: prepared.info.height,
            bytes,
            name: prepared.name,
        };
        let content = attachment.summary(&prepared.display_path);
        ToolExecution {
            tool_use_id: call.id,
            tool_name: call.name,
            tool_args: call.fields,
            content: content.clone(),
            conv_content: content,
            spawns_sub_agent: false,
            sub_agent_prompt: None,
            sub_agent_fork: false,
            sub_agent_label: None,
            sub_agent_model: None,
            exit_code: None,
            status: crate::tools::metadata::ToolStatus::Succeeded,
            result_kind: crate::tools::metadata::ToolResultKind::FileRead,
            presentation: None,
            artifacts: Vec::new(),
            signals: Vec::new(),
            failure_source: None,
            plan_command: None,
            needs_finalization: false,
            state_metadata: None,
            image_attachment: Some(attachment),
        }
    }

    async fn dispatch_sequential(&self, call: ToolCallEvent) -> Result<ToolExecution> {
        if let Some(tool) = self.find_custom_tool(&call.name).cloned() {
            let result = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(
                execute_custom(&self.ctx, &call, tool),
            ))
            .await
            .map_err(|_| anyhow::anyhow!("sequential tool task panicked"))??;
            mark_format_confirmed(&self.storm, &call.name, &call.storm_identity(), &result);
            return Ok(result);
        }
        let ctx = self.ctx.clone();
        let tool_name = call.name.clone();
        let name = call.name.clone();
        let identity = call.storm_identity();
        let storm = self.storm.clone();
        let result = tokio::task::spawn_blocking(move || {
            let tool = tool_registry()
                .iter()
                .find(|t| t.metadata().name == tool_name);
            Self::execute_one_sync(&ctx, &call, tool.map(|t| t.as_ref()))
        })
        .await??;
        mark_format_confirmed(&storm, &name, &identity, &result);
        Ok(result)
    }

    fn execute_one_sync(
        ctx: &ToolContext,
        call: &ToolCallEvent,
        tool_fn: Option<&dyn ToolExec>,
    ) -> Result<ToolExecution> {
        let review = ctx.review_evidence();
        let captured = if let Some(review) = review {
            review.capture_read(call, &ctx.goal_review_tool_context()?)?
        } else {
            None
        };
        let raw = dispatch_tool(ctx, call, tool_fn);
        if raw.status == ToolStatus::Succeeded
            && let (Some(review), Some(captured)) = (review, captured)
        {
            review.finish_read(captured)?;
        }
        let mut result = format_dispatched_result(ctx, call, raw);
        ctx.plan_store.bind_transition(&mut result)?;
        Ok(result)
    }

    pub(crate) async fn recover_plan_transactions(&self) -> Result<()> {
        self.ctx.plan_store.recover_pending(&self.ctx.store).await
    }

    pub(crate) async fn finish_plan_transition(
        &self,
        tool_use_id: &str,
        command: PlanCommand,
    ) -> Result<()> {
        self.ctx
            .plan_store
            .finish_transition(&self.ctx.store, tool_use_id, command)
            .await
    }

    /// VFS `NotImage` fallback: run the ordinary text Read for this call
    /// (classification deferred the decision; the backend reported no
    /// image).
    async fn execute_text_fallback(&self, call: ToolCallEvent) -> ToolExecution {
        let ctx = self.ctx.clone();
        // Static registry reference: safe to move into the blocking task.
        let tool_fn = tool_registry()
            .iter()
            .find(|t| t.metadata().name == call.name)
            .map(|t| t.as_ref());
        let call_for_task = call.clone();
        let name = call.name.clone();
        let identity = call.storm_identity();
        let result = match tokio::task::spawn_blocking(move || {
            Self::execute_one_sync(&ctx, &call_for_task, tool_fn)
        })
        .await
        {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => {
                failed_tool_result(call.id, call.name, call.fields, format!("{error:#}"))
            }
            Err(error) => failed_tool_result(
                call.id.clone(),
                call.name.clone(),
                call.fields.clone(),
                format!("text fallback task failed: {error}"),
            ),
        };
        mark_format_confirmed(&self.storm, &name, &identity, &result);
        result
    }

    /// Whether a candidate call was already confirmed as a pure model-format
    /// decode failure (used to let the recovery guard keep waiting for a real
    /// call instead of rewriting a known format problem).
    pub(crate) fn is_format_confirmed(&self, call: &ToolCallEvent) -> bool {
        if call.parse_error.is_some() {
            return true;
        }
        let Some(metadata) = self.metadata_for(&call.name) else {
            return false;
        };
        self.storm
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_format_confirmed(&call.name, &call.storm_identity(), metadata.mutating)
    }
}

async fn execute_custom(
    ctx: &ToolContext,
    call: &ToolCallEvent,
    tool: crate::tools::extension::RegisteredCustomTool,
) -> Result<ToolExecution> {
    let definition = &tool.definition;
    let started = std::time::Instant::now();
    let timeout = crate::tools::process::resolve_tool_timeout(
        None,
        ctx.tool_config.tool_timeout_secs,
        ctx.tool_config.tool_timeout_max_secs,
    )?;
    let timeout_secs = timeout.as_secs();
    let execution = tool.executor.execute(
        call.input_json.clone(),
        crate::tools::extension::ToolExecutionContext::new(
            ctx.cwd.clone(),
            ctx.interrupt.as_atomic(),
        ),
    );
    tokio::pin!(execution);
    let deadline = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs));
    tokio::pin!(deadline);
    let result = tokio::select! {
        result = &mut execution => Ok(result),
        _ = ctx.cancel.cancelled() => Err(format!("custom tool {} cancelled", definition.name)),
        _ = &mut deadline => Err(format!("custom tool {} timed out after {timeout_secs}s", definition.name)),
        _ = ctx.interrupt.notified() => Err(format!("custom tool {} interrupted", definition.name)),
    };
    let result = match result {
        Ok(returned) => returned,
        Err(reason) => {
            // Goal journals every extension before dispatch, including those
            // declared read-only. An abandoned future cannot confirm its effect.
            if definition.mutating
                || ctx
                    .goal_store
                    .snapshot()
                    .is_some_and(|goal| goal.pending_effects.contains(&call.id))
            {
                anyhow::bail!("{reason}");
            }
            Err(crate::tools::extension::ToolError::new(reason))
        }
    };
    let raw = match result {
        Ok(output) => {
            let status = if output.success {
                ToolStatus::Succeeded
            } else {
                failed_status(&output.content, output.exit_code, None)
            };
            ToolExecOutput {
                content: output.content,
                is_bash: false,
                conv_content: output.conversation_content.unwrap_or_default(),
                exit_code: output.exit_code,
                wall_ms: Some(started.elapsed().as_millis()),
                no_mutation: false,
                memo_candidate: None,
                spawns_sub_agent: false,
                status,
                diagnostics: Vec::new(),
                plan_command: None,
                state_metadata: None,
                result_kind: definition.result_kind,
                presentation: None,
                failure_source: None,
            }
        }
        Err(error) => {
            let detail = error.to_string();
            let argument = error.is_argument();
            let mut failed = ToolExecOutput::failed(
                format!("Error: tool execution failed: {detail}"),
                if argument {
                    ToolStatus::Failed(ToolFailureKind::ArgumentInvalid)
                } else {
                    failed_status(&detail, None, None)
                },
                definition.result_kind,
                Some(started.elapsed().as_millis()),
            );
            if argument {
                failed.failure_source = Some(ToolFailureSource::ModelFormat);
            }
            failed
        }
    };
    let formatted = format_dispatched_result(ctx, call, raw);
    if definition.mutating && formatted.succeeded() {
        ctx.bump_mutation();
    }
    Ok(formatted)
}

fn failed_status(
    content: &str,
    exit_code: Option<i32>,
    termination: Option<crate::tools::process::ProcessTermination>,
) -> ToolStatus {
    use crate::tools::process::ProcessTermination;
    // Structured supervision facts win over output text: a command that
    // merely printed the word "timeout" (or exited 130 on purpose) must not
    // be reclassified by keyword sniffing.
    if let Some(termination) = termination {
        match termination {
            ProcessTermination::Interrupted => return ToolStatus::Interrupted,
            ProcessTermination::TimedOut => {
                return ToolStatus::Failed(ToolFailureKind::Timeout);
            }
            ProcessTermination::Signaled(_) => {
                return ToolStatus::Failed(ToolFailureKind::ProcessFailed);
            }
            ProcessTermination::Exited(0) => {}
            ProcessTermination::Exited(_) => {
                return ToolStatus::Failed(ToolFailureKind::ProcessFailed);
            }
        }
    }
    let kind = crate::tools::metadata::classify_failure_kind(content, exit_code);
    if kind == ToolFailureKind::Aborted && content.to_lowercase().contains("interrupt") {
        ToolStatus::Interrupted
    } else {
        ToolStatus::Failed(kind)
    }
}

fn dispatch_tool(
    ctx: &ToolContext,
    call: &ToolCallEvent,
    tool_fn: Option<&dyn ToolExec>,
) -> ToolExecOutput {
    if let Some(t) = tool_fn {
        let metadata = t.metadata();
        let started = std::time::Instant::now();
        match t.execute(&call.input_json, ctx) {
            Ok(outcome) => {
                let status = if outcome.success {
                    ToolStatus::Succeeded
                } else {
                    failed_status(&outcome.content, outcome.exit_code, outcome.termination)
                };
                ToolExecOutput {
                    content: outcome.content,
                    is_bash: outcome.is_bash,
                    conv_content: outcome.conversation_content,
                    exit_code: outcome.exit_code,
                    wall_ms: Some(started.elapsed().as_millis()),
                    no_mutation: outcome.no_mutation,
                    memo_candidate: outcome.memo_candidate,
                    spawns_sub_agent: metadata.spawns_sub_agent,
                    status,
                    diagnostics: outcome.diagnostics,
                    plan_command: outcome.plan_command,
                    state_metadata: outcome.state_metadata,
                    result_kind: metadata.result_kind,
                    presentation: outcome.presentation,
                    failure_source: None,
                }
            }
            Err(e) => {
                let detail = e.to_string();
                // Argument decoding failures are model-format provenance: the
                // shared decode helper or an explicit `ToolError::argument`
                // guarantees no side effect happened before the error.
                let argument_error = crate::tools::args::is_argument_error(&e)
                    || e.downcast_ref::<crate::tools::extension::ToolError>()
                        .is_some_and(crate::tools::extension::ToolError::is_argument);
                let status = if argument_error {
                    ToolStatus::Failed(ToolFailureKind::ArgumentInvalid)
                } else {
                    failed_status(&detail, None, None)
                };
                // A failed execute must never mark the call as spawning a
                // sub-agent: the coordinator would launch a child with raw
                // fields even though the executor rejected the input.
                let mut failed = ToolExecOutput::failed(
                    format!("Error: tool execution failed: {detail}"),
                    status,
                    metadata.result_kind,
                    None,
                );
                if argument_error {
                    failed.failure_source = Some(ToolFailureSource::ModelFormat);
                }
                failed
            }
        }
    } else {
        ToolExecOutput::failed(
            format!("Error: tool execution failed: unknown tool: {}", call.name),
            ToolStatus::Failed(ToolFailureKind::Unknown),
            ToolResultKind::Text,
            None,
        )
    }
}

pub(super) fn hashline_target_paths(input: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in input.lines() {
        let trimmed = line.trim_start();
        if let Some(path) = crate::tools::hashline::section_header_path(line) {
            paths.push(path);
        } else if trimmed.starts_with("MV") && trimmed[2..].starts_with(char::is_whitespace) {
            // A moved file lands at the destination: validate the target so
            // `MV source.json -> destination.json` is still covered.
            let destination = trimmed[2..].trim();
            let destination = destination
                .strip_prefix(['\'', '"'])
                .and_then(|rest| rest.strip_suffix(['\'', '"']))
                .unwrap_or(destination);
            if !destination.is_empty() {
                paths.push(destination.to_string());
            }
        }
    }
    paths
}

/// Parse check for one JSON-ish file. `.jsonl` is validated line by line
/// (each line is an independent JSON value); `.json` is validated as a whole.
/// Returns Ok(()) or Err(line_number).
fn json_parse_check(path: &Path, content: &str) -> std::result::Result<(), usize> {
    if path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".jsonl"))
    {
        for (index, line) in content.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if serde_json::from_str::<serde_json::Value>(line).is_err() {
                return Err(index + 1);
            }
        }
        Ok(())
    } else {
        match serde_json::from_str::<serde_json::Value>(content) {
            Ok(_) => Ok(()),
            Err(error) => Err(error.line()),
        }
    }
}

/// For a successful Edit/Write on `.json`/`.jsonl` targets, verify every
/// touched file parses and return short note lines (or None when not
/// applicable). Edit inputs may contain several `[path#TAG]` sections and MV
/// destinations; each JSON target is checked. The note is capped so a large
/// batch cannot push the result past the configured limit.
pub(super) fn json_validity_note(ctx: &ToolContext, call: &ToolCallEvent) -> Option<String> {
    let paths: Vec<String> = if let Some(path) = call.fields.get("path") {
        vec![path.clone()]
    } else {
        call.fields
            .get("input")
            .map(|input| hashline_target_paths(input.as_str()))
            .unwrap_or_default()
    };
    let mut notes = Vec::new();
    for path_str in paths {
        if !(path_str.ends_with(".json") || path_str.ends_with(".jsonl")) {
            continue;
        }
        let path = resolve_summary_path(&ctx.cwd, &path_str);
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        match json_parse_check(&path, &content) {
            Ok(()) => notes.push(format!("JSON parse: ok ({path_str})")),
            Err(line) => notes.push(format!("JSON parse failed at line {line} ({path_str})")),
        }
    }
    if notes.is_empty() {
        return None;
    }
    let joined = notes.join("\n");
    // Keep the note within the result budget: many-file batches collapse to
    // one summary line instead of pushing the output past the limit.
    let note = if joined.len() <= ctx.tool_config.tool_result_max_bytes / 4 {
        format!("\n{joined}")
    } else {
        let ok_count = notes
            .iter()
            .filter(|n| n.starts_with("JSON parse: ok"))
            .count();
        let failed_count = notes.len() - ok_count;
        format!("\nJSON parse: {ok_count} ok, {failed_count} failed")
    };
    Some(note)
}

pub(super) fn resolve_summary_path(cwd: &Path, raw: &str) -> PathBuf {
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}
