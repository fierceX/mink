# 工具与能力

> 更新日期：2026-10-04

surface、资源、提示词与能力解析。

## Edit 协议绑定

Edit 是 runtime 配置变体，不是单一 schema 的运行时猜测。`ModelToolSurface::resolve()` 根据
`EditMode` 只物化 Hashline 或 Replace 中的一种 schema；相同配置重复构建保持字节稳定，
切换模式则改变 schema 和 immutable prefix fingerprint。提示词工作流通过互斥语义能力
`HashlineEdit` / `ContentReplaceEdit` 选择，并与 executor 使用同一份最终配置。

Hashline 维护 session 内完整文本历史、seen-lines 和跨调用剪贴板。历史边界是 30 个路径、
每路径 4 个版本、全局 64 MiB；stale 恢复只接受所有锚点唯一映射且共享一致偏移的情况。
Replace 不依赖 snapshot，以 exact 和归一化行窗口 fuzzy 匹配处理有限格式差异，但唯一性优先于相似度：高置信度候选
多于一个时仍拒绝。两种模式都保留现有 approval、路径安全、写入大小、artifact、信号系统和
串行 mutation 边界。Block Hashline 操作明确不支持，也不引入 tree-sitter。

## 工具执行模型

工具执行模型分为三层：模型可见的 surface（`enabled_tools` → `ModelToolSurface` →
resolved capabilities）、执行门禁（surface 校验 → StormBreaker → ToolExec dispatch）、
结果格式化（大小保护 → artifact → 双通道）。surface 与语义能力的完整设计见
[工具与能力](tools-and-capabilities.md)；
工具参数、执行流程和结果协议见 [工具参考](../reference/tools.md)。

### 关键不变式

- `enabled_tools` 是唯一工具启用输入；`ModelToolSurface` 在 session/prefix 构造前
  一次解析，`ToolRunner::execute_all()` 在执行前校验同一 surface
- 只读工具按连续批次并发执行；写入、执行、控制和 SubAgent 工具按调用顺序串行执行
- 工具结果统一经过 `format_tool_result()`：`tool_result_max_bytes` 截断 + artifact 落盘、
  Bash noise filter、Read/Write 行数摘要、Edit 新 header + diff
- 结果双通道：`content`（UI/默认 tool_result）与 `conv_content`（非空时优先进 LLM）
- 同步只读 VFS hook（`ReadOnlyFileSystem`）只替换普通路径后端，不注册新工具；
  `artifact://`、`skill://`、`rule://`、`session://` 不进入 VFS
- 工具执行失败返回 `Error:` 前缀的结构化消息，不 panic

## 系统提示词构建

`prompt.rs` 协调 `PromptDocument`，但不再拥有具体工具清单。提示词按所有权分层：

```
Core sections                 ← 通用行为，不含具体工具名
MISSION core overrides        ← 仅允许覆盖 allowlist core，标记 ExternalOverride
Tool prompt fragments         ← 仅随对应 active tool 加载
Capability workflow packs     ← 由正向事实求值并从 provider bindings 渲染
Rules/instruction/skills      ← 外部内容，不纳入生成内容工具引用保证
Output language               ← core
```

Plan 不参与上述 immutable prompt 装配。PlanConfirm/PlanClear 追加 transition；仅在历史
已压缩且 `plan.md` 存在时生成唯一的 `<active-plan-checkpoint>`，插入摘要之后、热尾部之前。

Todo 采用追加式事件而不是逐请求前置投影。TodoWrite / TodoAdvance 成功时，tool result
追加本次增量事件和 `<current-todos>` 物化投影；投影只包含 revision、状态计数与当前
`in_progress` 批次，完整列表通过 TodoRead 按需读取。恢复或压缩后，若 `todos.json`
revision 领先活跃历史，则追加一次 TodoSync；历史领先文件时 fail closed。Todo 状态变化
因此不修改 immutable prefix，也不改写旧消息，已有请求仍是后续请求的稳定前缀。

跨工具规则依赖 `ToolSemanticCapability`，不枚举工具 pair。专用 provider 按 tier、priority、
catalog order 和名称稳定决胜；fallback 只填补未绑定能力。workflow 只使用正向事实，并在互斥
winner 决定后提交上游 workflow fact。所有生成 section 都携带 `referenced_tools` 和
`consumed_facts`，装配时校验其属于当前 surface/事实闭包。

