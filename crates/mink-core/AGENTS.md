# Core Runtime Invariants (`mink-core`)

> 归属：`crates/mink-core/`；从根目录 `AGENTS.md` 拆分的 `mink-core` 运行时契约，改完 core 相关路径时必守。

## 关键不变式

### 压缩与持久化

- `TurnCompactor`：同一用户输入的内循环可重复压缩，不设次数上限；不得用隐藏常量给强制循环或 overflow 收缩封顶（终止条件是「每次必须严格下降/严格缩小」）。auto 按压力阈值触发，preflight/overflow 在请求超预算时强制折叠直到装得下、压无可压（`no safe boundary` / `savings too small`）或估算不再下降，随后仍按 fail-closed 拒绝发送超预算请求；发布前必须过净收益门控（auto 要求 ≥10% 净下降，强制路径要求严格下降），不满足则不提交。请求已超预算时，严格切点不可用（或严格候选装不进预算）会退化为 `_cut=degraded`（见下一条）。摘要侧失败在来源处定型为 `SummaryUnavailable`（可转应急）或 `CompactionInterrupted`（映射为 `TurnDecision::Interrupted`）：auto 失败且原请求仍可发送时不得阻断该请求，强制循环结束后仍超预算则转应急 checkpoint。
- `ImmutablePrefix`：system prompt/tools 变更必须 invalidate 并重建；每次构建/重建向 events.jsonl 写一条 `prefix_snapshot`（fingerprint/dependency_fingerprint/system_prompt/tools_json），缓存命中不得重复写。
- `conversation.jsonl` 完整保留且只追加；压缩只推进 `context-state.json` 的活跃投影边界；`ConversationStore` 缓存只保留活跃后缀并随 append 增量更新，模型请求只能经 `active_messages()` 读取活跃投影。
- `context-state.json` 必须同目录临时文件 + rename 原子替换，成功后更新内存并按新 `active_start` 裁剪缓存；JSONL 续写前修复未换行尾记录、以含换行的单缓冲区追加、append 经内部写锁串行化，读盘只容忍文件末尾半截记录。
- 状态发布区分**发布前失败**（旧文件不变）与“已发布但目录同步失败”：后者必须重读并比对**完整预期快照**、重做目录同步；无法恢复时闩锁 session（拒绝后续状态变更、以 fatal 结束当前 turn），禁止按旧 revision 重试。闩锁后 turn/压缩入口、同批工具派发与 `publish_state*` 一律拒绝（低层 `atomic_replace` 仅供显式恢复路径）；启动期写入与 telemetry/外观元数据（stats、session title）不在此列。
- 投影边界必须位于完整历史内、不得拆开 tool call/result 协议；cut point 优先保留最近 ≥2 条真实 user 消息（优先于纯 token 预算）。当请求已经超出输入预算、而严格规则找不到任何边界（热尾部目标吞掉整个窗口，或窗口只剩最近两个用户轮次），或严格候选（保留两条 user）装不进预算时，允许退化为「折到最新安全边界之前」的最后手段（`_cut=degraded`，当前请求仅以摘要文本保留）；仍然无边界时才 fail-closed 结束该轮，调用方应以下一条输入续跑。
- Goal 的自动注入状态只有两处（按需 GoalRead 另返回完整快照，审核者读候选绑定快照）：每轮追加的 `<current-goal>` round prompt（逐行可读、显式给出 `Round: N/M`、剩余轮次、绝对截止与 blocked 门槛）与压缩后挂载的低频稳定契约 checkpoint（不含任何每轮字段）；禁止把每轮变化的 Goal 状态挂进请求前缀（命令式 round prompt 文本、轮次计数与 `Decision` 只走尾部追加）。
- plan/todo 的**模型可见派生展示**必须按额度有损（`输入预算/8`，头尾保留 + 省略标记）：权威 `plan.md`/`todos.json`、revision 与计数不得被改写；候选验收与真实发送共用同一渲染函数（`checkpoint_prefix` / `sync_message_bounded`），禁止「预演短版、发送长版」。
- 应急 checkpoint（`trigger=emergency`、`_mode=emergency`）：仅在上一条与 LLM 摘要都不能让请求装下时提交；提交前必须①检查 runtime cancel 与 interrupt（入锁后、每次收缩前、提交前各一次，映射为 `Interrupted`）②验证活跃窗口没有未完成的工具交换（否则以 `incomplete tool exchange` 原样失败）③收缩每步严格变小并在到达地板后返回「无法继续」（不得原样返回同一文本）。候选验收（普通摘要与应急共用）必须把「候选折走 revision 后随之上场的 TodoSync」计入预算，材料优先级为固定有损标记 → 当前用户请求摘录 → 上一份摘要片段 → 最近已完成工具交换的结构化事实；`active_start` 推进到历史末尾（尾部为空），`conversation.jsonl` 只追加不重写；候选投影（摘录 + plan 派生片段 + 预演 TodoSync）必须过一遍 `estimate_openai_context_tokens` 才算提交，连最小摘录都装不下时返回 `Ok(false)`，由上层以「最小工作空间不可用」诊断 fail-closed。权威 plan/todo/conversation 不被改写。
- 正常路径的压缩统一调用 LLM 摘要，以唯一 internal user `<compacted-summary>` checkpoint 投影，不修改 immutable system/tools prefix；`context_compact_input_reduction=true` 只精简摘要请求，不改写完整历史或热尾部。当请求已经超出输入预算、而摘要侧不可用（输入装不下、调用/超时/重试耗尽、输出不合格、候选发布前验收不通过）时，允许**不调用 LLM 的确定性应急 checkpoint** 提交同一投影（有损摘录，见下一条）；取消、持久化 fault、正式历史协议损坏不得转为应急成功。
- auto 压力优先用同模型、同 system/tools 指纹、同 projection generation 的最近 provider prompt usage 校准（基线须为当前投影严格前缀）；preflight 始终保守本地估算，基线只存 runtime 内存；`prompt_usage_calibration_safe=false` 的后端禁用校准；支持 cache projection 的后端必须让摘要复用主请求的实际 system/tools 与历史公共缓存前缀，无法证明边界或超预算时按 reduction 配置降级。
- 压缩与子代理请求必须使用当前活动真实模型名与别名，并复用 runtime 共享 `LlmBackend`；provider context overflow 只在无可见输出时触发本 round 的收缩恢复：收缩次数不设硬上限（每次必须严格缩小，否则报错或转应急），且 overflow 收缩、其摘要请求与主请求共享同一 round 绝对期限（`request_timeout_secs`），overflow 后重进 stream 不得重置总期限。
- `max_context_tokens=0` 禁用 auto/preflight 与本地输入预算上限、保留手动压缩；压缩百分比、响应预留、热尾部、摘要输出预算来自显式配置（不推断隐式档位）；有限窗口下只有 `reserve < 窗口` 是硬约束（决定响应预留），摘要输出上限与热尾部是**软目标**：切点按可用空间收紧热尾部，摘要输出 cap 按「输入 + 输出 ≤ 窗口」动态下调（低于最小实用输出转应急），因此不做 S/T 组合硬校验。
- Agent JSONL `SdkOptions`、Python `SandboxConfig`、Rust `AgentOptions` 覆盖同一组压缩参数并映射到唯一 `Config`；runtime 创建 session 前调用 `validate_runtime_config()`。
- 每个**最终请求**的请求投影只构建一次（`RequestProjection`：单次图片消费投影、OpenAI 转换、token 估算与 fingerprint）；压缩提交或活跃历史变化后必须重建（无压缩轮 1 次，压缩轮 2 次以上）。auto/preflight/overflow、压缩候选验收与发送共享该投影，禁止在同一最终请求内重复全量转换或重复估算。发送 body 对无图片轮复用同一转换结果；有图片轮只 materialize 并二次转换一次。cache projection 的消息哈希必须从 backend 返回的 **provider 可见前缀** 计算，不得取完整请求。`strip_orphaned_tool_calls` 以 HashSet 线性扫描。

