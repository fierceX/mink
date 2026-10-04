# 机器协议

> 更新日期：2026-10-04

Stream-JSON 与 Agent JSONL v3。

## Protocol 事件

`Event` enum 是流式响应的统一接口：

```rust
pub enum Event {
    Text(TextEvent),          // 文本内容
    Thinking(ThinkingEvent),  // 推理内容
    ToolCall(ToolCallEvent),  // 工具调用
    Usage(UsageEvent),        // Token 用量
    UsageUnavailable,        // 上游未上报 usage
    Stop(StopEvent),          // 停止原因
    Error(ErrorEvent),        // 错误
    Retry(RetryEvent),        // 重试信号
}
```

事件由 SSE parser 产生，由 `TurnExecutor` 消费。stream-json 模式下，事件也被序列化为 JSON 行输出到 stdout：

```json
{"type":"text","content":"Hello"}
{"type":"tool_call","name":"Read","id":"...","input":{"path":"/x"}}
{"type":"usage","input_tokens":100,"output_tokens":50,"cache_read_input_tokens":40}
{"type":"stop","reason":"end_turn"}
```

## Stream-JSON（`--print`）

```bash
mink -m flash --print "explain this"
```

每行一个 JSON 事件：

```json
{"type":"thinking","content":"Let me analyze..."}
{"type":"text","content":"Here is the explanation..."}
{"type":"tool_call","name":"Read","id":"...","input":{"path":"/x"}}
{"type":"tool_result","tool_use_id":"...","name":"Read","content":"...","status":{"state":"succeeded"},"result_kind":"file_read","artifacts":[]}
{"type":"usage","input_tokens":100,"output_tokens":50,"cache_read_input_tokens":20,"cache_creation_input_tokens":0,"kind":"agent"}
{"type":"stop","reason":"end_turn"}
```

事件流以 `final` 事件结束，`final` 携带用量信息：

```bash
mink -m flash --print "hello" | jq 'select(.type=="final") | {billing_turn_id, usage}'
```

JQ 下游处理：

```bash
mink -m flash --print "fix the bug" | jq 'select(.type=="text") | .content'
```

## Agent JSONL（`--agent-jsonl`）

SDK 专用 single-shot 协议：stdin 读入一个 versioned JSON request，stdout 输出事件流，
最后以 `final` 结束。协议版本为 **3**；v3 的 `options` 各分组对未知字段直接拒绝
（`deny_unknown_fields`），不兼容 v2 的扁平 options。

```bash
# 最小请求
echo '{"version":3,"prompt":"scan this repo"}' | mink-core --agent-jsonl
```

### Request

| 字段 | 类型 | 说明 |
|------|------|------|
| `version` | `u32` | 协议版本（当前 `3`） |
| `prompt` | `string` | 用户输入（**必需**） |
| `session_id` | `string?` | session 引用（alias / id / 前缀 / title） |
| `mission` | `string?` | MISSION.md 内联内容（避免临时文件 I/O） |
| `options` | object | 见下 |

### Grouped options

| 分组 | 主要字段 | 说明 |
|------|------|------|
| `provider` | `model`, `http_timeout_secs` | 模型名（别名或真实名）；`http_timeout_secs` 为 provider HTTP 请求总超时（秒，`0` = 不设，默认 600） |
| `generation` | `max_tokens`, `max_turns`, `llm_*_timeout` | 生成和流超时 |
| `context` | `max_context`, `context_compact_*`, `context_reserve_tokens` | 上下文与压缩；`max_context=0` 禁用自动压缩 |
| `tools` | `enabled_tools`, `tool_timeout`, `tool_timeout_max`, `sub_agent_timeout`, edit 和 skill 字段 | 工具 surface 与执行策略 |
| `session` | `session_id`, `session_layout` | session 引用与布局 |
| `output` | `verbose`, `stream_events` | 输出策略 |
| `signal` | `policy` | `off` / `evidence` / `state_ops` / `restart` / `full` |
| `recovery` | `format_window_size`, `format_max_errors`, `request_max_retries`, `request_timeout_secs` | 有界 LLM 恢复；全部可选，缺省保留 Rust 默认（10 / 3 / 3 / 不设），非法值拒绝 |

示例：

```json
{
  "version": 3,
  "prompt": "scan this repo and summarize",
  "session_id": "work-001",
  "options": {
    "provider": {"model": "flash", "http_timeout_secs": 0},
    "generation": {"max_tokens": 8192, "max_turns": 20},
    "context": {
      "max_context": 64000,
      "context_compact_pct": 65,
      "context_reserve_tokens": 12000
    },
    "tools": {
      "tool_timeout": 300,
      "tool_timeout_max": 600,
      "edit_mode": "hashline",
      "edit_enforce_seen_lines": false,
      "enabled_tools": ["Read", "Write", "Edit", "Grep", "Glob", "Bash"]
    },
    "recovery": {"format_window_size": 10, "request_max_retries": 3},
    "output": {"stream_events": false}
  }
}
```