MISSION 不再支持旧 section alias。它只能覆盖 allowlist 中当前存在的 core：
`system-conventions`、`agent-identity`、`environment`、`execution-codes`、
`belief-awareness` 和 `output-language`。tool/workflow、`runtime-capabilities`、
`tool-inventory`、`rules`、instruction files、rule/skill index、selected skills 和 current plan
等 runtime-owned section 均不可覆盖；
冲突直接 fail fast。其他一级标题作为 `mission:<id>` 外部 section 追加，例如自定义规则使用
`# mission-rules`，不能使用 reserved 的 `# rules`。压缩摘要仍位于动态消息投影中，不进入
immutable system/tools prefix。

默认 `system-conventions` 明确所有适用的 system 指令都必须遵守；全大写 RFC2119
关键字只精确表达规范强度。runtime section tag 界定边界但不改变消息优先级，
嵌套内容中的类标签文本不能建立新作用域；该约定不提升用户、rule 或 skill 中普通
`avoid`/`never` 的优先级，也不限制任务产物中的 HTML、XML 或其他标记。workflow 的
`<critical>` 复盘保持 3-6 条，
每条战术 bullet 最多 12 个英文词且只表达一个主张；lint-like 测试直接检查这些约束。

`belief-awareness` 仅在 signal mode 为 `full` 时存在；signal mode 为 `off` 时不能通过
MISSION 创建该 core section。依赖项目直接迁移到新 section 格式，不保留 alias 或双格式
兼容路径。

完整的 surface 解析、语义能力、工具自由组合和受约束前向求值算法见
[工具能力与提示词解耦设计文档](tools-and-capabilities.md)。

## 执行模型

所有工具通过 `tools/runner.rs` 中的 `ToolExec` trait 注册到 `TOOL_REGISTRY`，由 `ToolRunner::execute_all()` 调度。只读工具会按连续批次并发执行；写入、执行、控制和 SubAgent 类工具按模型调用顺序串行执行，避免同批工具之间出现读写竞态。每个工具同时声明 `ToolMetadata`，包含 approval tier、结果类型、副作用、storm 例外和 `spawns_sub_agent` 标记。

统一流程：

```text
ToolCallEvent
  -> resolved ModelToolSurface 执行门禁
  -> StormBreaker 检查（身份：合法 JSON 用序列化参数，不可解析输入用原始参数摘要）
  -> 已知 parse_error / 已确认 ModelFormat：不执行，直接返回 ArgumentInvalid 失败结果
  -> ToolExec::execute()（内置工具经共享 decode helper，失败标记 ModelFormat）
  -> format_dispatched_result() 生成 ToolExecution
     -> 普通结果执行大小保护、Bash noise filter、Read-Write summary、Edit conv_content
     -> Plan/SubAgent 结果标记为待定稿
  -> 工具阶段原地完成 PlanCommand 交接；SubAgentCoordinator 完成延迟工作
  -> finalize_deferred_results() 对延迟结果执行大小保护
  -> SignalCollector 只观察最终 ToolExecution.status；Command 正文只用于诊断 regex
     （ModelFormat 不产生信号，只单独计数）
```

OpenAI SSE parser 在生成 `ToolCallEvent` 前合并碎片化 arguments，并要求输入是 JSON object。
参数不可解析时不再执行词法修复：候选带 parse error 与原始参数摘要交给模型重发（失败结果
`Failed(ArgumentInvalid)`，来源 `ToolFormat(ModelFormat)`），既不会静默跳过，也不会把占位 `{}`
当成功调用执行；缺名/缺 ID/重复 ID 的候选整批丢弃并反馈一次内部诊断。Scavenge 回收的候选调用也
通过同一个 `build_tool_call_event()` 结构化校验；DSML 参数头必须完整包含 `>`，缺失时生成降级候选和错误反馈，不执行部分参数。Runner 接收结构化 `input_json`，内置工具
经共享 decode helper 从 `input_json` 反序列化参数（`deny_unknown_fields` 不变）；参数不匹配时
返回 `Error: tool execution failed: invalid tool arguments: ...` 失败结果，不 panic、不放宽 schema。

工具结果有两个内容通道：

