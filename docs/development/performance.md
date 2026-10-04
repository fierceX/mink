# 性能与验证

> 更新日期：2026-10-04；维护文档，不进入官网发布产物。

此页维护测量方法与结果索引。架构说明运行机制，集成页说明 API 契约；性能数字按日期保存在独立记录中，避免随实现说明更新而失去测试环境和版本边界。

## 测量结果

| 记录 | 内容与证据 |
|---|---|
| [运行时：2026-10-03](benchmarks/runtime-2026-10-03.md) | Apple M4 Pro 全量矩阵、独立密度测试、部分失败/中断记录；脱敏 JSON 和日志随仓库保存 |
| [TUI：2026-10-04](benchmarks/tui-2026-10-04.md) | 渲染、可变尾部、PTY/tmux/SSH 输入延迟和正确性检查；保留原维护者的测量口径与限制 |

历史报告保留其原始 commit 与 dirty 标记，不作为当前 HEAD 的性能承诺。不同用例顺序、feature、工具链、fd 上限和 RSS 采样方式的结果不能直接归因于代码改动。

## 运行时基准

运行时性能与资源占用由 `crates/mink-core/benches/runtime_bench.rs` 测量，
所有 LLM 流量使用可注入的 mock backend，因此数字反映 mink 自身开销（不含模型延迟）。

```bash
cargo bench -p mink-core --bench runtime_bench              # 全量矩阵
cargo bench -p mink-core --bench runtime_bench -- --quick    # 缩减矩阵
cargo bench -p mink-core --bench runtime_bench -- --list     # 列出用例
./scripts/bench-runtime.sh                                   # 记录机器/OS/commit/rustc 并留存报告
make bench-quick                                             # 同上，缩减矩阵
```

用例矩阵：

| 用例 | 回答的问题 |
|------|-----------|
| `process_start` / `process_start_cli` | benchmark 空子进程 / CLI `--version` 的 spawn 到退出开销 |
| `runtime_start` / `session_create` | 已有 session 重启 / 全新 session 成本 |
| `idle_1/100/1000` | 服务化 session 密度（RSS、线程数、创建/关闭耗时） |
| `mock_turn_no_tool` / `mock_turn_1_tool` | Agent Loop 固定开销 / 工具派发开销 |
| `sse_1k/10k` | 内存 mock 事件流交付（events/s、pending backlog；不经过网络 SSE 解析） |
| `replay_1k/10k/100k` | 会话恢复（读盘 + 首请求投影） |
| `turns_500/2000` | 长会话内存曲线 |
| `compact_10/100` | 达到目标摘要请求数的整段工作负载成本与内存 |
| `concurrent_32/64/128` | 新建 runtime + 一轮 mock turn + shutdown 的并发吞吐 |
| `subagent_fanout_2/4/8` | 子代理扇出调度 |
| `sandbox_start` | 隔离 worker 成本（sandbox re-exec + runtime + 回合） |

指标口径（平台名称记录在 JSON `meta`，工作负载指标在 `samples[].extra`）：

- 延迟为重复集合的 P50/P95/min/max/mean，单位 `ms`；重复次数由 `--reps` 覆盖。
- 内存：Linux 用 `/proc/self/status` 的 `VmRSS`；macOS 用 `ps -o rss=`（当前值）；两平台均以 20ms 间隔采样当前 RSS 峰值。macOS 的 RSS 会高估真实 footprint（分配器保留），容量评估建议
  结合 `vmmap` 复核。
- 用例在同一进程内顺序执行，内存为进程累计值；每个用例的 `extra` 记录各自基线
  （`rss_before_kb`）与峰值，长会话用 `rss_curve` 观察趋势。

`scripts/bench-runtime.sh` 把机器、OS、commit（dirty 标注）、rustc/cargo 版本、RSS 口径
与 fd limit 写入报告头，并在 `target/bench/` 留存同名 JSON（结构化）与 txt（人类可读）。

### 判定与限制

