# Agents Guide

> 更新日期：2026-10-16

## 项目概览

Mink 是用 Rust 1.99+ 实现的轻量 AI coding agent，面向 DeepSeek/OpenAI-compatible API：单二进制、终端优先，也可作为库嵌入（`mink::runtime`）。核心能力：

- LLM 流式请求 → 工具执行 → 决策的内循环；REPL / TUI 两种终端模式，`AgentRuntime::start() → run_turn()/stream_turn() → shutdown()` 无需子进程
- 信号驱动的信念系统：轨迹证据注入（`[trajectory]`/`[detector]`）、编辑循环快照回滚、恢复首步守卫；`SignalPolicy` 分档，阈值/超参为内部常量，`MINK_SIGNAL_POLICY=off` 关闭
- 上下文自适应压缩：显式阈值/响应预留/热尾部/摘要预算，摘要以 `<compacted-summary>` checkpoint 投影；Todo（`todos.json` 权威快照 + revision/稳定 ID）、Artifact（超长输出落盘）、轻量资源读取（artifact/skill/rule/session URL）
- Goal：草案确认、独立只读审核、按需读取，显式跨轮目标、默认 20 turn/60 分钟绝对期限、三种验收、持久恢复与未知副作用对账；core SessionLease 覆盖所有可写 runtime
- 工具元数据与审批策略；Edit 双模式（Hashline snapshot / Replace exact+fuzzy，启动后固定）；子代理（isolated home，可目录级 fork）；TUI `Ctrl+V` 粘贴图片（session `attachments/` 传输副本 + 绝对路径，图片仍由 `Read` 捕获）

---

## Agent 工作规范

本文件每个任务都会注入；以下是本仓库对 agent 的行为约束，与「关键不变式」互补，违反任一条都需修正。

### 技能纪律

- **先看技能再动手**：任务与 skill-index 中某技能的适用场景匹配时，先 `Read skill://<name>` 并遵循其步骤执行，不要等用户提醒。
- **时机对应**（技能须在 skill-index 中可见）：编辑代码前核查调用点与假设 → `pre-code-check`；出现 bug、测试失败或异常行为 → `debugging`；实现新功能或修 bug → `tdd`；声称完成、commit 前 → `verification`；结构性、调用关系或影响面问题 → `ripwire` / `codegraph`；处理 Office 文档 → `officecli`。

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
- **期限测试**：正常 runtime 回归使用生产的日志提交与 shutdown 预算；短期限仅注入故障测试，不通过全局 `cfg(test)` 缩短健康路径的等待时间。
- **回归意识**：核心路径（压缩/持久化/编辑/信号）注意 `crates/mink-core/tests/invariants.rs` 与 regression 套件；删除或弱化被测试钉住的行为前先确认其已废弃。
- **停止-重析**：同一命令连续失败或输出异常时停下，重读代码与错误，换一种方法；禁止无修改重复相同命令。
- **假定不可信**：编译通过≠行为正确，测试全绿≠没有回归；用新证据（针对性测试/日志）确认因果，与先前结论矛盾时按新证据行事并明说。

### 架构与文档纪律

- **文档范围**：正式文档只描述已实现行为、设计约束、验证方法与测量证据；实施计划、临时审计及评审报告仅本地保留，不入库。
- **契约必守**：「关键不变式」不可逾越；确需变更协议时先更新本文件与 `docs/` 对应章节，并保持全仓一致。
- **同步文档**：架构/行为变更更新 `docs/concepts/` 的对应机制、`docs/guides/` 的操作说明及 `docs/reference/` 的唯一权威定义；集成契约更新 `docs/integration/`，新增公开页登记 `docs/manifest.json`。同步本文件（含更新日期）；用户可见变更记入 `CHANGELOG.md` Unreleased。维护记录放 `docs/development/`，不发布。
- **测试锚定**：新增行为必有测试；不变式守护优先落在 `tests/invariants.rs` 或模块级回归测试。
- **性能证据**：`docs/development/performance.md` 维护测量方法与索引，日期结果及脱敏 JSON/日志存 `docs/development/benchmarks/`；保留版本、dirty 标记、样本口径及失败/跳过，不把历史结果称为当前 HEAD 实测。日常输出留 `target/bench/`。
- **官网展示**：`docs/index.html` 是静态官网，与 server 工作台分离；以轻量可嵌入、长任务上下文、可靠编辑为主线。回放使用公开记录或独立实跑，标注节选/重建与来源，不编造结果或统计；终端示例的字号、行高、状态字段及窄屏优先级对照真实 TUI；中间统计须有事件来源并与最终总量核对，未记录值显示未知；安装示例核对公开 API，本地资源保留许可。改动运行 `node scripts/test-homepage.mjs`、`python3 -m unittest discover -s scripts -p 'test_hero_replay.py'` 并检查桌面/窄屏渲染。

