# 故障排查

> 更新日期：2026-10-04

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

## 下一步

查看[配置参考](../reference/configuration.md)、[工具参考](../reference/tools.md)或返回[文档总览](../start/overview.md)。
