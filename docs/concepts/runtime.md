# 运行时执行

> 更新日期：2026-10-05

完整进程隔离由 worker 的 sandbox re-exec 提供；macOS 写入规则须覆盖声明目录、临时目录与会话存储的字面/真实路径，以适配内核按真实路径判断的行为。运行时本身不隔离宿主进程，平台边界见[安全指南](../guides/security.md)。

turn、并发、取消与子代理。

## 核心数据流

### 单轮执行

```text
用户输入
  │
  ▼
OrchActor.handle_user_input()
  ├── belief.decay(config.signal.decay_per_input)
  ├── ctx.interrupt = false
  ├── resolve_active_model()
  └── TurnExecutor::execute()
       │
       ├── tools.reset_storm()
       ├── compactor.reset()
       ├── signal_processor.reset()
       ├── decision_engine.reset()
       ├── store.add_user(input)
       ├── ensure_prefix()
       │
       └── while turn < max_turns:
          ├── auto compact + preflight compact
          ├── 固定本 round 请求投影（含图片物化，只构建一次）
          ├── attempt 循环：建流 → 消费流 → 故障分类（共享同一可选总期限）
          │    ├── 可重试（502/429/连接重置/首事件与 idle 超时）：废弃候选，退避后重试（恰发一次 Retry）
          │    ├── 协议损坏：废弃候选，计本 round 一个格式错，预判窗口后重试
          │    ├── 永久/取消/耗尽：request_retry_exhausted / request_timeout / Interrupted
          │    └── 每次 attempt 独立结算 usage（MeteredStream → usage.jsonl）
          ├── scavenge thinking/text 中遗漏的工具调用（已识别但参数不可解析或 DSML 参数头不完整的调用作为降级候选呈现）
          ├── §6.3 结束判定：拒绝/截断/调用身份/可配对调用/完成/不可确认
          ├── store.add_assistant()（仅接受后的候选；ToolCall 展示与事件同点后移）
          ├── ToolRunner::execute_all()（模型格式失败标 ModelFormat，不执行坏参数调用）
          ├── 工具阶段原地完成 Plan 交接（PlanCommand → effect / append-only transition）
          ├── SubAgentCoordinator 启动/收集子代理
          ├── ToolRunner 统一定稿并保护延迟结果大小
          ├── ToolSignalProcessor 基于最终结果更新 belief（ModelFormat 仅单独计数）
          ├── store.add_tool_results()
          ├── 发射 AgentEventKind::ToolResult
          ├── 唯一的 round 结束点：格式窗口提交 true/false；超限→format_recovery_exhausted
          ├── 分支决策（Todo 提醒/证据注入），然后刷新所有已提交历史
          └── 循环结束 → OrchActor::finish_usage() 汇总 billing_turn_id → TurnOutcome
```

### 工具结果进入 LLM 与 UI

```text
ToolExec::execute()
  -> ToolOutcome { content, conversation_content, exit_code, ... }
  -> format_dispatched_result() -> ToolExecution
       普通结果立即执行大小保护、bash noise filter、Read/Write summary 和 Edit conv content
       Plan/SubAgent 结果保留待定稿标记
  -> SubAgentCoordinator 完成延迟工作（Plan 交接在工具阶段原地完成）
  -> finalize_deferred_results()
       对最终延迟结果执行大小保护，超限时写 artifact 并追加 artifact://<id>
  -> ToolSignalProcessor 采集最终结果
  -> ConversationStore::add_tool_results()
       使用 conv_content（若非空）否则使用 content
  -> AgentEventKind::ToolResult
```
`content` 受 `tool_result_max_bytes` 保护；`content_preview` 用于简短终端展示，presentation
携带 Plan/Todo 结构化状态。LLM conversation 由 `ConversationStore::add_tool_results()` 写入，
不依赖 UI preview。

### 信号系统

信号系统位于工具执行之后、下一轮 LLM 调用之前。`ToolSignalProcessor` 使用 `SignalCollector` 从工具结果中采集失败、错误模式和编辑循环信号，写入 `BeliefTracker`，再由 `DecisionEngine` 判断是否继续、注入恢复提示或中止当前 turn。

```text
ToolExecution
  -> SignalCollector
  -> BeliefTracker
  -> DecisionEngine
  -> None / Inject / Abort
```

每个用户输入开始时，belief 按 `Config.signal.decay_per_input`（默认 0.6）衰减；
ToolSignalProcessor、decision cooldown 和 StormBreaker 窗口重置。`MINK_SIGNAL_POLICY=off`
时，信号采集、belief 更新、注入和中止逻辑都关闭。

## Agent 主循环

### 单轮执行契约