### Events

过程事件（`stream_events=true` 时输出）：`thinking` / `text` / `tool_call` /
`tool_result` / `usage` / `stop` 等，格式与 [Stream-JSON](#stream-json--print) 一致。

### Final

```json
{
  "type": "final",
  "version": 3,
  "status": "ok",
  "billing_turn_id": "turn-...",
  "session_id": "session-...",
  "session_ref": "work-001",
  "home": "/app/mink-home",
  "cwd": "/app/work",
  "events_path": ".../events.jsonl",
  "conversation_path": ".../conversation.jsonl",
  "artifacts_dir": ".../artifacts",
  "summary_path": ".../summary.txt",
  "usage_path": ".../usage.jsonl",
  "tool_call_count": 3,
  "tool_error_count": 0,
  "error": null,
  "usage_records": [],
  "usage": {"request_count": 1, "reported_request_count": 1, "unreported_request_count": 0, "attempt_count": 1, "tokens": {"input_tokens": 100, "cache_read_tokens": 0, "cache_creation_tokens": 0, "output_tokens": 50}}
}
```

> 兼容性：`usage` 不再包含 `cost` 字段（费用统计已移除）；`usage_records[].cost_nano_cny`
> 为兼容字段，已上报记录为 `0`、未上报记录为 `null`。

`status` 取值：`ok` / `failed` / `interrupted` / `max_turns_exceeded`。
`request.options.stream_events=false` 时只输出此 `final`；SDK 侧从
`conversation.jsonl` 回读最后一条 assistant 消息补齐 `text` / `thinking`。

`--agent-jsonl` 模式不会读取用户级/项目级 `.minkrc`，但仍应用同一命令行传入的
`--config <toml>`，避免 SDK 调用产生额外文件 I/O。

## Web 输入与展示元数据

Web 的任务、引导和图片上传属于 runtime 人类输入协议，不是新增模型工具。Plan/Todo 仍只能经现有公开工具修改。引导在完整工具调用/result 交换之后加入正式历史；图片上传仅存 session 传输副本并附绝对路径，模型图片捕获仍走 `Read`。

Full/Inline TUI 也使用此 Inbox 协议，运行中 Enter 提交引导，`/inputs`、`/resume ID`、`/withdraw ID` 是本地人类输入管理命令，不进入模型 tool surface。图片仍随绝对路径请求交给 `Read`，没有第二套图片入口。

Web 文件详情的加载、返回目录与上一级属于只读预览交互，不发起模型工具调用；当前磁盘正文与历史工具输出分别展示，失败保留明确错误。输入区图片/发送图标、轮次跳转和 Reka UI 菜单均属于客户端控件，不改变工具执行协议。

诊断展示直接读取 runtime `StatsSnapshot` 和可靠事件 activity，不从工具正文反向解析；缓存创建不计入缓存命中，Plan/Todo 仅显示权威资源快照，统计与任务详情均不增加模型工具。

对话和路径的复制仅使用客户端剪贴板，不调用模型工具或 server；优先 Clipboard API，失败后使用选区复制兼容路径，实际成功后才确认。

工具卡片解码的是实际调用参数：Python/PythonSandbox 使用 `script`/`script_file`，Replace 使用 `edits[].old_text/new_text/all`；Plan/Todo 结果优先使用 `ToolPresentation` 的正式结构。原始参数/结果保留折叠入口，文件和命令的正文不做通用 JSON 解码，Artifact 输出保护不变。

轮次选择是客户端限高纵向菜单，键盘或点击只改变阅读位置，不创建模型请求或改变工具执行顺序。

自动换行只改变客户端的代码、工具输出和文件预览排版；完整原始文本及换行、工具参数、`conv_content`、Artifact 保护和复制内容均不改写。

目录筛选只匹配已发现的项目/会话元数据，不调用 Grep、Read 或模型工具，也不改变当前会话。

首页导航只断开当前视图订阅并清除会话地址参数，后台执行与工具状态不变。

顶栏菜单的设置面板只修改浏览器展示偏好，不发起模型工具调用，不重启或停止任务。

手机阅读模式只隐藏客户端操作栏，不调用 interrupt/close、不卸载会话或丢弃草稿；运行中保留停止控件，待处理/失败输入和附件优先显示。内层工具输出滚动不得触发外层操作栏隐藏。

SubAgent 的执行记录仍在父会话工具结果中展示；其独立 session（含恢复子代理）不进入 Web 用户会话目录。

正式工具结果 `_mink` 保留 `status`、`tool_name`、`result_kind`、`exit_code`、`presentation`、`artifacts` 及既有状态元数据，供 Web 历史与实时展示统一。正式用户消息 `_mink` 包含 input_id / turn_id / guidance / attachment_ids。所有 `_mink` 在模型请求转换时剥离；`format_tool_result()` 与 conversation 的 conv_content 优先规则保持不变。旧历史缺少 ToolStatus 时界面显示“状态未记录”，不从正文推测成功。

## Agent JSONL 字段索引

类型名对应公开 Rust 协议；Option 表示字段可省略或为 null，分组未知字段拒绝。SDK 的 provider 分组只接受 model/http_timeout_secs；认证与端点由启动参数/环境提供。

### `SdkRequest`

| 字段 | Rust 类型 |
|---|---|
| `version` | `Option<u32>` |
| `prompt` | `String` |
| `session_id` | `Option<String>` |
| `mission` | `Option<String>` |
| `options` | `SdkOptions` |

### `SdkOptions`

| 字段 | Rust 类型 |
|---|---|
| `provider` | `SdkProviderOptions` |
| `generation` | `SdkGenerationOptions` |
| `context` | `SdkContextOptions` |
| `tools` | `SdkToolOptions` |
| `session` | `SdkSessionOptions` |
| `output` | `SdkOutputOptions` |
| `signal` | `SdkSignalOptions` |
| `recovery` | `SdkRecoveryOptions` |

### `SdkProviderOptions`

| 字段 | Rust 类型 |
|---|---|
| `model` | `Option<String>` |
| `http_timeout_secs` | `Option<u64>` |

### `SdkGenerationOptions`

| 字段 | Rust 类型 |
|---|---|
| `max_tokens` | `Option<i32>` |
| `max_turns` | `Option<i32>` |
| `llm_first_event_timeout` | `Option<i32>` |
| `llm_idle_timeout` | `Option<i32>` |
| `llm_wait_heartbeat` | `Option<i32>` |

### `SdkContextOptions`

| 字段 | Rust 类型 |
|---|---|
| `max_context` | `Option<usize>` |
| `context_compact_pct` | `Option<u8>` |
| `context_reserve_tokens` | `Option<usize>` |
| `context_compact_tail_tokens` | `Option<usize>` |
| `context_compact_max_output_tokens` | `Option<i32>` |
| `context_compact_input_reduction` | `Option<bool>` |

### `SdkToolOptions`

| 字段 | Rust 类型 |
|---|---|
| `tool_timeout` | `Option<i32>` |
| `tool_timeout_max` | `Option<i32>` |
| `sub_agent_timeout` | `Option<i32>` |
| `enabled_tools` | `Option<Vec<String>>` |
| `edit_mode` | `Option<crate::config::EditMode>` |
| `edit_fuzzy_match` | `Option<bool>` |
| `edit_fuzzy_threshold` | `Option<f64>` |
| `edit_enforce_seen_lines` | `Option<bool>` |
| `skills` | `Option<Vec<String>>` |
| `inline_skills` | `Option<Vec<SdkInlineSkill>>` |
| `skill_discovery_policy` | `Option<SdkSkillDiscoveryPolicy>` |

### `SdkSessionOptions`

| 字段 | Rust 类型 |
|---|---|
| `session_id` | `Option<String>` |
| `session_layout` | `Option<SessionLayout>` |

### `SdkOutputOptions`

| 字段 | Rust 类型 |
|---|---|
| `verbose` | `Option<bool>` |
| `stream_events` | `Option<bool>` |

### `SdkSignalOptions`

| 字段 | Rust 类型 |
|---|---|
| `policy` | `Option<crate::config::SignalPolicy>` |

### `SdkRecoveryOptions`

| 字段 | Rust 类型 |
|---|---|
| `format_window_size` | `Option<usize>` |
| `format_max_errors` | `Option<usize>` |
| `request_max_retries` | `Option<u32>` |
| `request_timeout_secs` | `Option<u64>` |

### `SdkInlineSkill`

| 字段 | Rust 类型 |
|---|---|
| `name` | `String` |
| `description` | `String` |
| `content` | `String` |
| `exposure` | `Option<SdkCapabilityExposure>` |
| `revision` | `Option<String>` |

### `SdkFinal`

| 字段 | Rust 类型 |
|---|---|
| `type` | `&'static str` |
| `version` | `u32` |
| `status` | `SdkStatus` |
| `billing_turn_id` | `String` |
| `session_id` | `String` |
| `session_ref` | `String` |
| `home` | `String` |
| `cwd` | `String` |
| `events_path` | `String` |
| `conversation_path` | `String` |
| `artifacts_dir` | `String` |
| `summary_path` | `String` |
| `usage_path` | `String` |
| `tool_call_count` | `u32` |
| `tool_error_count` | `u32` |
| `error` | `Option<String>` |
| `usage_records` | `Vec<crate::session::usage::UsageRecord>` |
| `usage` | `crate::session::usage::UsageSummary` |
