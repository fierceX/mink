# 架构与模块

> 更新日期：2026-10-04

分层、职责与关键不变式。

运行时资源与 TUI 渲染的测量方法、日期记录和证据见仓库[性能与验证](../development/performance.md)。

## 核心原则

- **单进程主循环**：`OrchActor` 接收命令并为每个用户输入创建 `TurnExecutor`。
- **机器协议优先**：`--print` 输出 stream-json 事件并以 `final` 收尾，`--agent-jsonl` 输出 single-shot Agent JSONL 事件和 `final`；request 可用 `options.stream_events=false` 关闭过程事件，仅保留最终 `final`，便于上层同时支持流式和非流式任务。
- **Session 追加友好**：conversation、events、stats、summary、plan 都落在 session 目录。
- **长输出可恢复**：工具输出超过上限时写入 artifact，conversation 只保留摘要和引用。
- **读取入口统一**：`Read.path` 可读取文件和轻量 internal URL，并支持行 selector。
- **工具结果双通道**：LLM conversation 使用工具结果或工具自定义 `conv_content`，UI 通过
  `PresentedToolResultDisplay` 展示基础内容、成功状态、结果类型和结构化 presentation。
- **信号驱动干预**：工具失败、错误模式、编辑循环会降低 belief，并触发注入或中止。

## 运行时分层

```text
crates/mink-cli/src/main.rs         ← mink binary thin wrapper
crates/mink-cli/src/bin/mink-core.rs← mink-core binary thin wrapper
crates/mink-cli/src/cli.rs          ← mink / mink-core 共用 CLI adapter
  │
  │  CLI 参数解析 -> 配置合并 -> sandbox re-exec
  │  根据模式启动 one-shot / REPL / TUI / stream-json / Agent JSONL
  │  调用 mink::runtime 构造 AgentRuntime
  ▼
OrchActor (agent/orchestrator.rs)
  │  接收用户输入、模型切换、手动 compact 命令
  │  维护 BeliefTracker 和当前强制模型
  ▼
TurnExecutor (agent/turn.rs)
  │  单轮执行器：压缩 -> LLM stream（固定投影 + attempt 重试）-> scavenge ->
  │  候选接受 -> 工具 -> 信号 -> 格式窗口结算 -> 决策 -> 历史刷新
  │  组合 PrefixManager / TurnCompactor / ToolSignalProcessor /
  │  SubAgentCoordinator（模型与 backend 在构造时一次确定）
  ▼
┌─────── LLM 层 ────────┐
│ llm/client.rs         │ LlmBackend 注入、每次 `stream()` 一个物理请求的 OpenAI-compatible 客户端、usage 采集、模型名解析、请求选项、缓存投影 seam 与校准门控
│ llm/recovery.rs       │ 类型化上游错误（可重试/协议损坏/永久）、请求重试计数与 deadline、纯退避/Retry-After 计算、稳定终态码
│ llm/transport.rs      │ OpenAI chat/completions 请求构造、tool_choice 和 extra_body 合并
│ sse/openai.rs         │ SSE 增量解析、usage、stop、tool call 合并；坏参数/坏身份候选不执行、不静默跳帧
│ sse/toolcall.rs       │ tool_call 字段归一化、兼容调用唯一 ID、原始参数摘要（storm 身份）
└───────────────────────┘
         │
┌─────── 工具层 ────────┐
│ tools/runner.rs       │ ToolExec registry、resolved surface gate、StormBreaker、结果格式化
│ tools/metadata.rs     │ ApprovalTier、ToolResultKind、ToolMetadata
│ tools/file.rs         │ Read / Write / 双模式 Edit、selector、resource、prepare/commit
│ tools/hashline.rs     │ 非 Block grammar、行号/文本锚点坐标与 clipboard apply
│ tools/replace.rs      │ exact/行窗口 fuzzy 内容匹配、歧义诊断与缩进转换
│ tools/snapshot.rs     │ Hashline 版本历史、seen-lines、tag、淘汰与路径迁移
│ tools/search.rs       │ Glob / Grep
│ tools/vfs.rs          │ Read / Glob / Grep 的同步只读 VFS hook、请求/结果协议和格式化
│ tools/bash.rs         │ Bash 执行、超时、ANSI 过滤、安全检查、误用提示
│ tools/python.rs       │ 宿主 Python 执行
│ tools/sandbox_python.rs│ WASI CPython 沙箱执行（python-sandbox feature）
│ tools/plan.rs         │ PlanDraft / PlanConfirm / PlanClear 类型化命令
│ tools/todo.rs         │ TodoRead / TodoWrite / TodoAdvance、revision 事件协议
└───────────────────────┘
         │
┌─────── 资源与能力层 ───┐
│ resources/router.rs   │ ResourceRouter、ResourceHandler、scheme 注册和分发
│ resources/{artifact,skill,rule,session}.rs │ Read 轻量资源 handler
│ capabilities/mod.rs   │ CapabilitySnapshot，汇总 skills/context files/rules
│ capabilities/skills.rs│ SkillProvider、SkillSnapshot、runtime/filesystem/built-in skills
│ capabilities/{context_files,rules}.rs │ instruction files / rules snapshot
└───────────────────────┘
         │
┌─────── 信号与防护层 ──┐
│ guard/collector.rs    │ ToolFailed / ToolError / EditLoop 信号
│ guard/storm.rs        │ 重复工具调用抑制
│ agent/belief.rs       │ belief 滑动窗口和平滑
│ agent/decision.rs     │ Inject / Abort / cooldown / recovery guard
 │ config.rs             │ MINK_SIGNAL_POLICY 覆盖 / SignalPolicy 枚举
│ safety.rs             │ 危险 Bash 命令过滤
└───────────────────────┘
         │
┌─────── 持久化层 ──────┐
│ session/store.rs      │ append-only JSONL、活跃后缀缓存、tool_result 写入
│ session/artifacts.rs  │ artifact index、持久序号恢复、防覆盖完整输出
│ session/stats.rs      │ token、请求数统计
│ session/usage.rs      │ LLM 请求级 Token 明细 JSONL（UsageJournal / MeteredStream；兼容字段 cost_nano_cny：已上报为 0、未上报为 null）
│ session/compaction.rs │ 显式策略、非破坏式投影、LLM 摘要与确定性应急 checkpoint、压缩状态；压力校准与缓存对齐摘要
│ session/compaction_input.rs │ 可选摘要输入降噪
│ session/prefix.rs     │ ImmutablePrefix
│ session/plan.rs       │ PlanStore 与 append-only 计划状态转换
│ session/todo.rs       │ TodoStore、原子持久化与追加式物化投影
│ session/atomic_file.rs│ Plan/Todo 共用的同目录原子替换
│ session/init.rs       │ session 目录和共享状态初始化
└───────────────────────┘
         │
┌─────── UI 层 ─────────┐
│ crates/mink-core/src/runtime/events.rs │ AgentEventStream / EventSink 结构化事件协议
│ crates/mink-cli/src/ui/engine.rs │ REPL / human 输出
│ crates/mink-cli/src/ui/replay.rs │ REPL session 重放
│ crates/mink-cli/src/tui/         │ ratatui Full / Inline 双 TUI surface
└───────────────────────┘
```

