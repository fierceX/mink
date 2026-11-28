# Goal Subsystem Invariants (`mink-goal`)

> 归属：`crates/mink-goal/`；更新日期：2026-11-28；从 `crates/mink-core/AGENTS.md` 迁出的 Goal 运行时契约，修改 Goal 状态机、审核或插件装配时必守。

## 关键不变式

### 插件装配与 Core 边界

- `GoalPlugin::install_into(&self, &mut AgentOptions)` 只注入模型工具（`GoalRead`/`GoalDraft`/`GoalReview`）、`AgentOptions` turn policy 与压缩 checkpoint provider（`ContractCheckpointProvider`）；不提前触碰磁盘或锁。
- `GoalEngine::attach(handle, plugin)` 在 `AgentRuntime::start(options)` 获取 `SessionLease` 之后运行，从 session 目录加载并恢复 `GoalStore`（`goal.json` / `goal-transaction.json`），零 `Arc` 循环引用（持有 `AgentRuntimeHandle` 与 `ExtensionEmitter`，`mink-core` 不反向持有 `GoalEngine`）。
- 与 `mink-core` 的协同仅走 `mink::runtime::ext` 与 `handle.ext()` 暴露的通用编排器原语：`AgentTool` / `ToolExecutionMask`、`ToolEffectJournal` / `ToolCallObserver` / `ReadObservation`、`ExternalController`（`on_user_interrupt()` 在 `mink-core` 释放 `admission` 锁后回调，由 `GoalDriver::transition_gate` 串行化停机与验收状态转换）、`StopCause`、`InternalTurnRequest`（统一经 `OrchActor` 执行）/ `ExtensionEmitter` / `ArtifactReader` / `run_ephemeral_child`、`CompactionCheckpointProvider` / `SessionReader::conversation_with_sequence`。禁止在 `crates/mink-core/src/` 中引入任何 Goal 业务概念、schema 或特判（由 `crates/mink-core/tests/public_api_boundary.rs::mink_core_sources_contain_no_goal_coupling` 机械守护）。

### 生命周期、恢复与审核

