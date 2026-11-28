# Python 程序化工具调用（PTC / `RunCode`）

Mink 从 `v0.10.0` 起支持 **Python 宿主 Programmatic Tool Calling（PTC）**：模型可以在单次工具调用中编写一段支持顶层 `await` 的 Python 3 程序，通过 `RunCode` 在沙箱内批量调用、过滤并聚合多个工具结果，再将精简后的结构化摘要回传给模型。

完整工具参数与配置字段定义见[工具参考](../reference/tools.md#runcodepython-ptc)与[配置参考](../reference/configuration.md#ptc)。

---

## 运行模式（`PtcMode`）

| 模式 | 说明 | 适用场景 |
|---|---|---|
| `off`（默认） | 隐藏 `RunCode` 与 `<ptc-sdk>` | 需要完全保持传统逐轮直调行为 |
| `tool`（别名 `hybrid`，推荐） | `RunCode` 与普通工具同时暴露 | 单步操作直调，循环过滤/多步编排走 `RunCode` |
| `exclusive` | 仅保留 `RunCode` 与规划/目标/委派等控制工具直调，工作区数据与命令工具全部经 `await tools.<Name>(...)` 调度 | 最大化减少工具 schema 与中间输出占用 |

---

## 开启方式

### 终端 CLI / 配置文件

```bash
# 命令行临时开启
./target/release/mink -m flash --ptc tool -i

# 或通过环境变量
MINK_PTC_MODE=tool ./target/release/mink -m flash -i
```

在 `.minkrc` 中持久化配置：

```toml
[ptc]
mode = "tool"                 # off | tool | exclusive
backend = "subprocess-python" # subprocess-python | wasi-python（需启用 python-sandbox feature）
```

### Rust 嵌入与 Python SDK

- **Rust**：通过 `AgentOptions::with_ptc_mode(PtcMode::Tool)` 与 `with_ptc_*` 系列方法配置，详见 [Rust 集成](../integration/rust.md)。
- **Python SDK**：通过 `SandboxConfig(ptc_mode="tool", ...)` 配置，详见 [Python 集成](../integration/python.md)。

---

## 编排能力与安全边界

- **内置异步 API**：运行时预注入 `tools`（支持 `await tools.<Name>(...)`）、`await gather(*aws)`、`inspect(*values)`、`ToolCallError` 与 `Image` 句柄，同时兼容 `import tools`。
- **并发与串行屏障**：只读工具按 `max_parallel_sub_calls` 滚动并发执行；写入与命令工具自动在读写锁屏障上串行化。
- **结构化回传**：推荐用 `inspect(...)` 返回关键摘要而非全量 `print(...)`；超长输出自动落盘为 `artifact://`，多张图片仅在 `inspect(img)` 时计入图片预算。
- **沙箱隔离与解释器要求**：默认通过受控 `python3` 子进程经独立 FD 3 通道通信（不挂载 `$HOME`），要求 **`python3` ≥ 3.10**（启动校验会主动探测版本，低于 3.10 直接报错并给出诊断；macOS 15 自带 `/usr/bin/python3` 为 3.9.6，需安装 Python 3.10+ 并通过 `[ptc].python_executable` 指定或确保其位于 `PATH` 前部，亦可启用 `wasi-python` 后端）；开启 `python-sandbox` feature 时支持 WASI CPython 后端及线性内存上限（`max_memory_mb`，默认 `256`）。禁止在 `RunCode` 内递归调用 `RunCode`、`Python` 或规划/控制类工具。

---

## 实测收益（云 API 对比）

同一分析任务下关闭 PTC（`off`）与开启 PTC（`tool`）的实测均值对比（详见 [PTC 模式收益实测](../development/benchmarks/ptc-modes-2026-11-27.md)）：

| 指标 | 关闭 PTC（`off`） | 开启 PTC（`tool`） | 变化 |
|---|---:|---:|---|
| 工具结果回传总量 | 79.7KB | **18.9KB** | **−76.3%** |
| 非缓存 input | 40.4k | **14.9k** | **−63.3%** |
| **总 prompt** | 185.3k | **81.4k** | **−56.0%** |
| `output`（含代码约 280 tokens） | 2,382 | 2,713 | +13.9% |
| 云 API 墙钟 / 请求次数 | **16.8s** / 6.0 次 | 18.2s / 7.0 次 | +8.5% |
