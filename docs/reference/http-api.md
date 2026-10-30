# HTTP 与 SSE API

> 更新日期：2026-10-20

持久输入、快照与服务生命周期。

## Web 持久输入与快照协议

所有 session 路由接受 `?project=`。`POST /api/sessions/{id}/inputs` 接受 `{request_id, text, attachment_ids:[], target_turn_id:null|string}`，返回带 input_id、revision、turn_id、status 和 guidance 的持久回执。相同 request ID 与相同原始内容幂等，内容不同冲突；网络结果不确定时先 GET inputs?request_id=... 对账，不自动重复执行。PATCH / DELETE 使用 revision，resume 必须由用户明确触发。旧 `/turn` 保留并转入同一执行路径。

`GET /stream?snapshot=true` 首帧是 `session_snapshot`：generation、stream_sequence、conversation（物理行号 seq）、progress、current_turn、phase、running、inputs、capabilities、resources（Plan/完整 Todo/Artifact）及 diagnostics / activity / last_final。可选 `activity:{work_state,wait_elapsed_secs:null|number,active_sub_agents:[]}` 保留可靠事件推导的工作状态和模型等待时间；diagnostics 的完整 `title_update.stats` 保留轮次/请求数和互斥的未缓存输入、缓存读取、缓存创建分区。快照与订阅在同一发布边界建立。后续 `conversation_committed`、`inputs_updated`、`phase_updated` 和既有事件携带 generation 与 stream_sequence；旧 generation/重复水位丢弃，gap 后重取 snapshot，无需等待运行 turn 空闲。`after_conversation_seq` 定位暂态诊断的历史相邻位置。

Core 的新增 `AgentEventKind::ConversationCommitted {conversation_seq,message}` 经可靠 runtime 流发出，不能与实时候选文本混同。Web stable key 为 `message:{seq}:{block_index}` 或 `input:{input_id}`，正式历史接管 `live:{generation}:{stream_sequence}`（旧流兼容无 generation 的 key）。既有 SSE 模式不发送新增输入/提交/阶段控制事件；CLI 展示忽略 commit 通知，避免重复正文。

附件 POST 发送原始图片字节，返回内容寻址描述 `{id,mime,width,height,bytes}`，GET `/attachments/{id}` 只预览本 session 的传输文件；上传不会生成模型图片块。`/conversation?turns=true` 可按真实非 internal、非 guidance 用户轮次分页，保留完整工具交换；旧行分页参数与默认行为保持兼容。

## 概述

`mink-server` 延续 Mink 的轻量定位：单二进制、低运行时依赖，把同一个 runtime 暴露为 Web 服务——与 CLI/TUI/Python SDK 共享内核，多端行为一致：

- **REST API**：会话管理（列表/创建/删除/打开/关闭/中断）、conversation/plan/todo/goal/artifacts/files 读取，goal 的创建与控制，以及工作台的模型切换与手动压缩（`POST /api/sessions/{id}/model`、`POST /api/sessions/{id}/compact`）。控制请求**同时要求 `goal_id` 与 `revision`**：替换目标的 revision 从 1 重启，只有 id 能区分二者，缺 `goal_id` 返回 400、id 不匹配返回 409。`budget` 的 `soft_token_limit` 是三态补丁：省略字段保持当前上限、JSON 数字设置、显式 `null` 清除；`0`、负数、溢出与错误类型均被拒绝且不改变快照。`resume` 可继续 paused/blocked/budget_exhausted，也用于接起服务重启后恢复出的 active goal：先按容量账本预约许可、再 arm 驱动，由 core 携带租约启动唯一 run；被拒绝的 resume 不启动 run、不发出 provider 请求、也不释放不属于本次尝试的容量。读取响应带 `verifier` 表示该部署是否配置检查器。
- **SSE 实时流**：`/api/sessions/{id}/stream` 转发 core `AgentEvent` envelope（带 `stream_sequence` 传输序号、心跳与 gap 对账）
- **嵌入前端**：Vue SPA 构建产物直接嵌入二进制，单文件分发
- **共享会话布局**：与 TUI/CLI 使用同一 `~/.mink/projects` 目录，终端与浏览器无缝交接

```
┌──────────────────────────────────────────────┐
│ mink-server（单二进制）                        │
│  ├─ REST 路由（axum）                         │
│  ├─ SSE stream（SessionRuntime 广播转发）      │
│  ├─ Session registry（fs2 文件锁 lease）       │
│  ├─ Session runtime（阶段机 + 超时保护）       │
│  └─ 静态资源（嵌入 web dist / 磁盘 ServeDir）  │
└──────────────────────────────────────────────┘
```