- `--case` 按名称前缀选取，可用逗号列出多个前缀；`--reps` 只覆盖使用重复集合的用例，须大于 0。密度、长会话、SSE、压缩与 fanout 保留其固定工作负载。
- 当前报告格式为 v2：`selected_cases`、`skipped_cases`、`failures` 明确区分选择、不可用和失败。失败后仍保存已有样本/诊断并返回非零；密度创建不足、沙箱未完成、子代理未执行均不能算完整通过。旧版存档不补造这些字段。
- CLI 启动用例使用 `MINK_BIN` 或 `target/release/mink`，不存在时显式跳过；需要该项时先构建匹配版本的 release CLI。它测 `--version`，不代表 TUI 启动。
- `compact_*` 的 `ms_per_compaction` 是包含启动和普通 turn 的整段耗时除以摘要请求数，不是纯摘要延迟，也不是逐次 checkpoint 发布耗时。
- `n=1` 时 p50/p95 同为唯一观测；长会话和并发的 n 是同一次运行里的 turn/任务数，不是多次独立运行。比较回归须重新进行独立重复测量。
- 日志中的诊断事件丢失与 `AgentEventStream` 进度积压是两条通道；后者 `backlog_dropped=0` 不能证明磁盘诊断事件完整。Linux/macOS 之外的资源计数未实现，零占位不能解释为零占用。
- Linux `fds_*` 统计 `/proc/self/fd` 条目，macOS 统计 `lsof -p` 输出行（含非数字映射项）；后者是占用代理，比较时使用相同方式及前后增量，不混用两平台的绝对计数。

### 验证套件

```bash
make bench-check       # mock 工作负载、错误回执、缺 CLI 跳过、低 fd 上限部分失败
make bench-quick       # 缩减性能矩阵；仍会写磁盘、创建多会话
```

`bench-check` 不设硬性延迟/RSS 门槛，避免将机器负载噪声当作功能回归。完整矩阵按需运行，不在常规 CI 中测量千会话和十万条历史。

## TUI 测量

[TUI 实现与维护](tui.md)定义缓存、阅读锚点、分批绘制和 Inline 释放边界；[TUI 测量记录](benchmarks/tui-2026-10-04.md)保存特定日期的数字、操作脚本和限制。
`scripts/bench-tui.py` 测合成 TestBackend 绘制，`scripts/verify-tui-pty.py` 测输入到终端输出和退出恢复。两者不能与运行时 mock turn 或 CLI `--version` 时间混合比较。

## 结果入库规则

日常输出继续写 `target/bench/`，不批量提交临时产物。选定结果时新增 `benchmarks/<主题>-<日期>.md`，附同目录 `data/` 中的结构化报告与必要日志，记录机器、OS、工具链、源码版本、配置、单位、样本含义和失败/跳过情况；旧记录保持不变。

移除用户名、绝对项目/临时目录、主机身份、密钥和真实模型轨迹；保留数值及警告，不删除失败来改善结论。脱敏规则和原文件/入库文件的 SHA-256 随证据登记。没有保存原始样本时须明确说明，不从汇总值重建不存在的观测。

## 与其他文档的关系

[架构](../concepts/architecture.md)、[运行机制](../concepts/runtime.md)与[Rust 集成](../integration/rust.md)引用此入口查找容量测量；[用量参考](../reference/usage.md)定义模型计量，性能报告只引用该口径。维护者从根 README / AGENTS 进入本页，变更日志记录套件与修复，不承载整张性能表。

## 套件纳入验证：2026-10-04

此次验证针对当前工作区的修正版，不重写 2026-10-03 的历史基线。`make bench-check` 的 5 项回归通过：归档完整性、目录/非法参数、mock 工作负载、错误退出与低 fd 上限的部分密度失败。core/CLI 测试、fmt、workspace clippy 及 feature matrix 通过；精简 CLI 构建仍输出已有 `strip_ansi` dead-code 警告。

另用 mock backend 执行 2/4/8 个子代理和两次真实 macOS sandbox re-exec：子请求数分别为 2/4/8、工具错误 0；沙箱请求/成功 2/2。此运行与其他构建同时进行，仅验证行为，不作为新的延迟基线。

[JSON](benchmarks/data/runtime-2026-10-04-smoke/runtime-e7fb6f2+dirty-20261004T153004Z.json)、[日志](benchmarks/data/runtime-2026-10-04-smoke/runtime-e7fb6f2+dirty-20261004T153004Z.txt)和[来源/源码摘要](benchmarks/data/runtime-2026-10-04-smoke/provenance.json)保留 `e7fb6f2+dirty`，未改为纳入后的提交号。