### 提交纪律

- **说明范围**：提交说明与代码注释描述行为、原因和约束，不包含实施计划中的阶段编号或审计优先级标记。
- **卫生**：不提交敏感数据（模型轨迹、密钥、用户目录、内网地址）与实验性未跟踪产物；拿不准先问。
- **message**：一个主题一个提交，完整概括 what + why + 影响面；「版本号 + 文档同步」这类准备性改动可单独成提交。

---

## 运行时分层（概览）

```
main.rs → OrchActor (agent/orchestrator.rs) → TurnExecutor (agent/turn.rs)
  TurnExecutor 内循环（同一用户输入可多轮 tool_use）：
  1. 压缩检查（同一输入内可重复压缩，严格下降终止，无次数上限）
  2. LLM 流式请求（SSE → Event；工具层解析）
  3. Scavenge 回收遗漏工具调用 → 持久化 assistant 消息
  4. ToolRunner::execute_all（surface gate → StormBreaker → dispatch → 格式化/artifact）
  5. 持久化 tool results → Display 输出 → 信号采集 → belief → decision
```

分层：LLM 层（`llm/client.rs` 流式客户端与重试、`llm/transport.rs` 请求构造、`sse/*` 解析）、工具层（`tools/runner.rs` 注册与分发、`tools/file.rs` Read/Write/Edit、`tools/{bash,python,todo,plan}.rs`）、资源与能力层（`resources/router.rs`、`capabilities/*`）、信号层（`guard/*`、`agent/{belief,decision}.rs`）、持久化层（`session/{store,compaction,plan,todo,prefix,artifacts}.rs`）、UI 层（`crates/mink-cli/{ui,tui}/*`）。完整模块职责见 `docs/concepts/architecture.md`，REPL/TUI 细节见 `docs/concepts/runtime.md` 与 `docs/development/tui.md`。

---

## 关键不变式

以下契约均由代码或测试钉住，改动相关代码时必须保持；动机与设计依据见 `docs/concepts/runtime.md`。

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

### LLM 有界恢复

- 唯一恢复配置是 `Config.llm_recovery`（`format_window_size` 1..=1024、`format_max_errors` 0<=K<W、`request_max_retries` 0..=16、可选 `request_timeout_secs`>0）；Rust `AgentOptions::with_llm_recovery`、JSONL `options.recovery`、CLI/server `[recovery]`、Python 四字段全部映射到它并在 session 创建前校验，不得新增第二套默认值或策略层。（provider HTTP 总超时 `http_timeout_secs` 是传输层配置，不属于本组恢复策略。）
- 格式窗口是 turn 内的布尔滑动窗口（新用户 turn 清空、压缩不清空、子代理独立、不落盘）：每个可继续的 round 在唯一尾部先结算窗口一次（正常 false；需反馈纠错或曾有响应协议损坏为 true），再执行分支决策（todo 提醒/证据注入），最后刷新已提交历史——刷新必须在本轮所有追加之后，保证诊断、工具结果与注入状态都进入下一请求；预判（`would_exceed`）不得改变窗口；错误数 > K 以稳定前缀 `format_recovery_exhausted` 结束 turn 且不再发请求。
- 请求恢复由 runtime 的 attempt 循环所有：一个 round 的请求投影（含图片物化）只构建一次；内置 backend 每次 `stream()` 对应一次物理请求，外层恰为 retries+1 次调用；退避 1s/2s/4s…上限 10s，`Retry-After` 仍为最早重试时间且不被退避上限截短；取消、持久化闩锁与可选总期限在每次 attempt 前与退避中生效（取消优先于等待中的重试），总期限同时约束建流与流消费（到点停止等待/消费，已收到的 usage 照常结算），耗尽用 `request_retry_exhausted` / `request_timeout` 终止并保留诊断。
- 每个 attempt 独立结算 usage（成功 reported、失败或取消 unreported，恰一笔）；只有最终接受且指纹/投影匹配的主请求可更新 provider prompt usage 校准；自定义 backend 的聚合 `attempt_count` 原样保留，外层不乘算。
- 压缩摘要请求共用同一重试状态、错误分类与 attempt/deadline 语义；完整但不可用的摘要（空输出、工具调用、坏 stop 原因）在同一次压缩操作内追加用途纠正到临时请求尾部重试，不写父 conversation、不改公共缓存前缀、不推进主 agent 格式窗口。
- 上游错误必须类型化：`LlmUpstreamError`（Recoverable / ProtocolDamaged / Permanent，带可选 status/code/source 与受限诊断）是唯一分类依据；永久拒绝与未知 provider 错误默认失败且不重试；未类型化的自定义错误保持既有失败行为。