### LLM 有界恢复

- 唯一恢复配置是 `Config.llm_recovery`（`format_window_size` 1..=1024、`format_max_errors` 0<=K<W、`request_max_retries` 0..=16、可选 `request_timeout_secs`>0）；Rust `AgentOptions::with_llm_recovery`、JSONL `options.recovery`、CLI/server `[recovery]`、Python 四字段全部映射到它并在 session 创建前校验，不得新增第二套默认值或策略层。（provider HTTP 总超时 `http_timeout_secs` 是传输层配置，不属于本组恢复策略。）
- 格式窗口是 turn 内的布尔滑动窗口（新用户 turn 清空、压缩不清空、子代理独立、不落盘）：每个可继续的 round 在唯一尾部先结算窗口一次（正常 false；需反馈纠错或曾有响应协议损坏为 true），再执行分支决策（todo 提醒/证据注入），最后刷新已提交历史——刷新必须在本轮所有追加之后，保证诊断、工具结果与注入状态都进入下一请求；预判（`would_exceed`）不得改变窗口；错误数 > K 以稳定前缀 `format_recovery_exhausted` 结束 turn 且不再发请求。
- 请求恢复由 runtime 的 attempt 循环所有：一个 round 的请求投影（含图片物化）只构建一次；内置 backend 每次 `stream()` 对应一次物理请求，外层恰为 retries+1 次调用；退避 1s/2s/4s…上限 10s，`Retry-After` 仍为最早重试时间且不被退避上限截短；取消、持久化闩锁与可选总期限在每次 attempt 前与退避中生效（取消优先于等待中的重试），总期限同时约束建流与流消费（到点停止等待/消费，已收到的 usage 照常结算），耗尽用 `request_retry_exhausted` / `request_timeout` 终止并保留诊断。
- 每个 attempt 独立结算 usage（成功 reported、失败或取消 unreported，恰一笔）；只有最终接受且指纹/投影匹配的主请求可更新 provider prompt usage 校准；自定义 backend 的聚合 `attempt_count` 原样保留，外层不乘算。
- 压缩摘要请求共用同一重试状态、错误分类与 attempt/deadline 语义；完整但不可用的摘要（空输出、工具调用、坏 stop 原因）在同一次压缩操作内追加用途纠正到临时请求尾部重试，不写父 conversation、不改公共缓存前缀、不推进主 agent 格式窗口。
- 上游错误必须类型化：`LlmUpstreamError`（Recoverable / ProtocolDamaged / Permanent，带可选 status/code/source 与受限诊断）是唯一分类依据；永久拒绝与未知 provider 错误默认失败且不重试；未类型化的自定义错误保持既有失败行为。

