# Agents Guide

> 更新日期：2026-09-26

## 项目概览

Mink 是用 Rust 实现的轻量 AI coding agent，面向 DeepSeek/OpenAI-compatible API：单二进制、终端优先，也可作为库嵌入（`mink::runtime`）。核心能力：

- LLM 流式请求 → 工具执行 → 决策的内循环；REPL / TUI 两种终端模式，`AgentRuntime::start() → run_turn()/stream_turn() → shutdown()` 无需子进程
- 信号驱动的信念系统：轨迹证据注入（`[trajectory]`/`[detector]`）、编辑循环快照回滚、恢复首步守卫；`SignalPolicy` 分档，阈值/超参为内部常量，`MINK_SIGNAL_POLICY=off` 关闭
- 上下文自适应压缩：显式阈值/响应预留/热尾部/摘要预算，摘要以 `<compacted-summary>` checkpoint 投影；Todo（`todos.json` 权威快照 + revision/稳定 ID）、Artifact（超长输出落盘）、轻量资源读取（artifact/skill/rule/session URL）
- 工具元数据与审批策略；Edit 双模式（Hashline snapshot / Replace exact+fuzzy，启动后固定）；子代理（isolated home，可目录级 fork）；TUI `Ctrl+V` 粘贴图片（session `attachments/` 传输副本 + 绝对路径，图片仍由 `Read` 捕获）

---

## Agent 工作规范

本文件每个任务都会注入；以下是本仓库对 agent 的行为约束，与「关键不变式」互补，违反任一条都需修正。

### 改动纪律

- **先搜后改**：先 `Grep`/`Glob` 定位、再 `Read` 精确目标范围；禁止凭印象猜路径、行号、字段名或 API。
- **因果自检**：每处改动前答出 cause（改什么行为）/ effect（预期观察）/ verify（如何验证）；答不出就不改。
- **独立小步**：因果独立的改动分开验证与提交，不捆绑无关修改。
- **不动不知道的**：先读调用链与既有测试再决定。
- **走公开协议**：计划用 `PlanDraft`/`PlanConfirm`/`PlanClear`，Todo 用 `TodoRead`/`TodoWrite`/`TodoAdvance`；编辑文件先读后写；**禁止直接写 `plan.md`/`todos.json`/`context-state.json` 等运行时状态文件**。
- **不伪造成功**：失败如实报告；不得绕过 `format_tool_result`/artifact 输出保护或任何 fail-closed 边界。

### 验证与失败纪律

- **改完必验**：跑相关测试并读完整输出（`cargo test -p mink-core` 起手；涉及 CLI 加 `-p mink-cli`；构建矩阵改动跑 `make feature-matrix`）。
- **提交前干净**：`cargo fmt --all -- --check` 与 `cargo clippy --workspace --all-targets --all-features -- -D warnings` 必须无输出（CI 同款命令）。
- **报告证据**：声称“完成/修复/通过”前必须有命令输出佐证；无法验证就说明无法验证。
- **回归意识**：核心路径（压缩/持久化/编辑/信号）注意 `crates/mink-core/tests/invariants.rs` 与 regression 套件；删除或弱化被测试钉住的行为前先确认其已废弃。
- **停止-重析**：同一命令连续失败或输出异常时停下，重读代码与错误，换一种方法；禁止无修改重复相同命令。
- **假定不可信**：编译通过≠行为正确，测试全绿≠没有回归；用新证据（针对性测试/日志）确认因果，与先前结论矛盾时按新证据行事并明说。

### 架构与文档纪律

- **契约必守**：「关键不变式」不可逾越；确需变更协议时先更新本文件与 `docs/` 对应章节，并保持全仓一致。
- **同步文档**：架构/行为变更同步 `docs/ARCHITECTURE.md`、`docs/DESIGN.md`、`docs/USAGE.md`、`docs/tools.md` 及本文件（含更新日期）；用户可见变更记入 `CHANGELOG.md` Unreleased。
- **测试锚定**：新增行为必有测试；不变式守护优先落在 `tests/invariants.rs` 或模块级回归测试。

### 提交纪律

- **卫生**：不提交敏感数据（模型轨迹、密钥、用户目录、内网地址）与实验性未跟踪产物；拿不准先问。
- **message**：一个主题一个提交，完整概括 what + why + 影响面；「版本号 + 文档同步」这类准备性改动可单独成提交。

---

## 运行时分层（概览）

