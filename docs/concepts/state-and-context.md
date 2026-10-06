# 持久化与上下文

> 更新日期：2026-10-06

投影、原子发布、压缩和状态恢复。

## Session 结构

- **子代理所有权**：批次未完成事实只在 `SubAgentBatch.pending`；每个任务只有一个 channel 发送点；收集循环监听 cancel/绝对 deadline/10ms interrupt tick；`Drop` 负责 cancel+abort。终态由 `SubAgentStatus` 表达，Interrupted 不再映射为成功。
- **turn 装配**：`TurnExecutor::new/new_for_model` 是唯一构造路径，模型、`sub_agent_config` 与 coordinator 一次确定；主请求、压缩与子代理共用 `ctx.llm_backend`。
- **Plan 所有权与交接**：`AgentSharedContext.plan_store` 是 session 生命周期唯一实例；工具阶段原地 take `plan_command` 完成交接，不存在 handler/中转 Vec；启动期 `session/init.rs` 的恢复实例一次性使用后丢弃。
- **信号事实**：生产不在 processor 内累计完整信号副本；测试通过 `result.signals` 或 events.jsonl 的 `type=signal` 事件观察。

- **EventLog 所有权**：文件句柄、当前故障与已处理损失由 writer 线程独占（`WriterState`，无锁）；发送侧只持有队列、`send_lost` 原子与 init 错误锁；已报告损失水位在 `EventLogWriter.reported` 异步锁下推进。



Session 目录保存 conversation、events、metadata、summary、stats 和 artifacts，并按实际功能
生成 compaction、plan、todo 和 usage 状态文件。session 根目录由 `home`、`cwd`、`session_id`
和以下四种 layout 共同决定：

| Layout | `home` 含义 | session 目录 |
|--------|-------------|--------------|
| `project` / `ProjectScoped` | 用户或服务根目录 | `home/.mink/projects/<project_key(cwd)>/<session_id>/` |
| `home` / `HomeScoped` | 用户或服务根目录 | `home/.mink/sessions/<session_id>/` |
| `direct` / `Direct` | Mink session 集合根目录 | `home/<session_id>/` |
| `isolated` / `Isolated` | 当前 session 根目录 | `home/` |

默认入口：

- `mink` 和裸 `mink-core --agent-jsonl` 使用 `project`，保持历史 CLI 行为。
- Python SDK 默认使用 `home`，适合同一个 SDK home 下管理多个 session。
- Rust 嵌入式 `AgentOptions` 默认使用 `isolated`，适合外层服务已经按任务/session 创建独立目录。
- `direct` 适合服务持有一个共享 Mink 根目录，但仍希望 Mink 按 `session_id` 分目录。

以 `project` layout 为例：

```text
~/.mink/projects/<project_key>/<session_id>/
├── conversation.jsonl
├── events.jsonl
├── session.json
├── summary.txt
├── stats.json
├── context-state.json     # 首次提交压缩状态后生成
├── plan.md                # 确认计划存在时生成
├── plan.draft             # 未确认草稿存在时生成
├── plan-transaction.json  # 计划文件变更与 conversation 追加的事务 journal（事务期间存在，结束后移除）
├── todos.json             # 首次成功 Todo 变更后生成
├── inputs.json            # InputInbox 的持久回执、revision 与未应用输入
├── usage.jsonl            # 首次记录 LLM 请求后生成
├── attachments/           # CLI/TUI/Web 共用的内容寻址附件原始字节，随 session 保留
└── artifacts/
    ├── index.jsonl
    └── <tool>-0001.txt
```

`MINK_HOME` 可覆盖 CLI/SDK 的 home 根。`session_id` 是稳定内部 ID；除 `isolated` 外，它通常也是最终目录名。
`isolated` 中 `home` 自身就是 session 目录，`session_id` 仍写入 `session.json` 并用于事件、SDK final 和恢复引用。
`session.json` 保存用户可读的 alias、title、cwd 和时间戳。`--session NAME` 会按 alias、完整 id、id 前缀和 title解析已有 session，匹配不到时创建新的时间戳 session 并把 NAME 规范化为安全 alias。列表和解析路径对损坏的 `session.json` 采用 legacy fallback，不让单个坏 metadata 阻断恢复。`--continue` 会选择当前 layout 下最近修改的 session。

## 会话工作台：安全边界与可恢复输入

导航只收集用户会话：普通及恢复子代理由 Registry 扫描入口按目录名、metadata.id 的 `sub_`/`replan_` 前缀或 parent 字段排除，列表与查找一致。目录身份可覆盖旧子代理元数据缺失或继承的情况；用户 alias 不作为子代理标记。

