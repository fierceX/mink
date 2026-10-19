# 请求投影重构：2026-10-19

> 维护记录，不进入官网发布产物。数字来自单次 benchmark 运行，不是发布承诺。

## 目的

验证每个最终请求的请求准备从「重复投影/转换/序列化」收敛为一次 `RequestProjection` 后，长工具循环的实际开销变化。`tool_loop` 用例不触发压缩，因此每轮构建一次；压缩轮会在提交后重建。只测 mink 自身开销：LLM 流量使用可注入的 mock backend。

## 源码与工具链

- 基线：`7753952`（clean HEAD），bench 源码复制了同一份 `runtime_bench.rs` 用例。
- 重构：同一工作树（dirty），`git diff` SHA-256 `a13876271f69b8c08c8abcc0efbc4818096f1e169de7af72e611242379af6083`；未提交状态不宣称对应某个 commit。
- 机器：Apple M4 Pro，12 核，24GB；macOS 15.7.7 / Darwin 24.6.0 / aarch64。
- rustc 1.99.0（b940084d7），cargo bench 优化构建。
- 每个用例 n=1（单次 `run_turn`），p50/p95 都是同一次观测；不提供尾延迟证据。

## 用例

新增 `tool_loop_20` / `tool_loop_50`：单用户输入内由 mock backend 驱动 20/50 个工具 round。工具在 Read/Glob/Grep 之间轮换，避免 StormBreaker 将同工具重复调用识别为抑制；`tool_loop_50` 将 `max_turns` 提到 51 以覆盖全部 round。

```bash
cargo bench -p mink-core --bench runtime_bench -- --case tool_loop_20
cargo bench -p mink-core --bench runtime_bench -- --case tool_loop_50
```

## 结果

| 用例 | 基线总耗时 | 重构总耗时 | 基线 ms/round | 重构 ms/round | 变化 |
|---|---:|---:|---:|---:|---:|
| `tool_loop_20` | 27.82 ms | 22.59 ms | 1.391 | 1.129 | −18.8% |
| `tool_loop_50` | 81.46 ms | 64.57 ms | 1.629 | 1.291 | −20.7% |
| `request_build_800k` | 14.02 ms | 12.72 ms | — | — | −9.3% |

`extra` 记录（同一次运行）：

| 用例 | 基线 RSS 峰值 | 重构 RSS 峰值 |
|---|---:|---:|
| `tool_loop_20` | 21,040 KiB | 22,496 KiB |
| `tool_loop_50` | 20,896 KiB | 21,776 KiB |
| `request_build_800k` | 40,032 KiB | 35,408 KiB |

`tool_loop_50` 的 RSS 峰值反而更高，说明单次运行与分配器保留的噪声不可忽略；不据此声称内存改善。耗时也只反映该机器、该 mock 负载与该窗口大小。

结构化报告：`tool_loop` 的[基线 JSON](data/request-projection-2026-10-19/baseline.json)（SHA-256 `6f0042e341ee18f0d0cf8cb83b1c5d55e7b030f9ffe13b51f2c8687fc4ea3264`）与[重构 JSON](data/request-projection-2026-10-19/after.json)（SHA-256 `59e10ce3f498679b68c2773b5e6174669ea8db849507296bef6cbddc034cc7ec`）；`request_build_800k` 的[基线](data/request-projection-2026-10-19/baseline-800k.json)（`002061b91ce87d1cd1682efdcd764a755eb58ef02286a66ae17ba5d2e2f109c5`）与[重构](data/request-projection-2026-10-19/after-800k.json)（`3305535e79ba706a83cb130e89f8742fee96e7b6b8f83955c4c0d32d7a712f58`）。来源与限制见[provenance.json](data/request-projection-2026-10-19/provenance.json)。两份 JSON 的 `meta.machine`/`commit`/`rustc` 为 null（bench harness 未填），机器与工具链以本页记录为准。

`request_build_800k` 的另一次相邻运行得到基线 16.16ms / 重构 13.31ms（−17.6%）；单次观测的离散度明显，不能把 9.3% 或 17.6% 当作精确提升幅度。

## 解释边界

- 这是短消息、无图片历史；`RequestProjection` 的结构性收益（每个最终请求从约 8 次 messages 深拷贝/10 次全量序列化降到约 3 次 messages 序列化（估算、body、cache 哈希）+ 2 次 tools 序列化 + 1 次转换）并不能直接按比例映射到墙钟时间。
- 用例仍包含工具执行、持久化、事件日志和信号处理；请求准备只是总耗时的一部分。
- 未运行 `request_build_940k`、图片首轮或真实 provider；这些不在此记录中。
- 基线 bench 源码与重构工作树共用同一份 `runtime_bench.rs`，但 Cargo 构建路径不同；不将绝对数值与其他日期的矩阵混比。
- 本记录对应重构提交 `89556b6`。后续 cache projection 哈希对齐修复把哈希取值来源从完整请求改为 backend 返回的 provider 可见前缀，并调整了哈希时点；每个最终请求的投影/转换/序列化次数不变，本次 benchmark 未重跑。