`TurnExecutor::execute()` 是 agent 的核心循环，接收一个用户输入，执行零到多轮 LLM 调用，返回最终决策。一轮定义为一个 LLM 请求→响应→工具执行→继续/停止判断的完整周期。

```rust
// crates/mink-core/src/agent/turn.rs
pub async fn execute(
    &mut self,
    user_input: &str,
    belief: Option<&mut BeliefTracker>,
)
    -> Result<(TurnDecision, Vec<TurnEffect>)>
```

**TurnDecision** 有五类：

| 变体 | 含义 | 后续 |
|------|------|------|
| `Stop` | 正常结束（end_turn/stop） | 等待下个用户输入 |
| `Continue` | 有更多 LLM 调用 | 循环继续 |
| `Interrupted` | 被取消令牌中断 | 退出 |
| `MaxTurnsExceeded` | 当前输入超过最大内部循环次数 | 报告限制并结束 |
| `Failed(String)` | 不可恢复的错误 | 报告错误 |

**TurnEffect** 是 turn 内部工具副作用的完成标记，供调用者（OrchActor）补充 UI 提示：

| 变体 | 触发条件 | 调用者处理 |
|------|---------|-----------|
| `PlanCleared` | PlanClear 工具副作用已完成 | 显示计划已清空 |
| `PlanConfirmed` | PlanConfirm 工具副作用已完成 | 显示计划已确认 |

SubAgent 由 `SubAgentCoordinator` 在 turn 内部启动、收集和注入结果。

### 执行阶段

每轮 LLM 调用按固定阶段顺序执行：

```
步骤 0: 新用户输入初始化
  ├── reset_storm()
  ├── TurnCompactor::reset()
  ├── ToolSignalProcessor::reset()
  ├── DecisionEngine::reset()
  └── signal_recovery_guard = false
步骤 1: legacy store.add_user() / Inbox 应用新任务 + ensure_prefix()
步骤 2: 安全边界应用当前 turn 引导 → 自动压缩检查 + Preflight 紧急压缩
步骤 3: LLM 流式请求（SSE 解析 → Event stream）
步骤 4: Scavenge 回收（从 thinking/text 复原工具调用）
步骤 5: 持久化 assistant 消息和 usage
步骤 6: 工具执行（ToolRunner::execute_all）
  ├── resolved ModelToolSurface 执行门禁
  ├── StormBreaker 重复抑制
  ├── 工具阶段原地完成 PlanCommand 交接（effects / append-only transition）
  ├── SubAgentCoordinator 启动并收集子代理
  ├── 延迟结果统一执行大小保护
  ├── ToolSignalProcessor 基于最终结果采集信号并更新 belief
  ├── ConversationStore::add_tool_results()
  └── AgentEventKind::ToolResult
步骤 6.1: Plan transition 由 session 唯一 PlanStore 原子提交并追加内部 transition（不触发压缩）
步骤 7: DecisionEngine 决策继续、注入、中止或停止
```

各个阶段之间有严格的依赖关系：
- 同一用户输入的压缩可重复执行（不再有次数互锁）：auto / preflight / overflow 共用同一引擎入口，不设次数上限（终止条件是严格下降/严格缩小）；发布前过净收益门控——auto 要求 ≥10% 净下降，强制路径要求严格下降；Plan transition 不强制压缩
- 步骤 4 依赖步骤 3 收集的 thinking + text 内容
- 步骤 6 依赖步骤 4 补充后的 calls 列表
- 步骤 7 根据 stop_reason 决定是否循环

### LLM 调用循环

同一个用户输入可能触发多次 LLM 调用（工具调用→工具结果→再次调用 LLM）。每次响应先作为候选接受检查，再决定是否持久化、执行工具或结束：

1. interrupt 优先结束；明确 provider 错误、拒绝或过滤为终态失败，不作为格式错误反复重发。
2. `length/max_tokens` 废弃整份候选正文与工具调用，只结算 usage 并追加截断诊断，进入有界格式恢复。
3. 工具名、调用 ID 缺失或 ID 重复时整批调用不执行，追加一次格式诊断；身份合法的调用批进入工具执行阶段。
4. 无调用响应仅在 `end_turn/stop/done` 且正文非空时确认完成。空正文、thinking-only、无调用的 `tool_calls` 或空/未知原因进入格式恢复，不能静默成功。
5. 所有可继续分支在唯一 round 尾部结算格式滑动窗口，再执行 Todo 提醒、证据注入与决策。超出 `llm_recovery` 额度以 `format_recovery_exhausted` 失败；新输入清空窗口，压缩与引导不清空。
6. 继续前通过 `compaction.active_messages()` 刷新活跃投影；同时受 `max_turns`、取消、持久化故障与请求期限约束。