## 模块职责

### 入口与配置

| 文件 | 职责 |
|------|------|
| `crates/mink-cli/src/cli.rs` | **Mink / mink-core 共用 CLI adapter**，参数解析、配置合并、sandbox re-exec、模式分发；调用 `mink::runtime`，但 REPL/TUI 实现归属 CLI crate |
| `crates/mink-cli/src/main.rs` | `mink` binary thin wrapper → `mink_cli::cli::main_entry()` |
| `crates/mink-cli/src/bin/mink-core.rs` | `mink-core` binary thin wrapper → `mink_cli::cli::main_entry()` |
| `crates/mink-cli/src/config.rs` | CLI `Config`、参数解析、`.minkrc`/`--config` 合并、环境变量默认值、sandbox 配置 |
| `context.rs` | `AgentSharedContext` 和工具层 `ToolContext` |
| `assets.rs` | 编译期嵌入的 `tools.json` 和 skill 索引 |
| `capabilities/` | model-visible 能力 snapshot：skills、instruction files、rules，以及 source/exposure 元数据 |
| `capabilities/model_capabilities.rs` | session 级模型能力：图片输入能力/限额、能力指纹、`model-capabilities.json` 快照与兼容门控 |
| `resources/` | 轻量资源 URL 的注册式 router 和内置 handler；`Read` 是当前内置 provider，不拥有具体 resource scheme |
| `cancel.rs` | 父子 cancellation token |
| `safety.rs` | Bash 危险命令拦截 |
| `sandbox/` | Linux nsjail/bwrap 与 macOS sandbox-exec 自举 |
| `protocol.rs` | LLM 流式 `Event` 类型 |
| `events.rs` | typed event log 类型 |
| `errors.rs` | error 分类和用户提示 |
| `agent/text.rs` | 文本截断等通用工具 |

### mink-server（Server + Web）

| 文件 | 职责 |
|------|------|
| `crates/mink-server/src/main.rs` | 服务装配：config 加载、registry、idle reaper、graceful shutdown、嵌入/磁盘静态服务 |
| `crates/mink-server/src/api.rs` | REST + SSE 路由（sessions/conversation/plan/todo/artifacts/files/stream；project-aware `?project=`） |
| `crates/mink-server/src/session/registry.rs` | 会话扫描、fs2 advisory lock lease、typed RegistryError、并发 create/open/close/delete、usage.jsonl 汇总 |
| `crates/mink-server/src/session/runtime.rs` | AgentRuntime 包装、阶段机（Idle/Running/Cancelling/Closing/Closed）、forced terminal、AgentEvent→SSE JSON（stream_sequence） |