- `content`：工具层截断/过滤后的展示内容，会进入 UI 和默认 LLM tool result。
- `conversation_content` / `conv_content`：工具自定义给 LLM 的精简内容。非空时优先写入 conversation。

默认 `tool_result_max_bytes` 为 `100000`，可通过环境变量 `TOOL_RESULT_MAX_BYTES` 调整。

当工具输出超过上限时，完整输出会保存到当前 session 的 `artifacts/` 目录，工具结果中追加 `artifact://<id>`。可用 `Read` 按需读取，例如 `artifact://bash-0001:1-120`。ArtifactManager 从已有 index 的最大序号继续分配，并以独占创建写入正文；恢复 session 或 fork 后不会覆盖旧 artifact。

`Read` 当前是轻量资源的内置调用 provider。具体协议由 `ResourceRouter` 和各 scheme
handler 拥有，不能把 `skill://`、`rule://` 等协议视为 `Read` 工具自身合同：

- `artifact://<id>`：读取被截断工具输出。
- `skill://list` / `skill://list/all` / `skill://<name>` / `skill://<name>/<relative-path>`：通过当前 `Read` provider 列出、诊断或读取可用 skill；列表、正文和子资源读取来自同一 capability snapshot。`skill://all` 是 `skill://list/all` 的兼容别名；只有 filesystem-backed skill 支持 `<relative-path>` 子资源。
- `rule://list` / `rule://<name>`：列出或读取可用 rule。
- `session://current`：读取当前 session 摘要。
- `session://current/stats`：读取当前 session stats JSON。
- `session://current/messages`：读取最近 40 条 conversation 摘要。
- `session://current/messages/all`：读取全部 conversation 摘要。
- `session://current/history`：读取从完整 `conversation.jsonl` 生成的有损 transcript；省略 thinking 和完整工具结果正文。
- `session://current/artifacts`：列出当前 session artifacts。
- `session://current/todo`：读取当前 session Todo 快照（与 TodoRead 同源）。
- `session://current/plan`：读取当前 session 计划状态与内容（草稿/已确认/无，与 Plan 工具同源）。

这些资源都支持同样的行 selector，例如 `session://current/messages:1-20`。

### 嵌入式只读 VFS

嵌入式 runtime 可通过 `AgentOptions::with_read_only_file_system()` 注入同步
`ReadOnlyFileSystem`，替换普通路径上的 `Read`、`Glob`、`Grep` 后端。该机制不注册新工具，也不修改三个工具的 schema。

- 未注入时严格执行原有本地文件代码路径，包括本地路径解析、`ignore` 遍历和 Read snapshot。
- `artifact://`、`skill://`、`rule://`、`session://` 不进入 VFS，继续使用已有资源实现。
- 每次调用收到 `VfsScope { resource_session_id, agent_session_id }`。前者用于数据库分区，后者标识当前主代理或子代理。
- 未指定 `resource_session_id` 时默认使用 runtime session id；子代理继承父代理的 resource scope，但拥有自己的 agent session id。
- 虚拟路径按 POSIX 规则规范化，拒绝 `..` 越过虚拟根目录和 NUL 字节。
- 虚拟 Read 不生成 editable snapshot，因此 VFS runtime 不向模型暴露 `Edit`；`enabled_tools` 显式包含
  `Edit` 时启动失败。`Write` 仍操作本地文件，不修改 VFS。
- Glob/Grep 后端返回结构化结果。glob/regex 校验、文本格式和 100KB 搜索输出保护由 `mink-core` 保持。
- 后端必须自行实现 `glob` / `grep` 并遵守请求中的 `max_files` / `max_results`；`mink-core` 不提供第二套 VFS 搜索实现。

虚拟非 raw Read 输出示例：

```text
[read-only virtual file: knowledge/refunds.md]
1:# Refunds
2:Refunds are reviewed within two business days.
```

数据库适配由宿主应用实现。`mink-core` 不依赖具体数据库；
[`crates/mink-core/examples/redb_vfs.rs`](../../crates/mink-core/examples/redb_vfs.rs)
提供按 `resource_session_id` 隔离并惰性扫描的 redb 完整示例。

工具审批模式由 `--config $'[tools]\napproval_mode="write"'` 或 `.minkrc` 的 `[tools]` 配置控制：