```
main.rs → OrchActor (agent/orchestrator.rs) → TurnExecutor (agent/turn.rs)
  TurnExecutor 内循环（同一用户输入可多轮 tool_use）：
  1. 压缩检查（同输入最多一次，TurnCompactor 守卫）
  2. LLM 流式请求（SSE → Event；工具层解析）
  3. Scavenge 回收遗漏工具调用 → 持久化 assistant 消息
  4. ToolRunner::execute_all（surface gate → StormBreaker → dispatch → 格式化/artifact）
  5. 持久化 tool results → Display 输出 → 信号采集 → belief → decision
```

分层：LLM 层（`llm/client.rs` 流式客户端与重试、`llm/transport.rs` 请求构造、`sse/*` 解析）、工具层（`tools/runner.rs` 注册与分发、`tools/file.rs` Read/Write/Edit、`tools/{bash,python,todo,plan}.rs`）、资源与能力层（`resources/router.rs`、`capabilities/*`）、信号层（`guard/*`、`agent/{belief,decision}.rs`）、持久化层（`session/{store,compaction,plan,todo,prefix,artifacts}.rs`）、UI 层（`crates/mink-cli/{ui,tui}/*`）。完整模块职责见 `docs/ARCHITECTURE.md`，REPL/TUI 细节见 `docs/DESIGN.md` 与 `docs/TUI_OPTIMIZATION_ROADMAP.md`。

---

## 关键不变式

以下契约均由代码或测试钉住，改动相关代码时必须保持；动机与设计依据见 `docs/DESIGN.md`。

### 压缩与持久化

- `TurnCompactor`：同一用户输入的内循环最多压缩一次；auto、preflight、manual、overflow 统一经过该守卫并传播失败。
- `ImmutablePrefix`：system prompt/tools 变更必须 invalidate 并重建；每次构建/重建向 events.jsonl 写一条 `prefix_snapshot`（fingerprint/dependency_fingerprint/system_prompt/tools_json），缓存命中不得重复写。
- `conversation.jsonl` 完整保留且只追加；压缩只推进 `context-state.json` 的活跃投影边界；`ConversationStore` 缓存只保留活跃后缀并随 append 增量更新，模型请求只能经 `active_messages()` 读取活跃投影。
- `context-state.json` 必须同目录临时文件 + rename 原子替换，成功后更新内存并按新 `active_start` 裁剪缓存；JSONL 续写前修复未换行尾记录、以含换行的单缓冲区追加、append 经内部写锁串行化，读盘只容忍文件末尾半截记录。
- 状态发布区分**发布前失败**（旧文件不变）与“已发布但目录同步失败”：后者必须重读并比对**完整预期快照**、重做目录同步；无法恢复时闩锁 session（拒绝后续状态变更、以 fatal 结束当前 turn），禁止按旧 revision 重试。闩锁后 turn/压缩入口、同批工具派发与 `publish_state*` 一律拒绝（低层 `atomic_replace` 仅供显式恢复路径）；启动期写入与 telemetry/外观元数据（stats、session title）不在此列。
- 投影边界必须位于完整历史内、不得拆开 tool call/result 协议；cut point 必须保留最近 ≥2 条真实 user 消息（优先于纯 token 预算）。
- 所有压缩统一调用 LLM 摘要，以唯一 internal user `<compacted-summary>` checkpoint 投影，不修改 immutable system/tools prefix；`context_compact_input_reduction=true` 只精简摘要请求，不改写完整历史或热尾部。
- auto 压力优先用同模型、同 system/tools 指纹、同 projection generation 的最近 provider prompt usage 校准（基线须为当前投影严格前缀）；preflight 始终保守本地估算，基线只存 runtime 内存；`prompt_usage_calibration_safe=false` 的后端禁用校准；支持 cache projection 的后端必须让摘要复用主请求的实际 system/tools 与历史公共缓存前缀，无法证明边界或超预算时按 reduction 配置降级。
- 压缩与子代理请求必须使用当前活动真实模型名与别名，并复用 runtime 共享 `LlmBackend`；provider context overflow 只允许在无部分输出且本轮尚未压缩时触发一次压缩与一次重试。
- `max_context_tokens=0` 禁用 auto/preflight 与本地输入预算上限、保留手动压缩；压缩百分比、响应预留、热尾部、摘要输出预算来自显式配置（不推断隐式档位）；有限窗口下 reserve 与摘要输出必须小于窗口、热尾部小于主请求预算。
- Agent JSONL `SdkOptions`、Python `SandboxConfig`、Rust `AgentOptions` 覆盖同一组压缩参数并映射到唯一 `Config`；runtime 创建 session 前调用 `validate_runtime_config()`。

### LLM 有界恢复