### Goal

- `failed` 表示本 run 已结束，禁止自动调度；宿主显式 resume 可以重试同一目标，不按异常文字或历史版本分流。重试保留契约、iteration、用量、失败/阻断记录与原截止时间；过期或执行轮耗尽必须先明确调整预算，未决副作用先人工对账。completed/cancelled 禁止恢复和改预算；draft 须确认。审核阶段恢复仍复核原候选，最后一个执行轮允许续审；执行阶段恢复开启新轮，不重放已提交工具。当前 runtime 的 Faulted、持久化闩锁、关闭、执行未退出、容量和环境门槛不能被显式重试绕过。
- 已结束/非活动的 Goal 不得保留 `verifying`：取消与完成在载入/发布时归一 `idle`；paused、blocked、budget_exhausted 与 failed 的 `verifying` 保留，供 resume 重试原审核。普通用户 turn 的压缩只受当前 review 会话预算约束，不得因无关 Goal 的 review 期限失败整轮。
- 独立审核先读取已提交的执行轮最终结论；明确部分完成且有契约内下一步可轻量继续，完成声明、空白/含糊/矛盾或外部阻断必须核对证据。轻量继续只写 continuations，不修改 reviews、验收项通过状态或完成依据；连续三轮相同剩余要求升级检查，指纹不取自然语言措辞。强制检查标志先落盘，同一进程复用原只读子会话；所有阶段共用验收 deadline 与 usage。旧候选恢复仍完整审核，不重置历史或额度。
- GoalStore 是唯一权威状态；模型可 GoalRead、只修改未确认 proposal 的 GoalDraft；正常执行结束由宿主调度只读审核。审核子代理只有读取工具，自然返回描述性报告，不得调用 GoalReview 或修改 GoalStore；宿主 agent 在原会话的候选绑定内部提交轮调用 GoalReview 统一提交 decision=continue/complete/blocked、summary、未完成时的 next_step；客观阻断才提供 blocker 字符串，可选 remaining_requirements。runtime 绑定候选、报告与证据后发布；不要求模型复制候选或重写逐项表。描述性结论保留 report，不制造逐项 findings；旧完整提交仅保留解码。GoalReview 固定在主代理工具 schema 中，只有提交轮可执行；已解码的结论校验拒绝返回具体工具失败供同轮纠正，不计入格式恢复窗口。模型不能确认草案、修改已确认契约、额度或运行时状态文件。typed continuation 不得伪装人类输入；普通 turn 不得绕过活动目标与验收所有权。
- 驱动故障是稳定终态：不可恢复错误停止本 run 的调度并最多尝试一次落盘 `failed`，不得因磁盘仍为 `active` 自动重启（即使 failed 无法发布）；`goals().wait()` 返回运行时错误，`goals().start_driver()` / `goals().confirm()` 拒绝新激活，仅重开会话恢复。仍有工具/检查器未退出时先停止中，待其实际退出后才成为稳定故障；暂停/取消无法持久化（含未闩锁的发布前失败）时命令只按当前 run 身份记录带原始错误的 `Stopping` 并 interrupt，禁止继续调度与提交成功，由唯一 run 结算在 turn gate 空闲后发布 `Faulted`，不得提前发布稳定故障或把随后正常返回改写成 Idle。激活在任何状态写入前确认调度执行器与 readiness，拒绝返回错误且不写 active 契约；已写出的契约若仍无法启动 run 同样返回错误并闩锁 Faulted，不静默留下无人驱动的 active。
- 默认 20 turn/60 分钟从新草案确认时启动，暂停不重置；正常结束无需报告或补全轮。独立审核 blocked 需 `blocked_after_consecutive_rounds`（默认 3）轮*同一阻断*才结束目标，描述性结论的阻断身份为 `category`/契约版本/受影响验收 ID/依赖事实版本的持久化指纹（`blocker_fingerprint`）；自然语言依赖描述、新产物 ID 及本轮结论不参与身份，旧逐项审核保持原指纹，身份变化把同一阻断计数（`blocker_rounds`）重置为 1；`blocked_rounds` 仅记录连续 blocked 轮数，非 blocked 审核清零身份与两个计数。blocked 必须有实际读取的非空事实证据，审核与轮决策同次发布，恢复同一审核不增加阻断轮数。永久/格式/信号/持久化故障停止当前 run，不自动用新 turn 清空恢复计数；宿主明确重试保留 Goal 的持久计数，单个请求的自动重试与格式恢复上限不变。
- 正常 `shutdown()` 先取消 runtime token 再中断 turn gate：在飞的 goal round 只有在「gate 被中断且 runtime 未被取消」时才会结算为用户暂停，而正常关闭必须保留 active 意图供下一次打开恢复（`docs/reference/goals.md`）。
- 完成依据必须区分 agent_reviewed/verified/human_approved（model_reported 只保留旧终态历史）；验收前后核对声明证据、revision 与 conversation 序号。检查器缺失不得降级；证据变更用新 generation 在同一执行轮内重审，已知结果按返回时的证据版本复用，不重复执行检查器。新审核请求核对 Token 预算；人工批准核对绝对期限；审核者实际读取的新产物经宿主 ArtifactManager 导入父会话并保持 SHA-256 与引用一致。
- 独立审核的基础设施失败、无效输出或超时不得记为目标证据失败；持久化审核失败原因与 review_failure_stage 并保留候选；报告有效时只重试宿主提交，显式 resume 只重试审核且不消耗执行轮数。
- 审核报告通过 artifact 保护并交给宿主 agent，主会话只追加带 goal_adjudication 身份的内部消息；宿主提交轮不消费人类输入、不接纳任务执行工具，不额外消耗 Goal 执行轮数，与只读审核共用验收期限和 usage。无有效提交时返回真实宿主终止原因，不把子代理普通文字当成结论。候选绑定执行边界，acceptance_conversation_seq 绑定宿主提交后的历史水位；完成与恢复均核对水位，新引导令完成失效。
- core SessionLease 在恢复前获取，工具/检查器实际退出前不释放；草案生成超时须中断所属 turn 并等待退出；Goal 中已派发的扩展工具被放弃等待时保留未决记录并停止同批后续派发。未知副作用恢复必须 blocked，不能合成成功或自动重放。`goals().start_driver()` 只启用调度、不启动 run；宿主容量 run 只能经 `goals().confirm_and_start()` / `goals().resume_and_start()` 携带许可启动，恢复出的 active goal 只能由显式 resume 重新启动（无容量账本的生命周期宿主用 `goals().start_driver_and_run()`）；被拒绝的 resume 不启动 run、不发出 provider 请求，停止意图的发布失败不读作已保存。Rust/Web/Python 默认不自动驱动，终端只在 UI 接入后启动 active。宿主容量随目标 run 转移：`GoalRunLease` 只在对应 run（RunGuard）内存活，且只在 run 结束且 turn gate 空闲后释放；驱动未启用、已有 run、合同/持久化拒绝时激活返回错误且立即销毁租约，不得先启动旧 active 目标或留下无人调度的 active 目标。
- Goal 控制与只读面隔离：Rust 宿主通过 `handle.goals()` 访问生命周期控制，模型通过 `GoalRead` 读取契约，界面不得绕过 core 修改契约、额度或状态。