| 模式 | 自动允许 | 阻止/等待审批 |
|------|----------|---------------|
| `yolo` | Read / Write / Exec | 无，默认 |
| `write` | Read / Write | Exec |
| `always-ask` | Read | Write / Exec |

当前版本还没有交互式审批 prompt；必然需要 prompt 的工具不会进入模型可见 surface，历史或
异常调用仍会在 runner 中 fail closed。可用 `[tools.approval]` 为单个工具设置 `allow`、
`deny` 或 `prompt`。

### 工具选择

`enabled_tools` 是唯一工具启用入口，可由 CLI `--enabled-tools`、`.minkrc`、`--config`、
Rust `AgentOptions` 或 Agent JSONL `options.tools.enabled_tools` 设置。显式列表精确选择工具，空列表禁用
全部工具，未设置时使用 catalog 默认集合。`PythonSandbox` 属于 explicit-only 工具，不在
默认集合中，必须显式列出。未知、重复或当前构建 feature 不可用的名称会在创建 session 前
报错。

最终 surface 还会考虑 approval、主/子代理 role、filesystem backend 和硬依赖；schema、
语义能力、workflow、运行时引导和真实执行门禁都消费同一个解析结果。工具启用合同不包含
disable flag 或 sandbox `allow_*` 策略。

### 按需编译（PythonSandbox）

默认的完整 `mink` 终端构建包含 `PythonSandbox` 工具。SDK 精简二进制和最小构建默认不包含
wasmtime，可在构建时按需加入：

```bash
# 最小 mink 二进制（不含 TUI/REPL/PythonSandbox）
cargo build -p mink-cli --release --no-default-features --bin mink

# SDK 精简二进制 mink-core（不含 TUI/REPL/PythonSandbox）
cargo build -p mink-cli --release --no-default-features --features sdk-bin --bin mink-core

# SDK 精简二进制，手动加入 PythonSandbox
cargo build -p mink-cli --release --no-default-features --features "sdk-bin python-sandbox" --bin mink-core

# 完整终端构建（默认含 PythonSandbox）
cargo build --release
```

`mink-core` Rust 发布包只保留 runtime 和工具核心；REPL/TUI 相关依赖位于 `mink-cli`。
`--no-default-features` 可减少二进制体积约 30-40MB。`python-sandbox` feature 可与
`mink-cli` 的 `runtime` 或 `sdk-bin` 组合使用。

## 一、问题、目标与边界

Mink 可以通过 `enabled_tools`、approval、构建 feature、主/子代理角色和文件系统后端改变
模型实际可调用的工具集合。如果 system prompt 仍静态描述全部工具，即使执行层最终拒绝调用，
模型也会被不存在的能力误导。

### 1.1 问题本质

只过滤 tools schema 和拦截执行还不够：

```text
schema filter    决定模型收到哪些工具定义
execution gate  阻止异常或历史 tool call 越界执行
system prompt   仍可能推荐已经不可用的工具
```

常见错误包括：

- Bash 未进入当前 surface，prompt 仍要求运行 Bash；
- Edit 不可用，prompt 仍描述 anchored edit；
- TodoRead / TodoWrite / TodoAdvance 的实际组合与 prompt 描述不一致；
- 子代理仍被提示调用 SubAgent；
- VFS 没有 editable snapshot，prompt 仍承诺 Read → Edit；
- 只启用一个 Python provider，却同时描述宿主和沙箱 Python。

执行层报错只能在误导发生后止损。工具边界必须在 prompt 构建前确定。

另一个问题是组合爆炸。“读取路径”是能力，`Read` 是 provider；“搜索内容”是能力，
`Grep` 或受限 Bash 都可能提供它。直接为 `Read + Grep`、`Read + Bash`、
`CustomRead + Grep` 等工具 pair 编写规则，会随着 provider 数量乘法增长。

### 1.2 设计目标

1. Mink 生成的 prompt 和运行时控制消息只能引用当前模型可调用的工具。
2. schema、执行 gate、runtime policy 和 prompt 使用同一份 resolved surface/capabilities。
3. 工具可以自由增删和组合，不为每种工具 pair 增加兼容代码。
4. 专用 provider 优先，通用 provider 可以 fallback，但不能覆盖专用 provider。
5. 条件能力在实际调用处再次按参数校验。
6. 相同配置得到确定、可缓存、可测试的 bindings 和 prompt。
7. 外部内容和 runtime 生成内容具有明确所有权。

