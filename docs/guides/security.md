# 沙箱与安全

> 更新日期：2026-10-04

限定访问范围、审批和平台隔离。


## 操作起点

先明确工作目录与读写白名单，再选择平台支持的隔离方式。验证允许目录可读、禁止目录不可写；Rust 同进程 runtime 不自动隔离宿主，完整进程隔离需要 worker。

## 沙箱与安全

### 进程级沙箱

通过 OS 原生工具（Linux nsjail/bubblewrap、macOS sandbox-exec）包裹 Mink 进程。

`.minkrc` 的 `[sandbox]` 段：

```toml
[sandbox]
enabled = true
backend = "auto"                 # nsjail | bwrap | sandbox-exec | off
read_dirs = ["src", "tests"]
write_dirs = ["src"]
allow_network = true

# 仅 Linux nsjail cgroup：
max_memory_mb = 1024
max_pids = 64
timeout_secs = 600
```

### 平台差异

| 功能 | Linux nsjail | Linux bwrap | macOS sandbox-exec |
|------|-------------|-------------|---------------------|
| 写入限制 | ✅ 内核强制 | ✅ 内核强制 | ✅ 内核强制 |
| 读取限制 | ✅ 内核强制 | ✅ 内核强制 | ❌ 不生效 |
| 网络隔离 | ✅ namespace | ✅ namespace | ❌ 不生效 |
| memory/pids 资源限制 | ✅ cgroup | ❌ 不执行 | ❌ 不执行 |
| 后台自动启用 | ✅ | ✅ | ✅（写入限制） |

### 启动机制

Mink 检测 `[sandbox] enabled = true` 后自动通过 `exec()` 装入沙箱，设置 `MINK_SANDBOXED=1` 防无限递归。
不可用的后端会 fatal 退出，不会静默降级。

### PythonSandbox（CPython WASI 沙箱）

在 wasmtime + CPython WASI 中执行 Python，WASI 级进程隔离，无网络、无子进程、无 C 扩展。

**准备工作：** 下载 [cpython-wasi-build](https://github.com/brettcannon/cpython-wasi-build) 发布的 Python 3.13+ WASI 包：

```bash
curl -sL "https://github.com/brettcannon/cpython-wasi-build/releases/download/v3.13.13/python-3.13.13-wasi_sdk-24.zip" -o python-wasi.zip
unzip python-wasi.zip -d cpython-wasi
```

项目结构：
```
cpython-wasi/
├── python.wasm          # ~29MB
├── lib/python3.13/
└── LICENSE
```

配置：

```toml
[tools]
enabled_tools = ["Read", "Write", "Bash", "PythonSandbox"]

[sandbox_python]
wasm_path = "cpython-wasi/python.wasm"
stdlib_dir = "cpython-wasi"
timeout = 30
read_dirs = ["./data"]
write_dirs = ["./output"]
package_dirs = ["./packages"]
```

`enabled_tools` 是精确列表；`PythonSandbox` 必须显式列出。

路径权限规则：
- 仅在 `read_dirs` / `write_dirs` 中声明的目录可访问
- `write_dirs` 优先于 CWD 只读
- 路径穿越（`../`）无法逃逸 preopen 范围

## 8. 部署注意

- **单二进制分发**：`target/release/mink-server` 即可（含前端），无需 node/npm
- **重启更新前端**：嵌入产物随 `cargo build` 更新——修改前端后需**重新构建 mink-server** 再重启
- **多进程一致性**：lease 锁文件是跨进程稳定对象，不依赖 PID 存活判断；两个 server 进程
  同时操作同一会话时按文件锁顺序串行，冲突返回 409
- **超时保护**：`MINK_SERVER_TURN_TIMEOUT`（默认 1200s）防止 LLM/工具挂起卡死 running 状态；
  超时后进入 forced terminal 并关闭该会话 runtime
- **安全**：单用户部署假设；`sk-fake`/受限 key 可用于测试环境

## 下一步

查看[配置参考](../reference/configuration.md)、[工具参考](../reference/tools.md)或返回[文档总览](../start/overview.md)。