### Plan 与 Todo

- PlanDraft/PlanConfirm/PlanClear 必须通过类型化 `PlanCommand` 与 `PlanStore` 完成；文件错误必须返回模型、禁止空成功；已确认计划存在时禁止创建新草稿；PlanClear 同时清理陈旧草稿。
- PlanConfirm/PlanClear 成功工具结果写入后追加 confirmed/cleared 内部 user transition，不触发强制压缩；未压缩历史依赖 PlanDraft + transition，压缩后若 `plan.md` 存在则在摘要后投影唯一 `<active-plan-checkpoint>`，PlanClear 后移除。
- Plan 文件变更与 conversation 追加由 `plan-transaction.json` 可重放 journal 协调：未绑定操作回滚、已绑定操作幂等补齐（任一时刻不得与 `plan.md`/对话历史分叉）；PlanStore 由 `AgentSharedContext` 持有 session 唯一实例，`ToolContext` 克隆同一 Arc，Plan 交接在工具阶段原地完成。
- todo 权威完整快照在 `todos.json`；TodoWrite/TodoAdvance 成功后追加增量事件与 `<current-todos>` 物化投影，不做逐请求前置投影；两者依赖 TodoRead，使用最高可见 revision 与稳定 ID，stale 失败后重读。
- TodoWrite 只新增 pending/删除/替换正文，TodoAdvance 只做合法转换，均原子提交；session 恢复或压缩后若文件 revision 领先活跃历史只追加一次 TodoSync，历史领先文件时 fail closed。
- 同一 active batch 可有多个 in_progress，结束前提醒最多注入一次；一个 session 的 TodoStore 由单个 runtime 持有，不支持跨进程并发写或外部热编辑。