### 1.3 “自由组合”的含义

自由组合是指：

> 任意合法工具 surface 都通过同一组 capability offers 和 workflow requirements 自动求值，
> 不需要为每种工具组合编写分支。

它不表示：

- 自动推断工具的所有理论能力；
- 自动合成新的执行器；
- 解决通用任务规划或全局最优工具选择；
- 允许 workflow 修改工具 surface；
- 绕过 approval、sandbox、safety 或 ToolRunner gate。

## 二、四层解析架构

工具和提示词之间通过四层不可变结果连接：

```text
ToolCatalog
    │  schema + executor + metadata + build availability
    ▼
ModelToolSurface
    │  当前模型真正可见、可请求的工具
    ▼
ResolvedToolCapabilities
    │  当前 surface 提供的语义能力及 provider bindings
    ▼
ResolvedPromptWorkflows
    │  根据正向能力事实激活的跨工具规则
    ▼
PromptDocument
    │  core + tool fragments + workflows + external/session sections
    ▼
system prompt
```

核心约束是：

> Mink 生成内容中的工具引用必须是 `ModelToolSurface` 的子集；跨工具规则只能消费已经解析
> 成功的语义能力事实。

### 2.1 ToolCatalog

`ToolCatalog` 是工具身份的唯一目录，统一关联：

- tool schema；
- executor registration；
- `ToolMetadata`；
- build feature availability；
- 稳定 catalog order。

新增工具不能只修改 schema 或只注册 executor。catalog 必须能发现 schema、executor 和
metadata 不一致。

### 2.2 ModelToolSurface

`ModelToolSurface` 表示本次 runtime 中模型真正可见的工具。解析输入包括：

```text
enabled_tools exact selection
build features
approval policy
primary / sub-agent role
local filesystem / read-only VFS backend
tool hard dependencies
```

解析结果提供 active tools、发给 LLM 的 schemas、隐藏原因和稳定 fingerprint。

surface 是模型可调用工具的唯一事实源。PromptBuilder、ToolRunner 和 RecoveryPolicy 不得分别
从原始 config 再推导自己的工具集合。

### 2.3 可见性与执行的双层边界

```text
ModelToolSurface    防止模型被广告不可用工具
ToolRunner gate     防止历史消息、异常 backend 或伪造调用越界
```

两层使用同源 authorization 语义。执行 gate 是纵深防御，不是第二套工具解析算法。

## 三、语义能力与 Provider Binding

### 3.1 为什么使用语义能力

工具名是实现身份，能力是稳定产品语义。当前能力大致分为：

| 类别 | 示例 |
|------|------|
| 读取与搜索 | `PathRead`、`EditableSnapshotRead`、`ResourceRead`、`ContentSearch` |
| 文件变更 | `FileOverwrite`、`FileEdit`、`HashlineEdit`、`ContentReplaceEdit` |
| 执行与计算 | `ShellExec`、`FocusedVerificationExec`、`DataCompute` |
| 控制流程 | `TodoInspect`、`TodoStructureMutation`、`TodoProgressTransition`、`PlanDraft`、`PlanConfirm`、`PlanClear`、`Delegation` |

`ToolResultKind`、approval tier 和 `mutating` metadata 仍服务执行调度与安全，但粒度不足以推导
跨工具产品语义。

### 3.2 Capability offer

工具通过显式 `CapabilityOfferSpec` 声明自己能够提供什么：

| 字段 | 作用 |
|------|------|
| `provider_tool` | 提供能力的已注册工具 |
| `capability` | 稳定语义能力 |
| `tier` | `Specialized` 或 `Fallback` |
| `priority` | 同一 tier 内的优先级 |
| `available_if` | prefix 构建时可判断的静态条件 |
| `use_scope` | 实际调用参数必须满足的条件 |

能力使用显式注册，不能从“Python 理论上能读文件”或“Bash 理论上能完成一切”自动推导。
自动推导会扩大安全边界，并让 prompt 承诺超过产品支持范围。

### 3.3 静态可用性与参数 Scope

两类条件必须分开：

