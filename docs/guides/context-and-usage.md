# 上下文与用量

> 更新日期：2026-10-04

观察长任务压力、手动压缩与用量。


## 操作起点

长任务中用 `/status` 查看上下文占比，用 `/compact` 手动压缩（任务空闲时）。压缩成功后完整历史仍可读取；若最小工作空间也装不下，当前轮会明确失败，可缩短下一条输入或使用更大窗口。

## 上下文压缩

### 显式策略

所有参数显式配置，不根据窗口大小推断档位。正常路径使用 LLM 滚动摘要：

参数与默认值见[配置参考](../reference/configuration.md#上下文参数)。

触发点取百分比阈值和 `max_context - context_reserve_tokens` 中较早者。auto 使用最近一次
同模型、同 immutable prefix、同压缩 generation 的 provider prompt usage 校准压力；
preflight 始终使用保守本地估算。
`max_context_tokens=0` 禁用 auto/preflight 压缩，保留 `/compact`。

### 流程

1. 从活跃窗口选择不破坏 tool call/result 配对的边界
2. backend 支持时复用上一 Agent 请求的 system/tools 与 dropped 历史公共缓存前缀
3. 可选降噪只处理未缓存后缀；无法对齐时降级为全量 reduced/raw 摘要输入
4. LLM 合并旧 `<compacted-summary>` 与新增 dropped 历史
5. 原子提交 `context-state.json`（临时文件 + rename）
6. 以 internal user `<compacted-summary>` 投影摘要并裁剪运行时缓存
7. `conversation.jsonl` 保持完整且只追加

### 防护

- 同一输入可重复压缩直到装得下或压无可压（不设次数上限；不降即停）；auto 保留最小收益检查（节省 < 10% 跳过），强制触发只要真能省就压；已超预算且严格切点不可用时退化为 `_cut=degraded`（折到最新安全边界之前）；摘要侧不可用（输入装不下 / 调用超时或重试耗尽 / 输出不合格 / 候选装不下）且请求仍超预算时，转为**不调用 LLM 的确定性应急 checkpoint**（`trigger=emergency`、`_mode=emergency`，有损摘录），能装入预算才继续，否则明确失败
- 摘要侧错误在来源处分类：`SummaryUnavailable`（可转应急）/ `CompactionInterrupted`（turn 记为 interrupted）；auto 摘要失败但原请求仍能发送时不阻断发送；取消、持久化闩锁、正式历史协议损坏照旧原样传播（不伪装成功）
- 候选发布前完整预算验收：新摘要 + 动态 checkpoint + 保留尾部必须装得进主请求预算，否则不提交（避免「摘要成功但请求反而变大」在下一层失败）
- Preflight 预判：发送前按实际形态估算，超预算先压缩
- 摘要使用独立输出预算，发送前校验能否装入窗口
- 软额度与动态预算：热尾部（`context_compact_tail_tokens`）与摘要输出（`context_compact_max_output_tokens`）是**目标而不是地板**——切点会按可用空间收紧热尾部，摘要请求的输出上限按「输入 + 输出 ≤ 窗口」动态下调（低于最小实用输出时转应急）。硬约束只有 `reserve < 窗口`；大软目标 + 小窗口可以初始化，行为由每次请求的实际预算决定
- 纠错 attempt 追加诊断后重新估算输入并重算 cap，不会发出装不下的摘要请求
- plan/todo 的派生展示（模型可见 checkpoint）按「输入预算/8」有损截短（头尾保留 + 省略标记）：巨大 plan.md 或超长 todo 正文不再单独撑满窗口；权威 `plan.md`、`todos.json`、revision 与计数不变，`TodoRead` 仍返回完整内容
- 降噪只作用于摘要请求，不修改完整历史
- Provider overflow 恢复：无可见输出时在同一 round 严格收缩投影，可重复恢复；不设次数上限，共享绝对期限

### 调优

```toml
# 1M DeepSeek
[context]
max_context = "1M"
context_compact_pct = 94
context_reserve_tokens = 64000
context_compact_tail_tokens = 256000

# 64k 私有模型（须降低响应预留；其余软目标可按任务调优）
# max_context = "64K"
# context_compact_pct = 65
# context_reserve_tokens = 12000
# context_compact_tail_tokens = 16000
# context_compact_max_output_tokens = 4096
```

有限窗口必须大于响应预留；热尾部与摘要输出为软目标，可按可用空间自动收紧。

## 下一步

查看[配置参考](../reference/configuration.md)、[工具参考](../reference/tools.md)或返回[文档总览](../start/overview.md)。