### 信号系统

- `BeliefTracker` 初始 0.75，每输入按 `decay_per_input`（默认 0.6）衰减替代硬重置；`DecisionEngine` 冷却与 `StormBreaker` 每输入 reset。
- 软信号（ToolError/EditLoop/ArgumentError）单次且信念 ≥ warn 不干预；累计 ≥2 次软失败，或出现硬信号（ToolFailed/SafetyBlocked/CompileError/TestFailure）与结构化错误码（Timeout/ProcessFailed/SafetyBlocked/Aborted）才参与决策。
- 注入的是轨迹事实帧（`[trajectory]`/`[detector]`），禁止祈使句与“进入恢复模式”命令；去重哈希只覆盖证据事实文本，同一证据批不重复注入；响应事件携带证据文本（可回溯 conversation.jsonl）。
- 警告级状态操作保留共享工作区；为窗口内编辑路径注入 `[edit-recovery]`，提示重读、对照已提交的 Edit 差异与可用的读取基线。外部编辑器不共享运行时锁，因此不自动整文件恢复。
- 批内基础设施错误须按调用 ID 配对结果。已派发但无确定结果的调用以 `_mink.execution_outcome="unknown"` 持久化；已登记副作用在运行和重开时均保持待对账。
- 正文兼容协议由 backend 显式开启；正文仅接受独立 XML、bracket 或 DSML 包装序列，reasoning 仅接受独立 DSML 序列。裸 JSON 和解释示例保留为文本。
- 恢复守卫拦截必须生成真实信号喂回信念；连续拦截达 `guard_max_blocks` 必须绕过守卫并强制注入，禁止无限拦截；信念跌破 abort 时，非交互且策略允许 restart 则先尝试 replan，成功重置信念并继续；未成功时 full 策略记录 `signal_handover` 后 Failed，较低策略直接 Failed。禁止静默丢弃证据；策略重启子代理初始化失败必须降级（`signal_replan_error` 后返回 None），不得升级为整轮 Err。
- Recovery 首步资格来自 resolved semantic capabilities；Bash 的 `FocusedVerificationExec` classifier 与普通 Bash 安全/误用提示相互独立。