| `crates/mink-server/src/session/config.rs` | ServerConfig（env > toml > ~/.minkrc > 默认） |
| `crates/mink-server/src/web_assets.rs` | 嵌入前端服务（content-type/SPA fallback/缓存） |
| `crates/mink-server/build.rs` | 自动 npm build + dist 嵌入（include_str! 清单） |
| `crates/mink-server/web/` | Vue 3 SPA：单栏对话、指标行、面板体系、Edit 结构化渲染 |
Server 生命周期：Ctrl+C → axum serve 停止 → idle reaper abort → `registry.shutdown_all()`；
每 30s 扫描并按 `idle_close_secs` 关闭闲置会话；SSE 广播通道 1024、30s 心跳、
`stream_gap {missed}` 断线对账；删除会话前持有系统文件锁。

### Rust 库门面

| 文件 | 职责 |
|------|------|
| `runtime/mod.rs` | `mink::runtime` 公共 API 导出，供 `mink::prelude` facade 复用 |
| `runtime/builder.rs` | crate-private `build_runtime()` — 从 `AgentOptions` 的内部 resolved 配置构造 runtime |
| `runtime/config.rs` | 私有 resolved 配置 / `SessionPolicy` / `SessionInfo` |
| `runtime/handle.rs` | `AgentRuntime`（唯一 shutdown owner）/ 可克隆 `AgentRuntimeHandle` — `start()`, `handle()`, `run_turn()`, `stream_turn()`, `compact()`, `set_model()`, `interrupt_current_turn()`, `shutdown()` |
| `runtime/options.rs` | `AgentOptions` ergonomic builder，包括 LLM backend、只读 VFS 和 resource session scope 注入 |
| `runtime/events.rs` | turn-scoped `AgentEvent` envelope / `EventSink` / 异步 dispatcher / `EventDisplay` adapter |
| `runtime/tools.rs` | 稳定异步 `AgentTool` 自定义工具 API：`ToolDefinition` / `ToolExecutionContext` / `ToolOutput` / `ToolError` |
| `runtime/sdk_adapter.rs` | SDK option 映射、status/exit code 映射、`SdkFinal` 组装 |

### Agent 核心

| 文件 | 职责 |
|------|------|
| `agent/orchestrator.rs` | 命令循环、模型切换、手动 compact、turn 后处理 |
| `agent/turn.rs` | 单轮执行主流程和 tool_use 内循环 |
| `agent/prefix.rs` | `PrefixManager`，构建/复用 immutable prefix |
| `agent/compactor.rs` | `TurnCompactor`，封装同轮压缩防护 |
| `agent/tool_signals.rs` | 工具信号采集和 belief 更新 |
| `agent/sub_coordinator.rs` | SubAgent 批次的唯一 pending 所有者：启动、可中断收集、cancel/Drop 清理；结果按输入序回填 |
| `agent/sub_executor.rs` | 子代理独立 session / fork session 执行；`SubAgentStatus{Succeeded,Failed,Interrupted,TimedOut}` 终态只在边界转字符串 |
| `agent/belief.rs` | `BeliefTracker` |
| `agent/decision.rs` | `DecisionEngine` |
| `agent/recovery_policy.rs` | 基于已解析语义能力生成恢复提示并校验恢复首个调用；与普通 Bash 执行策略相互独立 |
| `llm/image_projection.rs` | 请求时 `tool_attachment` → `image_url` data-URL 物化、单次消费投影、配额防线与降级 |
| `crates/mink-core/src/config.rs` | `SignalPolicy` / 信号响应能力边界 |

### 工具系统

| 文件 | 职责 |
|------|------|
| `tools/metadata.rs` | `ToolMetadata`、approval tier、结果类型、副作用标记 |
| `tools/catalog.rs` | `tools.json`、executor registry 和 feature availability 的唯一目录 |
| `tools/surface.rs` | 按 `enabled_tools`、approval、role、backend、feature 和硬依赖解析模型可见工具面 |
| `tools/approval.rs` | 构建模型工具面时使用的非交互审批判定 |
| `tools/semantic_capabilities.rs` | 工具语义能力 offer、provider binding、scope classifier 和 fingerprint |
| `tools/runtime_guidance.rs` | 带结构化工具引用的运行时引导消息 |
| `tools/runner.rs` | `ToolExec` trait、`TOOL_REGISTRY`、resolved surface gate、并发调度、结果截断、artifact spill 和内置控制工具 |
| `tools/file.rs` | `ReadTool`、`WriteTool`、双模式 `EditTool`、selector、resource URL、图片读分类/准备、prepare/commit |
| `tools/image.rs` | 图片识别与头部探测：magic 预检（PNG/JPEG/GIF/WebP）、`ImageInfo`、`ImageAttachment` 与配额元数据 |
| `tools/hashline.rs` | 非 Block tokenizer/parser、行号/文本锚点坐标 apply、剪贴板操作 |
| `tools/replace.rs` | exact 与归一化行窗口 fuzzy 匹配、歧义诊断、缩进转换 |
| `tools/snapshot.rs` | Hashline 完整文本版本、seen-lines、xxHash tag、淘汰和路径恢复 |
| `tools/search.rs` | `GlobTool`、`GrepTool` |
| `tools/vfs.rs` | `ReadOnlyFileSystem`、`VfsScope`、结构化请求/结果、虚拟路径规范化、请求校验和结果格式化 |
| `tools/bash.rs` | `BashTool`、危险命令检查、误用提示 |
| `tools/python.rs` | `PythonTool` |
| `tools/plan.rs` | `PlanDraftTool`、`PlanConfirmTool`、`PlanClearTool` |
| `tools/todo.rs` | `TodoReadTool`、`TodoWriteTool`、`TodoAdvanceTool` 与追加式事件格式化 |
| `assets/tools.json` | 提供给模型的工具 schema |

