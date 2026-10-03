# mink-server：Server 与 Web 前端

> 更新日期：2026-10-03

---

## 1. 概述

`mink-server` 延续 Mink 的轻量定位：单二进制、低运行时依赖，把同一个 runtime 暴露为 Web 服务——与 CLI/TUI/Python SDK 共享内核，多端行为一致：

- **REST API**：会话管理（列表/创建/删除/打开/关闭/中断）、conversation/plan/todo/artifacts/files 读取
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

## 2. 快速开始

```bash
# 构建（build.rs 自动执行 npm run build 并嵌入前端）
cargo build -p mink-server

# 运行（默认端口 8765，读取 ~/.minkrc）
./target/debug/mink-server

# 指定端口 / 配置
MINK_SERVER_PORT=9000 ./target/debug/mink-server
./target/debug/mink-server path/to/mink-server.toml
```

打开 `http://localhost:8765` 即可使用 Web 界面。

## 3. 配置

优先级：**环境变量 > `mink-server.toml` > `~/.minkrc` > 默认值**。

| 配置项 | 环境变量 | mink-server.toml | 默认 |
|--------|----------|------------------|------|
| 监听地址 | `MINK_SERVER_HOST` | `[server] host` | `0.0.0.0` |
| 端口 | `MINK_SERVER_PORT` | `[server] port` | `8765` |
| Mink home | `MINK_HOME` | `[server] mink_home` | `$HOME` |
| 默认模型 | `MODEL` | `[server] model` / `~/.minkrc` `[provider] model` | `flash` |
| 最大并发会话 | `MINK_SERVER_MAX_RUNNING` | `[server] max_running` | `4` |
| 闲置自动关闭 | — | `[server] idle_close_secs` | `1800` |
| turn 超时 | `MINK_SERVER_TURN_TIMEOUT` | — | `1200`（秒） |

`~/.minkrc` 与 TUI/CLI 共享同一配置文件，**schema 完全一致**（分组格式，扁平键拒绝，与 CLI 相同的 `deny_unknown_fields` 规则）：

- 每个会话启动时按 **项目级 `<cwd>/.minkrc` 覆盖用户级 `~/.minkrc`** 的层级合并，覆盖 CLI 的同一套分组：
  `[provider]`（model/api_key/base_url/model_aliases/openai_*/http_timeout_secs/image_input/vision_models/`[provider.image]`）、`[generation]`、`[context]`、
  `[tools]` / `[tools.edit]`、`[signal]`、`[recovery]`、`[sandbox]` / `[sandbox_python]`。
- 会话运行时选项（provider/generation/context/tools/signal/recovery/sandbox/image）完整应用到 `AgentOptions`，
  与 CLI 的 `assemble_runtime_options` 行为一致；schema 对齐由 `crates/mink-server/tests/config_parity.rs` 机械校验。
- 环境变量 `MODEL` / `DEEPSEEK_API_KEY` / `DEEPSEEK_BASE_URL` / `MINK_IMAGE_INPUT` /
  `MINK_VISION_MODELS` / `MINK_SIGNAL_POLICY` /
  `LOG_EVENTS` 在文件层之上覆盖（server 文档化优先级：环境变量 > `mink-server.toml` >
  项目 `.minkrc` > 用户 `~/.minkrc` > 默认值）。

## 4. REST API

