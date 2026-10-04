# mink-core

> 更新日期：2026-10-05

`mink-core` 是对外发布的 Rust 包；库 crate 名为 `mink`。

这个子包只承载可嵌入的 agent runtime 和核心能力，不包含 REPL/TUI 的具体终端实现，也不生成
`mink` / `mink-core` 二进制入口。二进制入口位于 workspace 内部包
[`mink-cli`](../mink-cli/README.md)。

## 包内容

- `mink::runtime` / `mink::prelude`：Rust 嵌入式入口，提供唯一 shutdown owner `AgentRuntime`、可克隆 `AgentRuntimeHandle`、异步 `EventSink`、流式事件和 turn outcome。
- `mink::sdk_protocol`：Agent JSONL 协议类型和 SDK 适配。
- `mink::runtime::session`：只读 session 发现、读取与统一 usage 汇总。
- `src/agent`、`src/tools`、`src/session`、`src/llm`：Mink 的主循环、工具、持久化和 LLM 流式客户端核心。

## 依赖方式

```toml
[dependencies]
mink = { package = "mink-core", version = "0.6.7", default-features = false, features = ["runtime"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
anyhow = "1"
```

```rust
use mink::prelude::{AgentOptions, AgentRuntime};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let rt = AgentRuntime::start(
        AgentOptions::new("/tmp/mink-session", ".")
            .with_api_key(std::env::var("DEEPSEEK_API_KEY")?)
            .with_model("flash"),
    ).await?;

    let outcome = rt.run_turn("解释这段代码").await?;

    // 本轮 LLM 请求的 Token 汇总
    let u = &outcome.usage;
    println!("billing_turn_id: {}", outcome.billing_turn_id);
    println!("input: {}, cache_read: {}, output: {}",
             u.tokens.input_tokens, u.tokens.cache_read_tokens, u.tokens.output_tokens);

    // 每笔 LLM 请求明细
    for record in &outcome.usage_records {
        println!("  request {}: kind={:?}, status={:?}",
                 record.request_id, record.kind, record.status);
    }

    // usage.jsonl 文件路径（完整历史记录）
    println!("usage file: {}", outcome.session.usage_path.display());

    rt.shutdown().await?;
    Ok(())
}
```

## 详细说明

[Rust 集成](https://fiercex.github.io/mink/#docs/integration/rust.md)说明可靠流、Busy 门禁与 shutdown；[扩展契约](https://fiercex.github.io/mink/#docs/integration/extensions.md)说明 backend、只读 VFS 和初始化扩展；[配置参考](https://fiercex.github.io/mink/#docs/reference/configuration.md)与[用量参考](https://fiercex.github.io/mink/#docs/reference/usage.md)维护完整定义。

可运行示例：[custom_llm_backend.rs](examples/custom_llm_backend.rs)、[redb_vfs.rs](examples/redb_vfs.rs)、[web_api.rs](examples/web_api.rs)。