新增工具时需要同时实现 `ToolExec::metadata()`、注册 `TOOL_REGISTRY`、更新 `assets/tools.json`，并在 metadata 中声明 approval tier、result kind、副作用、`storm_exempt`、`spawns_sub_agent`。若工具参与跨工具工作流，还必须在 `semantic_capabilities.rs` 显式声明受支持的语义能力和调用 scope；schema 只描述该工具自身合同，不得静态推荐其他工具。

模型可见工具只有一条构造链：

```text
ToolCatalog -> ModelToolSurface -> ResolvedToolCapabilities
                                      ├── runtime policies
                                      └── PromptWorkflowResolver -> prompt
```

`AgentSharedContext` 和 `ToolContext` 共享同一个 `ToolResolutionContext`、surface 和 capability
bindings。发给 provider 的 schemas 直接来自 surface；prefix 直接消费解析结果，不独立解析或
过滤 `tools.json`。

### Session 与持久化

| 文件 | 职责 |
|------|------|
| `session/store.rs` | append-only conversation、活跃后缀缓存、流式读取和尾部修复 |
| `session/metadata.rs` | session identity、alias、title 和时间戳元数据 |
| `session/artifacts.rs` | artifact 索引、持久序号恢复和正文防覆盖写入 |
| `session/stats.rs` | session 累计 token 和请求数统计 |
| `session/usage.rs` | LLM 请求级 Token 明细 journal（兼容字段 `cost_nano_cny`：已上报为 0、未上报为 null） |
| `session/compaction.rs` | 显式压缩策略、非破坏式投影、LLM 摘要与摘要不可用时的确定性应急 checkpoint、压缩状态；provider usage 压力校准与缓存对齐摘要 |
| `session/compaction_input.rs` | 摘要请求输入降噪 |
| `session/prefix.rs` | ImmutablePrefix |
| `session/plan.rs` | PlanStore、原子计划状态转换和 append-only transition |
| `session/todo.rs` | TodoStore、revision、稳定 ID、原子批量提交和 revision 对账 |
| `session/image_cache.rs` | 内容寻址图片缓存（`<home>/.mink/cache/images/v1/`）：SHA-256 对象、两阶段提交、digest 校验、有界读取 |
| `session/atomic_file.rs` | Plan/Todo 状态文件共用的同目录临时文件和原子替换 |
| `session/paths.rs` | 四种 session layout 的路径推导 |
| `session/init.rs` | session 目录、conversation、stats 和 artifact 基础设施初始化 |

### Prompt document

系统提示词先构造成带 section ID、来源和工具引用元数据的 `PromptDocument`，再统一渲染：

```text
Core sections
  -> allowlisted MISSION core overrides
  -> active tool fragments
  -> resolved capability workflows
  -> runtime/external sections
  -> validate generated tool references
  -> render
```

MISSION 只能覆盖 `system-conventions`、`agent-identity`、`environment`、`execution-codes`、
`belief-awareness` 和 `output-language` 中当前实际存在的 core section。工具 prompt、
workflow、`runtime-capabilities`、`tool-inventory`（非空 surface 时自动生成的工具清单）、
`rules`、instruction files、索引、selected skills 和
`current-plan` 属于 runtime-reserved section；普通自定义一级标题作为外部 section 追加。
runtime-reserved section 冲突会在启动时 fail fast，不保留旧 alias。

Plan 不属于 `PromptDocument` 或 `ImmutablePrefix`。PlanConfirm / PlanClear 在成功工具结果后
追加内部 user transition，因此能在同一 turn 的下一次请求生效并保持 system/tools 稳定。
历史已压缩且 `plan.md` 存在时，runtime 在摘要之后插入唯一的
`<active-plan-checkpoint>`；两种 Plan transition 都不强制压缩。