```text
available_if
  prefix 构建时可判断
  例如 provider feature 已编译、local filesystem backend

use_scope
  工具调用发生时判断
  例如 local non-raw path、registered resource、focused verification command
```

resolver 只绑定静态可用 provider。运行时 policy 使用
`call_satisfies_capability()` 对真实参数再次判断。

scope classifier 必须复用 executor 的结构化解析结果，不能另写近似 regex。例如 Read 的
selector、URL 和 resource 分类必须由执行器与 capability policy 共享。

### 3.4 Provider 决胜

同一能力存在多个 active providers 时使用稳定顺序：

```text
Specialized > Fallback
priority: 高者优先
catalog order: 早者优先
tool name: 字典序最终决胜
```

解析结果 `CapabilityBinding` 包含一个 primary 和零到多个 alternatives。

`alternatives` 只保留当前 surface 中有效的 provider，用于路由说明和诊断。它不是旧实现的
fallback，也不会在 executor 失败后自动切换代码路径。

## 四、Workflow 与受约束前向求值

### 4.1 跨工具规则属于 Workflow

工具 schema 只能描述自己。涉及多个工具的规则属于 workflow：

```text
tool schema       一个工具自身的参数和结果合同
tool fragment     一个 active 工具自身的补充指导
workflow pack     多项语义能力共同成立时的跨工具规则
```

workflow requirement 不写 `Read + Grep`，而写：

```text
search-then-inspect:
  ContentSearch + PathRead

hashline-edit:
  EditableSnapshotRead + HashlineEdit
```

renderer 再从 capability bindings 注入实际 provider。

### 4.2 正向事实

`PromptFact` 只表示已经成立的能力事实：

```rust
ToolCapability(capability)
SpecializedWithFallback(capability)
```

workflow requirement 支持 `All`、`Any` 和 `AllWithAny`（单项能力用单元素 `All` 表达）。

不支持“没有 Grep 才使用 Bash”一类否定前提。fallback 由 provider tier 表达，不通过
“缺少某工具”的条件表达。

当前 workflow 示例：

| Workflow | 需求 | 行为 |
|----------|------|------|
| `search-then-inspect` | `ContentSearch + PathRead` | 从 bindings 渲染搜索后读取 |
| `hashline-edit` | `EditableSnapshotRead + HashlineEdit` | Hashline 编辑协议（互斥组） |
| `replace-edit` | `PathRead + ContentReplaceEdit` | Replace 编辑协议（互斥组） |
| `specialized-provider-routing` | 任一 `SpecializedWithFallback` | 说明专用 provider 与 fallback 的路由 |
| `specialized-mutation-routing` | `ShellExec` 加任一文件变更能力 | 文件创建、覆盖和锚定编辑优先使用专用 provider |
| `python-execution-routing` | host 或 sandbox Python capability | 只描述实际存在的 Python providers |
| `todo-inspection` | `TodoInspect` | 绑定权威快照 provider，约束 revision 和稳定 ID |
| `todo-structure` | `TodoInspect + TodoStructureMutation` | 只描述新增、正文替换和删除协议 |
| `todo-progress` | `TodoInspect + TodoProgressTransition` | 只描述 activate / complete / pause / reopen 协议 |
| `plan-lifecycle` | `PlanDraft + PlanConfirm + PlanClear` | 绑定草稿、确认和清理 provider |
| `memory-recall` | `ContentSearch + PathRead` | 低优先级记忆召回辅助流程 |

新增 provider 通常只增加 capability offer；只有出现新的产品级跨能力协议时才增加 workflow。

### 4.3 算法流程

解析采用单调、受约束的前向求值：

```text
A  解析 surface：enabled_tools / default activation / feature / approval / role / backend
B  收集 offers：丢弃 surface 外 provider 和 available_if=false 的 offer
C  解析 bindings：按 tier / priority / catalog order / name 选择 primary
D  校验 hard dependencies：例如 HashlineEdit 需要 EditableSnapshotRead、ContentReplaceEdit 需要 PathRead，TodoStructureMutation 和 TodoProgressTransition 需要 TodoInspect
E  生成初始 facts：ToolCapability / SpecializedWithFallback
F  前向激活 workflows：匹配正向事实，互斥组先决胜再提交，直到不动点
G  渲染 prompt packs：校验 referenced_tools 和 consumed_facts
```