引导属于正在运行的父 turn，不能打断当前请求、其重试或已接受工具批，也不重置格式窗口、轮次上限、账单 turn 或请求期限，不自动传给子代理。工具调用及结果完整持久化后，在下一请求准备与压缩检查之前顺序消费。正常 Stop 与引导准入共享 Inbox mutex：待处理输入先到则继续当前 turn，关闭准入先到则迟到引导明确失败。取消、永久失败、恢复耗尽、持久化 fault 与轮次上限始终优先结束。

输入状态为 `pending → applying → applied`；停止、失败或重启后未消费的输入转 `unapplied`，只可由用户明确续发；撤回为 `withdrawn`。request ID 与原始内容绑定，重复相同提交只返回既有回执，内容不同冲突。编辑和撤回以 revision 比较，只有 pending/unapplied 可变更，applying 禁止竞争修改。单条文本最多 128 KiB，未消费项最多 32 条。

应用前原子发布 applying，正式消息携带稳定 `_mink.input_id`、turn_id、guidance 与 attachment_ids，持久追加后发布 applied。追加不确定或历史已追加但回执无法发布时闩锁 session。重启检查完整正式历史及预期消息内容，已存在则补齐 applied，不存在则恢复 unapplied，重复或不一致 fail closed；不重复追加正式消息。

界面“已接收”仅表示持久接收，“已加入本轮上下文”仅表示正式历史交接，不宣称模型已遵循。权威终态前所有模型文本均视为已接受的中间回复或暂态候选；废弃候选不转换为正式工具。轮次/过程组只改变展示，不重排事件；重试、错误和引导保留明确边界，最终回复始终可见。

阅读意图按会话独立保存：外层消息身份 + 元素内偏移锚点，内层过程和详情滚动独立，分页前插与高度变化恢复锚点，明确提交或“返回最新内容”才恢复跟随。手动展开优先于展示模式、轮次终态和重连。离开视图或关闭浏览器只断订阅，任务继续；停止与释放 runtime 分别对应 interrupt 与 close。

Web 的点击反馈与运行事实分离：停止请求在途只锁定按钮，权威终态已到达或 generation/turn 已变化时，迟到回执不再写入停止阶段；编辑回执也不能覆盖更高 revision 或正式已应用输入。手机侧栏使用焦点约束与背景 inert，原生新建对话框优先处理键盘；关闭覆盖层恢复入口焦点且不改变对话阅读位置。Reka UI 菜单在外部点击、Esc 或选择时收起，Esc 只关闭当前菜单而不连带关闭手机抽屉。进入窄屏时收起桌面导航，避免转换成自动遮挡对话的抽屉。输入与附件、发送/停止使用同一圆角容器，模型信息移至诊断，快捷键提示收进按需设置面板。图标控件按场景使用 32px（触控 36px），会话行 44px，避免全局放大按钮挤占阅读空间。

目录筛选是本地展示投影，不建设全文历史搜索。项目名称只匹配工作目录末段，项目路径匹配完整目录；会话名称匹配标题、别名与 ID。范围切换保留关键词，清空只清除关键词，筛选不得停止或切换正在查看的任务。

首页是未选择会话的稳定状态：根地址始终打开首页，指定会话链接在加载时不先渲染首页。导航的明确选择优先于迟到的启动目录；离开视图清除地址中的会话身份并关闭详情，不发送停止或 close 请求。

设置与导航分离：顶栏操作菜单打开模态设置，左侧只保留任务导航。外观、过程、换行和布局立即保存，不重新打开会话；Esc 先关闭设置并恢复菜单入口焦点。

Web 复制不能假定安全上下文或 Clipboard API 存在。原生复制失败后尝试选区兼容路径，临时控件必须清理并恢复阅读选区与输入光标；两条路径均失败时报告错误，不显示“已复制”。

工具卡片的主要内容应是可读的调用和结果，JSON 留在“原始参数”中供核对。Replace 展示当次 old_text/new_text 与 all 语义，不冒充 Git diff；Hashline 保留原指令展示。Plan/Todo 以 presentation 的实际内容、revision、计数和变更为准，旧记录保留 Markdown/XML 兼容路径。文件和命令输出即使包含 JSON 也保持原始文本，未知工具状态不着色为成功；文件正文不经通用 JSON 参数解码。

轮次选择在纵向限高菜单中滚动，不横向铺开或让长标题撑宽；Reka UI 保留方向键、Home/End、选择关闭和焦点恢复。

正文的列宽由可用空间决定，长代码和工具正文不得撑宽消息列。默认开启自动换行，宽表格按列宽折行；关闭时保留代码/表格内部横向滚动，页面与对话外层宽度不变。选项同时出现在设置面板和轮次菜单，持久保存，不改复制、发送或文件的原始内容。

小于 768px 时，外层对话向上轻扫使内容前进达到 48px 后隐藏顶栏和输入卡片；下滑、轻点非交互正文、恢复按钮或 Esc 显示。隐藏只改变布局，不卸载输入或改变任务生命周期。焦点输入、附件、待处理/失败回执、停止/恢复/断线优先保持可见；运行态保留独立停止入口。布局、流式更新、分页和内层滚动不得被误判为用户手势，消息锚点和内层阅读状态保持。

