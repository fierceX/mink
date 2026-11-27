<p align="center">
  <img src="docs/assets/mink-mark.svg" alt="Mink" width="48">
</p>

<p align="center">
  <a href="https://crates.io/crates/mink-core"><img src="https://img.shields.io/crates/v/mink-core.svg" alt="Crates.io"></a>
  <a href="./LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="MIT licensed"></a>
  <a href="docs/start/quickstart.md"><img src="https://img.shields.io/badge/rust-1.99%2B-blue" alt="Rust 1.99+"></a>
  <a href="https://pypi.org/project/mink-agent"><img src="https://img.shields.io/badge/pypi-mink--agent-blue" alt="Python SDK"></a>
</p>

<p align="center">
  <strong>Rust 原生 · 终端优先 · 可嵌入的 AI Agent Runtime</strong>
</p>

Mink 既可在终端（REPL / TUI）与 Web 工作台中直接运行，也可嵌入到 Rust 或 Python 应用中；多端共享同一套运行时内核、工具协议与会话语义。

---

## 核心优势

- **轻量与高性能**：Rust 原生低开销，`AgentRuntime` 毫秒级启动与单轮调度、低内存占用；结合非破坏式前缀缓存与 PTC 编排，显著降低长任务输入成本。
- **单内核多端复用**：Rust 通过 `mink::runtime` 进程内调用；Python SDK（`mink-agent`）与 Web 服务复用同一 Rust 内核。
- **长任务可控**：显式上下文压缩、跨轮 Goal 编排与原子化 Plan/Todo 状态管理，长链路运行不失控。
- **可靠编辑与沙箱**：Hashline / Replace 双模式编辑、Python 宿主 PTC（`RunCode`）以及 macOS / Linux / WASI 多级沙箱隔离。

---

## 快速开始

> 前置：Rust `1.99+`，设置 `DEEPSEEK_API_KEY`（或自定义 OpenAI-compatible 端点）。

### 1. 终端 CLI

```bash
cargo build --release
./target/release/mink -m flash --tui          # Full TUI（--tui=inline 为原生滚动，-i 为 REPL）
./target/release/mink -m flash --continue -i  # 恢复最近会话
```

### 2. Rust 嵌入

```toml
# Cargo.toml（需搭配 tokio、anyhow）
mink = { package = "mink-core", version = "0.10.0", default-features = false, features = ["runtime"] }
```

```rust
let rt = mink::prelude::AgentRuntime::start(
    mink::prelude::AgentOptions::new("/tmp/mink-home", ".")
        .with_api_key(std::env::var("DEEPSEEK_API_KEY")?)
        .with_model("flash"),
).await?;
println!("{}", rt.run_turn("hello").await?.text);
rt.shutdown().await?;
```

### 3. Python SDK

```bash
pip install mink-agent
```

```python
from mink_agent import AgentSession, SandboxConfig

with AgentSession(SandboxConfig(api_key="sk-...", read_dirs=["src"])) as session:
    print(session.run("scan this repo and summarize")["text"])
```

---

## 性能实测（v0.10.0）

在 Apple M4 Pro（macOS 15.7.7、`rustc 1.99.0`、`release` / `bench` 构建）下的实测数据：

| 类别 | 指标 | 实测值 | 口径 | 报告 |
|---|---|---|---|---|
| 运行时 | `runtime_start` P50 | `9.17 ms` | Mock | [v0.10.0](docs/development/benchmarks/v0.10.0-2026-11-27.md) |
| 运行时 | `tool_loop_50` 均值 | `0.873 ms/round`（总 `43.64 ms`） | Mock | [v0.10.0](docs/development/benchmarks/v0.10.0-2026-11-27.md) |
| 并发（50×10） | `tool / warm` 延迟 / 吞吐 | Avg `58.57 ms` · `48.47 rps` | Mock | [并发 Mock](docs/development/benchmarks/autoagents-bench-mock-2026-11-27.md) |
| 并发（50×10） | 进程 Peak RSS | `12.33 MB` | Mock | [并发 Mock](docs/development/benchmarks/autoagents-bench-mock-2026-11-27.md) |
| TUI 渲染 | 1 万条消息暖输入帧 P95 | `0.022–0.143 ms`（冷布局 `15.66–16.46 ms`） | Release | [v0.10.0](docs/development/benchmarks/v0.10.0-2026-11-27.md) |
| PTC 收益 | 工具回传 / 总 prompt | `−76.3%` / `−56.0%`（`tool` vs `off`） | 云 API | [PTC 对比](docs/development/benchmarks/ptc-modes-2026-11-27.md) |

---

## 仓库结构与文档

| 包路径 | 职责 |
|---|---|
| [crates/mink-core](crates/mink-core/README.md) | 核心库 `mink-core`（库名 `mink`）：运行时、工具、PTC、会话与沙箱 |
| [crates/mink-cli](crates/mink-cli/README.md) | 终端二进制 `mink` 与 SDK 协议二进制 `mink-core`（REPL / TUI） |
| [mink_agent](mink_agent/README.md) | Python SDK（wheel 内置 `mink-core` 二进制） |
| [crates/mink-server](crates/mink-server/README.md) | Web 工作台服务端（REST + SSE + 内嵌前端） |

- **文档**：[文档总览](docs/start/overview.md) · [快速开始](docs/start/quickstart.md) · [Rust 集成](docs/integration/rust.md) · [Python 集成](docs/integration/python.md) · [系统架构](docs/concepts/architecture.md) · [配置参考](docs/reference/configuration.md) · [性能与验证](docs/development/performance.md)
- **记录与规范**：[CHANGELOG](CHANGELOG.md) · [AGENTS](AGENTS.md) · [MIT License](LICENSE)
- **参考项目**：[oh-my-pi](https://github.com/can1357/oh-my-pi) · [bash-agent](https://github.com/lloydzhou/bash-agent) · [dsh](https://github.com/deepseek-ai/deepseek-harness)