Todo 不使用逐请求前置动态状态。`TodoRead` 按需返回完整基线；TodoWrite / TodoAdvance
成功后把增量事件和 `<current-todos>` 紧凑物化投影作为 tool result 追加到 conversation
尾部。投影只含 revision、状态计数和当前 `in_progress` 批次；没有 active batch 但仍有
pending 条目时提示 TodoRead。恢复、fork 或压缩使文件 revision 领先活跃历史时追加一次
TodoSync；历史领先文件则 fail closed。该协议不修改 immutable prefix，旧请求保持为新请求
的完整消息前缀。

### Registered Resources and Capabilities

`Read` 当前承载两类读取：普通路径和轻量资源 URL。普通路径保持本地文件/VFS 后端语义；
轻量资源 URL 由 `ResourceRouter` 按 scheme 分发。`tools/file.rs` 只承担读取入口和 selector
处理，具体 resource 协议由 handler 表达，因此 `skill://` 等协议不归属于 `Read` 工具本身。

```text
Read.path
  ├── registered scheme://... -> ResourceRouter -> ResourceHandler
  ├── ordinary path + VFS     -> ReadOnlyFileSystem
  └── ordinary path           -> local filesystem + editable snapshot
```

内置 handler 当前覆盖：

| Scheme | 来源 | 说明 |
|--------|------|------|
| `artifact://` | session artifacts | 读取被截断工具输出 |
| `skill://` | `CapabilitySnapshot.skills` | 列出/读取当前 capability snapshot 中的 skill |
| `rule://` | `CapabilitySnapshot.rules` | 列出/读取当前 capability snapshot 中的 rule |
| `session://` | current session files | 读取当前 session 摘要、stats、messages、从 conversation JSONL 生成的有损历史 transcript 和 artifacts |

`CapabilitySnapshot` 是 prompt、selected skills、`skill://`、`rule://` 和 prefix fingerprint 的共享能力视图。它在 runtime 构建时由 provider 生成，并挂到 `AgentSharedContext`；子代理继承父代理的 snapshot，避免主代理和子代理看到不同能力集合。能力 snapshot 只描述可被模型看见或按名读取的内容，不负责工具执行、文件编辑或 VFS 数据访问。

selected skill 的 SKILL.md 正文是显式外部能力内容，即使没有资源读取 provider 也会进入
`<selected-skills>`。skill index 和 `skill://` 子资源访问由已解析的 `ResourceRead`
binding 提供，当前内置 provider 是 `Read`；具体 scheme 语义仍由 `ResourceRouter` handler
拥有。selected skill、索引和资源读取共享同一份 `CapabilitySnapshot`。

这套拆分的边界是：

- `resources/*` 只做 URL 到文本资源的适配，不重新发现 skill/rule。
- `capabilities/*` 只构建能力视图和依赖 fingerprint，不处理 `Read` selector。
- `prompt.rs` 只消费 snapshot，不扫描文件系统。
- `tools/file.rs` 只选择读取后端并应用 selector，不内联每个 resource 的业务逻辑。

### Read-only VFS hook

VFS 不是新增工具。它是嵌入式 runtime 对普通文件路径读取后端的可选替换，同时也是 surface
解析的 backend 输入：

```text
Read / Glob / Grep
  ├── read_only_fs == None
  │     └── 原有本地文件实现，执行路径和 snapshot 行为不变
  └── read_only_fs == Some(vfs)
        └── ReadOnlyFileSystem
              ├── VfsScope { resource_session_id, agent_session_id }
              ├── 同步 read / glob / grep
              └── 结构化结果 -> mink-core 统一格式化
```

注入链路为 `AgentOptions` → crate-private resolved 配置 → runtime builder → runtime/tool dependencies。未显式设置 `resource_session_id` 时使用当前 runtime session id。子代理复用同一个 `Arc<dyn ReadOnlyFileSystem>`，继承父代理的 `resource_session_id`，但使用自己的 `agent_session_id`，从而共享同一知识库作用域并保留调用方身份。

VFS 只接管普通路径。`artifact://`、`skill://`、`rule://` 和 `session://` 仍先走资源读取路径。Grep 对 registered resource 直接搜索 handler 返回的文本，不要求暴露底层物理路径。虚拟路径使用 POSIX 分隔符和词法规范化，拒绝越过虚拟根目录。Glob/regex 请求由工具层先校验，后端返回结构化路径或匹配行，`mink-core` 统一输出格式和 100KB 搜索输出保护。请求中的 `max_files` / `max_results` 是后端契约，后端必须自行遵守；核心不提供第二套 VFS 搜索实现。

虚拟文件是只读资源，不创建 Hashline snapshot。因此 VFS runtime 不向模型暴露 `Edit`；
若 `enabled_tools` 显式包含 `Edit`，启动会因缺少本地 editable snapshot provider 而失败。`Write` 仍是
本地文件操作，不会修改 VFS 内容。具体数据库适配不进入核心依赖；`crates/mink-core/examples/redb_vfs.rs`
展示了按 `resource_session_id` 分区、惰性范围扫描的 redb 后端。

### UI

