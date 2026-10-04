# 恢复与信号

> 更新日期：2026-10-05

有界格式恢复、请求重试和信念反馈。

## 维修流水线

维修机制分布在流式协议归一化、turn 级回收和工具执行门禁三个边界：

```text
SSE tool-call 候选组装与严格校验
→ Turn 级 Scavenge 回收与结构化校验
→ resolved ModelToolSurface gate
→ StormBreaker
→ ToolExec dispatch
```

### 步骤 1：候选组装与严格校验

SSE 按 index 合并 name、id 和 arguments。`build_tool_call_event()` 只接受完整 JSON object；不可解析的参数生成带 parse error 与原始参数摘要的降级候选，让模型下一 round 重发。非字符串 arguments 的错误在流内保持粘性，合法后续片段不能抹掉它。`length` / `max_tokens` 响应整体废弃；身份缺失或重复的调用批不持久化、不执行。

### 步骤 2：Scavenge（回收）

LLM 流式响应结束后，`crates/mink-core/src/agent/turn.rs` 从 `thinking`
（reasoning_content）和 `text`（普通文本）两个渠道回收遗漏的工具调用，补充到标准
tool-call 列表中。每个候选调用都通过 `build_tool_call_event()` 转为结构化事件；解析失败的
候选保留为降级候选进入同一有界纠错分支。去重使用 `(tool name, input_json)`，因此同名但参数不同的调用可以保留。

回收尝试顺序（`scavenge_tool_calls`）：

1. **DSML invoke** — `<|DSML|invoke name="Read">` DeepSeek 专用标记语言，不经过标准 tool_calls 字段
2. **XML 包装** — `<tool_call>{...}</tool_call>`
3. **Bracket 包装** — `[TOOL_CALL]{...}[/TOOL_CALL]`
4. **裸 JSON** — 扫描自由文本中的 `{name, arguments}` 形状
5. **OpenAI style** — `{"type":"function","function":{"name","arguments"}}`
6. **R1 variant** — `{"tool_name":"Bash","tool_args":{...}}`

这些格式是“容器协议”兼容层，不代表旧参数重新成为模型协议。回收层只把内容规整成
`{name, arguments}`，之后仍由当前 `tools.json` schema、`ToolExec` 参数反序列化和工具实现校验。
例如 XML/Bracket/R1 中可以恢复 `Read {"path":"src/lib.rs:40-80"}`，或下面的 Hashline 调用（TAG 必须来自实际 Read snapshot）：

```json
{"name":"Edit","arguments":{"input":"[src/lib.rs#0A3B]\nPUT 40:\n+    return new_value;"}}
```

