# 会话与运行中引导

> 更新日期：2026-10-04

恢复任务，在安全边界补充要求。


## 操作起点

先用 `--session my-fix` 命名任务。恢复后核对目录和已有回复；任务运行中直接 Enter 提交补充约束。`Accepted` 仅代表持久接收，`Added to context` 才表示正式历史已接管；最终结果不等于模型已遵循每条引导。停止后的未应用输入须明确续发。

## 会话管理

### Session layout

`MINK_HOME`（默认 `$HOME`）是 session 持久化根目录。不同入口使用不同 layout：

| Layout | 最终 session 目录 | 默认入口 |
|--------|-------------------|----------|
| `project` | `HOME/.mink/projects/<project_key>/<session_id>/` | CLI、裸 `mink-core` |
| `home` | `HOME/.mink/sessions/<session_id>/` | Python SDK |
| `direct` | `HOME/<session_id>/` | 显式配置 |
| `isolated` | `HOME/` | Rust `AgentOptions` |

选择建议：终端用户用 `project`（按项目隔离）；Python SDK 用 `home`；Rust API 按任务建独立目录用 `isolated`；共享 Mink 根目录用 `direct`。

### 目录结构

```
~/.mink/
├── history
└── projects/<project_key>/
    └── <session_id>/
        ├── conversation.jsonl ← 对话消息（JSONL 追加）
        ├── events.jsonl       ← 事件日志
        ├── session.json       ← 元数据：alias、title、时间戳
        ├── summary.txt        ← 压缩上下文快照
        ├── stats.json         ← Token 统计
        ├── context-state.json ← 首次压缩后生成
        ├── plan.md            ← 确认计划
        ├── plan.draft         ← 未确认草稿
        ├── todos.json         ← 首次 Todo 变更后生成
        ├── usage.jsonl        ← 首次 LLM 请求后生成
        ├── attachments/       ← TUI Ctrl+V 粘贴图片的暂存副本（内容寻址 PNG）
        └── artifacts/
            ├── index.jsonl
            └── <tool>-0001.txt
```

### 操作

```bash
mink -m flash --session my-fix "fix the bug"   # 命名会话
mink -m flash --session my-fix -i               # 恢复命名会话
mink -m flash --continue -i                     # 恢复最近会话
mink --list-sessions                            # 列出所有 session
```

`--session my-fix` 按 alias、完整 id、id 前缀和 title 匹配已有 session；匹配不到时创建新 session 并写入 alias。
值收紧：`--session` 后必须跟非空且不以 `-` 开头的参数（`--session` 裸用或 `--session -x`
直接报 missing value，避免与后续选项混淆）。
`--continue` 选择最近修改的 session，恢复时 replay 最近 10 轮 LLM 响应。

## 下一步

查看[配置参考](../reference/configuration.md)、[工具参考](../reference/tools.md)或返回[文档总览](../start/overview.md)。
