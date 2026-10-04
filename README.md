<p align="center">
  <img src="docs/assets/mink-wordmark.svg" alt="Mink" width="128">
</p>

[![Crates.io](https://img.shields.io/crates/v/mink-core.svg)](https://crates.io/crates/mink-core)
[![MIT licensed](https://img.shields.io/badge/license-MIT-blue.svg)](./LICENSE)
[![Rust 1.94+](https://img.shields.io/badge/rust-1.94%2B-blue)](https://blog.rust-lang.org/2025/06/05/Rust-1.94.0.html)
[![Python SDK](https://img.shields.io/badge/pypi-mink--agent-blue)](https://pypi.org/project/mink-agent)

**Rust 原生 · 终端优先 · 可嵌入**

Mink 是一个 Rust 实现的 **AI agent runtime**：面向终端，也面向系统。既适合在终端中直接
工作（REPL / Full TUI / Inline TUI），也适合嵌入到服务端、桌面端或内部工具中 —— Rust
嵌入通过 `mink::runtime` in-process 运行；Python SDK 通过 wheel 内置的
`mink-core --agent-jsonl` 子进程复用同一运行时内核与语义。

---

[TOC]

---

## 快速开始

### 终端使用

```bash
# 前置：Rust 1.94+，设置 DEEPSEEK_API_KEY 或通过配置指定 OpenAI-compatible 端点

# 编译
cargo build --release        # 或 make build

# REPL 交互模式
./target/release/mink -m flash -i

# Full TUI 全屏模式
./target/release/mink -m flash --tui

# Inline TUI 原生 scrollback 模式
./target/release/mink -m flash --tui=inline

# 单次查询 / 恢复最近会话
./target/release/mink -m flash "explain this project"
./target/release/mink -m flash --continue -i

# 使用自定义系统提示词
./target/release/mink --mission ./my-task.mission.md -i
```

### Python SDK

```bash
pip install mink-agent
```

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

### Rust 嵌入

```toml
[dependencies]
mink = { package = "mink-core", version = "0.6.6", default-features = false, features = ["runtime"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
anyhow = "1"
```

```rust
use mink::prelude::{AgentOptions, AgentRuntime};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let rt = AgentRuntime::start(
        AgentOptions::new("/tmp/mink-home", ".")
            .with_api_key(std::env::var("DEEPSEEK_API_KEY")?)
            .with_model("flash"),
    ).await?;

    let outcome = rt.run_turn("hello").await?;
    println!("{}", outcome.text);

    rt.shutdown().await?;
    Ok(())
}
```

---

## 核心特点

- **可嵌入的运行时内核** — `AgentRuntime::start() → run_turn() / stream_turn() → shutdown()` 完整生命周期。CLI、REPL、TUI 和 Rust 嵌入共享 in-process 运行时；Python SDK 通过内置 `mink-core` 二进制复用同一 Rust 内核，不需要维护多套 agent 内核。
- **长上下文与长任务可控** — 显式压缩参数 + LLM 摘要非破坏式投影 + 持久化 session 共同工作，上下文不无限膨胀，长任务可持续推进；`enabled_tools` 统一工具边界。
- **编辑与状态管理更可靠** — Hashline / Replace 双模式编辑（`Read` snapshot + 行锚定，或 exact/fuzzy 内容匹配）、artifact 超长输出回读、Plan journal 与 Todo revision 原子提交和 session 恢复机制，不把正确性交给运气。

---

## Workspace Packages
| 路径 | 职责 |
|------|------|
| [crates/mink-core](crates/mink-core/README.md) | Rust 发布包 `mink-core`，库 crate 名 `mink`，包含可嵌入 runtime、工具核心、session、sandbox 和 SDK 协议 |
| [crates/mink-cli](crates/mink-cli/README.md) | workspace 内部二进制包，生成 `mink` 终端二进制和 `mink-core` SDK 精简二进制，持有 REPL/TUI 实现 |
| [mink_agent](mink_agent/README.md) | Python SDK，wheel 内置无 TUI 的 `mink-core` 二进制 |
| [crates/mink-server](crates/mink-server/README.md) | Web 工作区服务器：REST + SSE + 嵌入前端，`build.rs` 自动构建并嵌入 web 产物 |

---

## 参考项目

| 项目 | 说明 |
|------|------|
| [oh-my-pi](https://github.com/can1357/oh-my-pi) | 开源 CLI agent（Bun/TypeScript），Edit 工具的行号锚定与快照协议参考实现 |
| [bash-agent](https://github.com/lloydzhou/bash-agent) | 终端 Agent（Bash 优先），交互与工具执行参考 |

---

## 文档入口

从[文档总览](docs/start/overview.md)选择阅读路径：[快速开始](docs/start/quickstart.md)、[Rust 集成](docs/integration/rust.md)、[Python 集成](docs/integration/python.md)或[架构](docs/concepts/architecture.md)。配置、工具、协议与统计的完整定义统一在[参考文档](docs/reference/configuration.md)。

版本记录见 [CHANGELOG](CHANGELOG.md)，开发约束见 [AGENTS](AGENTS.md)。

维护者的运行时/TUI 测量方法及按日期存档的结果见[性能与验证](docs/development/performance.md)。

## 许可

[MIT License](LICENSE)