Web 诊断与 TUI 状态栏共用统计口径：轮次/请求数、输入（未缓存输入 + 缓存读取 + 缓存创建）、输出、缓存命中率、上下文及上限/占比、信念、Plan/Todo、工作状态及等待时间。命中率取 `floor(缓存读取 × 100 / (未缓存输入 + 缓存读取 + 缓存创建))`，缓存创建属于未命中；无请求或旧记录缺失完整缓存分区时显示未知。generation 快照包含权威 stats 与 activity，正式历史提交不能抹掉当前等待计时。

TUI 的运行中 Enter 直接以当前 turn 身份准入 Inbox，不能排成下一轮或封口当前文本流。回执先显示等待安全边界，正式提交才回显引导；只有匹配 turn 的 Final 结束任务并通知。停止期间保留可编辑草稿，未应用指令持久保留供明确续发或 revision 撤回。含引导的重放以完整正式 conversation 轮次为准，避免只读 events 丢掉引导。

## 内存模型

### 运行时状态分层

运行时状态分为五个部分，各自有独立的生命周期和变更路径：

#### 1. ImmutablePrefix（不变前缀）

`crates/mink-core/src/session/prefix.rs`

承载 system prompt 和工具定义。一旦构建，在 session 期间应保持不变。变更会触发 fingerprint 失效，导致下一次 LLM 调用丢失前缀缓存。

```rust
pub struct ImmutablePrefix {
    system_prompt: String,
    tools_json: Vec<Value>,
    dependency_fingerprint: String,
    fingerprint: String,
}
```

压缩摘要始终以唯一 internal user `<compacted-summary>` checkpoint 投影，不修改 immutable system/tools prefix。

**fingerprint 校验**（`verify_fingerprint()`）：重新计算 system prompt、tools schema 和依赖
fingerprint 的联合指纹并与缓存值比对。校验失败时 `PrefixManager` 丢弃旧前缀并重新构建，不使用
已经漂移的缓存内容。

#### 2. ConversationStore（追加日志）

`crates/mink-core/src/session/store.rs`

JSONL 格式的持久化消息存储。正常运行只有追加操作：

```rust
// 追加（正常路径——O(1)，只检查文件尾部）
Append: repair tail → single-buffer line write → flush → 更新内存缓存

```

**内存缓存**：`cache: RwLock<Option<CachedLines>>`，其中 `CachedLines` 保存全局消息起点
`start` 和对应 `lines`。首次正常请求通过 `lines_from(active_start)` 流式解析并校验 JSONL，
只保留和缓存活跃后缀；append 增量追加到该缓存。压缩状态提交成功后按新的 `active_start` 裁剪缓存，
因此冷历史只保留在磁盘，不随 session 生命周期持续占用运行时内存。

`lines()` 仍可显式读取完整历史，但当缓存已经裁剪时不会用完整结果替换活跃缓存。
`last_assistant_message()` 优先从活跃缓存查找，必要时流式扫描磁盘，只保留最后一条 assistant，
避免 SDK final 或子代理结果收集重新加载完整 session。

续写前只反向扫描最后一条未换行记录：若它是完整 JSON，则先补换行；若它是崩溃留下的半截 JSON，则截断到上一条完整记录。新 JSON 与换行在同一个缓冲区中写入，避免再次制造可被后续记录拼接的尾部。

**消息格式**：

```json
{"role":"user","content":"..."}
{"role":"assistant","content":[{"type":"thinking","thinking":"..."},{"type":"text","text":"..."},{"type":"tool_use","id":"...","name":"...","input":{}}]}
{"role":"user","content":[{"type":"tool_result","tool_use_id":"...","content":"..."}]}
```

assistant 消息使用 content 数组承载多种内容类型（thinking/text/tool_use），而不是扁平字段。这是因为 Anthropic/DeepSeek 的 content block 格式是结构化的。

#### 3. Mutable Session State（可变会话状态）

压缩投影、计划和 Todo 使用独立的 session 状态文件：

- `context-state.json` 保存 `active_start` 和滚动摘要，决定 conversation 的当前活跃投影；
- `plan.draft` 和 `plan.md` 由 `PlanStore` 管理，确认/清除通过 append-only 内部 user
  transition 表达；历史压缩后活动计划投影为 `<active-plan-checkpoint>`；
- `todos.json` 保存 Todo 的权威完整快照，conversation 只追加成功变更事件、紧凑 active
  投影和必要的 TodoSync。

这些状态不进入 `ImmutablePrefix`，也不改写 `conversation.jsonl` 中的既有消息。Plan/Todo 状态文件
通过同目录临时文件、rename 或受控 unlink 提交；持久化成功后才更新进程内状态。

