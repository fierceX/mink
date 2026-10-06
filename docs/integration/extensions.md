# 扩展契约

> 更新日期：2026-10-06

LLM backend、只读 VFS 与初始化扩展。

## Runtime 事件接口

`mink-core` 只公开 `AgentEventStream` 与 `EventSink`。工具结果事件直接携带原始
`tool_name` / `tool_use_id`、`ToolStatus`、`ToolFailureKind`、presentation 和 artifact 元数据。
REPL/TUI 在 `mink-cli` 内把同一事件流投影为终端输出或 `TuiSignal`；实时路径与 replay 共用 reducer，
不从展示文本反推 Todo、artifact 或工具状态。

### 事件交付契约（可靠流 vs 尽力 observer）

- **turn 可靠流**（`AgentEventStream`）：`stream_turn` 返回的每 turn 事件流是可靠交付通道，宿主必须消费 `recv()` 直到结束或用 `outcome()` 等待结果；内部为 unbounded 队列。边界策略：可靠事件由结构约束（工具结果受 `format_tool_result` 上限），`Text`/`Thinking` 进度受 1 MiB pending 预算（含每事件 128B 结构最小值），`outcome()` 主动排空；上游 SSE 生产者队列有界（1024）并 async 背压。
- **EventLog 两级提交**：契约关键事件（`prefix_snapshot`、signal rollback/replan/handover）经 `log_critical_event` 异步、有期限，并同时响应 runtime cancel 与当前轮 interrupt（健康 writer 保留在途应答宽限，期限见[状态与上下文](../concepts/state-and-context.md)），等待 writer 单次写入应答（入队≠写入），失败向调用方传播，并保持 stream-json stdout 输出；`flush` 的 ack 等待同样有期限；诊断事件走 `send_best_effort`（可见丢弃 + 丢失报告）。SSE 队列有界仅指事件个数，单事件字节/解析缓冲与 runtime 可靠事件仍不在预算内。
- **尽力 observer**（`EventSink` + `EventDispatcher`）：有界队列（容量 1024），溢出时丢弃最新事件并告警一次，适合遥测，不承担 UI 完整性；**observer 投递独立于 stream 进度预算**（慢 stream 消费者不会连带饿死 observer）。
- **预算范围与延期项**：进度字节预算、慢消费者丢弃通知及 `outcome`-only 排空已有回归测试；可靠事件整体字节、SSE 单事件/解析缓冲仍未有统一预算，不能据此宣称整条事件链有固定内存上限。合并增量、可靠事件落盘溢出或明确中止策略仍属后续工作；长流式输出/RSS 测量见[性能与验证](../development/performance.md)。

### 事件词汇职责与转换边界

- 三套词汇职责不同，**不要求互相派生**：`EventLog` 服务审计/持久化/信号证据（含 `prefix_snapshot`、`user_input`、压缩检查等领域专属事件，直接落盘）；`AgentEventKind` 服务 turn 内运行时流（含 `Prompt`/`ClearLine` 等展示事件）；`TuiSignal` 服务 UI。
- 重叠转换只有两条边界，且都由 trait 强制穷尽：`Display` trait → `EventDisplay` → `AgentEventKind`；`Display` trait → `TuiDisplay` → `TuiSignal`。新增 `Display` 方法会在编译期要求所有前端实现；新增 `AgentEventKind` 变体会要求 `EventDisplay`/消费者处理。
- 字段保真由 `runtime::events_tests::event_display_preserves_overlapping_event_fields` 与 `tui::tests::tui_display_preserves_tool_call_and_title_fields` 钉住；领域专属事件不经 `AgentEvent` 派生，TUI 信号不反向喂给 `EventLog`；不建通用消息总线。

### 自定义 LLM backend

默认 OpenAI-compatible backend 支持 `openai_extra_body` 和 `openai_tool_choice`
适配大多数兼容端点：

```rust
use std::collections::BTreeMap;
use mink::prelude::{AgentOptions, AgentRuntime};
use serde_json::json;

let runtime = AgentRuntime::start(
    AgentOptions::new("/tmp/mink-session", ".")
        .with_model("local")
        .with_openai_reasoning_effort("high")
        .with_openai_tool_choice("auto")
        .with_openai_extra_body(BTreeMap::from([
            ("custom_budget".to_string(), json!(8192)),
        ]))
        .with_provider_http_timeout_secs(0), // 长流式生成：不设总超时
).await?;
```

### 自定义 backend 的 Retry / Usage 语义