### 4.4 终止性与互斥

事实集合只增不减：

- capability facts 在进入 workflow resolver 前固定；
- 每个 workflow 最多激活一次；
- 激活的 workflow 进入 `ResolvedPromptWorkflows.ordered`，不会新增 `PromptFact` 参与后续
  requirement；
- dependency graph 禁止未知依赖、自依赖和环。

因此循环最多提交 `W` 个 workflow，必然到达不动点。

同一 exclusive group 中存在多个 eligible workflow 时，必须先按 priority 和 declaration order
选出 winner，再提交事实。如果先提交后淘汰，loser 的事实可能错误激活下游 workflow。

### 4.5 算法选择

工具和 workflow 数量较小，解析采用简单扫描，不引入 Rete、Datalog、SAT/MaxSAT 或成本规划器。
当前算法依赖：

- 正向事实；
- 稳定 provider 决胜；
- 少量互斥组；
- 可解释的参数 scope。

通用规则引擎会额外引入否定、撤回、非确定性和调试成本，因此受约束前向求值更适合当前规模。

## 五、自由组合与组合爆炸

### 5.1 从乘法增长转为加法增长

假设 `PathRead` 有 3 个 providers，`ContentSearch` 有 4 个 providers：

```text
工具 pair 方案：最多需要描述 3 × 4 = 12 种组合
能力方案：      3 + 4 个 offers + 1 个 workflow
```

provider 增加时，pair 枚举接近乘法增长，capability offers 接近加法增长。workflow 只在产品
语义变化时增加，不随 provider 数量复制。

### 5.2 不同 Surface 的自动结果

| Surface | 解析结果 |
|---------|----------|
| Read + Grep | 激活 search-then-inspect，渲染 Grep → Read |
| Read + Grep + Bash | 保留 Bash fallback，额外激活 specialized routing |
| Bash only | 由 Bash 的受限 read/search bindings 渲染，不提及隐藏工具 |
| Read only | 不生成需要 `ContentSearch` 的 workflow |
| Read + Edit（local） | 激活对应的 `hashline-edit` 或 `replace-edit`（由 `edit_mode` 决定） |
| VFS 默认 surface | Edit 被过滤，不生成编辑 workflow |
| 单个 Python provider | 只描述该 provider |
| 无工具 | 只说明当前没有 callable runtime capability |

VFS 配置如果通过 `enabled_tools` 显式要求 Edit，surface 解析直接失败，而不是静默删除用户明确要求
的工具。

### 5.3 Hard dependency 与 Workflow requirement

```text
hard dependency
  工具或能力本身不完整，配置必须失败或隐藏

workflow requirement
  某段跨工具指导是否有足够事实成立
```

例如 HashlineEdit 没有 EditableSnapshotRead、ContentReplaceEdit 没有 PathRead 时是能力合同
不完整；search-then-inspect 缺少 ContentSearch 时只是不激活该 workflow，runtime 仍可使用
其他能力。

## 六、Prompt 所有权与按需加载

### 6.1 Section 所有权

Prompt 先构造成 `PromptDocument`。section 来源包括：

| Origin | 说明 |
|--------|------|
| `Core` | 工具无关的稳定行为 |
| `Tool` | 只属于一个 active tool 的 fragment |
| `Workflow` | 由能力事实激活的跨工具 pack |
| `External` | rules、instruction、skill、MISSION 自定义内容 |
| `ExternalOverride` | MISSION 对 allowlisted core 的替换 |
| `SessionState` | current plan 等 session 状态 |

加载规则：

```text
Core
  始终加载，但不得写具体可选工具名

Tool fragment
  仅所属工具在 surface 时加载
  只能引用所属工具

Workflow pack
  仅 requirement 被满足时加载
  provider 名称从 binding 注入

External / SessionState
  按各自来源加载
  不参与 mink 生成内容的工具引用证明
```

关闭工具时，与它相关的 schema、tool fragment 和不再成立的 workflow 会一起消失。

### 6.2 结构化校验

每个 workflow pack 携带：

```rust
RenderedPromptPack {
    content,
    referenced_tools,
    consumed_facts,
}
```

装配时必须验证：