| 文件 | 职责 |
|------|------|
| `crates/mink-core/src/runtime/events.rs` | `AgentEventStream`、`EventSink`、结构化工具事件与 `StatsSnapshot` |
| `crates/mink-cli/src/ui/engine.rs` | REPL 同步渲染 |
| `crates/mink-cli/src/ui/replay.rs` | REPL replay |
| `crates/mink-cli/src/tui/mod.rs` | TUI 入口和事件循环，包括冻结图片能力读取与后台剪贴板结果轮询 |
| `crates/mink-cli/src/tui/display.rs` | CLI 事件投影 -> `TuiSignal` 适配 |
| `crates/mink-cli/src/tui/signal.rs` | `TuiSignal` reducer |
| `crates/mink-cli/src/tui/state.rs` | 共享 transcript、Full viewport/click state、Inline committed state、Plan/Todo/Artifact、子代理状态和待发送粘贴图片 |
| `crates/mink-cli/src/tui/input.rs` | 键盘、粘贴、历史、详情页滚动和命令输入 |
| `crates/mink-cli/src/tui/clipboard.rs` | 剪贴板图片读取（macOS `osascript`）与 PNG 结构/限额校验 |
| `crates/mink-cli/src/tui/attachments.rs` | session `attachments/` 内容寻址暂存（SHA-256 命名、原子写、去重） |
| `crates/mink-cli/src/tui/command.rs` | slash command 解析 |
| `crates/mink-cli/src/tui/render.rs` | 渲染 facade 和布局 |
| `crates/mink-cli/src/tui/render/*` | content/detail/input/status 子渲染器 |
| `crates/mink-cli/src/tui/markdown.rs` | Markdown facade |
| `crates/mink-cli/src/tui/markdown/*` | normalize、block、inline、table、diff、types、util |
| `crates/mink-cli/src/tui/replay.rs` | TUI replay（粘贴 marker 回放时压成 `[image]`） |

## 决策与拒绝的替代方案

- **无依赖注入框架**：组装只在 `runtime/context_build.rs` 一个显式函数完成；测试经同一路径构建。拒绝全局服务定位器与 builder 框架。
- **三套事件词汇并存**：`EventLog`（审计/领域事件直接落盘）、`AgentEventKind`（turn 运行时流）、`TuiSignal`（UI）；只对重叠转换（Display → EventDisplay/TuiDisplay）做穷尽保真。拒绝"唯一发射源"式合并与通用消息总线。
- **配置前端未统一为共享 patch**：四个前端语义差异（None vs 空列表、map 合并、层级覆盖）已由矩阵与测试钉住；暂无证据表明收敛能减少映射点。若跨端漂移频发再引入 additive overrides。
- **锁中毒逐类处理**：可重建缓存恢复；权威状态 fail closed；不可错的辅助函数显式 panic。拒绝"一律 into_inner()"机械恢复。
- **事件流不做有界 channel**：生产者等待容量会让 outcome-only 消费者死锁；改为进度事件生产者侧字节预算 + outcome 主动排空。丢弃 stream 仍等于取消 turn。
- **工具声明保留 JSON schema + metadata 双源汇合**：catalog 加载时 1:1 校验；explicit-only 由声明携带，不做代码生成。
- **registry 租约/操作层未整体拆分**：活动 runtime 与租约共享私有字段集，拆分为跨文件 impl 只增加可见性调整而无行为收益；仅抽出 summary/usage 汇总。
- **fail-closed 用 session 闩锁而非重试**：发布后失败重读验证/重同步，无法恢复即闩锁并要求重启，避免在未验证状态上继续。

## 关键不变式

