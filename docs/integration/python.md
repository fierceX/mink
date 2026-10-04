# Python 集成

> 更新日期：2026-10-04

子进程调用、流式接口与磁盘会话复用。

## Python SDK

SDK wheel 内置无 TUI 的 `mink-core` 二进制，无需额外安装：

```bash
pip install mink-agent
```

### 最小示例

```python
from mink_agent import AgentSession, SandboxConfig

session = AgentSession(SandboxConfig(
    api_key="sk-...",               # 或设置 DEEPSEEK_API_KEY 环境变量
    read_dirs=["src"],
    signal_policy="full",             # off/evidence/state_ops/restart/full
))
result = session.run("scan this repo and summarize")
print(result["text"])
session.close()
```

单次快捷调用：

```python
from mink_agent import quick_run

result = quick_run("解释这段代码", read_dirs=["/path/to/project"], api_key="sk-...")
print(result["text"])
```

每次 `run()` 启动一个新的 `mink-core --agent-jsonl` 进程；持续交互通过相同的
`mink_home + session_id` 复用磁盘 session。同一 `AgentSession` 实例不支持并发调用；
并发任务应创建多个实例或由外层应用排队。

### SandboxConfig 关键配置

| 类别 | 字段 | 说明 |
|------|------|------|
| 路径 | `mink_home` / `session_layout` / `cwd` | session 根目录与布局（SDK 默认 `home`） |
| 文件系统 | `read_dirs` / `write_dirs` | agent 可读/可写目录 |
| 工具 | `enabled_tools` | 精确工具选择；`None` 用默认集合，显式列出 `PythonSandbox` 才启用它 |
| Edit | `edit_mode` / `edit_fuzzy_match` / `edit_fuzzy_threshold` / `edit_enforce_seen_lines` | 与 Rust/CLI 相同的双模式配置 |
| 信号 | `signal_policy` | `"off"` / `"evidence"` / `"state_ops"` / `"restart"` / `"full"` / `None`（继承 `MINK_SIGNAL_POLICY`） |
| 提示词 | `mission_file` / `mission_content` | MISSION.md 文件或内联内容（二选一，内联避免临时文件） |
| 技能 | `skills` / `inline_skills` / `skill_discovery_policy` | 技能选择与注入 |
| 压缩 | `max_context` / `context_compact_*` | 与 Rust/CLI 同一组压缩参数 |
| 超时 | `timeout_secs` / `tool_timeout` / `tool_timeout_max` / `sub_agent_timeout` / `llm_*` | 各层超时限制 |
| 沙箱 | `sandbox_backend` | `"auto"` / `"nsjail"` / `"bwrap"` / `"sandbox-exec"` / `"off"` |
| API | `api_key` / `api_url` / `model` | DeepSeek 配置 |

### 流式事件

```python
for event in session.stream_events("解释这段代码"):
    if event.type == "thinking_delta":
        ...
    elif event.type == "answer_delta":
        ...
```

`AgentStreamEvent.type` 常见值：`thinking_delta` / `answer_delta` / `tool_call` /
`tool_result` / `final`。原始 Agent JSONL 事件可用 `raw_stream()` 逐条获取。

## AgentSession API

### `run(prompt, *, extra_options=None, on_event=None) -> dict`

执行一个提示词并返回聚合结果。每次调用都会启动一个新的 `mink-core --agent-jsonl` 进程；持续交互通过相同的 `mink_home + session_id` 复用磁盘 session，而不是复用同一个进程。同一个 `AgentSession` 实例不支持并发调用；并发任务应创建多个实例或由外层应用排队。

Rust core 会在 `conversation.jsonl` 中完整保留历史，并通过 `context-state.json` 只恢复当前活跃后缀。
压缩不会删除旧消息；长 session 的冷历史留在磁盘，不会在每次调用中全部常驻内存。
`session://current/history` 提供从完整 `conversation.jsonl` 生成的有损检索视图；原始 thinking
和完整工具输出仍需读取 `conversation.jsonl` 或相关 artifact。

默认情况下 `run()` 会消费 Rust 侧输出的过程事件并聚合 `text`、`thinking`、工具调用和最终状态。传入 `on_event` 时，每个归一化后的 `AgentStreamEvent` 会同步回调给调用方，适合在不直接迭代 stream 的场景里做 UI 增量更新。

