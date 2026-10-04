# Rust 集成

> 更新日期：2026-10-04

进程内 runtime 生命周期与可靠事件。

评估会话密度和宿主开销时，参阅仓库中的[性能测量方法与记录](../development/performance.md)；历史 mock 结果不包含真实模型延迟。

## Rust 库 API 设计

### Rust 库嵌入

Rust 发布包为 `mink-core`，库 crate 名为 `mink`。发布包只包含可嵌入 runtime 和
`AgentEventStream` / `EventSink` 事件协议；终端 REPL/TUI 和二进制入口在 `mink-cli` workspace 包。
服务端嵌入时推荐只启用 runtime：

```toml
[dependencies]
mink = { package = "mink-core", version = "0.6.6", default-features = false, features = ["runtime"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
anyhow = "1"
```

公开入口为 `mink::prelude`、`mink::runtime`、`mink::sdk_protocol` 和 `mink::ui`。

### 只读输入变更与历史窗口

`handle.input_inbox().subscribe()` 返回
`tokio::sync::watch::Receiver<Arc<Vec<InputReceipt>>>`，初始值与订阅在准入锁内建立。
先消费 `borrow_and_update()`，再等待 `changed().await`；只在持久发布成功后通知，快照
包含 pending/applying/unapplied 项。watch 可以合并中间状态，权威正式交接仍由可靠
`ConversationCommitted` / Final 事件判断；写操作仍走公开准入、revision 编辑/撤回接口。

`SessionReader::conversation_turns(from, limit, tail, before)` 流式扫描 JSONL，只保留所需
轮次和当前轮次，保持物理 seq、internal、guidance 和完整工具交换；只容忍未换行的损坏
尾记录，正文损坏返回错误，limit=0 返回空窗口。
`SessionReader::sub_agent(id)` 提供父 session 内的只读子代理 Reader；ID 必须是单个路径
分量，canonical 路径必须仍位于父 session 的 subagents 目录，不接受目录穿越。

### 最小示例

```rust
use mink::prelude::{AgentOptions, AgentRuntime};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let rt = AgentRuntime::start(
        AgentOptions::new("/tmp/mink-session", ".")
            .with_api_key(std::env::var("DEEPSEEK_API_KEY")?)
            .with_model("flash"),
    ).await?;

    let outcome = rt.run_turn("hello").await?;
    println!("{}", outcome.text);

    rt.shutdown().await?;
    Ok(())
}
```

### 流式 turn

`stream_turn()` 逐条消费事件，结束时通过 `outcome()` 取回完整结果：

```rust
use mink::prelude::{AgentEventKind, AgentRuntime};

let mut stream = rt.stream_turn("explain")?;
while let Some(ev) = stream.recv().await {
    match ev.kind {
        AgentEventKind::Text { content } => print!("{content}"),
        AgentEventKind::Thinking { content } => eprint!("{content}"),
        AgentEventKind::ToolCall { name, summary, .. } => eprintln!("[tool] {name} {summary}"),
        AgentEventKind::Final { .. } => break,
        AgentEventKind::Error { message } => eprintln!("error: {message}"),
        _ => {}
    }
}
let outcome = stream.outcome().await?;
```

消费契约（三种方式，按需选择）：

- **持续消费**：如上循环 `recv()`；事件流是可靠通道，Reliable 事件（工具调用/结果、stop/error/usage、控制事件）不丢。
- **只要最终结果**：创建流后直接 `outcome()`；它会主动排空事件并释放进度字节，不会因未消费而积累无界积压。也可以直接用 `run_turn()`。
- **中途放弃**：丢弃 stream 会按既有契约取消当前 turn（消费者显式放弃）。若不希望取消，请不要丢弃未完成的流。

可合并的进度事件（`Text`/`Thinking`）受 1 MiB pending 字节预算约束：慢消费者下超限的增量会被丢弃，并以一条 `Info` 事件说明；最终结果不受影响。`stream.progress_backlog()` 返回 `(pending_bytes, dropped_deltas)` 供诊断。

需要从多个任务共享同一 runtime 时，调用 `rt.handle()` 获取可克隆的
`AgentRuntimeHandle`。handle 只提供 `run_turn`、`stream_turn`、`compact`、`set_model`
和 interrupt，所有克隆共享同一个 Busy 门禁；只有原始 `AgentRuntime` 拥有
`shutdown()`。0.5.0 不再暴露 raw actor sender、cancel token 或 interrupt flag。

全局观察者实现异步 `EventSink`：

```rust
#[async_trait::async_trait]
impl mink::runtime::EventSink for Observer {
    async fn on_event(&self, event: mink::runtime::AgentEvent) -> Result<(), String> {
        self.persist(event).await
    }
}
```

observer 通过固定容量队列与核心 turn 隔离；溢出只丢弃最新事件、告警一次并累计 dropped_events；observer 失败不会中断 turn，其错误会在
`AgentRuntime::shutdown()` 返回。Web SSE 会额外加入 `stream_sequence` 作为传输序号，
同时保留核心的 `turn_id + sequence`。事件名使用 `turn_started`、`title_update.stats`、
`sub_agent_status`、`sub_agent_output` 与 `turn_final.outcome`。

### AgentOptions 配置速查