### 嵌入事件与失败契约

- 公开 `mink::runtime::SessionEvent`（`EventLog` 别名）是 `events.jsonl` 的唯一稳定词汇。reader 只消费完整行：未知类型/坏 JSON 计入 `skipped`，末尾半截行不消费，偏移超出文件返回 `truncated` 且 `next_offset=0`；新增字段必须可选，变体只增不改（`#[non_exhaustive]`）。宿主不得另建字符串字段解析层。
- `SessionEventSink` 同步运行在调用线程：必须非阻塞、不得回调 runtime、不得 panic；`wants` 用于过滤 Text/Thinking/PrefixSnapshot 等高体积/敏感事件。`log_events=false` 仍投递，子代理继承父 sink 并携带自身 session id。压缩引擎的 `compact`/`compaction_check`/摘要 usage 必须走同一 sink 出口，不得直接写 writer。
- `TurnOutcome.failure` / `TurnFinal.failure` / `TurnError.failure` / `SdkFinal.failure` 是稳定 snake_case code，只能来自 typed tag/downcast，禁止用 error 文本决定宿主重试或告警。成功轮省略该字段；`Interrupted` 为 `None`；`MaxTurnsExceeded` 同时写 status 与 failure；外部直接构造 `TurnOutcome` 时需补 `failure: None`。Goal 结算必须先于唯一 `TurnFinal` 发布；结算失败改写结果后不得保留先前的成功终态。

### 工具面与执行