### 人类输入与 Web 工作台

- **服务器默认前端是 `apps/web`（React 工作台）**：`crates/mink-server` 的 build.rs 构建 `apps/web` 并把 `dist` 按字节嵌入二进制（资源含字体等非 UTF-8 文件，不得改回 `include_str!`）；开发时可用 `MINK_SERVER_DEV_WEB=<dir>/dist` 从磁盘服务，前端热迭代不重启 server。
- **新前端的设计层逐字节拷自 deepseek-harness**：`apps/web/src/{theme,ui/primitives,util,store}` 与各屏拷贝的 CSS module 除导入落点外不得手改，样式只消费拷贝的 `--dsw-*` token 层；来源与许可见 `apps/web/THIRD_PARTY_NOTICES.md`。改动这些文件时必须保持与上游同步（改前先看上游同名文件）。
- **控制面细节**：composer 的模型位是可点击菜单（清单来自运行时解析器，经 `GET/POST /api/sessions/{id}/model`，切换以运行时接受的 label 为准）；「+」打开命令面板（添加图片 + 契约里的全部命令，选中只写入草稿而不直接执行）；图标控件经拷贝 `Tooltip` 在 hover/聚焦时显示名称；打开失败的错误行带「重试」（只重跑打开，不重建 turn 动作）。
- **卡片家族与预览**：工具卡覆盖终端/文件/差异/搜索/待办/计划/委派（`tasks[]` 一行一个任务）/目标报告（类型、轮次、摘要、下一步、验收条件与证据），解析失败一律回退通用 in/out 卡；变更文件卡的每一行 hover 显示该文件的 hunks（取自转录，不二次请求）；设计核对页对每个家族都有样本。
- **草稿与附件按会话身份隔离**：`Conversation` 以会话身份为 key，草稿（`mink.draft:<project>:<id>`）与已上传附件 id（`mink.attachments:<project>:<id>`）随会话保存与恢复，切换会话不串、返回即可继续；存储不可用时退化为内存草稿。草稿必须按 key 在渲染期同步读取（keyed composer 首次挂载即拿到目标会话正文），不得靠 effect 延后换内容。
- **会话级异步身份与提交幂等**：投影、草稿、附件与在途提交按完整会话身份维护，提交记录（`mink.pending:<project>:<id>`）与草稿同生命周期：离开会话、刷新页面都不丢，只有服务端明确受理或拒绝才删除。发送/引导/回执合并/详情加载/历史分页/命令副作用只在发起它的会话仍是当前视图时落地；发送与 stop 的完成还必须验发起时的视图 epoch + 投影 runtime generation + 当前 turn（新的 `session_snapshot` 是更新的真相，旧 HTTP 失败不得回滚 running/写旧错误）。历史以投影 `messages` 为唯一已提交集合，按 `seq` 合并去重排序，分页不得覆盖快照之后的 committed 消息。提交以稳定 request ID 对账：答复丢失（网络）时先查回执或以同一 ID 重试，已受理不得再当失败恢复草稿；附件上传的非 200 拒绝与网络失败都落到失败条目及原因。
- **模型座位与目标详情的版本顺序**：模型读取与切换共享单调 request revision（每次 refresh、每次已接受切换与每次视图生命周期都推进），只有最新一次可写，A→B→A 与“先切换后回来”的旧响应一律作废；`/model`、`/flash`、`/pro` 成功后重读运行时。目标页签的 REST seed 在发起时记录投影目标更新计数，计数变化则丢弃 seed（清空/新建也不靠 null 或 ID 猜顺序），revision 相等时仍独立更新 `driven`。
- **新建任务面板**：接受任意 server 可达的工作目录（名称可选，已发现项目作候选），空目录也能创建第一个会话；`createSession(cwd)` 仍是服务端权威，被拒绝时保留用户输入。
- **新前端已实现的契约**：会话目录与打开（筛选/搜索四种范围、切换保留关键词、新建与删除）、`/conversation` 历史 + SSE 实时投影（`src/data` 的纯 reducer/SSE 客户端，`/open` 的 snapshot 提议优先）、历史分页（`before_seq` 回溯，翻页保持阅读位置）、composer 的 `/turn` 与 `/interrupt`、运行中引导（`/inputs` 队列：按 revision 编辑/撤回/续发，未取得 turn id 时拒绝并保留草稿）、附件与图片（粘贴/拖放/选择 → `/attachments` 会话副本，id 随输入提交，图片仍只经 Read 进上下文）、斜杠命令（与终端共用 `crates/mink-cli/src/tui/slash_commands.json` 契约）、工具卡家族（终端/文件/差异/搜索/通用）与逐轮「已编辑 N 个文件」摘要卡、消息 chrome（复制与该轮用量；旧轮次悬停显现）、目标条与目标边界卡（拷贝的 GoalBar 样式，暂停/恢复/取消/清除全部按 revision CAS）、详情面板五页签（任务/目标/文件/输出/诊断，诊断与 TUI StatsSnapshot 同口径）、轮次选择器与「返回最新内容」、移动端阅读 chrome（仅用户明确滚动触发，运行中/待处理强制可见，隐藏不 close/interrupt）、设置与偏好（外观/字号/过程展示）。
- **设计对照与校验**：`npm run check:design` 逐字节比对 `apps/web/src/{theme,ui,util,store}` 与 deepseek-harness 源码（`--source` 指定仓库根）；允许的差异只有登记过的导入落点改写，漂移即失败，CI 执行。与上游界面的视觉对照用 `dsh web` 起本地服务后由浏览器自动化截图/读取 `data-slot` 结构；对照图可能含用户私有会话，不得入库。
- **按「mink 没有的功能不抄」排除**：插件面板、Pin/Archive、权限 pill、`Standard mode` 模式标签、`Chat/Trajectory` 页签、消息时间戳（mink 对话行没有时间数据）。新增这类能力时先补运行时契约，再对齐界面。
- `InputInbox` 是 session 唯一持久输入准入边界；request ID 绑定原始内容，编辑/撤回比较 revision，仅 pending/unapplied 可修改。applying 先发布，正式历史以 `_mink.input_id` 接管，追加不确定或历史已写但回执发布失败必须闩锁；恢复核对完整预期消息，禁止重复追加。
- 引导绑定父 turn，在已接受工具交换完整持久化后、下一请求压缩检查前顺序消费；Stop 与准入共享锁。取消/故障/恢复耗尽/轮次上限优先，引导不重置格式窗口、账单 turn 或请求期限，不自动传给子代理；未应用项仅由用户明确续发。
- server 全局运行会话上限由唯一容量账本（`CapacityPool`）覆盖所有新 turn（含恢复旧引导）与目标驱动：普通 turn 的 permit 随 turn 任务结束释放，目标 run 的 permit 随 `GoalRunLease`/RunGuard 释放，且只在 run 结束且 turn gate 空闲后归还（暂停/取消后仍在退出的检查器或工具继续占容量）；同一 session 实例同时只有一个 permit，被拒绝的创建不占容量也不启动无关旧目标；等待人工验收没有执行任务时释放容量，恢复须重新申请。当前运行 turn 的引导与既有 request ID 的幂等重试不额外占用名额。配置路径是首个可选位置参数，未知 flag/多余参数须拒绝。
- Web mirror 经公开 Reader 初始化、可靠 runtime 事件更新，快照与订阅位置在同一发布锁建立；generation/水位拒绝旧事件。Goal 随投影与水位同一临界区镜像，快照/详情渲染不得在持有投影锁时获取会话状态锁（锁序固定为 state → projection）。离开视图只断订阅，不调用 close。草稿/附件/展开/阅读锚点按完整会话身份隔离，迟到响应不得污染当前视图；草稿写入必须递增 revision（改回原文不算未改），每个视图同时只允许一条在途命令，结果与提示/详情副作用只从发起尝试的 pending 上下文（identity/generation/revision）取判据并仅在视图与 runtime generation 仍当前时应用，成功且 revision 未变才清空草稿，服务器拒绝或传输失败保留正文供重试。
- Web 会话筛选只搜索已发现目录，项目名称（路径末段）与完整项目路径必须区分；会话范围包含标题、别名和 ID，切换范围保留关键词，不改变当前执行或会话排序。
- Web 根地址默认首页，会话刷新目标来自完整 URL 身份，不回退旧的本地选择；启动请求不得覆盖期间的明确导航，返回首页只断订阅。
- Web 设置使用按需打开的模态面板，菜单焦点交接完成后再捕获焦点；关闭须恢复入口，偏好修改不得重启任务或重置阅读状态。
- Web 菜单与移动覆盖层必须支持键盘关闭和焦点恢复；请求在途禁用重复操作，不以加载或 HTTP 回执覆盖更晚终态、runtime generation 或正式已应用输入。文件预览加载结果与滚动恢复均受完整身份及请求 revision 约束。
- Web 诊断必须与 TUI StatsSnapshot 口径一致：缓存命中只计 read、分母含 input/read/creation；缺失旧统计显示未知。完整 stats 与 activity 同镜像发布边界恢复，正式历史提交不得清除等待计时。
- Web 图标控件使用场景尺寸（32px/触控 36px、会话行 44px），禁止统一放大所有按钮；Reka UI 菜单优先处理方向键/Esc，手机覆盖层不得抢走菜单键盘事件或因响应式切换自动遮挡输入。
- Web 文本复制共用 Clipboard API/选区兼容路径，清理临时控件并恢复焦点、光标与选区；两条路径均失败不得显示成功。
- Web 工具卡片按实际参数与 ToolPresentation 展示，Replace/Hashline 均须支持；JSON 参数保留手动核对入口，正文不通用解码为参数，旧未知状态不得渲染为成功，Artifact 入口不得丢失。
- Web 轮次选择须为限高纵向列表，标题不得撑宽弹层；支持菜单内滚动、方向键/Home/End、选择关闭与焦点恢复。
- Web 正文/过程组须限制在可用列宽；自动换行只改展示并保存偏好，不改复制/发送/文件/工具结果的原始字节；关闭换行后的横向滚动只属于内部内容块。
- 手机操作栏仅由外层明确用户滚动隐藏，内层/布局/流式滚动不得触发；保留恢复入口、运行中停止、草稿与阅读锚点，输入焦点/附件/待处理或失败回执/恢复状态优先可见，隐藏不调用 close/interrupt。
- Web 会话扫描与查找共用子代理过滤：目录名或 metadata.id 为 sub_/replan_ 前缀、或 metadata.parent 存在时排除；不能因缺失、损坏或继承 metadata 将旧子代理展示为用户会话。
- CLI/TUI/Web 共用 core AttachmentStore：上传仅保存 session attachments 原始字节与绝对路径，图片上下文唯一入口仍为 Read。会话限定预览不得扩大工作目录文件访问范围。
- Full/Inline TUI 任务与运行中 Enter 引导均走 stream_input，准入不排到下一轮；仅正式 ConversationCommitted 才回显引导，Final 才结束轮次。尚未应用输入由 Inbox 保留，/resume ID 明确续发，/withdraw ID 按 revision 撤回；拒绝提交保留草稿及图片，不封口当前流。