会话目录扫描与 ID 查找统一排除子代理：目录名或 metadata.id 带 `sub_`/`replan_` 前缀，或 metadata.parent 存在。目录判断在读取元数据之前执行，旧子代理缺失、损坏或继承元数据也不会出现在列表；正常用户会话的 alias 不参与过滤。

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/sessions` | 会话列表（含运行 phase、未处理输入数、最近活跃终态及 tokens/context 汇总） |
| POST | `/api/sessions` | 创建会话 `{name, cwd?}`（对 active alias 幂等，不重复创建） |
| GET | `/api/sessions/{id}` | 会话状态（open/running、runtime generation、当前 turn、能力、阶段与最近终态） |
| DELETE | `/api/sessions/{id}` | 删除会话（先持有系统文件锁，阻止并发删除） |
| POST | `/api/sessions/{id}/open` | 打开（建立 session lease） |
| POST | `/api/sessions/{id}/close` | 关闭（中断当前 turn、释放 lease；关闭后由 idle reaper 兜底） |
| POST | `/api/sessions/{id}/turn` | 兼容发送消息，内部转持久输入执行路径 |
| POST / GET | `/api/sessions/{id}/inputs` | 提交任务/引导 / 读取回执与未应用输入 |
| PATCH / DELETE | `/api/sessions/{id}/inputs/{input_id}` | revision 编辑 / 撤回尚未消费输入 |
| POST | `/api/sessions/{id}/inputs/{input_id}/resume` | revision 明确用于新轮次 |
| POST | `/api/sessions/{id}/attachments` | 原始图片字节上传，返回 session 限定描述 |
| GET | `/api/sessions/{id}/attachments/{attachment_id}` | session 限定的图片预览 |
| POST | `/api/sessions/{id}/interrupt` | 中断当前 turn |
| GET | `/api/sessions/{id}/events` | events.jsonl 分页（`from_seq`/`limit`/`tail`/`before_seq`） |
| GET | `/api/sessions/{id}/conversation` | conversation.jsonl 轮次历史（同分页参数） |
| GET | `/api/sessions/{id}/plan` / `todo` / `artifacts` | 计划/Todo/Artifacts 读取 |
| GET | `/api/sessions/{id}/files?path=...&raw=true` | 文件树/内容（raw 取正文） |
| GET | `/api/sessions/{id}/stream` | **SSE 实时事件流** |

- **project 消歧**：所有 session 路由接受可选 `?project=`（0.4.0 起，用于跨项目同名 session）；不传时按 id 唯一匹配。
- **分页约束**：`limit` 必须在 `1..=2000`，超出返回 400；`tail`/`before_seq` 取“最接近目标”的末尾段，`from_seq` 前向读取保留开头；响应注入真实行号 `seq` 作为稳定 key。
- **错误映射**（typed `RegistryError`）：404 NotFound；409 Ambiguous / Locked / Busy；429 Capacity；500 Internal。

### 持久输入与附件

`inputs` POST 正文为 `{request_id,text,attachment_ids:[],target_turn_id:null|string}`。null 启动新任务；字符串必须匹配当前接受引导的 turn。每条文字（含图片路径）最多 128 KiB，未消费项最多 32 条。重复 request ID / 相同原始内容返回既有回执，不重启任务；相同 ID 不同内容返回 409。GET inputs 默认只返回未消费/未应用项，`?request_id=` 可查询任意既有回执（含 applied/withdrawn）。编辑正文为 `{revision,text}`，撤回与 resume 正文为 `{revision}`，stale revision 或 applying/applied 返回冲突。

回执状态 `pending → applying → applied`，失败/停止/重启未消费转 `unapplied`，撤回为 `withdrawn`。应用前发布 applying，正式 user 消息携带 `_mink.input_id`，持久追加后发布 applied；不确定追加或发布失败闩锁 session。重启对账完整预期消息，禁止重复历史。引导在工具交换完整提交后的下一请求安全边界生效，不能改变当前请求/重试/子代理；终态准入共享锁，迟到引导明确拒绝。未应用输入只由用户明确 resume，不自动运行。

附件 POST 是原始文件字节，无浏览器重新编码；能力/格式/完整解码/尺寸/字节/校验和/路径/权限均校验，返回 `{id,mime,width,height,bytes}`。PNG/JPEG/GIF/WebP 与会话能力取交集，数量最多 8，单图及总量各取 core 上限与 16 MiB 的较小值。仅保存 session attachments 传输副本，输入加入无歧义绝对路径，图片上下文仍由 Read 捕获。仅图片输入生成明确查看请求；移除 UI 卡片不删除已存文件。预览不扩大工作目录 files 权限。

默认 conversation 仍按物理行分页。`turns=true` 改为完整真实用户轮次分页（internal 与 guidance 不建立新轮次），保留工具调用/result 与引导归属；Web 每次 20 轮。

## 5. SSE 事件

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
| `tool_result` | tool_use_id/tool_name/content_preview/content/status/exit_code/result_kind/presentation/artifacts | 工具结果（status 为 ToolStatus，presentation 携带 Plan/Todo 结构化状态） |
| `signal` | signal_kind/severity/message | 信念信号 |
| `stop` / `retry` / `error` / `info` / `prompt` / `clear_line` | reason/message | 生命周期事件 |
| `title_update` | model/stats | 轮结束权威统计（`StatsSnapshot` 快照） |
| `sub_agent_status` / `sub_agent_output` | session_id/status/thinking/text/in_tokens/out_tokens | 子代理状态与输出 |
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

`GET /stream?snapshot=true` 首帧 `session_snapshot` 与订阅位置在同一 mirror 发布锁下建立，包含 generation、stream_sequence、conversation（最近 20 个真实用户轮次）、progress、current_turn、phase、running、inputs、capabilities、resources（Plan、完整 Todo、Artifact）、diagnostics 与 last_final。打开时镜像经公开 SessionReader 重建，运行中消费可靠 runtime 事件；best-effort EventSink 不用于权威恢复。

后续新增 `conversation_committed {conversation_seq,message}`、`inputs_updated {inputs}`、`phase_updated {phase}`，与既有 AgentEvent 均携带 generation、stream_sequence 和 after_conversation_seq。正式历史接管暂态 text/thinking，稳定身份为物理 `(seq,block_index)` 或 input_id；诊断保持相邻时序。旧 generation 与重复水位丢弃，gap 后立即重取 snapshot，无需等待 turn 空闲。旧 SSE 模式过滤新增输入/commit/phase 控制事件，仍转发原有协议。运行态、成功与失败由 turn_final/outcome 决定，stop 与 HTTP 成功均不提前完成任务。

### Web 交互

项目导航常驻桌面，右侧详情按需打开；1024–1279px 打开详情时隐藏导航，768–1023px 侧栏覆盖，手机详情为全屏，使用 100dvh/safe-area，body 不承担对话滚动。轮次中思考/工具组成紧凑过程组，回复、重试、错误与引导有独立边界；支持简洁/标准/详细模式与手动展开优先。

草稿、附件、失败提交、展开、消息锚点、内层/详情滚动按完整会话身份持久保留。切换、返回首页、关闭浏览器只停止正文订阅，不调用 close、不停止任务。断线/停止中禁用提交但仍可编辑；明确发送与“返回最新内容”恢复外层跟随。任务详情只读，文件是当前磁盘内容，工具/Artifact 是当时记录。

页面可见且有运行任务时目录每 2s 对账，否则每 15s；隐藏停止轮询，恢复可见立即刷新，只当前会话保持正文 SSE。新建任务在应用内填写 server 工作目录并生成唯一 alias；删除失败留在原位报告。

## 6. 静态资源与嵌入

- **自动构建**：`build.rs` 在 `cargo build` 时自动执行 `npm run build`（web/），产物复制到 `OUT_DIR/assets` 并生成 `assets.rs`（`include_str!` 内容表）
- **嵌入服务**：默认从二进制内容服务静态资源（content-type 映射、`index.html` no-cache、静态资源 immutable 缓存、SPA fallback 到 index.html）
- **开发模式**：`MINK_SERVER_DEV_WEB=1` 时回退磁盘 `web/dist`（前端热迭代，改完强刷即可）
- E2E 使用 `MINK_SERVER_DEV_WEB=1` 保证测试服务最新构建产物

## 7. 生命周期与并发语义

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

## 8. 部署注意

- **单二进制分发**：`target/release/mink-server` 即可（含前端），无需 node/npm
- **重启更新前端**：嵌入产物随 `cargo build` 更新——修改前端后需**重新构建 mink-server** 再重启
- **多进程一致性**：lease 锁文件是跨进程稳定对象，不依赖 PID 存活判断；两个 server 进程
  同时操作同一会话时按文件锁顺序串行，冲突返回 409
- **超时保护**：`MINK_SERVER_TURN_TIMEOUT`（默认 1200s）防止 LLM/工具挂起卡死 running 状态；
  超时后进入 forced terminal 并关闭该会话 runtime
- **安全**：单用户部署假设；`sk-fake`/受限 key 可用于测试环境

## 9. 测试

```bash
cd crates/mink-server/web
npm run e2e             # 真实浏览器 + 真实 server + 本地模拟模型
npm run test            # reducer/controller/SSE/组件状态测试
npm run typecheck && npm run build
cargo test -p mink-core -p mink-cli -p mink-server
```

E2E 通过 global-setup 构造隔离临时 home + 模板会话与本地模拟 OpenAI-compatible provider，覆盖 1440/1024/768/390px、引导/续发/附件/分页锚点；失败产出
trace/error-context 供 AI 自愈；测试产物（test-results/）已加入 .gitignore。