- 每个用户输入开始时重置 StormBreaker、decision cooldown 和 interrupt；belief 按 `decay_per_input`（默认 0.6）衰减而非硬重置。
- 同一用户输入的 tool_use 内循环可重复压缩（每次必须严格降低请求估算，压不动即 fail-closed；不设次数上限）；PlanConfirm / PlanClear 不强制压缩。
- `conversation.jsonl` 是完整的 append-only 消息历史；压缩只推进 `context-state.json` 中的投影边界。
- JSONL 续写先处理未换行尾部：完整 JSON 补换行，半截 JSON 截断后再以单缓冲区追加新记录。
- `context-state.json` 通过同目录临时文件和 rename 原子替换，内存状态只在替换成功后更新。
- `ConversationStore` 缓存由 `start + lines` 组成，只保留 `active_start` 之后的活跃后缀；压缩提交成功后同步裁剪缓存。
- 正常 turn、压缩和最终回复提取不加载完整历史；恢复时流式解析并校验 JSONL，但只保留并缓存活跃后缀。
- 投影边界必须位于完整历史内，并且不能拆开 tool call/result 协议。
- 所有压缩统一调用 LLM 摘要；摘要以唯一 internal user `<compacted-summary>` checkpoint 投影，不修改 immutable system/tools prefix。
- 摘要输入优先复用上一 Agent 请求的 system/tools 与历史公共缓存前缀（backend 声明 `cache_projection`），无法证明边界或超预算时按 reduction 配置降级；auto 压力以最近同模型、同指纹、同投影代际的 provider prompt usage 校准，preflight 保持保守本地估算，`CompactionCheck` 事件记录压力来源。
- Plan 状态变更以 append-only transition 持久化在 conversation 尾部（confirmed/cleared 内部 user 消息），压缩后活动 `plan.md` 以唯一的 `<active-plan-checkpoint>` 投影，PlanClear 后移除；文件变更与 conversation 追加由 `plan-transaction.json` 可重放 journal 协调，两者都不进入 immutable prefix。
- Todo 权威完整快照保存在 `todos.json`；conversation 尾部追加增量事件和当前 active batch 的紧凑物化投影，不进入 immutable prefix。
- `TodoWrite` 和 `TodoAdvance` 各自依赖 `TodoRead`，并使用最高可见 revision 与稳定 ID；stale revision fail closed。
- TodoWrite 只新增 `pending` 条目、删除条目或替换正文，TodoAdvance 只转换进度；批量更新通过同目录临时文件和 rename 原子提交，持久化成功后才更新内存。
- 文件 revision 领先活跃历史时追加一次 TodoSync，历史 revision 领先文件时 fail closed；同一 active batch 允许多个 `in_progress` 项。
- 一个 session 的 `TodoStore` 由单个 runtime 独占写入；不支持多个 runtime 并发写同一 `todos.json`，也不观察运行期间的外部热编辑。
- 压缩请求使用 turn/编排器传入的活动真实模型名和别名，并通过 runtime 注入的共享 `LlmBackend` 发送。
- 子代理启动时使用从父配置克隆并写入当前活动模型的 child config，LLM backend 复用父 runtime 实例。
- 压缩阈值、响应预留、热尾部和摘要输出预算来自显式配置，不根据上下文窗口推断策略。
- 开启输入降噪时，只精简摘要请求中的 thinking、工具参数和工具结果；完整历史和热尾部保持原样。
- Agent JSONL、Python SDK 和 Rust runtime 暴露并映射同一组上下文压缩参数；runtime 在创建 session 前统一校验有限窗口的 reserve、tail、摘要输出和主请求输入预算关系。
- `max_context_tokens=0` 禁用 auto/preflight 压缩和请求预算上限，但保留手动压缩；真实 context overflow 在无可见输出时按严格缩小规则重复恢复，共享 round 绝对期限且不设次数上限。
- 子代理始终使用父 session 下的 isolated home；fork 在 runtime 初始化前克隆完整 session 状态并重置身份与遥测文件。
- `ImmutablePrefix` 变更必须通过 prefix manager / invalidate 路径。
- `ConversationStore` append 时保持活跃后缀缓存一致；显式完整历史读取是一次性读盘操作，缓存继续保持为活跃后缀。
- `ToolRunner::format_tool_result()` 是工具输出进入 LLM/UI 前的最大字节保护。
- `ModelToolSurface` 在 session/prefix 构造前统一决定 schema 可见性；`ToolRunner::execute_all()` 在 StormBreaker 前再次校验同一 resolved surface，形成执行层纵深防线。
- 超长工具输出必须保存为当前 session artifact；初始化从索引最大序号继续，正文使用独占创建，恢复和 fork 后不得覆盖旧 artifact。
- `Read` 本地非 raw 输出记录 snapshot；raw 和 immutable resource 不生成可编辑 snapshot。
- registered resource 必须先于 VFS 处理；未知 URL-like scheme fail closed，不落入普通路径或 VFS。
- 未注入 VFS 时，`Read` / `Glob` / `Grep` 必须继续执行原有本地实现；VFS 分支不得改变本地路径语义或测试。
- prompt skill index、selected skills、`skill://` 和 `rule://` 必须来自同一 `CapabilitySnapshot`；`ImmutablePrefix` 的依赖 fingerprint 同时包含 capability snapshot、tool surface、provider bindings 和 active prompt workflows。
- 虚拟 `Read` 永远不生成可编辑 snapshot；VFS surface 隐藏 `Edit`，`Write` 仍只针对本地文件系统。
- VFS 后端必须使用 `resource_session_id` 隔离数据；`agent_session_id` 只标识具体调用代理。
- Hashline Edit 必须从 session snapshot 解析 tag；stale 锚点仅在唯一映射且共享偏移时恢复。
- Replace Edit 默认要求唯一候选；多个高置信度候选必须 fail closed。
- AgentEvent 投影必须保留 tool call/result 的调用 ID、状态和 presentation。
- TUI 输入 cursor 必须落在 UTF-8 char boundary。
- TUI 初始化从当前 session 的 Plan/Todo 状态文件建立详情基线，实时 presentation 在该基线上更新。
- Full TUI 使用应用内完整 transcript、mouse capture、click map 和可逆折叠。
- Inline TUI 已完成消息只写入原生 terminal scrollback 一次；只有连续 sealed 前缀可以推进 committed 边界。
- Inline 空闲状态保留最后一个 sealed item，并使用 scrolling region 提交更早的稳定前缀；
  新工作开始后才提交该尾部。