Replace 模式使用 `path + edits`，两种模式都不接受旧 `path + patch`；完整定义见[工具参考](../reference/tools.md#edit)。`Read` 的行范围仍写在
`path` 选择器里；外部框架习惯的读参（`limit`/`offset`/`selector` 等）以及 `Grep`/`Python` 的
同类字段只被接受并忽略，真正未知的字段仍会在模型与执行层被拒绝；`Edit old_string/new_string`
会被拒绝。

### 步骤 3：Surface Gate 与 StormBreaker（重复抑制）

`enabled_tools` 是唯一工具启用输入：`None` 使用 catalog 默认集合，空列表禁用全部，显式列表精确选择；`PythonSandbox` 是 explicit-only 工具。`ModelToolSurface` 再结合 approval、角色、文件系统后端、编译 feature 和硬依赖完成一次解析。默认 `yolo` 允许全部 tier；`write` 自动允许 Read/Write、阻止 Exec；`always-ask` 自动允许 Read、阻止 Write/Exec。单工具 `allow/deny/prompt` 可覆盖模式。当前没有交互式 prompt，`prompt` 会 fail closed。

resolved `ModelToolSurface` 是唯一工具边界：`PrefixManager` 从它生成 tools schema 和能力工作流，`ToolRunner` 在真实执行前检查同一个 surface。这样即使恢复会话、模型异常输出或自定义 backend 产生未暴露的调用，也只会写入错误 tool result，不会执行。运行时直接消费 resolved surface；disable flag、sandbox `allow_*` 和独立的 runner approval 判定不属于执行合同。

`ToolRunner` 先校验调用属于 resolved `ModelToolSurface`，再把每个工具调用的
`(name, args_json)` 放入滑动窗口。检测到同一对 `(name, args)` 在同类窗口中计数 >3 次（窗口 6），
则抑制该调用，返回抑制说明：

```rust
StormDecision::Suppress(reason) => {
    results.push(ToolExecution {
        status: ToolStatus::Blocked(ToolBlocker::StormBreaker),
        content: format!("Error: {reason}"),
        ...
    });
}
```

**Mutating 清空规则**：当 metadata 标记为 mutating 的工具被调用时，清空窗口中的 read-only
条目。这允许 edit→re-read 模式正常执行。

**StormExempt**：SubAgent、PlanDraft、PlanClear、PlanConfirm、TodoWrite、TodoAdvance
跳过风暴检测。TodoRead 是普通只读工具；TodoWrite 只负责结构更新，TodoAdvance 只负责
进度转换，两者都是 revision 驱动的 session 状态变更并通过 `TodoStore` 原子提交。

## 信号驱动的信念系统

> 实现收敛：`ToolSignalProcessor` 不再累计完整信号副本（原 `signals`/`collected_signals` 已删除）；事实来源是 `ToolExecution.signals` 与 events.jsonl 的 `type=signal` 事件。

信号系统是 Mink 的反馈回路：工具执行质量被采集为信号，合并为单一信念度 `B`，
低信念时向 LLM 注入修正提示（或中止），构成闭环。完整设计（设计思想、信号采集、
信念计算、决策干预、边界情况、组件接口、展示协议）见
[信号采集与决策](#三信号采集)。

### 关键不变式

- 信号采集：`ToolFailed`（退出码/`Error:` 前缀）、`ToolError`（regex 启发式）、
  `EditLoop`（滑动窗口序列检测）三类；一次调用多条信号取 `max(severity)`，不叠加
- 信念计算：Beta-Binomial 拉普拉斯平滑，先验 `α=3, β=1`（无观测时 `B=0.75`），
  滑动窗口 W=16，跨输入按 `Config.signal.decay_per_input`（默认 0.6）衰减替代硬重置
- 分层响应：`B ≥ remind` 不干预（单次软信号只记录）；提醒区注入轨迹证据；警告区叠加
  快照回滚与恢复首步守卫（拦截喂回信念）；`B < abort` 用户接管；档位由 `SignalPolicy`
  配置，阈值/超参为内部策略常量
- 注入协议：以独立 User 消息（`[trajectory]`/`[detector]` 事实帧）写入 conversation，
  不污染 system prefix；`RecoveryPolicy` 按 resolved capabilities 校验恢复首步
- `MINK_SIGNAL_POLICY=off`：不生成 `<belief-awareness>` prompt 段，不采集、不注入、
  不回滚、不接管、不启用恢复守卫
- 错误分类（`errors.rs` 的 `ErrorCategory`：Network/Auth/RateLimit/Parse/Tool/Internal）
  仅用于日志与用户提示，不驱动任何决策

## LLM 有界自愈

设计目标是“有限故障下继续推进”，不是通用恢复框架：只增加两个小型状态对象（turn 内格式窗口、logical request 内的重试计数/退避状态），复用现有 `LlmBackend`、`TurnExecutor`、工具失败结果、`Retry` 事件与 usage 记录。

### 四种处置

- 接受：完整响应或确定性兼容（合法调用配 `stop`、旧式 `function_call`、已支持的 `data:` 拼写/缺 DONE 但有 finish_reason）。
- 反馈并继续：工具参数/调用格式错误与不可用输出——失败工具结果或一次受限内部诊断，下一 round 纠正；模型可见的错误包括 `invalid tool arguments`（serde 规则未放宽）、`<tool-call-format-error>`（整批身份不可配对）、`<output-truncated>`（length 候选整体废弃）、`<incomplete-response>`（空正文/未知 stop）。
- 重试当前请求：502/429/连接重置/首事件与 idle 超时（不占格式窗口）与 SSE 损坏/异常 EOF（占一个格式错）；固定投影重试、恰发一次 Retry、可取消退避、`Retry-After` 为最早重试时间。
- 结束：永久拒绝、恢复耗尽、取消、本地不可恢复故障；稳定错误前缀 `format_recovery_exhausted` / `request_retry_exhausted` / `request_timeout`，绝不空成功。

### 关键取舍

- 所有可继续的 round 共用同一个尾部：先结算窗口一次，再执行分支决策（todo 提醒、`[trajectory]` 证据注入），最后重载已提交历史——刷新必须在本轮所有追加之后；反馈诊断、工具结果与注入状态必须出现在下一次请求中。
- scavenge 的候选解析必须区分“无候选”与“明确候选解析失败”：先识别包装标签再解析内部（闭合/未闭合 × JSON 合法/非法 × 名字正常/缺失 全部归入降级候选），DSML invoke 同样（头部/块/参数截断均降级且切片安全；参数头完整匹配含 `>` 的分隔符后才读取值，声明为 JSON 的参数解码失败不得回退为字符串）；`function.arguments` 字段缺失（默认空对象）与类型错误（保留原始 payload 与 parse error）必须区分；普通 JSON 示例仍不误判。
- SSE 流式 `arguments` 类型错误是粘性的（不在流内纠正，模型下一 round 重发）；首事件/idle 期限只由真实进度推进，Retry 不延长 idle；失败 attempt 在重试前先取消自身子 token。
- 格式窗口按 round 计（不是按工具、不是按 attempt）：一个 round 内多个坏参数调用、多次损坏流只占一个 `true`；预判（`would_exceed`）不改变窗口，提交只在唯一 round 结束点发生，且在所有真实工具结果持久化之后。
- 参数错与请求重试严格分离：参数错不能归入“原样重试同请求”，502 不能靠给模型追加提示恢复；已知格式调用不消耗恢复守卫，也不触发 belief/回滚/重启。所有模型可见内置工具的模型参数解码经共享 helper（不完整迁移会造成真实失败信号与格式额度的错配）。
- 不确定的词法修复不再执行：截断 JSON 只带 parse error 与原始参数摘要交给模型重发；正文中已识别的工具调用同样如此（降级候选进入纠错分支），普通 JSON 示例仍不会被当成格式错误。
- 可选总期限是整次逻辑请求的上限：建流、流消费与退避等待共用同一个 deadline；到点时停止等待/消费并保留已收到的 usage，重试不会延长它（主请求与压缩摘要请求共用该语义）。
- 内置 backend 一次调用 = 一次物理请求，重试责任收敛到 runtime attempt 循环，避免嵌套重试相乘；自定义 backend 内部的多次物理请求只按既有聚合 `attempt_count` 记录（不拆明细、不外层乘算）。

## 一、概述

信号系统是 Mink 的反馈回路核心。每次工具调用后采集执行质量的信号，合并为单一信念度，根据信念度自动决定是否向 LLM 注入修正提示或中止任务。整个回路由三个组件串联：

> **更新**：响应层已重构为分层响应模型（轨迹证据注入 / 状态操作 / 策略重启 /
> 用户接管）。本节保留检测与信念数学的设计说明；执行器语义以下图为准。

```
工具执行完毕
       │
SignalCollector.collect(name, output, exit_code, content)
       ├── ToolFailed — 确定性失败（退出码非零 / "Error:" 前缀）
       ├── ToolError  — 启发式检测（regex 匹配错误关键词）
       └── EditLoop   — 序列模式检测（编辑-检查循环）
       │
       ▼
BeliefTracker.observe(signals)     # 先验/窗口/衰减来自内部 Config.signal 常量
       │  拉普拉斯平滑（默认 α=3, β=1） + 滑动窗口（默认 W=16）
       │  B = α / (α + β) ∈ [0, 1]
       ▼
EvidenceTracker（guard/evidence.rs）
       │  重复调用 / 失败聚类 / 预算消耗 → 轨迹事实
       ▼
DecisionEngine.decide_with_signals(B, errors, hard_signals)
       ├── 单次软失败且 B ≥ warn_threshold → None（记录不干预；累计 ≥ 2 次软失败参与决策）
       ├── B 在提醒区 → 轨迹证据注入（[trajectory]/[detector]，无命令）
       ├── B 在警告区 → 证据注入 + 快照回滚 + 恢复首步守卫（拦截喂回信念）
       ├── 守卫连续拦截 ≥ guard_max_blocks → 绕过守卫并强制证据注入
       └── B < abort_threshold → 用户接管（signal_handover 事件）
```

**为什么注入形态从命令改为证据**：模板注入的内容全部可从模型已有上下文推导，信息增益
约等于零；运行时相对模型的唯一信息优势是完整结构化轨迹，因此响应改为注入**模型算不出来
的行为统计**（重复调用、失败聚类、预算消耗），并在警告区叠加**状态操作**（快照回滚）与
拦截反馈。

三个组件职责清晰、零外部依赖（仅 BeliefTracker 引用 Signal 类型），可独立修改和替换。

## 二、设计思想

信号系统的设计灵感来自两个学科：工程控制论和贝叶斯统计。前者提供回路框架和稳定性保障，后者提供信念更新的数学基础。

### 2.1 工程控制论：负反馈回路

整个信号系统是一个典型的负反馈控制系统：

```
扰动（工具错误）→ 测量（信号采集）→ 控制器（信念度 + 决策）
  → 执行器（注入/中止）→ 被控对象（LLM 行为）→ 回到测量
```

**反馈回路**：工具执行质量被实时测量、量化为信念度，信念度低于阈值时向 LLM 注入修正提示，LLM 调整行为，下一轮工具执行质量因此改善——构成闭环。没有这个回路，LLM 的行为是开环的：每次调用独立，错误不积累、不修正、不学习。

**负反馈**：信念度低 → 注入（施加修正力）→ 行为改善 → 信念度回升 → 停止注入。修正力与偏差反向。没有负反馈，错误会持续累积直至任务失败。

**冷却机制**等价于控制论中的**抗积分饱和**（anti-windup）。当信念度持续偏低时，如果每次工具循环都注入，等效于积分项持续累积、执行器饱和（模型对重复提示脱敏）。冷却在注入后暂时切断反馈，让执行器（LLM）有时间消化修正、展示效果，避免控制信号过冲。

**阈值分区**对应控制论中的**多级保护**（multiple guard bands）：

| 信念度区间 | 控制动作（分层响应） | 类比 |
|:----------:|---------|------|
| ≥ remind（默认 0.70） | 无操作，自由运行（软信号单独出现亦然） | 稳态，无需干预 |
| warn~remind（默认 0.50~0.70） | 轨迹证据注入（提醒级） | 预警区，注入事实不注入命令 |
| abort~warn（默认 0.30~0.50） | 证据注入 + 快照回滚 + 恢复首步守卫 | 保护区，状态操作 + 拦截反馈 |
| < abort（默认 0.30） | 用户接管（HandOver 事件，交互重锚定） | 极限区，请求外部信息 |

### 2.2 贝叶斯统计：信念的量化

信念度不是拍脑袋的分数，而是基于贝叶斯 Beta-Binomial 模型的概率推断。

**Beta 分布作为信念模型**：Beta 分布是定义在 [0,1] 上的连续概率分布，有两个形状参数 α 和 β。α 代表"成功证据"（工具调用正常），β 代表"失败证据"（工具调用出错）。信念度 B = α/(α+β) 是 Beta 分布的均值——这是给定当前观测后，对"工具调用成功的概率"的最大后验估计。

**为什么是 Beta 而不是滑动平均**：
- 滑动平均（如 EMA）没有先验概念——初始时刻没有任何信息，前几次观测的估计极不稳定
- Beta 分布允许注入**先验知识**：α=3, β=1 编码了"我们信任这个模型大概率能正确使用工具"的先验假定。新用户输入刚启动时、还没有任何观测时，B=0.75 已经是一个合理的起点

**先验的物理含义**：α=3, β=1 等价于在真实观测之前假设了 3 次成功和 1 次失败。选择这个比值而不是 (1,1) 或 (10,1) 是因为：
- (1,1) 太保守：初始 B=0.50，无观测时假设"好坏各半"，不符合使用模型的前提
- (10,1) 太自信：初始 B=0.91，需要大量错误证据才能拉低信念，延迟干预
- (3,1) 的初始 B=0.75：1 次严重错误后降至约 0.63，适中敏感

**证据不叠加的贝叶斯解释**：一条观测中如果同时出现 ToolFailed(1.0) 和 ToolError(0.9)，取 max 后 failure=1.0。这是因为一个工具调用只有"成功/失败"两种真实状态，多条信号是对同一状态的不完美观测。如果叠加权重，等效于把一次失败计为多次，违反了 Beta 分布中"一次试验对应一次伯努利观测"的假设。

### 2.3 两项思想的交汇

工程控制论提供**形式**（回路结构、稳定性保障），贝叶斯统计提供**内容**（信念的数学表达）。两者交汇在一点：**置信度加权反馈**。

传统控制系统的反馈信号通常是一个直接测量的物理量（温度、位置、电压）。这里没有直接可测的"agent 可靠性"——我们只有间接信号（工具输出文本、退出码）。贝叶斯推断将这些杂散的间接信号合成为一个稳定的统计量 B，这个 B 再作为控制器的输入。用概率论为控制器提供干净的测量值，是这套系统的核心创新。

滑动窗口（W=16）则是从控制论角度的补充：它是一个**有限记忆的遗忘因子**。Beta 分布天然是累积的（所有历史观测都计入 α 和 β），但在 agent 场景下，旧错误不应永久拖累信念——agent 可能已经修正了问题。滑动窗口截断记忆，等价于控制论中的"有限带宽"：系统只对最近 N 次工具调用的质量敏感，超过窗口的历史不再影响当前控制决策。

## 三、信号采集

### 3.1 设计原则

所有信号通过同一个入口产生：`SignalCollector.collect()`。`turn.rs` 只负责调用并传递参数，不做任何内联检测。这样保证了信号来源可追溯、可测试，新增信号类型只需修改 `collect()` 和 `SignalKind`。

### 3.2 三类信号

| 信号 | 来源 | 检测方式 | 确定性 | 权重 |
|------|------|---------|--------|:----:|
| **ToolFailed** | 工具执行结果 | `exit_code ≠ 0` / `"Error:"` 前缀 | ✅ | 1.0 |
| **ToolError** | 工具输出文本 | 预编译 regex 匹配 | ❌ | 0.3~0.9 |
| **EditLoop** | 工具调用序列 | W=6 滑动窗口 | ✅ | 0.4~0.9 |

#### ToolFailed

确定性工具失败，两条检测路径互不重叠：

- **Bash 工具**：通过 `child.status.code()` 获取进程退出码，非零即产生信号。退出码通过 `ToolExec` trait 的返回值透传，不经过输出文本分析。
- **其他工具**（Read/Write/Edit/Glob/Grep/Plan 工具）：Rust 函数直接执行，无退出码。失败时 `Result::Err` 格式化为 `"Error: ..."` 前缀，Plan/SubAgent 延迟工作先完成并执行统一大小保护，之后 `collect()` 再通过最终结果的 `"Error:"` 前缀捕获。

权重固定为 1.0——工具真失败就是铁的事实，不按类型区分。

#### ToolError

启发式检测，基于 `OnceLock` 全局预编译 regex 模式库（只编译一次）：

| 模式 | 权重 | 说明 |
|------|:----:|------|
| `error[E\d+]:` | 0.9 | Rust 编译错误 |
| `error: aborting due to \d+ previous error` | 0.9 | Rust 批量错误 |
| `FAILED ...::...` | 0.8 | 测试失败 |
| `FAILURES===` | 0.8 | Pytest 失败 |
| `Traceback (most recent call last):` | 0.8 | Python 异常 |
| `Permission denied` / `EACCES` | 0.5 | 权限拒绝 |
| `command not found` / `No such file` / `does not exist` | 0.5 | 找不到 |
| `Timed? ?out` / `timeout` / `killed` | 0.3 | 超时 |

按优先级顺序匹配（编译 → 测试 → 权限 → 超时），匹配到第一条即返回。regex 无法 100% 确定命令失败，因此权重低于 ToolFailed。

#### EditLoop

用于捕获"盲写"循环——agent 持续编辑而不读文件确认。维护 W=6 的滑动窗口，记录最近工具调用名称：

| 条件 | 严重度 | 含义 |
|------|:-----:|------|
| Edit 次数 = 5 | 0.6 | 窗口内绝大部分是编辑 |
| Edit 次数 = 6 | 0.8 | 几乎全是编辑 |
| Edit↔Diff 交替 1 次 | 0.4 | 编辑-对比的早期循环 |
| Edit↔Diff 交替 2 次 | 0.7 | 循环已建立 |
| Edit↔Diff 交替 ≥ 3 次 | 0.9 | 严重的盲写循环 |

Edit↔Diff 交替检测还要求窗口内完全无 Bash/Grep/Read 操作——有读操作说明 agent 至少尝试理解代码，不是纯盲写。

### 3.3 ModelFormat（模型输出格式，非工具失败）

模型生成的工具参数解码失败（内置共享 decode helper 或 `ToolError::argument`）不属于工具运行失败，
而是**格式反馈**：结果状态为 `Failed(ArgumentInvalid)` 并携带内部来源 `ToolFormat(ModelFormat)`。

- **不进信号链路**：不生成 Signal、不进入 `belief.observe`、不累加 evidence 的软/硬失败、不触发回滚/重启/Abort；诊断事实是那条失败工具结果自身（模型可见、可重发）。
- **单独计数**：`ToolSignalProcessor` 维护独立的 format 计数，turn 层在 events.jsonl 记一条 `llm_recovery` 诊断（category=`tool_format`）。
- **与守卫/Storm 的优先级**：已知格式调用（parse_error 或历史上相同参数已确认 ModelFormat）不消耗恢复守卫——守卫保持激活等待下一次有效调用；重复坏参数被 StormBreaker 抑制时仍保留 ModelFormat 来源，不能转成硬 `ToolFailed`。同一轮混有真实执行失败时，真实失败照常参与决策。
- **参数身份**：storm 比较键对不可解析输入使用原始参数摘要（合法 JSON 用序列化参数），不同的坏 payload 不会因共享占位 `{}` 被合并为一个调用。

## 四、信念度计算

### 4.1 从信号到观测

一次工具调用可能产生多个信号（如编译失败既有非零退出码又有 `error[E0308]`）。多条信号合并为一条观测，**取 max(severity)**，不叠加：

```
signals = [ToolFailed(1.0), ToolError(0.9)]
  → total_failure = max(1.0, 0.9) = 1.0
```

| 工具调用 | 信号 | Observation |
|---------|------|:----------:|
| `Read(path)` | 无 | success=1.0, failure=0 |
| `Bash(cargo build)` | ToolFailed(1.0) + ToolError(0.9) | success=0, failure=1.0 |
| `Edit(path)` | EditLoop(0.9) | success=0.1, failure=0.9 |
| `Bash(make test)` | ToolError(0.5) | success=0.5, failure=0.5 |

不叠加的理由：ToolFailed 和 ToolError 通常指向同一问题（编译失败既有 exit code 1 又有 error[E0308]），叠加会错误加重惩罚。

### 4.2 拉普拉斯平滑

信念度使用贝叶斯 Beta 推断，拉普拉斯平滑：

```
α = α_先验 + Σ 窗口内 success_weight
β = β_先验 + Σ 窗口内 failure_weight

B = α / (α + β)
```

先验 α=3, β=1，初始 B=3/4=0.75。这是"信任先验"——使用这个模型本身就意味着信任它大概率能正确使用工具。随着观测累积，先验效应被自然淹没。

### 4.3 滑动窗口

BeliefTracker 维护 `VecDeque<Observation>`，窗口 W=16。每次 `observe()`：

1. `Observation::from_signals()` 将信号合并为一条观测
2. 窗口满则 `pop_front()` 最旧观测，从 α/β 中减去对应值
3. α += success_weight，β += failure_weight
新用户输入按 `Config.signal.decay_per_input`（默认 0.6）衰减：累计证据向先验回拉，
替代硬重置——跨轮重复失败累积升级、偶然失败自然消退；`reset()` 保留，等价于衰减因子 0。

### 4.4 计算示例（W=4）

```
调用1: Read           → α=4.0 β=1.0 B=0.800
调用2: Bash(错误 0.9)  → α=4.1 β=1.9 B=0.683
调用3: Bash(错误 0.9)  → α=4.2 β=2.8 B=0.600
调用4: Read           → α=5.2 β=2.8 B=0.650
调用5: Grep（窗口满）  → α=5.2 β=1.8 B=0.743
```

旧错误自然滑出窗口后，信念逐步回升。

## 五、决策与干预

### 5.1 分层响应模型

决策按信念分区选择响应层级（阈值当前为内部策略常量，默认值见括号）：

```
B ≥ remind（0.70）            ─→ 无操作；单次软信号只记录不干预
warn ≤ B < remind（0.50）     ─→ 轨迹证据注入（提醒级）
abort ≤ B < warn（0.30）      ─→ 证据注入 + 快照回滚 + 恢复首步守卫
B < abort（0.30）             ─→ 用户接管（结构化接管报告）
```

响应层级由 `MINK_SIGNAL_POLICY` 门控（evidence / state_ops / restart / full），
允许按部署环境裁剪状态操作、策略重启与接管的行为面；阈值与超参是内部策略常量
（`Config.signal`），不通过 `.minkrc` / `--config` 暴露。

阈值的物理含义：

| 基准值 | 含义 |
|:-----:|------|
| 0.30 | 5 次连续严重错误后 ≈ 0.33——严重失真的下限 |
| 0.50 | 2 次严重 + 1 次轻微 ≈ 0.50——"好坏参半"的分界 |
| 0.70 | 初始 B=0.75，1 次错误后 ≈ 0.63——"基本顺利"的下界 |

### 5.2 注入位置

响应发生在 `turn.rs::execute()` 的循环内部，工具结果完成延迟工作和大小保护之后、下一轮
LLM 调用之前。`TurnExecutor` 持有一个持久化的 `DecisionEngine` 实例，在 tool-use 继续路径
调用它的 `decide_with_signals()`：

```
工具执行 → Plan/SubAgent 结果定稿 → SignalCollector.collect() → belief.observe()
stop = "tool_use"
  ├─ engine.decide_with_signals(belief, hard_failures, soft_failures)
  │   ├─ 引擎内部检查冷却 → 跳过
  │   ├─ 提醒级 → store.add_runtime_user([trajectory] 事实) + 冷却
  │   ├─ 警告级 → 证据注入 + 快照回滚 + 恢复首步守卫
  │   └─ 接管级 → signal_handover 事件后返回 Failed
  └─ messages = compaction.active_messages() → 下一轮 LLM
```

注入消息是一条独立的 User 消息，追加在对话存储尾部（`add_runtime_user()`）。不修改
system prompt（保护前缀缓存）。LLM 在下一轮调用时自然看到。

### 5.3 注入内容

```
[trajectory]
- 4 identical calls to Grep(pattern="missing_helper") returned no results
- 2 failed calls to Bash(cargo check): process exited with code 101
- budget: 3 of 6 evidence slots used by repeats
[detector] belief 0.63 (reference only)
```

`DecisionEngine` 只产生结构化 `RecoveryDirective`（信念 + 严重级）；文本由
`EvidenceTracker` 渲染：重复调用、失败聚类与预算消耗的聚合事实，按新鲜度哈希去重，
同一证据批不重复注入。警告级的恢复首步守卫由 `RecoveryPolicy` 依据 resolved
capabilities 校验首个调用（拦截反馈喂回信念，连续拦截达 `guard_max_blocks` 后绕过并
强制证据注入）。

provider bindings 和动态工具引用的完整设计见
[工具能力与提示词解耦设计文档](tools-and-capabilities.md)。

### 5.4 冷却机制

信念度持续偏低时，如果每次工具循环都注入，会导致上下文膨胀和模型脱敏。冷却机制解决这个问题：注入后自动跳过接下来若干次 `decide()` 调用。

冷却完全封装在 `DecisionEngine` 内部，调用方无感知：

```rust
pub struct DecisionEngine {
    cooldown_remaining: usize,  // 内部管理
    // ...
}

pub fn decide_with_signals(&mut self, belief: f64, hard_signals: usize, soft_failures: usize) -> Decision
```

**工作流程**：

```
engine.decide_with_signals(belief, hard, soft)
  ├── B < abort → 用户接管（强制清除冷却）
  │
  ├── cooldown_remaining > 0
  │   └── cooldown_remaining -= 1, 返回 None
  │
  ├── 单次软失败且 B ≥ warn → None（记录不干预）
  ├── B < warn → Inject(Warning) + cooldown = 3
  ├── B < remind → Inject(Reminder) + cooldown = 3
  └── else → None
```

| 参数 | 默认值 | 含义 |
|------|:------:|------|
| `DEFAULT_COOLDOWN_TURNS` | 3 | 注入后跳过的 `decide()` 次数 |
| 接管绕过冷却 | true | B < abort 时不等冷却 |

**冷却生命周期**：

| 事件 | 行为 |
|------|------|
| 新用户输入 | `engine.reset()` 清零（`TurnExecutor.execute()` 入口调用） |
| 接管触发 | 内部清零 |
| Inject 发生 | 设为 3 |

**为什么是 3 次**：单次严重错误（severity=0.9）需要约 2 次 clean call 恢复至 B=0.70。冷却比恢复多 1 拍，确保信念真正稳定后才允许再次注入。3/16 ≈ 19%，只占信念滑动窗口的不到五分之一。

### 5.5 系统提示词信念度感知

为了让模型在被注入时理解上下文并主动调整行为，默认系统提示词中加入 `<belief-awareness>` 区块，位于 `<execution-codes>` 之后：

```
<belief-awareness>
Runtime reliability analysis may append a user message beginning with [trajectory]: a
factual summary of your recent tool calls (repetitions, failures, budget use) and a
[detector] note with a belief score. Treat it as additional evidence about your own
recent behavior — not a new user request and not a command. Re-read the affected files
or inspect the failing commands before further edits.
</belief-awareness>
```

统一使用英文，不做本地化。设置 `MINK_SIGNAL_POLICY=off` 时，不生成 `<belief-awareness>`
区块，也不执行信号采集、信念更新、证据注入、回滚、接管和恢复首步守卫。

Recovery 首步资格与普通 Bash 安全/误用策略是两套合同。前者只决定信号恢复后的首个调用能否
解除守卫，不改变普通 Bash 的危险命令检查和误用提示。

## 六、信号链路完整路径

```
┌─ 工具执行 (tools/runner.rs) ────────────────────────────────┐
│ ToolExec::execute() -> Result<ToolOutcome>                  │
│ format_dispatched_result() -> ToolExecution                 │
│ 错误统一转换为 "Error: tool execution failed: ..."           │
└──────────────────────┬───────────────────────────────────────┘
                       │
┌─ 结果定稿 (agent/turn.rs) ─▼────────────────────────────────┐
│ 工具阶段原地完成类型化 PlanCommand 交接                       │
│ SubAgentCoordinator 完成并替换延迟子代理结果                  │
│ finalize_deferred_results() 执行统一大小保护                  │
└──────────────────────┬───────────────────────────────────────┘
                       │
┌─ 信号处理 ───────────▼───────────────────────────────────────┐
│ ToolSignalProcessor::process(final ToolExecution)           │
│   ├─ SignalCollector::collect(...)                          │
│   │    ├─ ToolStatus::Failed / Blocked → typed signal       │
│   │    ├─ detect_error(regex) → ToolError                   │
│   │    ├─ push history → detect_edit_loop → EditLoop         │
│   │    └─ 失败且无信号时按成功标志/错误码合成硬信号            │
│   ├─ EvidenceTracker::record(tool, args, summary, failed)   │
│   └─ belief.observe(&result.signals)                        │
│        ├─ Observation::from_signals(signals) → max(severity)│
│        └─ α += success_weight, β += failure_weight          │
│ 标题栏实时更新 belief()                                      │
└──────────────────────┬───────────────────────────────────────┘
                       │
┌─ tool-use 继续决策 ──▼───────────────────────────────────────┐
│ engine.decide_with_signals(belief, hard, soft)              │
│   ├─ 引擎内部检查冷却 → 跳过                                 │
│   ├─ 提醒级 → store.add_runtime_user([trajectory] 事实) + 冷却│
│   ├─ 警告级 → 证据注入 + 快照回滚 + 恢复首步守卫              │
│   └─ 接管级 → signal_handover 事件 → return Failed          │
│ messages = compaction.active_messages()（含注入消息）         │
└──────────────────────┬───────────────────────────────────────┘
                       │
┌─ 下一轮 LLM ─────────▼───────────────────────────────────────┐
│ LLM 收到:                                                  │
│   [User] fix the tests                                     │
│   [Assistant] tool_calls(A, B)                             │
│   [User] result_A, result_B                                │
│   [User] [trajectory] 重复调用/失败聚类/预算消耗 + [detector]  │
│   → LLM 把轨迹事实作为证据，重读受影响文件后再修改            │
└────────────────────────────────────────────────────────────┘
```

## 七、边界情况

### 7.1 exit_code = 0

Bash 命令成功退出（code 0）不产生 ToolFailed 信号。

### 7.2 多条 regex 同时匹配

`detect_error()` 按优先级顺序匹配，匹配到第一条即返回。不会产生多个 ToolError。

### 7.3 窗口未满时 EditLoop 不触发

前 5 次工具调用不会产生 EditLoop 信号（`seq_window = 6`）。

### 7.4 证据批去重与冷却

同一证据批按新鲜度哈希去重（`evidence_dedup_window`），冷却期内不重复注入；
冷却内再次触发注入时注入"无新证据"占位帧。

### 7.5 冷却期内信念继续恶化

如果冷却期内信念继续恶化至 B < abort，用户接管绕过冷却直接触发，安全优先。

### 7.6 冷却与信念恢复的时间关系

单次严重错误需约 2 次 clean call 恢复至 0.70。冷却期 3 次，比恢复多 1 拍。如果第 2 次 clean call 后信念已达标，冷却期结束后不会多余注入。

## 八、组件接口

### 8.1 组件表

| 组件 | 文件 | 公开方法 |
|------|------|---------|
| SignalCollector | `guard/collector.rs` | `new()`, `collect()` |
| EvidenceTracker | `guard/evidence.rs` | `record()`, `render()`, `is_fresh()`, `edited_paths` |
| BeliefTracker | `agent/belief.rs` | `new_with_priors()`, `observe()`, `belief()`, `reset()`, `decay()` |
| DecisionEngine | `agent/decision.rs` | `from_config()`, `decide_with_signals()`, `reset()`, `cooldown_remaining()` |
| ToolSignalProcessor | `agent/tool_signals.rs` | `process()`（采集 + 证据记录 + 信念更新一站式） |

组件零外部依赖；阈值/先验/窗口/衰减全部来自内部 `Config.signal`（不对外暴露为配置协议）。

### 8.2 DecisionEngine 接口

```rust
pub fn decide_with_signals(&mut self, belief: f64, hard_signals: usize, soft_failures: usize) -> Decision
pub fn reset(&mut self)
pub fn cooldown_remaining(&self) -> usize
```

`DecisionEngine` 内部管理冷却计数器。`TurnExecutor` 持有一个持久化的 `DecisionEngine`：

```rust
decision_engine: DecisionEngine,
```

### 8.3 相关文件

| 文件 | 职责 |
|------|------|
| `crates/mink-core/src/agent/decision.rs` | 分层响应决策、阈值、冷却和 reset |
| `crates/mink-core/src/guard/evidence.rs` | 轨迹证据构造与渲染（重复调用/失败聚类/预算截断/新鲜度去重） |
| `crates/mink-core/src/agent/tool_signals.rs` | 工具信号处理、证据记录与信念更新 |
| `crates/mink-core/src/agent/recovery_policy.rs` | 从已解析能力校验恢复首个调用 |
| `crates/mink-core/src/agent/turn.rs` | 证据注入、快照回滚、策略重启与用户接管 |
| `crates/mink-core/src/config.rs` | 解析 `SignalPolicy`；数值策略保持内部固定 |
| `crates/mink-core/src/prompt/core.rs` | policy 非 `off` 时生成不含具体工具名的 `<belief-awareness>` core |

## 九、信念度实时展示

信念度在每次工具调用后实时更新，通过两种渠道展示。

### 9.1 TUI 状态栏

```
flash B:0.73 T:12 R:45 I:200K(50%) O:20K C:400K(40%)
```

| 字段 | 含义 |
|------|------|
| `B:0.73` | 信念度（0.0 表示未追踪） |
| `T:12` | 对话轮次 |
| `R:45` | API 请求次数 |
| `I:200K` | 总输入 tokens |
| (50%) | 缓存命中率 |
| `O:20K` | 总输出 tokens |
| `C:400K` | 缓存相关 tokens |

> 费用统计已移除：状态栏不再展示累计费用；流式等待心跳显示为状态栏精简标签（如 `·30s`）。

信念度语义：

| B 值 | 含义 |
|------|------|
| > 0.70 | 顺利 |
| 0.50~0.70 | 偶有小错 |
| 0.30~0.50 | 频繁出错 |
| < 0.30 | 严重 |

当前信念度仅显示数值，未做颜色区分。

### 9.2 终端标题栏

非 TUI 模式下，通过 ANSI escape `\x1b]0;...\x07` 将信念度写入终端窗口标题，格式与 TUI 状态栏一致。

### 9.3 更新时机

1. **每次工具调用后**——`BeliefTracker.observe()` 执行后立即更新 `StatsSnapshot.belief`
2. **Phase 4 决策前**——标题栏刷新最新信念度
3. **本轮结束后**——最终信念度持久显示

## 十、当前限制

- 冷却轮数来自内部 `SignalConfig`，没有公开配置入口。
- 冷却按 `DecisionEngine::decide_with_signals()` 调用次数递减；Abort 绕过冷却，新用户输入重置冷却状态。
- TUI 信念度显示数值，未按信念区间设置颜色。

## 十一、相关文件

| 文档 | 内容 |
|------|------|
| `AGENTS.md` | 操作手册、快速参考 |
| `docs/concepts/architecture.md` | 运行时分层、信号链路图 |
| `docs/concepts/runtime.md` | 信号驱动的信念系统（主题五） |
