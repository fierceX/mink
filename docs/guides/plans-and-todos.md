# 计划与待办

> 更新日期：2026-10-04

确认计划并使用 revision 管理执行进度。


## 操作起点

先要求模型提出计划，审阅草稿后在对话中确认。Plan 管理已确认的总体方向，Todo 管理逐项执行状态；两者有独立权威文件与事务，不要手工编辑运行时状态。

## 计划系统（Plan）

三个内置工具管理计划生命周期；三者都进入 resolved tool surface 时才加载计划工作流。

### 生命周期

```
LLM 提议 → PlanDraft(草稿) → 用户确认 → PlanConfirm → confirmed transition
  → TodoRead/TodoWrite/TodoAdvance 执行 → PlanClear → cleared transition
```

- `PlanDraft(content)`：保存或取消草稿（空 content = 取消）。已确认计划存在时拒绝创建。
- `PlanConfirm()`：原子 rename `plan.draft → plan.md`，成功工具结果后追加 confirmed transition。
- `PlanClear()`：删除 `plan.md`、清理残留 `plan.draft`，成功工具结果后追加 cleared transition。

PlanConfirm/PlanClear 不强制压缩。历史压缩后，活动 `plan.md` 以稳定的
`<active-plan-checkpoint>` 出现在摘要 checkpoint 之后。
执行阶段的 Todo 工具协议见 [工具系统 · Todo 协议](../reference/tools.md#todoread)。

## 下一步

查看[配置参考](../reference/configuration.md)、[工具参考](../reference/tools.md)或返回[文档总览](../start/overview.md)。

## Todo 操作

先让模型 `TodoRead` 取得最高可见 revision 和稳定 ID，再以 `TodoWrite` 增删或替换正文，以 `TodoAdvance` 转换状态；stale 后须重读。一个 batch 可以有多个 `in_progress`。权威完整快照在 `todos.json`，增量事件与 `<current-todos>` 是模型可见投影。恢复或压缩后缺失最新 revision 时只追加一次 TodoSync。

Plan 的 confirmed/cleared transition 与压缩后的 active-plan checkpoint 管总体计划；Todo 有独立 revision 和同步机制。完整参数见[工具参考](../reference/tools.md#todoread)，持久化事务见[状态与上下文](../concepts/state-and-context.md)。