- Inline 进入详情页时保存主视图 terminal，退出 alternate screen 后恢复同一对象并重绘，
  不创建额外 inline viewport。
- Full 与 Inline 共用结构化卡片、Markdown 和自动折叠策略；Inline 主视图不启用 mouse capture。
- 含 Artifact 元数据的折叠卡片必须保留首个 `artifact://ID`，详情读取保持 256 KiB 上限。
- Plan、Todo 和 Artifact 详情使用扣除水平 padding 后的内容宽度生成可视行，垂直滚动范围基于
  折行后的行数。
- 子代理详情使用稳定 `session_id`，不要回退到裸 `line_idx` 作为视图主键。

## 不变式（Invariants）

代码中维护以下不变式：

| 不变式 | 位置 | 违反后果 |
|--------|------|---------|
| compact 后 messages 必须刷新 | `agent/turn.rs`、`agent/compactor.rs` | LLM 发送过期数据 |
| compact 后必须重建消息投影 | `agent/compactor.rs` | LLM 发送完整冷历史或过期边界 |
| 压缩摘要只能进入动态消息投影 | `session/compaction.rs` | prefix cache 因摘要变化失效 |
| Todo 文件原子提交后才能更新内存 revision | `session/todo.rs` | 文件与运行时状态分叉，stale write 失效 |
| Todo 状态只通过成功工具结果或 TodoSync 追加 | `session/todo.rs`、`tools/todo.rs`、`agent/turn.rs` | 前置投影破坏消息前缀，或文件与上下文 revision 分叉 |
| 输入降噪只能修改摘要请求 | `session/compaction_input.rs` | 完整历史或热尾部发生信息损失 |
| 压缩使用调用方活动模型和共享 backend | `turn.rs`、`orchestrator.rs`、`session/compaction.rs` | 模型切换后摘要发往启动模型或绕过宿主 backend |
| 子代理启动配置继承当前活动模型 | `turn.rs`、`sub_coordinator.rs`、`sub_executor.rs` | 模型切换后 child 请求退回父启动模型 |
| 所有嵌入入口映射完整压缩参数 | `sdk_protocol.rs`、`sdk_adapter.rs`、`mink_agent/__init__.py` | 私有小窗口模型只能依赖外部 TOML |
| runtime 在 session 创建前校验上下文预算组合 | `config.rs`、`runtime/builder.rs` | 首次请求才因零输入预算或不可压缩热尾部失败 |
| StormBreaker 窗口每用户输入重置 | `agent/turn.rs` | 跨意图抑制误判 |
| BeliefTracker 每用户输入衰减、ToolSignalProcessor/DecisionEngine 每用户输入重置 | `agent/orchestrator.rs`、`agent/turn.rs` | 跨意图信号累积误升级 |
| 同一用户输入的压缩不设次数上限，但发布前必须净收益达标（auto ≥10%、强制严格下降） | `agent/turn.rs`、`agent/compactor.rs` | 同一投影上重复摘要、死循环或发布越压越大的摘要 |
| PrefixManager 校验完整依赖 fingerprint，漂移时重建 ImmutablePrefix | `agent/prefix.rs`、`session/prefix.rs` | 缓存偏移不可检测 |
| Plan/SubAgent 延迟结果完成并执行大小保护后才能采集信号 | `agent/turn.rs`、`tools/runner.rs` | 信号观察占位结果或未保护正文 |
| store 只缓存活跃后缀，append 增量更新 | `store.rs` | 长 session 内存持续增长或读盘性能下降 |
| compact 提交后按 active_start 裁剪 store 缓存 | `compaction.rs` | 冷历史继续常驻内存 |
| 压缩不删除 conversation 历史 | `compaction.rs` | session 无法完整恢复或重放 |
| artifact 序号恢复且正文独占创建 | `artifacts.rs` | fork/恢复后覆盖历史 artifact |
| 图片能力在 session 初始化冻结；恢复/切换按能力指纹兼容门控 | `capabilities/model_capabilities.rs`、`runtime/context_build.rs`、`agent/orchestrator.rs` | 升级或改配置后旧视觉会话被拒绝启动 |
| conversation 只存 `image://` 引用，请求时才物化；每个引用只完整发送一次 | `llm/image_projection.rs`、`session/store.rs` | 重复 base64 放大请求体或缓存缺失时谎报"已附加" |
| 图片对象内容寻址、独占创建、读回 digest 校验；子代理不继承父缓存 | `session/image_cache.rs`、`agent/sub_executor.rs` | 覆盖/篡改对象或 fork 后引用损坏 |
| 图片批次配额从 0 计数且在物化层重复校验；超限本轮失败、历史降级 | `tools/runner.rs`、`llm/image_projection.rs` | 导入或损坏会话绕过图片预算 |