#### 4. VolatileScratch（每轮清除）

每个新用户输入开始时重置的状态：
- StormBreaker 窗口（`tools.reset_storm()`）
- `TurnCompactor` 的同轮压缩标记
- `ToolSignalProcessor`、`DecisionEngine` 和恢复首步守卫

`thinking` 和 `text` 缓冲区则在每次 LLM 流式请求中重新创建，不跨请求复用。

这些 turn 级状态会跨同一用户输入的多次 tool-use 请求保持，但不会泄漏到下一个用户输入。

#### 5. Session Artifacts（会话资源）

`crates/mink-core/src/session/artifacts.rs`

超长工具输出不直接丢弃。`ToolRunner` 在统一结果格式化阶段，如果 `ToolOutcome.content` 超过 `tool_result_max_bytes`：

1. 完整内容写入当前 session 的 `artifacts/<id>.txt`。
2. `artifacts/index.jsonl` 追加 `ArtifactRecord`。
3. 工具结果保留截断摘要，并追加 `artifact://<id>`。
4. 后续可通过 `Read artifact://<id>` 或 `Read artifact://<id>:N-M` 按需读取。

artifact 跟随 session 生命周期，不跨 session 共享。

`ArtifactManager` 初始化时扫描有效 index 记录的数字后缀，从最大序号加一继续分配。
正文文件使用 `create_new` 独占创建；即使存在未写入 index 的孤立文件也只会继续取下一个 ID，
不会覆盖恢复或 fork 继承的历史 artifact。

---

### 共享上下文生命周期表

`AgentSharedContext` 字段按生命周期分类；新增字段必须归入其中一类并登记重置点，不可凭名字猜测。

| 生命周期 | 字段（代表） | 共享/重置方式 |
|---|---|---|
| 构建期冻结 | `config`、`api_url`、`session_layout`、`cwd`/`home`、`capability_snapshot`、`tool_config`、`tool_surface`、`tool_capabilities`、`tool_resolution_context`、`model_capabilities`、`resource_router`、`vfs_scope` | 构建后不变；变更需重建 prefix/session |
| 会话级服务 | `store`、`artifacts`、`todo_store`、`plan_store`（经 `ToolContext` 克隆同一 Arc）、`snapshots`、`stats`、`usage`、`compaction`、`read_memo`、`memo_epoch`、`memo_mutation`、`persistence_fault`、`event_log_writer`、`image_cache` | Arc 共享；同一 session 内唯一；闩锁/epoch 必须跨 Turn/Tool/压缩共享 |
| 跨请求去重 | `warned_image_ids` | **会话级**，不得当作每轮状态清空 |
| 每轮重置 | `interrupt`、`this_turn_image_ids` | `TurnExecutor::reset_local_state` 重置；子代理与父**共享** `interrupt`，子代理局部超时只取消自己的 linked cancel 令牌 |
| 惰性/节流 | `immutable_prefix`、`stream_flush_last`、`event_log_warned` | prefix 失效重建；`stream_flush_last` 为 **context 级**节流（非每轮）；`event_log_warned` 整个会话只警告一次 |
| 关键共享关系（有定向测试） | `interrupt`（父→子共享）、`cancel`（`linked_child_token`：父→子传播，子不波及父）、`memo_epoch`（压缩提交后 bump 对工具可见）、`persistence_fault`（Todo/Plan/Compaction/Turn 共享同一闩锁） | 见 `sub_agent_interrupt_is_shared_and_child_cancel_is_linked`、`memo_epoch_is_shared_and_bumped_by_compaction`、`session_fault_latch_is_shared_with_plan_and_todo_stores` |

分组重构仅在生命周期表能证明减少重复或改善所有权时进行；`warned_image_ids` 与 `stream_flush_last` 不得因命名被误归为每轮状态。

## 上下文压缩

### 显式策略参数

压缩行为由以下显式配置直接控制，不根据上下文窗口推断策略档位：