- `failed` 表示本 run 已结束，禁止自动调度；宿主显式 resume 可以重试同一目标，不按异常文字或历史版本分流。重试保留契约、iteration、用量、失败/阻断记录与原截止时间；过期或执行轮耗尽必须先明确调整预算，未决副作用先人工对账。completed/cancelled 禁止恢复和改预算；draft 须确认。审核阶段恢复仍复核原候选，最后一个执行轮允许续审；执行阶段恢复开启新轮，不重放已提交工具。当前 runtime 的 Faulted、持久化闩锁、关闭、执行未退出、容量和环境门槛不能被显式重试绕过。
- 已结束/非活动的 Goal 不得保留 `verifying`：取消与完成在载入/发布时归一 `idle`；paused、blocked、budget_exhausted 与 failed 的 `verifying` 保留，供 resume 重试原审核。普通用户 turn 的压缩只受当前 review 会话预算约束，不得因无关 Goal 的 review 期限失败整轮。
- 独立审核先读取已提交的执行轮最终结论；明确部分完成且有契约内下一步可轻量继续，完成声明、空白/含糊/矛盾或外部阻断必须核对证据。轻量继续只写 continuations，不修改 reviews、验收项通过状态或完成依据；连续三轮相同剩余要求升级检查，指纹不取自然语言措辞。强制检查标志先落盘，同一进程复用原只读子会话；所有阶段共用验收 deadline 与 usage。旧候选恢复仍完整审核，不重置历史或额度。
- GoalStore 是唯一权威状态；模型可 GoalRead、只修改未确认 proposal 的 GoalDraft；正常执行结束由宿主调度只读审核。审核子代理只有读取工具，自然返回描述性报告，不得调用 GoalReview 或修改 GoalStore；宿主 agent 在原会话的候选绑定内部提交轮调用 GoalReview 统一提交 decision=continue/complete/blocked、summary、未完成时的 next_step；客观阻断才提供 blocker 字符串，可选 remaining_requirements。runtime 绑定候选、报告与证据后发布；不要求模型复制候选或重写逐项表。描述性结论保留 report，不制造逐项 findings；旧完整提交仅保留解码。GoalReview 固定在主代理工具 schema 中，只有提交轮可执行；已解码的结论校验拒绝返回具体工具失败供同轮纠正，不计入格式恢复窗口。模型不能确认草案、修改已确认契约、额度或运行时状态文件。typed continuation 不得伪装人类输入；普通 turn 不得绕过活动目标与验收所有权。
- 驱动故障是稳定终态：不可恢复错误停止本 run 的调度并最多尝试一次落盘 `failed`，不得因磁盘仍为 `active` 自动重启（即使 failed 无法发布）；`wait()` 返回运行时错误，`start_driver()` / `confirm()` 拒绝新激活，仅重开会话恢复。仍有工具/检查器未退出时先停止中，待其实际退出后才成为稳定故障；暂停/取消无法持久化（含未闩锁的发布前失败）时命令只按当前 run 身份记录带原始错误的 `Stopping` 并 interrupt，禁止继续调度与提交成功，由唯一 run 结算在 turn gate 空闲后发布 `Faulted`，不得提前发布稳定故障或把随后正常返回改写成 Idle。激活在任何状态写入前确认调度执行器与 readiness，拒绝返回错误且不写 active 契约；已写出的契约若仍无法启动 run 同样返回错误并闩锁 Faulted，不静默留下无人驱动的 active。
- 默认 20 turn/60 分钟从新草案确认时启动，暂停不重置；正常结束无需报告或补全轮。独立审核 blocked 需 `blocked_after_consecutive_rounds`（默认 3）轮*同一阻断*才结束目标，描述性结论的阻断身份为 `category`/契约版本/受影响验收 ID/依赖事实版本的持久化指纹（`blocker_fingerprint`）；自然语言依赖描述、新产物 ID 及本轮结论不参与身份，旧逐项审核保持原指纹，身份变化把同一阻断计数（`blocker_rounds`）重置为 1；`blocked_rounds` 仅记录连续 blocked 轮数，非 blocked 审核清零身份与两个计数。blocked 必须有实际读取的非空事实证据，审核与轮决策同次发布，恢复同一审核不增加阻断轮数。永久/格式/信号/持久化故障停止当前 run，不自动用新 turn 清空恢复计数；宿主明确重试保留 Goal 的持久计数，单个请求的自动重试与格式恢复上限不变。
- 正常 `shutdown()` 先取消 runtime token 再中断 turn gate：`GoalEngine` 通过 `handle.ext().subscribe_stop_cause()` 区分 `StopCause::UserInterrupt`（落盘 `paused`）与 `StopCause::RuntimeShutdown`（保留 `active` 意图供下次打开恢复）（`docs/reference/goals.md`）。
- 完成依据必须区分 agent_reviewed/verified/human_approved（model_reported 只保留旧终态历史）；验收前后核对声明证据、revision 与 conversation 序号。检查器缺失不得降级；证据变更用新 generation 在同一执行轮内重审，已知结果按返回时的证据版本复用，不重复执行检查器。新审核请求核对 Token 预算；人工批准核对绝对期限；审核者实际读取的新产物经宿主 `import_artifact` 导入父会话并保持 SHA-256 与引用一致。
- 独立审核的基础设施失败、无效输出或超时不得记为目标证据失败；持久化审核失败原因与 review_failure_stage 并保留候选；报告有效时只重试宿主提交，显式 resume 只重试审核且不消耗执行轮数。
- 审核报告通过 artifact 保护并交给宿主 agent，主会话只追加带 `goal_adjudication` 身份的内部消息；宿主提交轮不消费人类输入、不接纳任务执行工具，不额外消耗 Goal 执行轮数，与只读审核共用验收期限和 usage。无有效提交时返回真实宿主终止原因，不把子代理普通文字当成结论。候选绑定执行边界，acceptance_conversation_seq 绑定宿主提交后的历史水位；完成与恢复均核对水位，新引导令完成失效。
- core SessionLease 在恢复前获取，工具/检查器实际退出前不释放；草案生成超时须中断所属 turn 并等待退出；Goal 中已派发的扩展工具被放弃等待时保留未决记录并停止同批后续派发。未知副作用恢复必须 blocked，不能合成成功或自动重放。`start_driver()` 只启用调度、不启动 run；宿主容量 run 只能经 `confirm_and_start()` / `resume_and_start()` 携带许可启动，恢复出的 active goal 只能由显式 resume 重新启动（无容量账本的生命周期宿主用 `start_driver_and_run()`）；被拒绝的 resume 不启动 run、不发出 provider 请求，停止意图的发布失败不读作已保存。Rust/Web/Python 默认不自动驱动，终端只在 UI 接入后启动 active。宿主容量随目标 run 转移：`GoalRunLease` 只在对应 run（`RunGuard`）内存活，且只在 run 结束且 `wait_turn_idle()` 返回后释放；驱动未启用、已有 run、合同/持久化拒绝时激活返回错误且立即销毁租约，不得先启动旧 active 目标或留下无人调度的 active 目标。
- Goal 的自动注入状态只有两处（按需 `GoalRead` 另返回完整快照，审核者读候选绑定快照）：每轮经 `InternalTurnRequest` 尾部追加的 `<current-goal>` round prompt（显式给出 `Round: N/M`、剩余轮次、绝对截止与 blocked 门槛）与压缩后经 `CompactionCheckpointProvider` 挂载的低频稳定契约 checkpoint（不含任何每轮字段）；禁止把每轮变化的 Goal 状态挂进请求前缀。
- 扩展生命周期通知通过 `ExtensionEmitter::emit_extension_event` / `emit_live_extension_event` 发布 `goal_updated` / `goal_finished`；界面不得绕过 `GoalEngine` / `GoalStore` 修改契约、额度或状态。