1. `referenced_tools` 全部位于当前 surface；
2. `consumed_facts` 全部位于 resolved fact set；
3. `consumed_facts` 能证明 workflow requirement；
4. section ID 全局唯一；
5. tool section 只引用所属 active tool。

文本扫描只能作为补充 lint，不能替代结构化引用合同。

### 6.3 外部内容与 MISSION

AGENTS.md、MISSION 自定义正文、rules、selected skill 和历史 plan/message 属于外部内容，不做
静默工具名清洗。“外部”只表示所有权，不表示内容天然安全或一定符合当前 surface。

MISSION 不能覆盖 runtime-owned tool/workflow section；普通自定义 section 作为外部内容追加。
具体格式和 reserved section 合同由 [使用手册](../guides/customization.md#mission自定义系统提示词) 定义。

selected skill 正文独立于资源读取 provider，由 `SkillSnapshot.selected` 直接进入 prompt。
skill index 和子资源访问由已解析的 `ResourceRead` binding 提供；具体工具可以提供该能力，
但不拥有 Skill 协议。

## 七、运行时复用、安全与缓存

### 7.1 Runtime policy 复用 Bindings

`DecisionEngine` 只产生结构化 Recovery directive；`RecoveryPolicy` 从 resolved inspection
capabilities 选择当前 provider，并对首个真实调用执行参数级 scope 校验。

详细的信号、阈值和恢复协议见
[信号系统设计文档](recovery-and-signals.md)。这里的关键边界是：

```text
Bash ShellExec 合法
    ≠
Bash 调用满足 FocusedVerificationExec
```

一个调用可以按工具合同合法执行，却不满足某个更窄的 runtime policy。Recovery classifier
不能反向修改普通 Bash 执行合同。

### 7.2 依赖方向

```text
prompt          → resolved tools
runtime policy  → resolved tools
tool runner     → resolved surface

tools 不导入 prompt workflow
renderer 不重新解析 provider
policy 不重新解析 surface
```

这避免 tools ↔ prompt 循环依赖，也避免不同组件得到不同能力视图。

### 7.3 确定性与 Prefix

surface、capability bindings 和 active workflows 都有稳定 fingerprint。fingerprint 使用稳定
顺序和显式版本前缀，不能依赖 `HashMap` 遍历或未承诺稳定性的 hasher。

`ImmutablePrefix` 的依赖 fingerprint 必须覆盖：

- tool schemas / surface；
- capability provider bindings；
- active prompt workflows；
- capability snapshot；
- MISSION、rules、instruction files 和 selected skills 等稳定 prompt 输入。

相同输入必须产生 byte-identical bindings 和 prompt；影响 prompt 的输入变化必须使 prefix
失效。

current plan 不属于 immutable prefix。它在每次请求时从 session 状态投影为独立的动态
system message；状态变化只替换该消息，不保留旧计划，也不使稳定 prefix 失效。

## 八、核心不变式与验证原则

### 8.1 核心不变式

1. `ModelToolSurface` 是模型可见工具的唯一事实源。
2. 语义能力只能来自显式 offer，不能从工具理论能力自动推导。
3. provider 选择必须确定，fallback 不能覆盖 specialized provider。
4. workflow 只消费正向事实，互斥 winner 必须在提交事实前确定。
5. renderer 只能使用 resolved bindings，不能自行重新选择 provider。
6. Mink 生成内容的工具引用必须是当前 surface 的子集。
7. 条件能力在实际调用处必须再次通过参数级 scope。
8. capability 系统不能绕过 approval、safety、sandbox 或执行 gate。
9. Tool fragment 只描述所属工具；跨工具规则必须进入 workflow。
10. 影响 prompt 的 surface、bindings、workflows 和外部输入必须进入 prefix fingerprint。

### 8.2 验证原则

验证必须同时覆盖正向结果和缺失内容：

- schema、surface、执行 gate 和 prompt 使用同一配置矩阵；
- specialized provider 胜出，fallback 只保留为 alternative；
- `available_if` 和 `use_scope` 分别在静态、参数级边界生效；
- workflow dependency 的 unknown/self/cycle fail closed；
- renderer 不能引用 surface 外工具或未成立事实；
- 工具移出 surface 后，与其相关的生成内容必须消失；
- empty surface 不得产生工具建议；
- MISSION 不能覆盖 runtime-owned section。
