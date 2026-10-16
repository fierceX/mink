# 故障排查

> 更新日期：2026-10-10

定位模型、编辑、恢复和持久化失败。


## 操作起点

先保存错误前缀、会话 ID 和失败步骤，再查看 `events.jsonl` / `usage.jsonl`。认证或模型错误检查 provider；`ArgumentInvalid` 让模型重发参数；`persistence fault` 应停止修改并核对磁盘状态，不按旧 revision 重试。

## 故障排查

```bash
# 检查 API key
curl -H "Authorization: Bearer $DEEPSEEK_API_KEY" https://api.deepseek.com/v1/models

# verbose 模式
mink -m flash -v "hello"

# 扩大上下文窗口避免溢出
mink -m flash --config $'[context]\nmax_context="1M"' -i

# 查看 session 列表
mink --list-sessions

# 查看事件日志中的信念变化
grep '"belief"' events.jsonl | jq '{type, belief}'

# 查看前缀快照（system prompt + tools 指纹，用于离线重建请求前缀）
grep '"prefix_snapshot"' events.jsonl | jq '{version, fingerprint, dependency_fingerprint}'

# 查看请求级缓存明细（缓存命中率可算：兼容拼写路径已通，原生拼写为兜底）
cat usage.jsonl | jq '{input_tokens, cache_read_tokens, cache_creation_tokens}'

# 查看注入历史（轨迹证据注入落在 conversation.jsonl）
grep '\[trajectory\]' conversation.jsonl
```

### 缓存指标观测

DeepSeek 的 context caching 用量通过 OpenAI 兼容字段回传：
`prompt_tokens_details.cached_tokens`（DeepSeek 返回的就是这个兼容拼写，
不是原生 `prompt_cache_hit_tokens`）。mink 的解析链以兼容拼写优先、原生拼写
兜底（`prompt_cache_hit_tokens`）。三类输入统计是互斥分区：
`input_tokens = prompt_tokens - cache_read_tokens - cache_creation_tokens`。

**缓存命中率可正常计算**：命中率 = 累计 `cache_read_tokens` / 累计
(`input_tokens + cache_read_tokens + cache_creation_tokens`)；标题栏 `I:` 字段括号内
即该比率。前缀命中要求 system prompt、
tools 与历史消息前缀字节级稳定。计划确认/清除通过 append-only transition 表达，
压缩后的活动计划 checkpoint 位于稳定动态前缀中；
`prefix_snapshot` 事件（events.jsonl）可用于离线重建请求前缀并归因
"这次为什么 cache miss"。

### Goal 相关现象

- 目标一直 active 但不动：终端在 UI 就绪后自动接起；server 不会自动续跑，需在界面显式「恢复」（先预约容量许可，再由 core 启动唯一 run）；恢复出的目标被拒绝时不会发出 provider 请求。
- 暂停/取消报错而不是显示已保存：停止意图无法写盘时 run 先进入 stopping，执行实际退出后发布 faulted（`goal stop intent could not be persisted`）。核对磁盘/权限后重开会话恢复，不要反复重试同一命令。
- 模型自报 blocked 未结束目标：同一阻断（按报告 `summary`/`next_step` 指纹识别）需连续达到 `blocked_after_consecutive_rounds`（默认 3）轮；不同阻断会重置计数，可见 `blocked_rounds`（连续 blocked 轮数）与预算视图的阻断门槛（同一阻断连续轮数）。
- 检查器验收不可用：部署未配置 `MINK_GOAL_CHECK` 时，`GET /api/sessions/{id}/goal` 的 `verifier` 为 false，工作台禁用该选项并隐藏 `/goal verified`。

## 下一步

查看[配置参考](../reference/configuration.md)、[工具参考](../reference/tools.md)或返回[文档总览](../start/overview.md)。