`messages` 在本轮所有正式追加之后刷新，确保下一轮 LLM 调用
看到最新工具结果、信号注入消息、计划变更和子代理结果，同时不会把冷历史重新加载进模型上下文。

## SSE 流式解析

### 解析器状态机

`OpenAIParser` 是一个增量状态机，处理 `data: {...}\n\n` 格式的 SSE 帧：

```rust
pub struct OpenAIParser {
    stop_reason: String,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_input_tokens: i64,
    cache_creation_input_tokens: i64,
    saw_text: bool,
    pending_calls: BTreeMap<i64, PendingCall>,
    pending_usage: Option<UsageEvent>,
    pending_stop: Option<String>,
    saw_done: bool,
    saw_usage: bool,
}
```

每次 `process_line()` 调用处理一行 SSE 数据。多个 chunk 之间的工具调用按 `index` 合并到 `pending_calls` map 中。

### 关键边界处理

**工具调用跨 chunk 合并**：OpenAI 流式 API 将工具调用的 id、name、arguments 分多个 chunk 发送。Parser 用 `BTreeMap<i64, PendingCall>` 按 index 聚合，在 finish_reason="tool_calls" 时一次性 flush。

**推理内容拆分**：`reasoning_content` 和 `reasoning` 两个字段都被识别。DeepSeek R1 使用 `reasoning_content`，OpenAI o1 使用 `reasoning`。

**缓存 token 读取**：`prompt_tokens_details.cached_tokens` 在 usage 解析时通过多层 fallback 提取：

```rust
usage.get("cached_tokens")
    .or_else(|| usage.get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens")))
```

### usage 事件延迟发出

OpenAI-compatible provider 可能在 finish chunk、独立 usage chunk 或 `[DONE]` 前后给出终态信息。
Parser 暂存 usage 和 stop reason，并通过 `saw_done` / `saw_usage` 保证终态事件按协议只发出一次。

### LLM backend 注入

默认 backend 是 `OpenAiCompatibleBackend`。`AgentOptions` 可以注入
`Arc<dyn LlmBackend>`，主代理、子代理和自动压缩共用同一个 backend trait，不复制 agent loop、
tool runner、session 写入或 usage 统计。

runtime 在构建阶段创建或接收共享 backend。主代理、子代理和压缩分别构造带有不同
`LlmPurpose` 的 `LlmRequest`，并提交到该 backend。
`TurnExecutor` 创建子代理协调器时，会从父配置克隆一份 child config，并把 `model` 设置为当前
活动 `LlmBackend` 的别名或真实模型名；存在别名时同时写入该别名到真实模型的映射，保证配置自洽。
每个子代理使用这份显式配置构建 context，因此模型切换后的活动模型和父 runtime 注入的 backend
会同时传递到子代理。

`OpenAiCompatibleBackend` 负责把当前上下文转换为 `LlmRequest`：

- `model`：经过 `ModelResolver` 解析后的真实 provider 模型名。
- `model_alias`：用户请求的别名，如 `flash`、`pro` 或 `model_aliases` 自定义别名。
- `messages` / `tools` / `system_prompt`：当前 prefix 和 conversation 状态。
- `cancel` / `display` / `purpose`：取消、状态输出和请求归属。

自定义 backend 只返回 `LlmEvent` 流。失败时可用 `LlmRequestFailure { attempt_count, error }`
保留重试次数，`MeteredStream` / `UsageCapture` 仍统一写入 `usage.jsonl`。

## SubAgent（子代理）

### 隔离执行

`SubAgentExecutor` 为每个子代理创建一个完全独立的 `AgentSharedContext`：

```rust
pub async fn new(parent_ctx, session_id, fork) -> Result<Self> {
    // 1. 在父 session 的 subagents/<id>/ 创建 isolated home
    // 2. fork 模式在 runtime 初始化前递归克隆父 session
    // 3. 重置 child session identity、events、stats 和 usage 文件
    // 4. 从 isolated home 正常初始化 store、compaction 和 artifacts
    // 5. 创建 linked child cancel token（父取消→子取消，子取消不影响父）
}
```

### 两种模式

**独立模式（默认）**：在父 session 的 `subagents/<session_id>/` 创建空的 isolated home。
继承父 session 的模型、API URL、工具集和 capability snapshot，但不继承 session 历史。

**Fork 模式**：在构建子 runtime 之前递归克隆父 session 目录，跳过父 session 已有的
`subagents/`。克隆后清除 `session.json`、`events.jsonl`、`stats.json` 和 `usage.jsonl`，
使子代理拥有新的身份与用量统计；conversation、context state、plan、artifacts 及同目录
session 状态会整体继承。压缩引擎在正常初始化时直接加载克隆后的状态。ArtifactManager 同样从
克隆后的 index 恢复下一个序号，并以独占创建方式写入，
因此子代理继续产生 artifact 时不会覆盖父历史中的正文或破坏已有 `artifact://` 引用。