- 唯一恢复配置是 `Config.llm_recovery`（`format_window_size` 1..=1024、`format_max_errors` 0<=K<W、`request_max_retries` 0..=16、可选 `request_timeout_secs`>0）；Rust `AgentOptions::with_llm_recovery`、JSONL `options.recovery`、CLI/server `[recovery]`、Python 四字段全部映射到它并在 session 创建前校验，不得新增第二套默认值或策略层。
- 格式窗口是 turn 内的布尔滑动窗口（新用户 turn 清空、压缩不清空、子代理独立、不落盘）：每个可继续的 round 在唯一尾部先结算窗口一次（正常 false；需反馈纠错或曾有响应协议损坏为 true），再执行分支决策（todo 提醒/证据注入），最后刷新已提交历史——刷新必须在本轮所有追加之后，保证诊断、工具结果与注入状态都进入下一请求；预判（`would_exceed`）不得改变窗口；错误数 > K 以稳定前缀 `format_recovery_exhausted` 结束 turn 且不再发请求。
- 请求恢复由 runtime 的 attempt 循环所有：一个 round 的请求投影（含图片物化）只构建一次；内置 backend 每次 `stream()` 对应一次物理请求，外层恰为 retries+1 次调用；退避 1s/2s/4s…上限 10s，`Retry-After` 仍为最早重试时间且不被退避上限截短；取消、持久化闩锁与可选总期限在每次 attempt 前与退避中生效（取消优先于等待中的重试），总期限同时约束建流与流消费（到点停止等待/消费，已收到的 usage 照常结算），耗尽用 `request_retry_exhausted` / `request_timeout` 终止并保留诊断。
- 每个 attempt 独立结算 usage（成功 reported、失败或取消 unreported，恰一笔）；只有最终接受且指纹/投影匹配的主请求可更新 provider prompt usage 校准；自定义 backend 的聚合 `attempt_count` 原样保留，外层不乘算。
- 压缩摘要请求共用同一重试状态、错误分类与 attempt/deadline 语义；完整但不可用的摘要（空输出、工具调用、坏 stop 原因）在同一次压缩操作内追加用途纠正到临时请求尾部重试，不写父 conversation、不改公共缓存前缀、不推进主 agent 格式窗口。
- 上游错误必须类型化：`LlmUpstreamError`（Recoverable / ProtocolDamaged / Permanent，带可选 status/code/source 与受限诊断）是唯一分类依据；永久拒绝与未知 provider 错误默认失败且不重试；未类型化的自定义错误保持既有失败行为。

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
- 回滚只作用于循环窗口内被编辑路径，目标为最后一次 Read/Write 完整内容基线（编辑后内容不得作为回滚目标，Replace 的 Read 同样记录基线）；写回经 `publish_state_with_permissions`（权限先设置到临时文件再发布；权限读取/设置失败不得发布、也不得记成功回滚）且仅当磁盘与基线不一致；写回后 bump memo mutation，以 `signal_rollback` 落事件。
- 恢复守卫拦截必须生成真实信号喂回信念；连续拦截达 `guard_max_blocks` 必须绕过守卫并强制注入，禁止无限拦截；信念跌破 abort 进入用户接管（`signal_handover` 事件落盘后 Failed），禁止静默丢弃证据；策略重启子代理初始化失败必须降级（`signal_replan_error` 后返回 None），不得升级为整轮 Err。
- Recovery 首步资格来自 resolved semantic capabilities；Bash 的 `FocusedVerificationExec` classifier 与普通 Bash 安全/误用提示相互独立。

### 工具面与执行

- approval 在构建 `ModelToolSurface` 时解析；`ToolRunner::execute_all()` 在 StormBreaker 前校验调用属于同一 resolved surface，真实执行只接受 surface 内工具（disable flag 与沙箱策略不属于运行时合同）；只并发连续只读工具，写入/执行/控制/SubAgent 工具按调用顺序串行执行。
- `enabled_tools` 是唯一工具启用输入：`None` 用 catalog 默认集、空列表禁用全部、显式列表精确选择；`PythonSandbox` explicit-only，仅显式列出时进入 surface。
- 子代理终态由内部 `SubAgentStatus` 表达（Interrupted/TimedOut 不得映射为成功）；未完成子代理只在 `SubAgentBatch.pending`，每个任务只有一个结果发送点，`Drop` 必须 cancel+abort；fork 在 runtime 初始化前以目录级克隆继承父 session 状态，子代理复用父 `LlmBackend` 与当前活动模型。
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
- skill index、selected skills、`skill://`/`rule://` 必须来自同一 `CapabilitySnapshot`；selected skill 正文不依赖资源读取 provider。