如果 `SandboxConfig.stream_events=False`，SDK 会在 Agent JSONL request 的 `options` 中传入 `stream_events=false`。Rust 侧不会向 stdout 输出 `thinking`、`text`、`tool_call`、`tool_result` 等过程事件，只输出最终 `final`；`run()` 会从 session `conversation.jsonl` 回读最后一条 assistant 消息，尽量补齐最终 `text` / `thinking`。这个模式用于不需要流式展示的长程任务，可减少 stdout 事件处理开销。

| 返回字段 | 类型 | 说明 |
|---------|------|------|
| `text` | `str` | agent 的文本回复 |
| `thinking` | `str` | agent 的推理过程（如有） |
| `tool_calls` | `list[dict]` | agent 执行的工具调用记录 |
| `tool_results` | `list[dict]` | 工具结果事件 |
| `events` | `list[dict]` | 本次调用的完整 JSONL 事件 |
| `status` | `str or None` | `final.status`，如 `ok`、`failed`、`interrupted` |
| `session_id` | `str or None` | Rust 侧解析后的真实 session id |
| `session_ref` | `str or None` | 调用方传入的 session 引用或真实 id |
| `home` | `str or None` | 实际 `MINK_HOME` |
| `events_path` | `str or None` | session `events.jsonl` 路径 |
| `conversation_path` | `str or None` | session `conversation.jsonl` 路径 |
| `artifacts_dir` | `str or None` | session artifacts 目录 |
| `summary_path` | `str or None` | session summary 路径 |
| `usage_path` | `str or None` | session `usage.jsonl` 路径 |
| `billing_turn_id` | `str or None` | 本次根用户 Turn 的计量归属标识 |
| `usage_records` | `list[dict]` | 本次 Turn 的 Agent、压缩和子代理 LLM 请求明细 |
| `usage` | `dict` | 本次 Turn 的 Token、请求次数汇总 |
| `tool_call_count` | `int` | 本次调用执行的工具调用数量 |
| `tool_error_count` | `int` | 本次调用检测到的工具错误数量 |
| `exit_code` | `int` | 进程退出码（0 表示成功） |
| `error` | `str or None` | 错误信息（如有） |
| `stderr` | `str` | 进程原始错误输出（调试用） |

### `stream(prompt, *, extra_options=None) -> Iterator[dict]`

兼容旧版本的流式接口，逐条产出原始 dict 事件。新代码优先使用 `stream_events()`。

### `raw_stream(prompt, *, extra_options=None) -> Iterator[dict]`

执行一个提示词并逐条产出 Rust Agent JSONL 协议的原始 dict 事件。普通事件会即时返回；最终事件为 `{"type": "final", ...}`，其中包含 `status`、session 路径和 stderr。

### `stream_events(prompt, *, extra_options=None) -> Iterator[AgentStreamEvent]`

执行一个提示词并逐条产出归一化事件对象。`AgentStreamEvent.type` 常见值：

| 类型 | 说明 |
|------|------|
| `thinking_delta` | 中间思考增量 |
| `answer_delta` | 最终回答文本增量 |
| `tool_call` | 工具调用开始 |
| `tool_result` | 工具调用结果 |
| `final` | 本次调用完成，包含状态和 session 路径 |

事件对象可通过 `event.to_dict()` 转回 dict。QA/聊天类前端建议使用 `thinking_delta` 和 `answer_delta` 分离展示中间思考和最终回答。

### `close()`

清理资源。会话目录保留在磁盘上，可供查阅。


## 支持平台

| 平台 | 架构 | 沙箱（由 Rust 内部处理） |
|------|------|------|
| macOS | arm64（Apple Silicon） | sandbox-exec |
| macOS | x86_64（Intel） | sandbox-exec |
| Linux | x86_64 | nsjail / bubblewrap |
| Linux | aarch64 | nsjail / bubblewrap |

## 下一步

完整字段见[配置参考](../reference/configuration.md#sandboxconfig-配置项)，JSONL 数据见[机器协议](../reference/protocols.md)。