如果父 runtime 注入了只读 VFS，子代理复用同一个 `Arc<dyn ReadOnlyFileSystem>`。两种模式都继承父代理的 `resource_session_id`，但 `agent_session_id` 使用子 session id；fork 只影响对话与 session 文件，不改变知识库分区。

### 结果收集

子代理完成时，通过 `last_assistant_message()` 从活跃缓存读取最后一条 assistant；缓存中不存在时
才流式扫描子 session，并且只保留最后一个匹配消息。返回 thinking 和 text，不包含工具调用细节。

```rust
if let Some(message) = child_store.last_assistant_message().await? {
    // 提取第一个 thinking block 和第一个 text block
}
```

### 并发控制

`SubAgentPool` 使用 `tokio::sync::Semaphore` 限制最大并发数（默认 8）。每个子代理占用一个 permit，完成后释放。
首次并发启动时，共享 `subagents/` 父目录按 `AlreadyExists` 幂等创建，并在创建竞争结束后重新校验为
实体目录；随机 session 子目录仍严格独占创建，冲突时直接失败。

结果通过 `mpsc::UnboundedSender` 发送回 orchestrator，由 `handle_sub_agent_result()` 注入父会话。

每个子代理有独立超时。超时后会取消子代理的 child token 并返回 failed 结果，父会话继续执行。

## 并发模型

### 异步边界

```
┌──────────────── Tokio Runtime ────────────────┐
│                                                 │
│  OrchActor  ←── mpsc channel ── SubAgent pool  │
│      │                                          │
│  TurnExecutor                                   │
│      │                                          │
│  ToolRunner::execute_all()                      │
│      │                                          │
│  read batch spawn_blocking() ...                │
│  sequential spawn_blocking()                    │
│      │              │                           │
│  file::read()    bash::execute()                │
│  (同步 I/O)     (子进程 + wait)                 │
└─────────────────────────────────────────────────┘
```

### 状态共享

所有共享状态通过 `Arc<AgentSharedContext>` 传递。内部可变性使用：
- `RwLock` — store cache（读多写少）
- `Mutex` — StormBreaker 窗口（写多）、immutable_prefix 缓存
- `AtomicBool` — dirty 标记
- `AtomicBool` — 当前 turn interrupt 标志
- `mpsc` — Orchestrator 命令、TUI signal、子代理结果收集

### 取消传播

`CancellationToken` 用于全局退出；`AgentSharedContext::interrupt` 用于当前 turn 中断。取消或中断时：
1. 主循环退出
2. SSE stream 在 25ms 检查窗口内停止
3. Bash / Python 工具检查 interrupt 并尝试杀掉子进程
4. 子代理使用 linked child token 接收父取消；子代理超时只取消自身，不取消父会话

---

### 锁分类与中毒行为

锁按“中毒后能否继续使用”分类；不采用“一律恢复”，也不以 `unwrap()`→`expect()` 充当修复。

| 类别 | 位置 | 中毒行为 | 依据 |
|---|---|---|---|
| 可重建/派生 | `read_memo`、`snapshots`、`immutable_prefix`、`stream_flush_last`、子代理 Capture 显示缓冲 | 取回数据继续（单次插入/替换，无跨字段不变式；显示缓冲允许丢失） | 派生数据可由源重建，继续使用不会伪装已验证状态 |
| 线程独占（无锁） | event-log `WriterState`（file/failure/last_loss/processed_lost） | 仅 writer 线程读写；跨线程只通过 FlushAck 快照 | 单一所有权消除多余同步 |
| 权威内存状态 | `TodoStore.state` | **写路径 fail closed**：`lock_for_write()` 闩锁 session 并返回错误；读路径取最后一致快照 | panic 中断可能留下撕裂 revision；恢复需重启 |
| 文件事务串行化 | `PlanStore.transition_lock` | 取回继续；真相在文件/journal，恢复由 `recover_pending`/`ensure_no_pending_transaction` 重放完成 | 锁只做串行化，不承载内存不变式 |
| 租约/活动状态（server） | `Registry.active`/`operation_locks`/`create_locks` | 可返回错误的操作经 `lock_active_state()` 返回 `Internal`（要求重启 server）；签名不可错的辅助函数显式 panic 并注明原因 | 活动 runtime 与租约不可在未验证状态下继续 |
| Tokio Mutex | server 操作/创建互斥 | 无中毒机制；取消一致性由既有 gate/取消测试覆盖 | Tokio 锁不具备 poisoning 语义 |

新增锁必须归入上表某一类并配对应测试；缓存类恢复需说明数据为何自洽，状态类不得静默继续。
