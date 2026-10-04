# 用量与统计

> 更新日期：2026-10-04

请求 journal、Token 分区与缓存口径。

## Token 用量

每轮 `run_turn()` 结束后，`TurnOutcome` 携带本轮所有 LLM 请求的 Token 消耗。

### 字段说明

| Rust 字段 | Python 字段 | 说明 |
|-----------|------------|------|
| `billing_turn_id` | `billing_turn_id` | 本轮稳定标识；Agent、压缩、子代理共用 |
| `usage_records` | `usage_records` | 每笔 LLM 请求明细 |
| `usage` | `usage` | `UsageSummary` 汇总：请求数、attempt 数、Token |
| `session.usage_path` | `usage_path` | `usage.jsonl` 路径 |

### UsageSummary

| 字段 | 说明 |
|------|------|
| `request_count` | 本轮 usage 记录数（每次可观察 attempt 独立结算） |
| `reported_request_count` | 返回 usage 的请求数 |
| `unreported_request_count` | 未返回 usage 的请求数 |
| `attempt_count` | 请求尝试总数（含首次调用；自定义 backend 聚合值原样累计） |
| `tokens` | [TokenUsage](#tokenusage) |

### TokenUsage

| 字段 | 说明 |
|------|------|
| `input_tokens` | 输入 Token（已减缓存命中） |
| `cache_read_tokens` | 缓存读取 Token（供应商报告） |
| `cache_creation_tokens` | 缓存创建 Token（供应商报告） |
| `output_tokens` | 输出 Token |

### 采集路径

```text
Turn / Compaction / SubAgent → MeteredStream → usage.jsonl
→ OrchActor::finish_usage() → TurnOutcome
```

Agent 工具循环、自动压缩、子代理共享同一 `billing_turn_id`。手动压缩使用 `operation-*`。

### 费用说明

费用统计已移除：上游模型单价随时段（高峰/空闲）与官方调价变动，本地无法准确持续计价。
`UsageRecord.cost_nano_cny` 作为兼容字段保留：已上报记录恒为 `0`，未上报记录为 `null`；
`UsageSummary` 不再包含费用字段，终端/服务端不再展示费用。

### usage.jsonl 格式

```json
{"version":1,"billing_turn_id":"turn-...","request_id":"request-...",
 "kind":"agent","origin_session_id":"session-...","model":"deepseek-v4-flash",
 "attempt_count":1,"status":"reported",
 "tokens":{"input_tokens":100,"cache_read_tokens":40,"cache_creation_tokens":0,"output_tokens":50},
 "cost_nano_cny":0,"reason":null,"completed_at":"2026-06-18T00:00:00Z"}
```

### Rust 库中访问

```rust
use mink::prelude::{AgentOptions, AgentRuntime};

let rt = AgentRuntime::start(
    AgentOptions::new("/tmp/mink-session", ".")
        .with_api_key(std::env::var("DEEPSEEK_API_KEY")?)
        .with_model("flash"),
).await?;
let outcome = rt.run_turn("解释这段代码").await?;

println!("input: {}, output: {}",
    outcome.usage.tokens.input_tokens, outcome.usage.tokens.output_tokens);
for record in &outcome.usage_records {
    println!("  {}: kind={:?}, status={:?}", record.request_id, record.kind, record.status);
}
rt.shutdown().await?;
```

### Python SDK 中访问

```python
from mink_agent import AgentSession, SandboxConfig

session = AgentSession(SandboxConfig(api_key="sk-...", read_dirs=["."]))
result = session.run("解释这段代码")
print(f"input: {result['usage']['tokens']['input_tokens']} tokens")
for record in result['usage_records']:
    print(f"  {record['request_id']}: kind={record['kind']}")
session.close()
```

### CLI 中查看

```bash
mink -m flash --print "hello" | jq 'select(.type=="final") | {billing_turn_id, usage}'
cat ~/.mink/projects/<project_key>/<session_id>/usage.jsonl | jq -c
```

## 状态栏统计口径

`T` 是真实用户 turn 数，`R` 是已结算 LLM 请求数；`I` 显示未缓存输入加缓存读取，`O` 为输出，`C` 为当前上下文与上限占比。缓存命中率为 `floor(read × 100 / (input + read + creation))`，只将 read 计作命中；缺失分区或旧统计显示未知，不以 0 代替。`B` 为信念，不是任务正确率。Web diagnostics 与 TUI 使用相同 StatsSnapshot，activity 中的等待时间从可靠事件恢复。

reported/unreported 都保留请求明细；计量完整不等于模型请求或任务成功。费用由宿主另行计算。

## UsageRecord 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `version` | `u32` | journal 版本，当前 1 |
| `billing_turn_id` | `String` | 主用户 turn 或手动 operation 归属 |
| `request_id` | `String` | 请求记录稳定 ID |
| `kind` | `UsageKind` | agent / compaction / sub_agent |
| `origin_session_id` | `String` | 实际发起请求的会话 |
| `model` | `String` | 实际模型名 |
| `attempt_count` | `u32` | 可观察尝试数，含首次；自定义 backend 可上报聚合值 |
| `status` | `UsageStatus` | reported / unreported |
| `tokens` | `Option<TokenUsage>` | 未上报为 null，不伪造 0 |
| `cost_nano_cny` | `Option<u64>` | 费用兼容字段，已上报 0 / 未上报 null |
| `reason` | `Option<String>` | 未上报诊断原因，可空 |
| `completed_at` | `String` | 结算时刻字符串 |