参数与默认值见[配置参考](../reference/configuration.md#上下文参数)。

自动触发点取“窗口百分比”和“窗口减去响应预留”中较早者。手动、preflight、overflow
会绕过百分比判断，但使用同一个热尾部和摘要实现。`max_context_tokens=0` 禁用 auto/preflight
压缩和本地请求预算上限，但保留手动压缩。

### 非破坏式投影

`conversation.jsonl` 始终保存完整消息。压缩在 `context-state.json` 中提交
`active_start + summary`，模型请求只投影动态摘要和边界后的热尾部。
边界只选择字符串 user 消息或 assistant 消息，禁止从 tool result 开始，从而保持
tool call/result 配对。`context-state.json` 是活跃投影状态的唯一来源；状态损坏或边界超过历史长度时直接报错。
`context-state.json` 使用同目录临时文件写完后 rename，替换成功后才更新进程内状态并裁剪
ConversationStore 的活跃缓存。主 turn、tool_use 循环刷新和压缩评估统一使用
`active_messages()` / `lines_from(active_start)` 构建模型上下文。

### 摘要生成与输入降噪

正常压缩将被折叠的活跃消息送入当前 LLM，并与已有摘要合并：

“当前 LLM”由调用方的活动模型决定：turn 内压缩使用当前 `LlmBackend` 的真实模型名和别名，
手动压缩使用 `OrchActor::resolve_active()` 的结果。摘要请求统一通过 runtime 注入的
`Arc<dyn LlmBackend>` 发送。

```
Merge the conversation turns above with the previous context snapshot.
Use exactly these fields:
Task focus:
Latest request:
Progress:
Tool evidence:
Reflections:
```

摘要请求使用独立的最小 system prompt，不加载主 agent 的 skills、rules、工具定义和 mission
（但 backend 声明 `cache_projection` 时复用主请求的 system/tools 与历史公共缓存前缀
以保留缓存命中）。输出预算由 `context_compact_max_output_tokens` 独立控制。摘要结果统一
作为 internal user `<compacted-summary>` checkpoint 投影，保持 immutable system/tools
prefix；`summary.txt` 仅作为 session 元数据和人工检查缓存。

开启 `context_compact_input_reduction` 时，`compaction_input.rs` 先把被折叠消息转换为紧凑
transcript：用户和 assistant text 保留，thinking 删除，工具参数限制为 1000 字符，工具结果限制为
2000 字符并保留头尾、错误/失败/退出状态和 artifact 证据。该转换只影响摘要请求，不修改
`conversation.jsonl`、热尾部或历史资源。

摘要输出按不透明文本处理。提示词要求固定字段，运行时只校验请求成功、正常 stop 和清洗后非空，
不解析或强制校验字段标题。

### 防护措施

**同轮可重复压缩**（`TurnCompactor::compactions_this_turn`）：auto、preflight 和 overflow 共用同一入口，不设次数上限；每次
提交后重新投影并重估，只要求严格降低请求估算（不降即停，避免在同一投影上重复摘要），且发布前必须过净收益门控（auto ≥10% 净下降，强制路径严格下降）；压无可压时仍以 fail-closed
拒绝发送超预算请求。长任务（尤其嵌入式单输入长任务）因此不再在首次压缩后必然失败。同轮内的 overflow 收缩、其摘要请求与主请求共享 round 绝对期限（`request_timeout_secs`）。

**最后手段切点（`_cut=degraded`）**：当请求已经超出输入预算（`local_tokens > request_input_limit`）而严格切点搜索
返回 0（热尾部目标吞掉整个窗口，或窗口只剩最近两个用户轮次），或严格切点存在但「保留最近两条真实 user」的候选连
「固定前缀 + 派生展示 + 预演 TodoSync + 最小摘要位」都放不下时，退化为「折到最新安全边界之前」：忽略热尾部 token
目标与「保留最近两条真实 user 消息」守卫，把新边界之前的整段折进摘要（当前请求仅以摘要文本保留），而不是直接让本轮
fail-closed。未超预算的 auto/manual 路径不启用该退化，仍按严格规则判定。

**应急 checkpoint（`trigger=emergency`、`_mode=emergency`）**：即使有了退化切点，LLM 摘要本身仍是单点依赖（
摘要输入装不下、调用超时/重试耗尽、输出不合格、候选装不下）。因此在「请求已经超出输入预算」时额外提供一层
**不调用 LLM 的确定性应急投影**：

- 提交前置条件：runtime cancel / interrupt 在入口、入锁后、每次收缩前与提交前各检查一次并映射为
  `Interrupted`；活跃窗口存在未完成的 tool call/result 交换时拒绝提交（协议损坏不得被摘录掩盖）；
  收缩每步必须严格变小（省略标记计入额度），到达地板即返回「无法继续」而不是原地重试。
- 材料优先级固定：有损标记 → 当前用户请求摘录（头尾裁剪 + 省略量）→ 上一份摘要片段 → 最近已完成工具交换的结构化
  事实（工具名 / 调用 ID / 结果头尾摘录，不做展示文本反解）。摘录逐步缩短/删块直到候选投影装进预算；连最小摘录都装
  不下时返回「最小工作空间不可用」，由上层 fail-closed，而不是发送空壳请求。
- 投影纪律：`active_start` 推进到历史末尾（尾部为空）；`conversation.jsonl` 只追加；plan/todo 权威文件与 revision 不被
  改写；提交仍然只经 `commit_state`（原子发布 + generation/epoch/repair-loss 规则不变）。
- 分层与分类：严格切点 → 退化切点 → LLM 摘要 → 应急 checkpoint；摘要侧错误在来源处定型为 `SummaryUnavailable`
  （可转应急）或 `CompactionInterrupted`（映射为 `TurnDecision::Interrupted`）。取消、持久化 fault、正式历史协议损坏
  一律原样传播——应急只替代“上下文装不下”这一种失败，不吞任何安全/耐久性故障。
- auto 摘要失败且原请求仍可发送时不阻断发送；强制循环结束仍超预算或 provider 报 overflow 且常规压缩无收益时才进入应急。

**软额度与动态摘要预算**：`context_compact_tail_tokens` / `context_compact_max_output_tokens` 是目标而非地板。
切点搜索按可用空间收紧热尾部；每次摘要 attempt（含纠错追加之后）都重新估算输入并计算
`S_call = min(S_config, 窗口 − 输入)`，低于最小实用输出（`MIN_PRACTICAL_SUMMARY_TOKENS`）时直接转应急，
而不是发送装不下的请求。配置校验只保留硬约束 `reserve < 窗口`。manual 压缩入口与 turn 共用同一套
请求形状（真实 system/tools）、候选验收与应急回退；应急摘录只列出仍存在于 artifact 索引中的
`artifact://` 引用。provider overflow 时按「固定前缀 + 可变额度折半」给出更小目标，且每次收缩都必须让
请求严格变小（不设次数上限）。plan/todo 的**模型可见派生展示**同样受额度约束
（`derived_display_tokens = 输入预算/8`，头尾保留 + 省略标记）：固定前缀之外，动态 checkpoint 也不再是
无界成本；权威文件、revision 与计数保持真实，`TodoRead` 仍返回完整内容。

**最小收益检查**（`CompactionEngine::evaluate_and_compact`）：auto 触发下，如果压缩节省的 token 不足当前总量的 10%，
跳过压缩，防止小上下文场景下的无意义压缩；强制触发（preflight / overflow）在请求已经超预算时只要真能省（`saved > 0`）就压，
否则「需要的削减量小于 10% 阈值」会变成无法恢复的必然失败。

**Preflight 预算检查**：按转换后的 OpenAI messages、system prompt 和 tools schema 估算输入。
超过 `max_context_tokens - effective_max_tokens` 时反复强制压缩（每轮重估，直到装得下、没有合法切点或不再下降）；
仍然超预算则不发送请求。

**摘要预算检查**：摘要请求按最小 system prompt、原始或降噪后的输入和独立输出预算估算；超出
`max_context_tokens` 时在发送前失败，不依赖 provider 隐式截断。

**Provider overflow 恢复**：如果 provider 在尚未输出文本、thinking 或工具调用前明确返回 context
overflow，则缩小投影并重试；同一 round 可反复收缩，每步必须严格缩小，不设次数上限。收缩、摘要和重发共用绝对期限；没有收益时转应急或明确失败。

## Session 与持久化

### 目录布局

每个 session 有独立的目录：

| Layout | `home` 含义 | session 目录 |
|--------|-------------|--------------|
| `project` | 用户或服务根目录 | `home/.mink/projects/<project_key(cwd)>/<session_id>/` |
| `home` | 用户或服务根目录 | `home/.mink/sessions/<session_id>/` |
| `direct` | Mink session 集合根目录 | `home/<session_id>/` |
| `isolated` | 当前 session 根目录 | `home/` |

CLI 默认使用 `project`，Python SDK 默认使用 `home`，Rust 嵌入式 `AgentOptions` 默认使用 `isolated`。
`direct` 用于一个共享 Mink 根目录下保存多个 session。`isolated` 用于外层服务已经按任务/session
创建独立目录的场景，此时不再追加 `session_id` 子目录。

以 `project` layout 为例：

```
~/.mink/projects/<project_key>/<session_id>/
├── conversation.jsonl   ← 对话消息（逐行追加 JSON）
├── events.jsonl          ← 事件日志（每行一个事件）
├── session.json          ← session 元数据：alias、title、cwd、时间戳
├── summary.txt           ← 压缩后的上下文快照
├── stats.json            ← Token 用量统计
├── context-state.json    ← 首次提交压缩状态后生成
├── plan.md               ← 确认计划存在时生成
├── plan.draft            ← 未确认草稿存在时生成
├── todos.json            ← 首次成功 Todo 变更后生成
├── usage.jsonl           ← 首次记录 LLM 请求后生成
└── artifacts/            ← 超长工具输出
    ├── index.jsonl
    └── bash-0001.txt
```

`project_key` 是当前工作目录路径经过安全转义后的字符串，确保不同项目间的 session 隔离。
`session_id` 是稳定内部 ID；除 `isolated` 外通常也是目录名。`isolated` 的目录名由外层服务决定，
但 `session_id` 仍写入 `session.json` 并用于事件、SDK final 和恢复引用。

### JSONL 约束

**追加**：`append_line()` 使用 `OpenOptions::append`，通过 store 内部写锁串行化。写入前修复
未换行尾记录，然后以包含换行的单缓冲区追加，并同步更新内存缓存。

**读取**：模型上下文使用 `lines_from(active_start)`。恢复已有 session 时流式解析并校验完整 JSONL，
但只保留活跃边界后的消息并缓存为 `CachedLines { start, lines }`；后续 turn 直接复用该后缀。
`lines()` 只用于显式完整历史读取，且不会扩大已经裁剪的活跃缓存。如果文件末尾存在没有换行的
半截 JSONL，会跳过这条不完整记录；已经以换行结束的坏 JSONL 仍按错误处理，避免静默吞掉真实损坏。

**压缩**：不重写 JSONL。`CompactionEngine` 从活跃缓存读取 `active_start` 后缀，选择新的安全边界；
状态文件通过临时文件和 rename 原子替换，成功后同时推进内存状态并裁剪 store 缓存。
session 恢复和重放仍可按需读取全部原始消息；`session://current/history` 提供有损检索视图。

### Session 恢复

`--continue` 模式通过读取最新 session 目录的时间戳来选择最近的 session。`--session NAME` 会先按 alias、完整 id、id 前缀和 title 解析已有 session；匹配不到时创建新的时间戳 session，并将 NAME 规范化后写入 `session.json` 的 alias。解析时也会尝试规范化后的 alias，因此 `feature x` 能恢复 alias 为 `feature-x` 的 session。坏 `session.json` 不阻断列表和解析，会回退到目录名与 `summary.txt`。

恢复时会 replay 最近 10 轮 LLM 响应事件（从 events.jsonl 读取），在交互式终端重新渲染历史对话。

### 事件日志丢失确认

- **状态所有权**：writer 线程独占 `WriterState{file,failure,last_loss,processed_lost}`（无 Mutex/Atomic）；共享的只有发送侧 `send_lost` 原子、`init_error` 锁与测试计数。已报告损失水位由 `EventLogWriter.reported`（async Mutex，兼作 flush 串行化）持有，收到 FlushAck 后才推进；取消/超时的 flush 不消费未报告损失。

`events.jsonl` 由专用 writer 线程串行追加，队列有界（满时阻塞，不静默丢弃）。丢失的确认与报告遵循以下所有权规则：

- **发送侧计数**：writer 线程创建失败、通道断开、入队拒绝由发送侧计入 `send_lost`——不存在的 writer 无法承担统计。
- **writer 计数**：writer 只累计其在处理中实际未能持久化的事件（open/write 失败）为单调 `processed_lost`，并在每个 Flush 屏障返回快照；**writer 不等待调用方确认，也不因等待而停止消费**。事件丢失原因单独记录，不因后续重新打开成功而被清除。
- **确认水位**：flush 调用方串行化；只有实际收到屏障结果的调用方推进共享 `reported` 水位。取消/超时发生在收到结果之前时不推进，因此同一批丢失会在下一次 flush 再次报告。
- **报告语义**：`send_lost + processed_lost > reported` 时 flush 返回错误（含累计丢失数与最近一次丢失原因），并把水位推进到该值（报告一次）；重新打开文件只恢复可写状态，不清零未报告丢失。
- **边界**：`flush` 是通道/处理屏障，不等于 `sync_all` 断电持久化；不引入请求序号或双向确认协议。

### 事件流消费契约与进度预算

- 三种消费方式：持续消费（`recv()` 循环）；只要结果（直接 `outcome()`，**主动排空事件并释放进度字节**，不积累无界积压）；中途放弃（drop stream 按既有契约取消当前 turn，是消费者的显式选择）。
- 进度事件（`Text`/`Thinking`）有 1 MiB 的 pending 字节预算：每个事件按 `max(payload, 128B)` 计费（空 delta 也占用队列槽位，不得零成本）；超限的 delta 仅对 stream 出口丢弃，并以一条可靠 `Info` 通知。**不把 agent 事件流改为 bounded channel**：生产者等待容量会让 outcome-only 消费者死锁。
- 可靠事件（工具调用/结果、stop/error/usage、控制事件）不进预算也不丢弃；其总量由结构约束：工具调用/结果数受 turn 机制限制、payload 受 `format_tool_result` 上限约束。**上游 SSE 生产者队列已改为有界（1024）并用 async send 背压**；observer 通道保持有界（溢出丢弃并告警）。
- 验证：单元（预算边界、空 delta 计费、一次性通知、可靠事件不被丢弃、**stream 不消费时 observer 仍收到全部增量**）+ SSE（满容量背压、丢弃消费者后生产者退出）+ 集成（8 MiB 与 32 MiB 两档慢消费：pending ≤ 1 MiB、dropped > 0、`outcome()` 正常完成）。
- 发送失败回收：仅进度事件能到达发送点（已 reserve）；`tx.send` 失败时按返回事件的 payload 归还本次预留，Info/Stop 等无 progress_len 不误释放；`release` 用 debug_assert 守住“不得超额释放”。
- **范围限定（复核结论）**：本项证明的是“进度事件与上游 SSE 队列有界 + 可靠事件结构有界”；可靠事件突发（大量工具结果）的峰值字节未做定量测量，属 deferred 验证项。

### 关键事件与诊断事件（复核修复）

- `EventLog::is_critical()` 定义契约关键事件：`PrefixSnapshot`、`SignalRollback(Error)`、`SignalReplan(Error)`、`SignalHandover`。这些事件会被读回重建状态（前缀缓存、恢复审计），必须**可靠、有期限**地提交。
- 关键事件走 `log_critical_event`（async）：`AppendCritical` 携带单次写入应答，调用方等待的是 **writer 的真实 open/write 结果**（入队成功不算成功）；等待为异步、有期限（5s，正常回归与生产一致），同时响应 **runtime cancel 与当前轮 interrupt**（`interrupt_current_turn()` 设置的同一标志），不阻塞 tokio worker。
- 等待语义：健康 writer 的提交不被打断（先给 500ms 宽限拿到在途应答）；只有真正停摆的等待才被 cancel/interrupt 提前结束。失败返回给调用方；调用方不得按成功推进（prefix 失败不更新缓存、下一次 `ensure()` 重建；信号恢复失败向上传播）；cancel/interrupt 统一保持 interruption 分类。
- `EventLogWriter::flush` 的应答等待同样受内部期限约束（超时返回错误，不再无限等待 ack），orchestrator 每轮 flush 在 writer 停摆时也不会挂起整个 turn。
- 关键事件同时沿用 `log_event` 的 stream-json stdout 输出：三条入口共用同一个 `emit_stream_json` 实现，避免关键事件从既有协议输出中消失。
- 诊断事件仍走 `send_best_effort`：满队列可见丢弃并计入丢失报告。`log_event` 收到关键事件时兜底走可靠路径并告警，契约调用点应显式使用 `log_critical_event`。
- 反压边界表述精确化：SSE 队列（1024）只限制**事件个数**，不限制单事件字节与解析缓冲；runtime 可靠事件（工具结果等）不计入 1 MiB 进度预算。整条事件链路"内存完全有界"仍不成立。

### shutdown 预算与日志回压

- `shutdown` 的收尾阶段各有明确预算（5s，正常回归与生产一致）：gate/orchestrator、event dispatcher、event log、usage、stats、compaction projection 分别超时并把超时列为失败；**阻塞 IO（usage.flush）经 `spawn_blocking` 移出 async worker**（stats/projection 原本已在 blocking pool），超时只界定调用者等待，**"调用返回"不等于"后台写入完成"**，不谎报完成。
- `EventLogWriter::flush` 用 `try_send` + async 重试至内部期限（30s，入队重试与应答共享起始时刻），不阻塞 async worker；runtime 收尾仍受外层 5s 阶段预算约束。**异步调用方（turn/runtime 的 `log_event`）改用 `send_best_effort`**：队列满时可见地丢弃诊断事件并计入丢失报告，不再阻塞 tokio worker；同步测试仍可用阻塞 `send`。
- 短期限只通过参数注入到停摆/超时单测，不以全局 `cfg(test)` 改写健康路径的预算。延迟 writer 应答、满队列延迟恢复和阻塞 flush 回归验证超过 200ms 后仍按生产预算完成；原有故障单测继续验证有界失败，不忽略错误、不自动重试失败断言。
- 超时后写入线程保持单一所有权：不启动第二个 writer、不接管未完成状态；未完成状态如实上报。
- 证据：paused-writer 测试（队列满时 flush 有界返回、`send_best_effort` 不阻塞且丢失可见、恢复后 1024 条事件不丢不重）；`flush_stage` 与 `flush_stage_blocking` 期限单测（含真实阻塞闭包）；正常 runtime 的真实 shutdown 在预算内完成。端到端"暂停真实 writer + shutdown"及磁盘挂死注入需要生产测试开关，列为 deferred 验证项；本项不宣称已覆盖真实磁盘阻塞的最坏时延。

### 请求取消与未上报用量

- 每个已发出的 backend 请求在请求开始处创建 `UsageGuard`（唯一完成权，不可 Clone）；建流成功移交给 `MeteredStream`，显式失败由 guard 记录 `request_failed`。
- 调用方取消/首事件 deadline 丢弃建流 future 时，guard 在 Drop 中记录一条 `Unreported`（reason 含 `request_cancelled_before_usage`，`attempt_count` 为 1，tokens/费用为 None，不虚构重试次数）——已开始的请求不会从 journal 消失。
- 本地图片投影失败发生在请求进入 backend 之前，不产生记录（不伪装成已发出请求）。
- 压缩（compaction）使用同一 guard：`compaction_interrupted`/`request_failed` 各记一条；流中断由 `MeteredStream` Drop 兜底，保证每个请求最多一条记录。