### Display 与 TUI

- Display 实现必须完整转发 `ToolCallDisplay` / `PresentedToolResultDisplay` 结构化字段，不得丢失 `tool_use_id`、presentation 或 artifact 元数据。
- REPL/TUI 输出不可信 payload（模型文本/thinking、工具输出/摘要、错误）前必须经共享控制序列清洗（thinking/text 块内跨 chunk 保留解析状态、类型切换与消息边界 reset、每条错误 reset stderr、子代理 thinking/text 两段分别清洗、stdout/stderr 状态独立）；renderer 的颜色/标题码在清洗之后添加。
- TUI 光标必须落在 UTF-8 char boundary，输入/删除按 char boundary 处理。
- TUI 粘贴图片只产生 session `attachments/` 传输副本并把绝对路径写入用户消息；图片进入上下文的唯一入口仍是 `Read` 捕获；重复粘贴按内容寻址路径去重，路径不可无歧义表示时 fail closed。
- Inline TUI 只提交连续且 sealed 的 transcript 前缀，committed 项不得修改或重复写入原生 scrollback；空闲保留最后一个 sealed item，新工作开始后才能推进 committed 边界。
- Inline 详情视图不得丢弃或重建主视图 terminal（否则 alternate screen 内容叠加残影）；Full TUI 保留主视图鼠标命中、可逆折叠与应用内 viewport，不受 Inline committed 边界影响。
- TUI 实时 signal 与 replay 必须经同一个 reducer，结构化工具状态不得从展示文本反向解析；Todo 增量 presentation 合并到当前完整状态；Plan/Todo/Artifact 详情先按内容宽度折行再算滚动范围。

---

## 新增工具步骤

1. 在 `crates/mink-core/src/tools/*.rs` 实现 `ToolExec` 或辅助函数。
2. 在 `tools/runner.rs` 的 `TOOL_REGISTRY` 注册。
3. 在 `crates/mink-core/src/assets/tools.json` 添加 schema（字段与 serde 一致）。
4. 在 `metadata()` 声明 approval tier、结果类型、副作用（mutating）、spawns_sub_agent、storm_exempt。
5. 需要压缩给 LLM 的内容时设置 `ToolOutcome.conversation_content`。
6. 补充单元测试：schema/registry 一致性、approval、错误路径、截断、artifact、信号与安全边界；协议变更同步 `docs/tools.md`。

---

## 开发提示

```bash
cargo test              # 日常测试（跳过重型测试）
cargo test -p mink-core --lib           # 仅 core 单元测试
cargo test --workspace                  # 全 workspace
cargo test --features slow-tests -- --include-ignored   # 全量（含 WASM 沙箱，较慢）
make build / make check / make test / make feature-matrix / make regression-all
cargo fmt --all -- --check && cargo clippy --workspace --all-targets --all-features -- -D warnings

# 调试（session 目录内）
grep '"belief"' events.jsonl | jq '{type, belief}'
grep '"signal_rollback"\|"signal_handover"' events.jsonl
grep '"prefix_snapshot"' events.jsonl | jq '{version, fingerprint, dependency_fingerprint}'
./target/release/mink --print "..."   # stream-json 模式
./target/release/mink --tui
```

`crates/mink-core/src/tools/sandbox_python.rs` 的重型测试默认跳过（wasmtime JIT 执行 CPython WASM，CPU 密集），CI 用 `--features slow-tests -- --include-ignored` 全量覆盖。

---

## 文档索引

| 文档 | 说明 |
|------|------|
| `docs/ARCHITECTURE.md` | 运行时分层、模块职责、核心数据流（模块查找首选） |
| `docs/DESIGN.md` | 设计取舍与不变式详述 |
| `docs/USAGE.md` | CLI 参数、配置、会话管理、工具参考 |
| `docs/EMBEDDING.md` | Rust 库 / Python SDK 嵌入、Token 用量 |
| `docs/PROTOCOL.md` | `--print` stream-json 与 `--agent-jsonl` 协议 |
| `docs/server.md` | mink-server REST/SSE API、生命周期与并发语义 |
| `docs/tools.md` | 内置工具参数与行为（Read/Edit 协议细节） |
| `docs/设计哲学-工具能力与提示词解耦.md` | 工具 surface、语义能力、prompt 所有权 |
| `docs/设计哲学-信号系统.md` | 信号系统完整设计 |
| `docs/TUI_OPTIMIZATION_ROADMAP.md` | TUI 当前实现和维护建议 |
