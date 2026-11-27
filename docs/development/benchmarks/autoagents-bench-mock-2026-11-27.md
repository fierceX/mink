# 并发 Agent Loop（Mock 后端）性能记录：2026-11-27

> **Mock 测试结果**：本文仅记录 `mink-core` 使用进程内 Mock `LlmBackend` 的并发基准实测结果，不包含真实 LLM API 网络延迟、HTTP 序列化与 SSE 流式解析开销，也不作跨框架对比。维护记录，不进入官网发布产物。

测量方法与索引见[性能与验证](../performance.md)，测试入口为 [autoagents_bench.rs](../../../crates/mink-core/examples/autoagents_bench.rs)。

## 环境与口径

- **版本与提交**：`mink-core 0.10.0`（基于 `f0f53bd` + 工作树新增示例 `crates/mink-core/examples/autoagents_bench.rs`）
- **机器与系统**：Apple M4 Pro，macOS 15.7.7 / Darwin arm64
- **工具链**：`rustc 1.99.0 (b940084d7 2026-09-28)`，`--release` 优化构建
- **测试负载**：`--requests 50 --concurrency 10`
  - **Mock 后端**：进程内 `BenchMockBackend`（实现 `LlmBackend` trait），`llm-only` 模式返回 1 轮 JSON 文本；`tool` 模式执行 2 轮 ReAct（第 1 轮触发 `trip_data` 工具对 1,000 行内存数据聚合统计，第 2 轮返回最终 JSON）。
  - **生命周期**：`cold` 为每请求独立目录 `AgentRuntime::start()` → `run_turn()` → `shutdown()`；`warm` 为预热 10 个并发 `AgentRuntime` 实例复用处理 50 次请求。
  - **进程资源采样**：通过 macOS `/usr/bin/time -l` 采集单次运行的 `real` / `user` / `sys`、`maximum resident set size`（Peak RSS）与 `peak memory footprint`。

---

## Mock 测试结果（`--release`，50 请求 / 10 并发）

### 延迟、冷启动与吞吐量

| 场景 | Cold Start Avg | Cold Start P50 | Latency Avg | Latency P50 | Latency P95 | Latency P99 | Throughput | Wall Time | Deterministic OK |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `llm-only` / `cold` | `251.36 ms` | `248.48 ms` | `472.65 ms` | `473.90 ms` | `511.59 ms` | `538.94 ms` | `20.85 rps` | `2.40 s` | `50/50` |
| `tool` / `cold` | `261.70 ms` | `265.23 ms` | `503.19 ms` | `504.73 ms` | `584.52 ms` | `607.90 ms` | `19.53 rps` | `2.56 s` | `50/50` |
| `tool` / `warm` | `40.24 ms` | `40.48 ms` | `58.57 ms` | `64.07 ms` | `88.49 ms` | `92.02 ms` | `48.47 rps` | `1.03 s` | `50/50` |

> 注：`warm` 模式的 `Cold Start` 为预热阶段 10 个并发 `AgentRuntime` 的启动耗时统计；`cold` 模式的 `Cold Start` 为 50 次请求逐次 `AgentRuntime::start()` 耗时。

### 进程内存与 CPU（`/usr/bin/time -l`）

| 场景 | Real | User | Sys | Peak RSS（`maximum resident set size`） | Peak Memory Footprint | Voluntary / Involuntary Context Switches |
|---|---:|---:|---:|---:|---:|---:|
| `llm-only` / `cold` | `3.44 s` | `0.06 s` | `1.13 s` | `12.23 MB` (`12,828,672 B`) | `8.72 MB` (`9,142,680 B`) | `11,977 / 15,445` |
| `tool` / `cold` | `2.61 s` | `0.09 s` | `1.08 s` | `13.47 MB` (`14,123,008 B`) | `9.78 MB` (`10,256,792 B`) | `12,114 / 16,271` |
| `tool` / `warm` | `1.05 s` | `0.05 s` | `0.29 s` | `12.33 MB` (`12,926,976 B`) | `8.64 MB` (`9,060,760 B`) | `1,728 / 3,681` |

---

## 复现命令

```bash
cargo build --release -p mink-core --example autoagents_bench
/usr/bin/time -l ./target/release/examples/autoagents_bench --mode llm-only --lifecycle cold --requests 50 --concurrency 10
/usr/bin/time -l ./target/release/examples/autoagents_bench --mode tool --lifecycle cold --requests 50 --concurrency 10
/usr/bin/time -l ./target/release/examples/autoagents_bench --mode tool --lifecycle warm --requests 50 --concurrency 10
```
