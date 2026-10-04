# mink-agent 使用文档

> 更新日期：2026-10-05

## 简介

mink-agent 是 [Mink](https://github.com/fierceX/mink) 的 Python 封装。SDK 专用的 `mink-core` 二进制内置在 pip 包中，无需额外安装。

```bash
pip install mink-agent
```

发布工作流提供 macOS arm64 和 Linux x86_64（GNU / musl）的 wheel，安装时按 wheel 标签选择兼容系统版本。macOS Intel 与 Linux aarch64 当前没有预构建 wheel；需要从源码构建 SDK 二进制并验证目标环境，详见[平台与构建](../docs/integration/python.md#支持平台)。

## 快速开始

```python
from mink_agent import AgentSession, SandboxConfig

config = SandboxConfig(
    api_key="sk-...",                            # 或设置 DEEPSEEK_API_KEY 环境变量
    read_dirs=["/path/to/project/src"],           # agent 可读取的目录
    write_dirs=["/path/to/project/src"],          # agent 可写入的目录
    signal_policy="full",                           # off/evidence/state_ops/restart/full
    stream_events=True,                            # 可选：是否输出过程事件
)

session = AgentSession(config)
result = session.run("把 src/handler.rs 重构成使用 Result 类型")
print(result["text"])
session.close()
```

### 单次快捷调用

```python
from mink_agent import quick_run

result = quick_run(
    "解释这段代码",
    read_dirs=["/path/to/project"],
    api_key="sk-...",
)
print(result["text"])
```

## 阅读入口

[Python 集成](../docs/integration/python.md)维护 AgentSession API、事件与会话复用；[配置参考](../docs/reference/configuration.md#sandboxconfig-配置项)维护 SandboxConfig 完整字段；[安全指南](../docs/guides/security.md)说明各平台的隔离边界。

每次 `run()` 启动一个新的 `mink-core --agent-jsonl` 子进程，通过相同 home/session ID 复用磁盘状态；同一个实例不支持并发调用。