- runtime 拥有对外层 `stream()` 调用的重试责任：内置 backend 一次调用 = 一次物理请求；自定义 backend 若希望复用 runtime 重试，返回 `LlmUpstreamError`（`UpstreamFailureKind::Recoverable`/`ProtocolDamaged`，可选 HTTP status、provider code、`Retry-After` 与受限诊断），可选包在 `LlmRequestFailure{attempt_count, error}` 中（该结构曝光 source 链，运行时能穿透取出结构化根因）。未类型化错误保持既有的失败行为（不重试）。
- 每次 attempt 重新调用 `stream()`；重试次数由 `LlmRecoveryPolicy.request_max_retries` 限制，退避 1s/2s/4s…上限 10s，`Retry-After` 作为最早重试时间。backend 内部自带的多次请求不受限，也请如实上报聚合 `attempt_count`（外层不乘算，也不拆不可观察的明细）。
- `Event::Retry` 是 backend 内部的“本次候选作废”通知：会重置当前尝试累积的文本/工具调用（agent 与压缩摘要一致），但不获得新的重试额度、不重置 attempt 计时；runtime 自己发起的重试会另行发一次 Retry 控制事件。
- 每个请求只应发送一次 `Event::Usage`；首个 Usage 记为已上报，之后的
  Usage 事件只记一条 Unreported 诊断（不会静默丢弃，也不会覆盖首条记录）。
- `Event::UsageUnavailable` 记为 unreported（reason=`provider_usage_missing`）。

非 OpenAI-compatible 协议可实现 `mink::runtime::LlmBackend` 注入：

```rust
use std::sync::Arc;
use mink::prelude::{AgentOptions, AgentRuntime};

let runtime = AgentRuntime::start(
    AgentOptions::new("/tmp/mink-session", ".")
        .with_model("local")
        .with_llm_backend(Arc::new(MyLlmBackend::new())),
).await?;
```

实现要点：
- 从 `LlmRequest` 读取 system prompt、messages、tools、取消 token 和模型名
- `LlmRequest.model` 是解析后的真实模型名；`LlmRequest.model_alias` 保留用户请求的别名
- 失败时返回 `LlmRequestFailure { attempt_count, error }`，usage 日志可记录重试次数
- 图片能力：实现 `image_input_capability(model)`（trait 默认 `Unsupported`，fail
  closed）。OpenAI-compatible 后端按 `with_vision_models` 列表声明；自定义后端不开此
  方法则会话保持 text-only，也可用 `AgentOptions::with_image_input(...)` 显式声明
  （优先级高于 backend 声明）
- 缓存投影（可选）：实现 `cache_projection(request, source_prefix_len)` 可让压缩摘要
  复用上一 Agent 请求的 system/tools 与历史公共缓存前缀（返回 provider 可见的整条前缀
  或其指定长度切片）；不实现时返回默认 `None`，摘要自动降级为全量输入，功能不受影响
- 校准门控：若 backend 会动态改写请求（如注入/删减消息或改变工具面），声明
  `prompt_usage_calibration_safe() -> false` 可关闭 auto 压缩的 provider usage 校准，
  回退到保守本地估算（默认 `true`，仅对 provider 可见内容与本地请求一致时适用）

完整示例：

```bash
cargo run -p mink-core --example custom_llm_backend
```

### 嵌入式只读 VFS

私有化服务可替换 `Read`、`Glob`、`Grep` 的普通路径后端为数据库，而不注册新工具：

```rust
use std::sync::Arc;
use mink::prelude::{AgentOptions, AgentRuntime};

let vfs = Arc::new(MyReadOnlyFileSystem::open("knowledge.db")?);
let runtime = AgentRuntime::start(
    AgentOptions::new("/tmp/mink-session", ".")
        .with_resource_session_id("tenant-task-001")
        .with_read_only_file_system(vfs),
).await?;
```

实现同步的 `mink::runtime::ReadOnlyFileSystem` trait。每个操作收到 `VfsScope`：
- `resource_session_id`：知识库数据分区；子代理继承该值
- `agent_session_id`：实际发起调用的主代理或子代理 session id

虚拟 Read 是只读的，不产生 Hashline snapshot；`Write`/`Edit` 仍操作本地文件。VFS runtime
不会暴露 Edit；显式要求 Edit 会在启动时失败。
`artifact://`、`skill://`、`rule://`、`session://` 不进入 VFS。
完整 redb 示例见
[`crates/mink-core/examples/redb_vfs.rs`](../../crates/mink-core/examples/redb_vfs.rs)。


## 初始化扩展

宿主实现 `mink::runtime::PrefixSource` 提供不可变前缀，或实现 `PostInitHook` 在正常 session 初始化完成后运行逻辑。通过 `AgentOptions::with_prefix_source()` / `with_post_init_hook()` 注入。它们不替代共享的 runtime 创建、持久化与取消边界。完整可运行扩展见 [custom_llm_backend.rs](../../crates/mink-core/examples/custom_llm_backend.rs)、[redb_vfs.rs](../../crates/mink-core/examples/redb_vfs.rs)和 [web_api.rs](../../crates/mink-core/examples/web_api.rs)。

## 下一步

事件消费与唯一 shutdown owner 见 [Rust 集成](rust.md)；输出格式见[机器协议](../reference/protocols.md)。