### Goal

- `failed` 表示本 run 已结束，禁止自动调度；宿主显式 resume 可以重试同一目标，不按异常文字或历史版本分流。重试保留契约、iteration、用量、失败/阻断记录与原截止时间；过期或执行轮耗尽必须先明确调整预算，未决副作用先人工对账。completed/cancelled 禁止恢复和改预算；draft 须确认。审核阶段恢复仍复核原候选，最后一个执行轮允许续审；执行阶段恢复开启新轮，不重放已提交工具。当前 runtime 的 Faulted、持久化闩锁、关闭、执行未退出、容量和环境门槛不能被显式重试绕过。

- 独立审核先读取已提交的执行轮最终结论；明确部分完成且有契约内下一步可轻量继续，完成声明、空白/含糊/矛盾或外部阻断必须核对证据。轻量继续只写 continuations，不修改 reviews、验收项通过状态或完成依据；连续三轮相同剩余要求升级检查，指纹不取自然语言措辞。强制检查标志先落盘，同一进程复用原只读子会话；所有阶段共用验收 deadline 与 usage。持久快照 v4、契约 v2，旧候选恢复仍完整审核，不重置历史或额度。

- GoalStore 是唯一权威状态；模型可 GoalRead、只修改未确认 proposal 的 GoalDraft；正常执行结束由宿主调度只读审核。审核子代理只有读取工具，自然返回描述性报告，不得调用 GoalReview 或修改 GoalStore；宿主 agent 在原会话的候选绑定内部提交轮调用 GoalReview 统一提交 decision=continue/complete/blocked、summary、未完成时的 next_step；客观阻断才提供 blocker 字符串，可选 remaining_requirements。runtime 绑定候选、报告与证据后发布；不要求模型复制候选或重写逐项表。描述性结论保留 report，不制造逐项 findings；旧完整提交仅保留解码。GoalReview 固定在主代理工具 schema 中，只有提交轮可执行；已解码的结论校验拒绝返回具体工具失败供同轮纠正，不计入格式恢复窗口。模型不能确认草案、修改已确认契约、额度或运行时状态文件。 typed continuation 不得伪装人类输入；普通 turn 不得绕过活动目标与验收所有权。
- 驱动故障是稳定终态：不可恢复错误停止本 run 的调度并最多尝试一次落盘 `failed`，不得因磁盘仍为 `active` 自动重启（即使 failed无法发布）；`wait_goal` 返回运行时错误，`start_goal_driver`/`confirm_goal` 拒绝新激活，仅重开会话恢复。仍有工具/检查器未退出时先停止中，待其实际退出后才成为稳定故障；暂停/取消无法持久化（含未闩锁的发布前失败）时命令只按当前 run 身份记录带原始错误的 `Stopping` 并 interrupt，禁止继续调度与提交成功，由唯一 run 结算在 turn gate 空闲后发布 `Faulted`，不得提前发布稳定故障或把随后正常返回改写成 Idle。激活在任何状态写入前确认调度执行器与 readiness，拒绝返回错误且不写 active 契约；已写出的契约若仍无法启动 run 同样返回错误并闩锁 Faulted，不静默留下无人驱动的 active。
- 默认 20 turn/60 分钟从新草案确认时启动，暂停不重置；正常结束无需报告或补全轮。独立审核 blocked 需 `blocked_after_consecutive_rounds`（默认 3）轮*同一阻断*才结束目标，描述性结论的阻断身份为 `category`/契约版本/受影响验收 ID/依赖事实版本的持久化指纹（`blocker_fingerprint`）；自然语言依赖描述、新产物 ID 及本轮结论不参与身份，旧逐项审核保持原指纹，身份变化把同一阻断计数（`blocker_rounds`）重置为 1；`blocked_rounds` 仅记录连续 blocked 轮数，非 blocked 审核清零身份与两个计数。blocked 必须有实际读取的非空事实证据，审核与轮决策同次发布，恢复同一审核不增加阻断轮数。永久/格式/信号/持久化故障停止当前 run，不自动用新 turn 清空恢复计数；宿主明确重试保留 Goal 的持久计数，单个请求的自动重试与格式恢复上限不变。
- 正常 `shutdown()` 先取消 runtime token 再中断 turn gate：在飞的 goal round 只有在「gate 被中断且 runtime 未被取消」时才会结算为用户暂停，而正常关闭必须保留 active 意图供下一次打开恢复（`docs/reference/goals.md`）。
- 完成依据必须区分 agent_reviewed/verified/human_approved（model_reported 只保留旧终态历史）；验收前后核对声明证据、revision 与 conversation 序号。检查器缺失不得降级；证据变更用新 generation 在同一执行轮内重审，已知结果按返回时的证据版本复用，不重复执行检查器。新审核请求核对 Token 预算；人工批准核对绝对期限；审核者实际读取的新产物经宿主 ArtifactManager 导入父会话并保持 SHA-256 与引用一致。
- 独立审核的基础设施失败、无效输出或超时不得记为目标证据失败；持久化审核失败原因与 review_failure_stage 并保留候选；报告有效时只重试宿主提交，显式 resume 只重试审核且不消耗执行轮数。Web 目标条与详情显示审核进行中或可重试状态。
- 审核报告通过 artifact 保护并交给宿主 agent，主会话只追加带 goal_adjudication 身份的内部消息；宿主提交轮不消费人类输入、不接纳任务执行工具，不额外消耗 Goal 执行轮数，与只读审核共用验收期限和 usage。无有效提交时返回真实宿主终止原因，不把子代理普通文字当成结论。候选绑定执行边界，acceptance_conversation_seq 绑定宿主提交后的历史水位；完成与恢复均核对水位，新引导令完成失效。
- core SessionLease 在恢复前获取，工具/检查器实际退出前不释放；未知副作用恢复必须 blocked，不能合成成功或自动重放。`start_goal_driver` 只启用调度、不启动 run；宿主容量 run 只能经 `confirm_and_start_goal`/`resume_and_start_goal` 携带许可启动，恢复出的 active goal 只能由显式 resume 重新启动（无容量账本的生命周期宿主用 `start_goal_driver_and_run`）；被拒绝的 resume 不启动 run、不发出 provider 请求，停止意图的发布失败不读作已保存。Rust/Web/Python 默认不自动驱动，终端只在 UI 接入后启动 active。宿主容量随目标 run 转移：`GoalRunLease` 只在对应 run（RunGuard）内存活，且只在 run 结束且 turn gate 空闲后释放；驱动未启用、已有 run、合同/持久化拒绝时激活返回错误且立即销毁租约，不得先启动旧 active 目标或留下无人调度的 active 目标。
- Goal 在界面只呈现：TUI 状态栏 `goal:<状态> <剩余>/<上限>` 标记可点击打开只读详情（`/goals` 等效），会话打开从 `goal.json` 恢复、运行中由 `GoalUpdated` 更新；Web 经 `GET /api/sessions/{id}/goal` 读取当前快照（活运行时优先，回退 `goal.json` 的 `current`，不含 history），经 `POST .../goal` 与 `POST .../goal/{action}` 以宿主身份创建与控制，每次变更必须是当前 `revision` 的 compare-and-set，且一律经 `AgentRuntimeHandle` 落到 core；SSE 转发 `goal_updated`。界面不得绕过 core 修改契约、额度或状态。

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
- 恢复守卫拦截必须生成真实信号喂回信念；连续拦截达 `guard_max_blocks` 必须绕过守卫并强制注入，禁止无限拦截；信念跌破 abort 时，非交互且策略允许 restart 则先尝试 replan，成功重置信念并继续；未成功时 full 策略记录 `signal_handover` 后 Failed，较低策略直接 Failed。禁止静默丢弃证据；策略重启子代理初始化失败必须降级（`signal_replan_error` 后返回 None），不得升级为整轮 Err。
- Recovery 首步资格来自 resolved semantic capabilities；Bash 的 `FocusedVerificationExec` classifier 与普通 Bash 安全/误用提示相互独立。