`AgentOptions` 的字段保持私有。运行时策略通过 `ProviderOptions`、`GenerationOptions`、
`ContextPolicy`、`ToolOptions` 和 `SessionPolicy` 分组传入；不存在公开 `Config` 或可变逃生口。

| 类别 | 方法 |
|------|------|
| Provider | `with_provider_options()`；另有 `with_api_key()` / `with_base_url()` / `with_model()` 快捷方法 |
| Generation | `with_generation_options()` |
| Context | `with_context_policy()` |
| Tools | `with_tool_options()`；`enabled_tools=None` 用默认集合，空列表禁用全部 |
| Session | `with_session()` / `with_session_layout()`（或布局快捷方法） |
| Signal | `with_signal_policy(SignalPolicy)` |
| Recovery | `with_llm_recovery(LlmRecoveryPolicy { format_window_size, format_max_errors, request_max_retries, request_timeout_secs })` |
| OpenAI | `with_openai_reasoning_effort()` / `with_openai_tool_choice()` / `with_openai_extra_body()` / `with_openai_token_param()` / `with_openai_include_usage()` / `with_provider_http_timeout_secs()` |
| 多模态 | `with_image_input(ImageInputCapability)` / `with_vision_models(Vec<String>)` / `with_image_limits(ImageLimitsOverrides)` |
| 能力 | `with_mission_content()` / `with_selected_skills()` / `with_runtime_skill_content()` / `with_skill_discovery_policy()` / `with_resource_handler()` / `with_read_only_file_system()` / `with_resource_session_id()` |
| 后端 | `with_llm_backend()` / `with_sandbox()` / `with_sandbox_python()` |

### 沙箱

同进程 `AgentRuntime` 不会自动 sandbox 当前进程。需要完整进程级沙箱时，由业务服务
spawn 自身 worker 子进程：worker 先调用 `mink::runtime::reexec_in_sandbox()` 进入沙箱，
再创建 `AgentRuntime`（hidden worker 模式，参考
`crates/mink-core/examples/web_api.rs` 的完整实现）。

## 为什么是 Rust 库

`mink-core --agent-jsonl` 通过 stdin/stdout 子进程调用已经可用，但：

- 子进程启动成本（测量方法见仓库性能维护记录）
- JSON 序列化/反序列化开销
- 无法共享内存中的 session store
- 无法订阅实时 typed event

Rust 发布包名为 `mink-core`，库 crate 名为 `mink`。`mink-core` 发布包不包含 REPL/TUI
实现；终端二进制和 UI 实现由 workspace 中的 `mink-cli` 包持有。服务端依赖时推荐只启用嵌入式 runtime：

```toml
[dependencies]
mink = { package = "mink-core", version = "0.6.6", default-features = false, features = ["runtime"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
anyhow = "1"
```

`mink::runtime` / `mink::prelude` 解决这些问题：**同一套 OrchActor / TurnExecutor / ToolRunner 核心，但无进程边界**。

### 三入口共用核心

```text
mink CLI ──────────┐
mink-core SDK ─────┤
Rust crate mink ───┘
         │
    mink-cli::cli::main_entry()
         │
    AgentRuntime::start(AgentOptions)
         │
    OrchActor::run()
```

两个二进制入口通过 `crates/mink-cli/src/cli.rs` 调用 `mink::runtime`，Rust 库调用方直接使用
`mink::runtime` / `mink::prelude`。三者最终都进入同一 `runtime::builder` 和 orchestrator 核心，不允许分叉逻辑。

### API 分层

| 层 | 类型 | 定位 |
|----|------|------|
| **唯一构建入口** | `AgentOptions` + grouped options | 私有状态，无配置逃生口 |
| **并发入口** | `AgentRuntimeHandle` | 可克隆，共享同一 turn/control gate，不拥有 shutdown |
| **stream** | `AgentEventStream` | per-turn 实时事件，`recv()` + `outcome()` |

嵌入式调用方可通过 `AgentOptions::with_read_only_file_system()` 注入 VFS，并通过
`AgentOptions::with_resource_session_id()` 指定业务知识库分区。VFS trait 和请求/结果类型从
`mink::runtime` 导出。

### 关键设计决策

| 决策 | 理由 |
|------|------|
| `EventSink` 为异步 observer trait | 1024 容量 dispatcher 隔离慢 observer；溢出丢弃最新事件、告警并保留 observer |
| `TurnOutcome` 聚合 text/thinking | 调用方不订阅事件也能拿到结果 |
| `shutdown()` 分阶段 5s 预算 | 防止 orchestrator 死锁时无限等待 |
| `stream_turn()` 返回 `RuntimeResult` | 与 `run_turn()` 共用非阻塞 permit，忙时携带活动 turn ID |
| `with_llm_backend()` | 宿主注入自定义 backend，主代理/摘要/子代理共享 |

### 隐藏 worker 模式

私有化业务服务可以通过自身隐藏 worker 分支 + `sandbox::reexec_in_sandbox()` 实现进程级沙箱。沙箱配置走 argv，任务数据走 stdin（re-exec 后读），和 Mink CLI 流程完全一致。该隐藏分支属于业务服务实现细节，不要求 `mink` / `mink-core` 暴露新的公开 CLI。
