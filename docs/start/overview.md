# 文档总览

> 更新日期：2026-10-04

Mink 是轻量 Rust AI coding agent：终端、Web、Rust 进程内 runtime 与 Python SDK 共用执行内核。按目标选择一条路径；第一次任务无需先读设计文档。

## 第一次运行

[快速开始](quickstart.md) → [终端操作](../guides/terminal.md)或 [Web 工作台](../guides/web.md) → [会话与引导](../guides/sessions-and-guidance.md) → [实跑案例](../examples/guided-env-parser.md)。

## 嵌入已有系统

[Rust 集成](../integration/rust.md)是进程内调用；[Python 集成](../integration/python.md)通过子进程复用会话。自定义模型、知识库和初始化逻辑见[扩展契约](../integration/extensions.md)。

## 理解运行机制

[架构](../concepts/architecture.md) → [执行与取消](../concepts/runtime.md) → [持久化与上下文](../concepts/state-and-context.md) → [工具与能力](../concepts/tools-and-capabilities.md) → [恢复与信号](../concepts/recovery-and-signals.md)。图片的单次消费单列在[读图机制](../concepts/images.md)。

## 查阅完整定义

参数、默认值及优先级看[配置参考](../reference/configuration.md)，工具 schema 看[工具参考](../reference/tools.md)，数据字段看[机器协议](../reference/protocols.md)与[HTTP API](../reference/http-api.md)，计量口径看[用量参考](../reference/usage.md)。维护记录仅在仓库的 development 目录，版本记录以根目录 [CHANGELOG](../../CHANGELOG.md)为准。

## 全部公开文档

官网从统一文档清单生成本节目录。仓库读者可查看 [manifest.json](../manifest.json)，其顺序即阅读顺序。