### 工具面与执行

- approval 在构建 `ModelToolSurface` 时解析；`ToolRunner::execute_all()` 在 StormBreaker 前校验调用属于同一 resolved surface，真实执行只接受 surface 内工具（disable flag 与沙箱策略不属于运行时合同）；只并发连续只读工具，写入/执行/控制/SubAgent 工具按调用顺序串行执行。
- macOS sandbox-exec 写入白名单须覆盖声明目录、临时目录和会话存储的字面路径与可解析真实路径，避免符号链接别名使已允许写入失败；不据此扩大读取边界。
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
- skill index、selected skills、`skill://`/`rule://` 必须来自同一 `CapabilitySnapshot`；selected skill 正文不依赖资源读取 provider。frontmatter 按 Agent Skills 最小子集解析（`name` 合法时优先于目录名、`description`、`when_to_use`、`hide`/`disable-model-invocation`，支持 `>`/`|` 块标量；未闭合 header 视为不存在）；skill index 与 `skill://list` 为有损渲染（描述去控制字符、折叠单行、转义尖括号、1,536 字符截断；总量超预算按来源降级删除描述，技能名始终保留），`skill://list/all` 与 CLI/TUI 保留完整描述。

### Display 与 TUI

- macOS TUI 通知优先使用终端原生单条 OSC；不得用 `osascript display notification` 回退（点击属于脚本编辑器）。外部 `terminal-notifier` 必须携带终端 bundle 的 `-activate`，输出不能污染 TUI。