- approval 在构建 `ModelToolSurface` 时解析；`ToolRunner::execute_all()` 在 StormBreaker 前校验调用属于同一 resolved surface，真实执行只接受 surface 内工具（disable flag 与沙箱策略不属于运行时合同）；只并发连续只读工具，写入/执行/控制/SubAgent 工具按调用顺序串行执行。
- macOS sandbox-exec 写入白名单须覆盖声明目录、临时目录和会话存储的字面路径与可解析真实路径，避免符号链接别名使已允许写入失败；不据此扩大读取边界。
- `enabled_tools` 是唯一工具启用输入：`None` 用 catalog 默认集、空列表禁用全部、显式列表精确选择；`PythonSandbox` explicit-only，仅显式列出时进入 surface。
- 子代理终态由内部 `SubAgentStatus` 表达（Interrupted/TimedOut 不得映射为成功）；未完成子代理只在 `SubAgentBatch.pending`，每个任务只有一个结果发送点，`Drop` 必须 cancel+abort；fork 在 runtime 初始化前以目录级克隆继承父 session 状态，子代理复用父 `LlmBackend` 与当前活动模型。
- 子代理身份：父 `events.jsonl` 的 `sub_agent` 事件携带可选 version/label/mode/parent_session_id/depth/stop_reason/duration_ms/phase，目录折叠只是派生视图（events 仍为 best-effort，conversation 仍是权威）；子 `session.json` 写 `parent` 与 `subagent{version,origin,mode,label,depth}`，origin 区分 `sub_agent`/`goal_review`/`replan`。
- 子代理结果：模型可见文本只取最后一条含非空 text 的 assistant 的全部 text 块（空为 `(no output)`，无匹配时回退 captured），默认不注入 thinking；失败/超时由有界 diagnostic（含截断标记总长 <=4096 字节、含 phase 与稳定错误码）承载；`_mink.sub_agent` 携带结构化身份/用量/耗时，未测量时带 `usage_known/elapsed_known=false`；父 display 的 SubAgentOutput 展示侧有界，tool_result 大小与 artifact 保护不变。
- 子代理生命周期：`sub_agent_timeout_secs`（必须 >0）从获得并发名额后起算并覆盖 setup+run；不存在批次 deadline；批内上限 20 只排队不拒绝，状态按 `launched(queued)→running→终态` 记录；fork 克隆按目录项检查取消；setup 失败/超时/取消由清理 guard 删除部分 child home；`elapsed_ms`/`duration_ms` 不含排队。
- 子代理委派边界：`sub_agent_max_depth` 默认 1（0 禁用、最大 3），深度持久化在子会话元数据；`depth < max_depth` 时 surface 才保留 SubAgent，运行时以 `child_depth = depth + 1 > max_depth` 返回 SafetyBlocked；`model` 仅接受 `ModelResolver` 别名或当前实际模型。
- 子代理提示词：子角色渲染固定委派 scope 声明，到达深度上限时显式给出剩余 0；主代理不渲染该段。
- `TurnExecutor::new/new_for_model` 是模型与 backend 的唯一装配路径；禁止第二 backend 字段或构造后覆盖模型；`SubAgentCoordinator` 不持有 Config；默认 approval mode 为 `yolo`，`prompt` 无交互式 UI 时 fail closed。
- `format_tool_result()` 是工具输出进入 LLM/UI 前的统一最大字节保护，超长写 `artifact://<id>`；写入 conversation 用 `conv_content`，为空回退 `content`；`Bash`/`Python` 必须在 `ToolContext.cwd` 下执行，Bash 未显式设 timeout 时用稳定全局 tool timeout。
- `StormBreaker` 每新输入重置；同轮所有调用（含 mutating）统一计数；抑制在同类相同调用于窗口中计数 `count > threshold`（默认 threshold 3、窗口 6）时触发，不同调用不误伤。
- 工具参数解码失败（内置 helper / `ToolError::argument`）标记 `ToolFailureSource::ModelFormat`：产生 `Failed(ArgumentInvalid)` 失败结果并单独计数，不进入 belief/evidence、不触发回滚/重启/Abort；已知格式调用不消耗恢复守卫，被 StormBreaker 抑制时仍保留模型格式来源。所有模型可见内置工具的模型参数解码入口必须走共享 helper（含 Write/Glob/Grep/SubAgent），参数化测试覆盖全入口。
- 正文中已识别的工具调用（scavenge）若参数不可解析，必须作为降级候选进入同一纠错分支：wrapper 标签本身即候选标记（先识别标签再解析内部；闭合/未闭合 × JSON 合法/非法 × 名字正常/缺失都不得落回普通文本成功）；DSML invoke 标记同理（头部/块/参数截断均降级；参数头必须完整匹配含 `>` 的分隔符，禁止固定偏移跳过未验证字符，切片安全且禁止 panic）；声明为 JSON 的参数解码失败不得回退为字符串，残缺参数不得当作完整调用执行；OpenAI 形态 `function.arguments` 字段缺失与类型错误必须区分（后者保留原始 payload 与真实错误，不得回退为 `{}`）；普通 JSON 示例仍不算工具调用。
- SSE 流式 `arguments` 类型错误是**粘性**的：非字符串值不得忽略，后续片段（含空片段与合法片段）不得“在流内纠正”，一律产出降级候选交由下一 round 重发，不做流内纠正状态机。
- 首事件/idle 期限只由真实进度推进：Retry 控制通知不更新有效进度时间（主请求与压缩摘要 attempt 同时生效）；失败的 attempt 在退避/重试前先取消自己的子 token。
- 流消费只构造候选：正式 assistant/tool call 在响应接受后才持久化，ToolCall 展示与 `tool_call` 事件也延迟到完整响应判定之后（废弃候选不得以“已执行调用”出现）。
- 缺工具名/缺调用 ID/重复 ID 的候选工具批整批丢弃并反馈一次有界内部诊断（不先写非法 tool call、不编造结果、不执行任何子调用）；length/max_tokens 候选整体废弃（文本与调用均不执行）；空正文 stop、thinking-only、`tool_calls` 无调用或空/未知 reason 必须计格式错误并反馈，不得静默空成功；明确拒绝/过滤是终态失败，不作为格式错误反复重发。
- Plan 与 SubAgent 结果必须在延迟工作完成并经过统一大小保护后再进入信号采集；`ArtifactManager` 从已有 index 最大序号继续，正文独占创建，禁止覆盖恢复或 fork 继承的 artifact。
- `AgentEventStream` 是 unbounded 可靠通道：可靠事件（工具调用/结果、stop/error/usage、控制事件）不丢；`Text`/`Thinking` 进度受 1 MiB pending 字节预算，超限丢弃并只通知一次；`outcome()` 必须主动排空并释放进度字节；丢弃未完成的 stream 等同取消 turn。
- `EventLog` 关键事件（`prefix_snapshot`、signal rollback/replan/handover）必须经 `log_critical_event` 异步、有期限地等待 writer 写入应答（入队成功不算成功），同时响应 runtime cancel 与当前轮 `interrupt_current_turn()`（健康 writer 有宽限期，仅停摆等待被提前结束）并传播失败；仍需产出 stream-json stdout；prefix 快照失败时不得更新前缀缓存；诊断事件可用 `send_best_effort` 但丢失必须计入报告（SSE 有界仅覆盖事件个数 1024）。
- EventLog writer 独占文件/当前故障/损失计数（`WriterState`，无锁）；已报告损失水位仅在收到 FlushAck 后推进，取消 flush 不消费损失；生产不保留信号测试镜像与 stdout 计数，事实来源是 `result.signals`、events.jsonl 事件与真实 stdout。

