# 机器协议

> 更新日期：2026-10-03

本文面向机器消费方：通过 `--print`（stream-json）或 `--agent-jsonl`（Agent JSONL
single-shot 协议）与 Mink 集成。终端交互与配置见 [使用手册](USAGE.md)；Rust/Python
嵌入见 [嵌入与 SDK 使用](EMBEDDING.md)。

---

[TOC]

---

## Web 持久输入与快照协议

详见 [server.md](server.md)。所有 session 路由接受 `?project=`。`POST /api/sessions/{id}/inputs` 接受 `{request_id, text, attachment_ids:[], target_turn_id:null|string}`，返回带 input_id、revision、turn_id、status 和 guidance 的持久回执。相同 request ID 与相同原始内容幂等，内容不同冲突；网络结果不确定时先 GET inputs?request_id=... 对账，不自动重复执行。PATCH / DELETE 使用 revision，resume 必须由用户明确触发。旧 `/turn` 保留并转入同一执行路径。

`GET /stream?snapshot=true` 首帧是 `session_snapshot`：generation、stream_sequence、conversation（物理行号 seq）、progress、current_turn、phase、running、inputs、capabilities、resources（Plan/完整 Todo/Artifact）及 diagnostics / activity / last_final。可选 `activity:{work_state,wait_elapsed_secs:null|number,active_sub_agents:[]}` 保留可靠事件推导的工作状态和模型等待时间；diagnostics 的完整 `title_update.stats` 保留轮次/请求数和互斥的未缓存输入、缓存读取、缓存创建分区。快照与订阅在同一发布边界建立。后续 `conversation_committed`、`inputs_updated`、`phase_updated` 和既有事件携带 generation 与 stream_sequence；旧 generation/重复水位丢弃，gap 后重取 snapshot，无需等待运行 turn 空闲。`after_conversation_seq` 定位暂态诊断的历史相邻位置。

Core 的新增 `AgentEventKind::ConversationCommitted {conversation_seq,message}` 经可靠 runtime 流发出，不能与实时候选文本混同。Web stable key 为 `message:{seq}:{block_index}` 或 `input:{input_id}`，正式历史接管 `live:{generation}:{stream_sequence}`（旧流兼容无 generation 的 key）。既有 SSE 模式不发送新增输入/提交/阶段控制事件；CLI 展示忽略 commit 通知，避免重复正文。

附件 POST 发送原始图片字节，返回内容寻址描述 `{id,mime,width,height,bytes}`，GET `/attachments/{id}` 只预览本 session 的传输文件；上传不会生成模型图片块。`/conversation?turns=true` 可按真实非 internal、非 guidance 用户轮次分页，保留完整工具交换；旧行分页参数与默认行为保持兼容。


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

---

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
`tool_result` / `usage` / `stop` 等，格式与 [Stream-JSON](#stream-jsonprint) 一致。

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
  "usage": {"request_count": 1, "tokens": {...}}
}
```

> 兼容性：`usage` 不再包含 `cost` 字段（费用统计已移除）；`usage_records[].cost_nano_cny`
> 为兼容字段，已上报记录为 `0`、未上报记录为 `null`。

`status` 取值：`ok` / `failed` / `interrupted` / `max_turns_exceeded`。
`request.options.stream_events=false` 时只输出此 `final`；SDK 侧从
`conversation.jsonl` 回读最后一条 assistant 消息补齐 `text` / `thinking`。

`--agent-jsonl` 模式不会读取用户级/项目级 `.minkrc`，但仍应用同一命令行传入的
`--config <toml>`，避免 SDK 调用产生额外文件 I/O。

---

## 相关文档

- 终端用户手册：[USAGE.md](USAGE.md)
- 嵌入与 SDK 使用：[EMBEDDING.md](EMBEDDING.md)
- Python SDK 协议适配：[mink_agent/README.md](../mink_agent/README.md)