- TUI 主循环按事件唤醒，空闲不绘制/克隆 Inbox；正文暖缓存输入帧只访问视口，高度索引与消息 ID/revision/宽度缓存保持一致，宽度变化分批布局。详情按资源身份/revision/宽度缓存，滚动只切片，后台结果按 generation 拒绝过期查询。
- TUI 阅读锚点在追加、封口、折叠和 resize 后保留；只有明确提交或 Ctrl+L/`/latest` 恢复跟随。输入移动/删除按 grapheme，迟到准入回执只能清理对应草稿 revision；撤回/续发已有输入不消耗未发送草稿与图片，文本编辑不消耗新图片；停止等待权威 Final。
- Inline 每批最多提交 256 行，插入成功才推进消息/行位置；失败立即退出、不重试部分写入。已提交正文/presentation/行缓存释放，子代理详情经公开 Reader 按需读取；resize 不重写 committed 内容。
- Inbox 只读 watch 的初始快照与订阅同准入锁建立，持久发布成功后通知；watch 可合并状态，不能替代正式历史和权威终态。Reader 历史窗口流式保留完整轮次，子代理读取不得越出父 session。
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
6. 补充单元测试：schema/registry 一致性、approval、错误路径、截断、artifact、信号与安全边界；协议变更同步 `docs/reference/tools.md`。

---

## 开发提示

```bash
cargo test              # 日常测试（跳过重型测试）
cargo test -p mink-core --lib           # 仅 core 单元测试
cargo test --workspace                  # 全 workspace
cargo test --features slow-tests -- --include-ignored   # 全量（含 WASM 沙箱，较慢）
make build / make check / make test / make feature-matrix / make regression-all / make bench / make bench-quick / make bench-check
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

- `docs/start/overview.md`：三条阅读路径与完整清单。
- `docs/concepts/architecture.md`：模块查找首选；运行契约见 `runtime.md`、`state-and-context.md`、`tools-and-capabilities.md`、`recovery-and-signals.md`、`images.md`。
- `docs/guides/`：终端、Web、会话与引导、Plan/Todo、上下文、图片、定制、安全和排查。
- `docs/integration/`：Rust、Python、扩展契约。
- `docs/reference/`：配置、工具、机器协议、HTTP API 和用量的唯一完整定义。
- `docs/development/`：TUI、性能、Web 与官网维护；`docs/migration.md` 为迁移对照。
- `make docs-check` / `make docs-site`：统一校验与组装，产物 `target/docs-site`。