### Read / Edit 协议

- `Read` 的可行为参数只含 `path`（行范围用 `path:N`/`N-M`/`N+K`/`:raw` 选择器），`limit`/`offset`/`range`/`path_range`/`path_selector`/`selector` 为兼容字段；`Grep` 的 `head_limit`/`output_mode`/`-i`、`Python`/`PythonSandbox` 的 `command`/`code` 同属兼容字段（接受即忽略，未知字段仍 fail closed）；全工具 schema 字段必须与 serde 接受字段一致且 `additionalProperties:false`（catalog 一致性测试强制）。
- 行选择器 `N+K` 必须饱和；offset 超总行数报错而非回读幻影行号；`split_content_lines("")` 返回空集（空文件 0 行），Read/Write/Edit 与 hashline 解析共用该语义。
- `Read` 本地非 raw 输出记录 snapshot；raw 或 immutable resource 不生成可编辑 snapshot；Read memo 命中需 len/mtime/epoch/mutation_epoch 一致且范围覆盖；压缩提交成功后 bump epoch，Write/Edit 成功后 bump mutation；子代理 memo 独立，仅本地文件。
- Hashline stale 恢复仅当所有锚点唯一映射且共享一致偏移；Replace 多候选必须拒绝；Edit no-change 幂等成功仅当位置精确且最终状态可验证一致（`hashline::already_applied`），任何歧义退回 soft no-op → 3 次硬错误的 fail-closed 路径。
- registered resource URL 先于 VFS 处理，未知 URL-like scheme fail closed；Grep 可搜索 registered resource 文本，resource path 不接受 selector/glob（返回行号供后续 Read selector）。
- 嵌入式 runtime 注入同步只读 VFS 仅替换 Read/Glob/Grep 后端，未注入时严格保持本地执行路径；VFS 调用同时携带继承的 `resource_session_id` 与当前 `agent_session_id`；虚拟 Read 不生成 snapshot，Edit/Write 始终本地。
- prompt 资产纪律：每个 `<critical>` 3-6 条战术 bullet、每条 ≤12 英文词且单一主张；示例置尾；禁 token/budget 措辞；不写引擎内部机制（`tests/prompt_discipline.rs` 机械执行）。
- `tool-inventory` 内容必须与当前 `ModelToolSurface` 名称集一致，空 surface 才用 `runtime-capabilities`；MISSION 只能覆盖 allowlisted core，runtime-owned section fail fast；普通自定义规则不得使用 reserved 的 `# rules`。
- skill index、selected skills、`skill://`/`rule://` 必须来自同一 `CapabilitySnapshot`；selected skill 正文不依赖资源读取 provider。frontmatter 按 Agent Skills 最小子集解析（`name` 合法时优先于目录名、`description`、`when_to_use`、`hide`/`disable-model-invocation`，支持 `>`/`|` 块标量；未闭合 header 视为不存在）；skill index 与 `skill://list` 为有损渲染（描述去控制字符、折叠单行、转义尖括号、1,536 字符截断；总量超预算按来源降级删除描述，技能名始终保留），`skill://list/all` 与 CLI/TUI 保留完整描述。

## 新增工具步骤

1. 在 `crates/mink-core/src/tools/*.rs` 实现 `ToolExec` 或辅助函数。
2. 在 `tools/runner.rs` 的 `TOOL_REGISTRY` 注册。
3. 在 `crates/mink-core/src/assets/tools.json` 添加 schema（字段与 serde 一致）。
4. 在 `metadata()` 声明 approval tier、结果类型、副作用（mutating）、spawns_sub_agent、storm_exempt。
5. 需要压缩给 LLM 的内容时设置 `ToolOutcome.conversation_content`。
6. 补充单元测试：schema/registry 一致性、approval、错误路径、截断、artifact、信号与安全边界；协议变更同步 `docs/reference/tools.md`。