## REST API

会话目录扫描与 ID 查找统一排除子代理：目录名或 metadata.id 带 `sub_`/`replan_` 前缀，或 metadata.parent 存在。目录判断在读取元数据之前执行，旧子代理缺失、损坏或继承元数据也不会出现在列表；正常用户会话的 alias 不参与过滤。

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/sessions` | 会话列表（含运行 phase、未处理输入数、最近活跃终态及 tokens/context 汇总） |
| POST | `/api/sessions` | 创建会话；Web 传 `{cwd,draft_id,first_prompt?}`，`draft_id` 是首发送幂等键；无名称时生成唯一内部 alias，不再复用 `unnamed` |
| GET | `/api/workspace?path=` | 仅校验工作目录并返回 `{cwd,project_key,name}`，不创建项目或会话 |
| GET | `/api/sessions/{id}` | 会话状态（open/running、runtime generation、当前 turn、能力、阶段与最近终态） |
| DELETE | `/api/sessions/{id}` | 删除会话（先持有系统文件锁，阻止并发删除） |
| POST | `/api/sessions/{id}/open` | 打开（建立 session lease） |
| POST | `/api/sessions/{id}/close` | 关闭（中断当前 turn、释放 lease；关闭后由 idle reaper 兜底） |
| POST | `/api/sessions/{id}/turn` | 兼容发送消息；可选 `request_id`，相同 ID+内容幂等返回既有回执，不同内容 409；未提供时服务端生成 legacy ID |
| POST / GET | `/api/sessions/{id}/inputs` | 提交任务/引导 / 读取回执与未应用输入 |
| PATCH / DELETE | `/api/sessions/{id}/inputs/{input_id}` | revision 编辑 / 撤回尚未消费输入 |
| POST | `/api/sessions/{id}/inputs/{input_id}/resume` | revision 明确用于新轮次 |
| POST | `/api/sessions/{id}/attachments` | 原始图片字节上传，返回 session 限定描述 |
| GET | `/api/sessions/{id}/attachments/{attachment_id}` | session 限定的图片预览 |
| POST | `/api/sessions/{id}/interrupt` | 中断当前 turn |
| GET / POST | `/api/sessions/{id}/goal` | 读取目标快照（活运行时优先，回退 `goal.json` 的 `current`；默认摘要，`?full=true` 取完整契约）/ 创建目标并启用驱动 |
| POST | `/api/sessions/{id}/goal/{action}` | 目标控制（pause/resume/cancel/clear/budget/approve/reconcile），必须带 `goal_id` + `revision`；响应为流摘要 |
| POST | `/api/sessions/{id}/model` | 切换会话模型，返回解析后的标签与统计快照 |
| POST | `/api/sessions/{id}/compact` | 手动压缩当前上下文 |
| GET | `/api/sessions/{id}/events` | events.jsonl 分页（`from_seq`/`limit`/`tail`/`before_seq`） |
| GET | `/api/sessions/{id}/conversation` | conversation.jsonl 轮次历史（同分页参数；`turns=true&max_bytes=` 限制单批体积） |
| GET | `/api/sessions/{id}/plan` / `todo` / `artifacts` | 计划/Todo/Artifacts 读取 |
| GET | `/api/sessions/{id}/artifacts/{name}` | 单个 artifact 正文（1 MiB 读取上限） |
| GET | `/api/sessions/{id}/files?path=...&raw=true` | 文件树/内容（raw 取正文） |
| GET | `/api/sessions/{id}/stream` | **SSE 实时事件流** |
| GET | `/health` | 健康检查 |

- **project 消歧**：所有 session 路由接受可选 `?project=`（0.4.0 起，用于跨项目同名 session）；不传时按 id 唯一匹配。
- **新对话创建**：Web 在首次发送前不创建服务端会话。点击项目/新建项目只进入 composing 视图；首次发送用稳定 `draft_id` 创建会话，`first_prompt` 生成 `title`/`first_prompt`（最多 128 KiB，超出直接 400）。同一 draft id 的网络重试返回同一会话；无文本创建标记 `untitled=true`，后续首个非空文本用户消息回填标题。空名称不再落成共享的 `unnamed` 会话。列表摘要把 `first_prompt` 截到 512 字节并加省略号，避免大首条输入放大会话目录。
- **分页约束**：`limit` 必须在 `1..=2000`，超出返回 400；`tail`/`before_seq` 取“最接近目标”的末尾段，`from_seq` 前向读取保留开头；响应注入真实行号 `seq` 作为稳定 key。`turns=true` 时 `max_bytes`（默认 512 KiB，上限 16 MiB）限制一批完整轮次的序列化体积：`tail`/`before_seq` 丢最旧轮并保证最新一轮完整，`from_seq` 则从请求起点向后截断。
- **错误映射**（typed `RegistryError`）：404 NotFound；409 Ambiguous / Locked / Busy；429 Capacity；500 Internal。

### 持久输入与附件

`inputs` POST 正文为 `{request_id,text,attachment_ids:[],target_turn_id:null|string}`。null 启动新任务；字符串必须匹配当前接受引导的 turn。每条文字（含图片路径）最多 128 KiB，未消费项最多 32 条。重复 request ID / 相同原始内容返回既有回执，不重启任务；相同 ID 不同内容返回 409。GET inputs 默认只返回未消费/未应用项，`?request_id=` 可查询任意既有回执（含 applied/withdrawn）。编辑正文为 `{revision,text}`，撤回与 resume 正文为 `{revision}`，stale revision 或 applying/applied 返回冲突。

回执状态 `pending → applying → applied`，失败/停止/重启未消费转 `unapplied`，撤回为 `withdrawn`。应用前发布 applying，正式 user 消息携带 `_mink.input_id`，持久追加后发布 applied；不确定追加或发布失败闩锁 session。重启对账完整预期消息，禁止重复历史。引导在工具交换完整提交后的下一请求安全边界生效，不能改变当前请求/重试/子代理；终态准入共享锁，迟到引导明确拒绝。未应用输入只由用户明确 resume，不自动运行。

附件 POST 是原始文件字节，无浏览器重新编码；能力/格式/完整解码/尺寸/字节/校验和/路径/权限均校验，返回 `{id,mime,width,height,bytes}`。PNG/JPEG/GIF/WebP 与会话能力取交集，数量最多 8，单图及总量各取 core 上限与 16 MiB 的较小值。仅保存 session attachments 传输副本，输入加入无歧义绝对路径，图片上下文仍由 Read 捕获。仅图片输入生成明确查看请求；移除 UI 卡片不删除已存文件。预览不扩大工作目录 files 权限。

默认 conversation 仍按物理行分页。`turns=true` 改为完整真实用户轮次分页（internal 与 guidance 不建立新轮次），保留工具调用/result 与引导归属；Web 每次 20 轮。

## SSE 事件

`/stream` 订阅 SessionRuntime 的广播通道（容量 1024），原样转发 core `AgentEvent` 的
`{turn_id, sequence, kind}` envelope，并附加 `stream_sequence` 传输序号。事件名与
`AgentEventKind` 的 serde 命名一致（core 的 `final` 在 SSE 层改写为 `turn_final`）。
30s 无事件时发送 `: ping` 心跳注释帧防止中间代理断开；广播落后时收到
`stream_gap {missed}` 后连接结束，客户端应走权威恢复（见下）。

| type | 字段 | 说明 |
|------|------|------|
| `turn_started` | — | turn 开始 |
| `thinking` / `text` | content | 流式输出 |
| `tool_call` | id/name/summary/input | 工具调用（含完整参数，前端结构化渲染） |
| `tool_result` | tool_use_id/tool_name/content/status/exit_code/result_kind/presentation/artifacts | 工具结果（status 为 ToolStatus，presentation 携带 Plan/Todo 结构化状态；实时帧不重复发送前端未使用的 `content_preview`） |
| `signal` | signal_kind/severity/message | 信念信号 |
| `stop` / `retry` / `error` / `info` / `prompt` / `clear_line` | reason/message | 生命周期事件 |
| `title_update` | model/stats | 轮结束权威统计（`StatsSnapshot` 快照） |
| `sub_agent_status` / `sub_agent_output` | session_id/status/thinking/text/in_tokens/out_tokens | 子代理状态与输出；`text`/`thinking` 在实时帧中有界（32 KiB） |
| `turn_final` | outcome | 权威终态：完整 `TurnOutcome`（含 status/error/usage） |
| `turn_error` | error | 超时/取消后的强制错误终态 |
| `stream_gap` | missed | 广播落后 N 条；随后连接结束 |

- **语义分工**：`turn_started` / `turn_final` 与 `stop` 职责分离——stop 只记录结束原因，
  final 持有权威运行态（含 outcome error）；外部 shutdown 竞争下先发布 Closed 相关终态，
  再补 timeout stop/final，且每个终态最多发布一次。
- **协议一致性**：`crates/mink-server/protocol-fixtures/agent-events.json` 是 core 与
  server 共享的协议 fixture，mink-core 测试对其做反序列化 round-trip，保证两端口径一致。
- 前端 reducer 以 `turn_final` 为权威状态；历史展示来自 `/conversation` 与 `/events`
  （注入 `seq`），实时事件与历史通过 `seq` / `live:{generation}:{stream_sequence}`（旧流兼容无 generation 的 key） 区分 key。

### 新版原子快照订阅

`GET /stream?snapshot=true` 首帧 `session_snapshot` 与订阅位置在同一 mirror 发布锁下建立，包含 generation、stream_sequence、conversation、has_older、progress、current_turn、phase、running、inputs、capabilities、resources（Plan、完整 Todo、Artifact）、diagnostics、activity 与 last_final。conversation 只带**最近 4 个真实用户轮次且不超过 256 KiB**（最新一轮始终完整保留；超出时丢更早的整轮），has_older 表示还有可用 `/conversation?before_seq=` 加载的历史。`goal` 是流摘要：保留 bar 与状态判断所需字段（含 `iterations_count`、最新 review/continuation、budget 来源），不带 `usage_records`、完整 `reviews`/`iterations`/候选与报告；详情页用 `GET /goal?full=true` 取完整契约。`goal` 与 `stream_sequence` 在同一临界区更新，渲染快照不获取会话状态锁（锁序固定为 state → projection），`hidden` 目标归一为 `null`。activity 为 `{work_state,wait_elapsed_secs:null|number,active_sub_agents:[]}`，由可靠事件更新并与快照同时发布；active_sub_agents 是未结束子代理 ID 列表，单个子任务结束不抹掉其他活动任务，主动中断后工作状态回到 idle。旧客户端可忽略新字段。diagnostics 保留完整 `title_update.stats`，包括轮次、请求数和独立缓存读取/创建计数。打开时镜像经公开 SessionReader 重建，运行中消费可靠 runtime 事件；best-effort EventSink 不用于权威恢复。

快照在锁内只复制字段与建立订阅，JSON 序列化在锁外完成；大 session 不再长时间阻塞 live 发布。REST JSON 与嵌入静态资源经 `CompressionLayer` 压缩；SSE 由默认 predicate 排除，避免压缩缓冲给每一帧增加延迟。

后续新增 `conversation_committed {conversation_seq,message}`、`inputs_updated {inputs}`、`phase_updated {phase}`，与既有 AgentEvent 均携带 generation、stream_sequence 和 after_conversation_seq。正式历史接管暂态 text/thinking，稳定身份为物理 `(seq,block_index)` 或 input_id；诊断保持相邻时序。旧 generation 与重复水位丢弃，gap 后立即重取 snapshot，无需等待 turn 空闲。旧 SSE 模式过滤新增输入/commit/phase 控制事件，仍转发原有协议。运行态、成功与失败由 turn_final/outcome 决定，stop 与 HTTP 成功均不提前完成任务。

Web diagnostics 的统计定义见[用量与统计](usage.md#状态栏统计口径)。


## 生命周期与并发语义

- **SessionRuntime 阶段机**：`Idle → Running → Cancelling → Closing → Closed`；
  interrupt 只作用于 Running/Cancelling，随后等待 turn 有界退出并复位。
- **forced terminal**：turn 超时或外部 shutdown 竞争时，先登记强制终态（saw_stop/saw_final
  对账），发布缺失的 `stop`/`turn_final`/`turn_error`，保证客户端一定看到终态且不重复。
- **graceful shutdown**：Ctrl+C → axum serve 停止 → idle reaper 终止 →
  `registry.shutdown_all()`（逐会话 interrupt + 有界 join + runtime shutdown）。
- **idle reaper**：每 30s 扫描，自动关闭超过 `idle_close_secs` 未活动的会话。
- **session lease**：fs2 advisory file lock——锁文件永久保留，独占打开的文件句柄持有
  lease；删除会话前先获取并持有同一把系统文件锁，阻止其他 Registry 或进程删除使用中的
  会话（Locked/409）。TUI/CLI 不持锁，同一会话避免多端并发写。
