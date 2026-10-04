# 配置参考

> 更新日期：2026-10-04

参数、默认值、优先级与校验。

## 配置与参数

### CLI 参数

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `PROMPT` | — | 用户输入（位置参数） |
| `-m` / `--model` | `flash` | 模型名。`flash` / `pro` 是默认别名，也可直接指定任意 OpenAI-compatible 模型名 |
| `--mission PATH` | — | 加载 MISSION.md |
| `--session NAME` | 自动生成 | 命名会话 |
| `--continue` | — | 恢复最近的 session |
| `--list-sessions` | — | 列出所有 session |
| `--list-skills` | — | 列出可用 skill |
| `--skill NAME` | — | 选择要加载的 skill；可重复传入，按传入顺序去重 |
| `-i` / `--interactive` | auto | REPL 交互模式 |
| `--tui` / `--tui=full` | — | Full TUI |
| `--tui=inline` | — | Inline TUI |
| `--print` | — | ndjson 结构化输出，事件格式见 [机器协议](protocols.md) |
| `--agent-jsonl` | — | Agent JSONL 协议（stdin request，stdout 事件流 + final），详见 [机器协议](protocols.md) |
| `--api-key KEY` | env | 覆盖 API Key |
| `--base-url URL` | 默认端点 | 覆盖 API 端点 |
| `--enabled-tools <list>` | 默认集合 | 逗号分隔的精确工具列表；`none` 禁用全部 |
| `--edit-mode <mode>` | `hashline` | `hashline` / `replace`；runtime 启动后固定 |
| `--edit-fuzzy-match <bool>` | `true` | Replace 行窗口模糊匹配开关 |
| `--edit-fuzzy-threshold <n>` | `0.95` | Replace 阈值，有限数且在 `0.0..=1.0` |
| `--edit-enforce-seen-lines <bool>` | `false` | Hashline 是否强制锚点已由 Read/Grep 展示 |
| `--config <toml>` | — | TOML 字符串设置配置 |
| `-v` / `--verbose` | `false` | 详细日志 |

### `--config` TOML 格式

中低频参数通过 `--config` 传递：

```toml
[provider]
model = "flash"
api_key = "sk-xxx"
base_url = "https://api.deepseek.com/v1"
openai_reasoning_effort = "max"
openai_include_usage = true
openai_token_param = "max_tokens"
openai_tool_choice = "auto"
http_timeout_secs = 600                # provider HTTP 请求总超时（秒）；0 = 不设总超时（长流式生成）
image_input = "on"                     # 可选：显式开启/关闭图片能力（覆盖 backend 声明）
vision_models = ["deepseek-v4-flash-vision-exp"]  # 可选：视觉模型列表（空列表=全部关闭）

[provider.model_aliases]
flash = "deepseek-v4-flash"
pro = "deepseek-v4-pro"

[provider.openai_extra_body]
custom_boolean = true
custom_budget = 8192

# 多模态图片限额（可选子表）：只覆盖已支持图片能力的会话，见下文
[provider.image]
detail = "high"                        # high | low（token 估算档位）
max_images_per_request = 600          # 单工具批图片数量预算防线（每批从 0 计数）
max_image_bytes_per_request = "16M"   # 单工具批原始字节预算防线
max_image_bytes = "16M"               # 单张图片原始字节上限

[generation]
max_tokens = 4096
max_turns = 20
llm_first_event_timeout = 60
llm_idle_timeout = 90
llm_wait_heartbeat = 30
output_format = "stream-json"

[context]
max_context = "500K"
context_compact_pct = 94
context_reserve_tokens = 64000
context_compact_tail_tokens = 256000
context_compact_max_output_tokens = 8192
context_compact_input_reduction = false

[tools]
tool_timeout = 300
tool_timeout_max = 600
sub_agent_timeout = 120
max_search_files = 5000
max_search_results = 1000
enabled_tools = ["Read", "Write", "Edit", "Grep", "Glob", "Bash"]
approval_mode = "write"
skills = ["python", "debugging"]

[tools.edit]
mode = "hashline"
fuzzy_match = true
fuzzy_threshold = 0.95
enforce_seen_lines = false

[signal]
policy = "full" # off / evidence / state_ops / restart / full

[recovery]
# 有界 LLM 自愈（缺省字段保留内置默认值）
format_window_size = 10   # 每个用户 turn 的格式错误滑动窗口 W（1..=1024）
format_max_errors = 3     # 窗口内允许多少次格式错误 K（0 <= K < W）
request_max_retries = 3   # 单个逻辑请求在其后的重试调用数（0..=16，默认共 4 次请求）
request_timeout_secs = 30 # 可选：单个逻辑请求的总期限（含全部尝试与等待，> 0）

[sandbox_python]
wasm_path = "/path/to/python.wasm"
read_dirs = ["./data"]
```

旧的顶层扁平字段不再接受；解析器会报告 unknown field。算法阈值、先验、衰减、证据长度、冷却和恢复限制是内部策略，不属于配置协议。
`llm_first_event_timeout` 覆盖「请求建立（含等待响应头）＋首事件」的**总预算**：建流返回不会重置计时，`Retry` 不延长；Ctrl+C 在建立阶段同样生效，中断映射为 Interrupted。
`--agent-jsonl` 模式不会读取 `.minkrc`，但仍应用命令行 `--config`。

#### LLM 有界恢复（`[recovery]`）

一次 `run_turn()` 内的自愈限制全部来自这一组配置（Rust `Config.llm_recovery`、JSONL `options.recovery`、Python `SandboxConfig` 同名参数）：

- 模型输出格式错误采用滑动的 round 窗口：`format_window_size`（默认 10）与 `format_max_errors`（默认 3）。每个完成判定的 round 只占一个槽；坏参数工具调用返回 `ArgumentInvalid` 失败结果让模型重发，缺名/缺 ID/重复 ID 的候选批整体丢弃并反馈一次内部诊断，`length`/`max_tokens` 候选整体废弃，空正文/未知 stop 不再静默成功；错误数超过 K 以 `format_recovery_exhausted` 结束，已执行工具不回滚。
- 请求暂时故障（408/409/425/429/500/502/503/504、连接重置、首事件/idle 超时、响应协议损坏）自动重试：`request_max_retries`（默认 3）表示首次之外允许的重试调用数，退避 1s/2s/4s…上限 10s，`Retry-After`（秒数或 HTTP-date）作为最早重试时间；401/403、模型不存在、未知 provider 错误不重试。
- `request_timeout_secs`（默认不设）限定一个逻辑请求的总期限，包含全部重试与等待，重试不会延长它。
- 传输层还有一道**provider HTTP 总超时**（`[provider] http_timeout_secs`，默认 600 秒，`0` = 不设）：单次物理请求（含流式响应体读取）超过它就中断并归入可重试故障。它先于 `request_timeout_secs` 生效，长流式生成（文档生成、长思考）必须调大或设 `0`，否则每次尝试都会被固定墙钟时间砍断（表现为 `request_timeout: … error decoding response body … operation timed out`，且耗时呈 600s 的整数倍）。该字段属于传输层配置，不是 `[recovery]` 恢复策略。
- 参数错反馈与请求重试是两条独立分支：参数错不会原样重发同一请求，502 也不会靠给模型追加提示恢复。
- 取消、持久化闩锁与 `max_turns` 优先：取消立即结束等待中的重试；格式恢复消耗正常 round/max_turns，超限时返回 `format_recovery_exhausted`，未超限但轮次用尽仍是 `MaxTurnsExceeded`。

#### 信号系统（分层响应模型）

信号系统的响应按信念度分层（设计依据见 `docs/concepts/recovery-and-signals.md`）：

- **记录不干预**：软信号（regex 嗅探类）单独出现且信念尚可时，只记录不改行为；
- **证据注入**：信念进入提醒区后注入 `[trajectory]`/`[detector]` 轨迹事实
  （重复调用、失败聚类、预算消耗），不注入命令；
- **状态操作**：警告区把循环窗口内编辑过的文件回滚到最近快照，并启用恢复首步守卫
  （拦截会喂回信念，连续拦截达 `guard_max_blocks` 后绕过并强制证据注入）；
- **策略重启**：同一输入内连续第 2 次警告，或非交互环境下信念跌破 abort 阈值时，
  以 fresh 子代理（不继承父对话）重新规划后继续；
- **用户接管**：交互环境下信念跌破 abort 阈值时，输出结构化接管报告
  （证据/编辑路径/选项）并返回失败，等待用户重锚定。

`MINK_SIGNAL_POLICY=off` 关闭全部信号采集、证据注入、回滚、接管与守卫。

#### 多模态（图片输入）

视觉能力在 session 初始化时解析并**冻结**（持久化到 `model-capabilities.json`），
分辨率优先级：显式 `image_input` > backend 声明（`vision_models` 列表）> 关闭。
内置默认视觉模型为 `deepseek-v4-flash-vision-exp`；其余模型保持 text-only
（fail closed）。`[provider.image]` 只覆盖已支持会话的限额，不会把文本会话变成
视觉会话。设计说明见 `docs/concepts/images.md`。

- 开启后：`Read` 图片文件或 `image://sha256:<hex64>` 会把图片附加到**下一次** LLM
  请求（OpenAI `image_url` data-URL，原字节 base64 无转换）；图片路径拒绝行
  selector / `:raw`。默认限额：600 张/批、16MB/批、16MB/单图、16384px 边长、
  1600 万像素。
- 单次消费：图片完整发送一次后，历史中的引用降级为文本提示
  （`[Previously attached image: ...]`）；需要重新看图用
  `Read image://sha256:<hex64>` 重新注入（幂等）。
- TUI 粘贴：`Ctrl+V` 把剪贴板 PNG 落到 session `attachments/`，消息只带绝对路径
  （约定而非契约，模型需调用 `Read`）；见[粘贴图片](../guides/images.md#粘贴图片ctrlvmacos)。
- 升级/换模型注意：会话冻结的能力指纹与新配置不一致时（库升级改变默认限额、
  修改 `[provider.image]`、切换到不同能力模型），启动或 `/model` 切换会失败
  （fail closed），需要新建会话；未启用图片的文本会话不受影响。

### 配置文件

`~/.minkrc`（用户级）和 `<project>/.minkrc`（项目级）可选配置。
优先级：CLI 参数 > `--config` TOML > 项目 `.minkrc` > 用户 `~/.minkrc` > 环境变量 > 默认值。
例外：`MINK_LIMITS`（JSON sandbox 限制）仍是 CLI 之后最高优先级，高于所有配置文件；
4 个 `MINK_EDIT_*` 变量则低于全部文件层（CLI > `--config` > 项目 `.minkrc` > 用户
`~/.minkrc` > env > 默认）。
环境变量在文件层之前应用，因此 `[tools]` / `[generation]` 等文件配置会覆盖同名环境变量。

`log_events = true` 时 `events.jsonl` 是诊断与审计通道：关键事件（`prefix_snapshot`、signal 恢复）使用有期限的可靠提交（写失败会反馈给调用方）；普通诊断事件在 writer 停摆、队列满时为可见降级——事件被丢弃并计入丢失报告，不会阻塞运行时。

```toml
# ~/.minkrc
[provider]
api_key = "sk-xxx"
base_url = "https://api.deepseek.com/v1"
model = "flash"
openai_reasoning_effort = "max"
openai_include_usage = true
openai_token_param = "max_tokens"
openai_tool_choice = "auto"

[provider.model_aliases]
flash = "deepseek-v4-flash"
pro = "deepseek-v4-pro"

[generation]
max_tokens = 81920
max_turns = 40
llm_first_event_timeout = 60
llm_idle_timeout = 90
llm_wait_heartbeat = 30
log_events = true

[context]
max_context = "1M"
context_compact_pct = 94
context_reserve_tokens = 64000
context_compact_tail_tokens = 256000
context_compact_max_output_tokens = 8192
context_compact_input_reduction = false

[tools]
tool_timeout = 600
tool_timeout_max = 600
sub_agent_timeout = 120
max_search_files = 5000
max_search_results = 1000
enabled_tools = ["Read", "Write", "Edit", "Grep", "Glob", "Bash"]
approval_mode = "yolo"               # yolo | write | always-ask

[tools.edit]
mode = "hashline"
fuzzy_match = true
fuzzy_threshold = 0.95
enforce_seen_lines = false

[tools.approval]
Bash = "prompt"                      # allow | deny | prompt
Read = "allow"

[signal]
policy = "full"                      # off | evidence | state_ops | restart | full
```

`tool_timeout` 是 Bash/Python/自定义工具未显式指定 `timeout` 时的默认值；`tool_timeout_max`
是单次工具调用的硬上限（默认 600 秒）。显式 `timeout` 或默认值超过该上限时 fail closed /
钳制到上限，`tool_timeout_max` 最低可设为 5 秒。

`openai_extra_body` 会合并到 `/chat/completions` 请求体中。`model`、`messages`、`stream`、`tools`、`tool_choice`、`max_tokens`、`max_completion_tokens` 不会被 extra body 覆盖。

**`enabled_tools` 是模型工具 surface 的唯一输入。** 未设置时使用 catalog 默认集合；未知名称、重复名称、缺少硬依赖或 feature 未编译的工具会在创建 session 前报错。`PythonSandbox` 必须显式列出。最终 schemas、按需 tool/workflow prompt、Bash 路由、Signal Recovery 和真实执行门禁同时依据该 surface 收缩，不存在额外 disable flag 或 sandbox 工具策略。
当前版本尚未实现交互式审批 prompt。默认 `yolo` 允许所有 approval tiers。

### 环境变量

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `DEEPSEEK_API_KEY` | — | **必需。** API 密钥 |
| `DEEPSEEK_BASE_URL` | `https://api.deepseek.com/v1` | 自定义 API 端点 |
| `TOOL_RESULT_MAX_BYTES` | `100000` | 单条工具结果截断上限 |
| `FILE_WRITE_MAX_BYTES` | `1048576` | Write/Edit 工具写入上限 |
| `MAX_SEARCH_FILES` | `5000` | Glob/Grep 最大遍历文件数 |
| `MAX_SEARCH_RESULTS` | `1000` | Grep 最大匹配结果行数 |
| `LOG_EVENTS` | `true` | 设为 `0`/`false` 关闭 events.jsonl |
| `MINK_SIGNAL_POLICY` | `full` | `off` / `evidence` / `state_ops` / `restart` / `full` |
| `MINK_IMAGE_INPUT` | — | 显式图片能力开关：`on` / `off`（覆盖 backend 声明） |
| `MINK_VISION_MODELS` | — | 视觉模型 id 逗号分隔列表（替换内置默认；空值关闭） |
| `MINK_EDIT_MODE` | `hashline` | Edit 协议：`hashline` / `replace` |
| `MINK_EDIT_FUZZY_MATCH` | `true` | Replace 模糊匹配开关 |
| `MINK_EDIT_FUZZY_THRESHOLD` | `0.95` | Replace 模糊阈值，`0.0..=1.0` |
| `MINK_EDIT_ENFORCE_SEEN_LINES` | `false` | Hashline seen-line 守卫 |
| `MINK_KEYBOARD_ENHANCEMENT` | auto | `off` 跳过 Kitty keyboard protocol 探测与启用（Shift+Enter 失效，Ctrl+J / Alt+Enter 仍可用） |
| `MINK_HOME` | `$HOME` | session 存储目录覆盖 |
| `MINK_LIMITS` | — | JSON sandbox 限制配置 |

搜索上限多层保护：`MAX_SEARCH_FILES`（文件遍历）、`MAX_SEARCH_RESULTS`（匹配行数）、
工具自身 100KB 输出保护、`TOOL_RESULT_MAX_BYTES` 最终截断。
`scanned first N files` = 文件数上限触发，`truncated at N results` = 匹配数上限触发，
`output > 100000 bytes` 或 artifact 提示 = 字节数保护触发。

`MINK_SIGNAL_POLICY=full` 时，低 belief 注入会要求 Recovery 首步先检查状态。Recovery 首步资格是独立的参数级能力判断，不等同于普通 Bash 安全策略。

## 配置系统

### 合并优先级

```
CLI 参数 > --config TOML > 项目 .minkrc > 用户 ~/.minkrc > 环境变量 > 代码默认值
```

`config.rs` 中，配置加载先读取环境变量和默认值，再依次合并用户级 / 项目级 `.minkrc` 和 `--config`，最后用 CLI 参数覆盖。

```rust
pub fn apply_provider_defaults(cfg: &mut Config) -> Result<()> {
    // 1. 环境变量覆盖特定字段
    if let Ok(v) = std::env::var("TOOL_RESULT_MAX_BYTES") { ... }
    if let Ok(v) = std::env::var("FILE_WRITE_MAX_BYTES") { ... }
    // 2. API Key: CLI 参数或配置文件 > DEEPSEEK_API_KEY
    // 3. Base URL: CLI 参数或配置文件 > DEEPSEEK_BASE_URL > 默认 DeepSeek base URL
    // 4. 模型默认: flash alias
    // 5. 验证: API Key 或 Base URL 至少一个存在
```

`flash` / `pro` 是默认模型别名，分别解析到 DeepSeek 默认模型。`.minkrc` 或 `--config` 的
`[provider.model_aliases]` 可覆盖这些别名，也可新增自定义别名。未命中别名的 `model` 会作为真实模型名原样传给 LLM backend。

OpenAI-compatible 请求通过 `[provider]` 段控制：

```toml
[provider]
openai_reasoning_effort = "max"       # off/none/false/disabled 表示不发送
openai_include_usage = true
openai_token_param = "max_tokens"     # max_tokens | max_completion_tokens
openai_tool_choice = "auto"           # auto | none | required，或 JSON 对象

[provider.openai_extra_body]
custom_boolean = true
custom_budget = 8192
```

`openai_extra_body` 仅补充 provider 扩展字段，不覆盖 `model`、`messages`、`stream`、
`tools`、`tool_choice`、`max_tokens` 和 `max_completion_tokens`。非 OpenAI-compatible
协议由 `LlmBackend` 注入处理。

显式 CLI `--config <toml>` 解析失败必须 fail fast；用户级/项目级 `.minkrc` 解析失败同样 fail fast（读取文件时非 NotFound 错误会先输出 warning 再返回错误）。

### size 解析

`parse_size_bytes()` 支持 `k`/`m`/`g` 后缀：

```rust
"100"   → 100
"1k"    → 1000
"500K"  → 500000
"1m"    → 1000000
"2M"    → 2000000
```

用于 `max_context` 字段。CLI 中低频配置通过 `--config <toml>` 或
`.minkrc` 传入。

### 环境变量分类

| 类别 | 变量 | 用途 |
|------|------|------|
| API | `DEEPSEEK_API_KEY`, `DEEPSEEK_BASE_URL` | 认证和端点 |
| 大小 | `TOOL_RESULT_MAX_BYTES`, `FILE_WRITE_MAX_BYTES` | 输出限制 |
| 信号 | `MINK_SIGNAL_POLICY` | `off` / `evidence` / `state_ops` / `restart` / `full`，默认 `full` |
| 沙箱 | `MINK_LIMITS` | JSON 格式 sandbox 限制配置 |
| 调试 | `LOG_EVENTS`, `MINK_HOME` | 日志和 session 路径 |

`context_compact_pct`、`context_reserve_tokens`、`context_compact_tail_tokens`、
`context_compact_max_output_tokens`、`context_compact_input_reduction`、`enabled_tools`、
`[provider]` 的 OpenAI-compatible 参数和 `[tools] approval_mode` 可通过 `.minkrc` 或 `--config` 配置。
Agent JSONL `SdkOptions` 和 Python `SandboxConfig` 同样直接暴露 `max_context` 及五个压缩参数，
由 `runtime::sdk_adapter` 映射到共享的 `Config`。

`validate_runtime_config()` 在 runtime 创建任何 session 文件前执行组合校验。有限窗口下要求
`context_reserve_tokens < max_context`；热尾部和摘要输出是按可用空间动态收紧的软目标，不做组合硬校验。`max_context=0` 保留禁用自动压缩和本地输入预算限制的语义。

### 工具审批配置

`ToolConfig` 从启动时解析完成的内部配置派生，随 `ToolContext` 进入工具层：

```toml
[tools]
approval_mode = "yolo" # yolo | write | always-ask

[tools.approval]
Bash = "prompt"        # allow | deny | prompt
Read = "allow"
```

approval 在构建 `ModelToolSurface` 时解析；`ToolRunner::execute_all()` 在 StormBreaker 和实际
执行前再次校验同一 resolved surface。当前 `prompt` 没有交互 UI，因此 prompt policy 会在
surface 解析阶段 fail closed。

---

### 配置语义矩阵与前端收敛决策

四个前端各有映射（core `AgentOptions`/`ResolvedConfig`、CLI `CliConfig`+TOML+overrides、server `AgentConfig` 分层 merge+env、Python dict）。字段语义按类固定，边界由对照测试钉住：

| 语义类别 | 代表字段 | 缺省（未提供） | 显式空 | 合并/替换 |
|---|---|---|---|---|
| 标量 Option | `model`、`max_tokens`、各 timeout | 保持上一层/默认 | 解析器拒绝空串 | 后层覆盖（`pick`） |
| 三态列表 | `enabled_tools` | `None`＝默认工具集 | `Some([])`＝禁用全部 | 后层整体替换，不合并 |
| 映射 | `model_aliases`、`approval`、`openai_extra_body` | 空表 | —— | 逐键合并，同键后层覆盖 |
| 子表 | `sandbox`、`sandbox_python` | —— | —— | 逐字段 pick 合并 |
| 前端专属优先级 | CLI `--config`、server env | —— | —— | env > project > user > defaults |

**决策 D2＝暂缓统一入口（选项 C）**：共享 patch 需要为上述每类语义建模并跨 crate 公开新类型；当前没有证据表明收敛能减少映射点（各前端的解析与层级合并本身不可省）。本轮以矩阵 + 边界测试收尾（`enabled_tools_none_is_default_set_and_empty_list_disables_all`、`merge_overrides_present_fields_and_keeps_absent_ones`、`merge_container_semantics_are_explicit`）；若后续跨端不一致频繁出现，再以本矩阵为规格引入 additive overrides。

## Server 配置

优先级：**环境变量 > `mink-server.toml` > `~/.minkrc` > 默认值**。

| 配置项 | 环境变量 | mink-server.toml | 默认 |
|--------|----------|------------------|------|
| 监听地址 | `MINK_SERVER_HOST` | `[server] host` | `0.0.0.0` |
| 端口 | `MINK_SERVER_PORT` | `[server] port` | `8765` |
| Mink home | `MINK_HOME` | `[agent] mink_home` | `$HOME` |
| 默认模型 | `MODEL` | `[agent] model` / `~/.minkrc` `[provider] model` | `flash` |
| 最大并发会话 | `MINK_SERVER_MAX_RUNNING` | `[server] max_running` | `4` |
| 闲置自动关闭 | — | `[server] idle_close_secs` | `1800` |
| turn 超时 | `MINK_SERVER_TURN_TIMEOUT` | — | `1200`（秒） |

`~/.minkrc` 与 TUI/CLI 共享同一配置文件，**schema 完全一致**（分组格式，扁平键拒绝，与 CLI 相同的 `deny_unknown_fields` 规则）：

- 每个会话启动时按 **项目级 `<cwd>/.minkrc` 覆盖用户级 `~/.minkrc`** 的层级合并，覆盖 CLI 的同一套分组：
  `[provider]`（model/api_key/base_url/model_aliases/openai_*/http_timeout_secs/image_input/vision_models/`[provider.image]`）、`[generation]`、`[context]`、
  `[tools]` / `[tools.edit]`、`[signal]`、`[recovery]`、`[sandbox]` / `[sandbox_python]`。
- 会话运行时选项（provider/generation/context/tools/signal/recovery/sandbox/image）完整应用到 `AgentOptions`，
  与 CLI 的 `assemble_runtime_options` 行为一致；schema 对齐由 `crates/mink-server/tests/config_parity.rs` 机械校验。
- 环境变量 `MODEL` / `DEEPSEEK_API_KEY` / `DEEPSEEK_BASE_URL` / `MINK_IMAGE_INPUT` /
  `MINK_VISION_MODELS` / `MINK_SIGNAL_POLICY` /
  `LOG_EVENTS` 在文件层之上覆盖（server 文档化优先级：环境变量 > `mink-server.toml` >
  项目 `.minkrc` > 用户 `~/.minkrc` > 默认值）。

## 上下文参数

| 参数 | 默认值 | 作用 |
|---|---:|---|
| `max_context` | 1000000 | 0 禁用 auto/preflight 与本地输入上限 |
| `context_compact_pct` | 94 | 自动压力百分比，1..=100 |
| `context_reserve_tokens` | 64000 | 主响应预留，有限窗口须小于窗口 |
| `context_compact_tail_tokens` | 256000 | 热尾部软目标，必须大于 0 |
| `context_compact_max_output_tokens` | 8192 | 摘要输出软上限，必须大于 0 |
| `context_compact_input_reduction` | false | 仅精简摘要输入 |

热尾部和摘要输出按实际空间动态收紧，不做 S/T 组合硬校验。机制见[持久化与上下文](../concepts/state-and-context.md)。

## SandboxConfig 配置项

### 会话与路径

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `mink_home` | 用户 home 目录 | 传给 Rust 的 `MINK_HOME` 根目录。也可通过 `MINK_HOME` 环境变量设置 |
| `session_layout` | `"home"` | session 路径布局。Python SDK 默认写入 `mink_home/.mink/sessions/<session_id>`；可设为 `"project"` 使用 CLI 兼容布局，`"direct"` 写入 `mink_home/<session_id>`，或 `"isolated"` 直接使用 `mink_home` 作为当前 session 目录 |
| `mission_file` | `None` | 自定义系统提示词文件路径（MISSION.md） |
| `mission_content` | `None` | 自定义系统提示词内容（字符串），与 `mission_file` 二选一 |
| `cwd` | 当前目录 | agent 的工作目录 |

### 文件系统访问

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `read_dirs` | `[]` | agent 可读取的目录（相对路径基于 cwd 解析） |
| `write_dirs` | `[]` | agent 可写入的目录 |

### 工具控制

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `enabled_tools` | `None` | 精确工具选择；`None` 使用默认集合，`[]` 禁用全部，显式列出 `PythonSandbox` 才启用它 |
| `allow_network` | `True` | 是否允许沙箱进程访问网络（LLM API 需要） |

### 资源限制

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `timeout_secs` | `600` | agent 运行总超时（秒），超时会终止 agent 进程组 |
| `tool_timeout` | `600` | 单次工具调用超时（秒） |
| `tool_timeout_max` | `600` | 单次 Bash/Python/自定义工具调用的超时上限（秒，最低 5） |
| `sub_agent_timeout` | `300` | 子代理执行超时（秒） |
| `llm_first_event_timeout` | `60` | 等待首个模型 stream event 的秒数 |
| `llm_idle_timeout` | `90` | 模型 stream 空闲超时（秒） |
| `llm_wait_heartbeat` | `30` | 等待模型响应的提示间隔；设为 `0` 关闭提示 |
| `max_tokens` | `81920` | 输出 token 上限 |
| `max_turns` | `40` | 最大循环轮数 |
| `max_context` | `1000000` | 模型上下文 token 上限；设为 `0` 时禁用自动压缩和本地输入预算限制 |
| `context_compact_pct` | `94` | 自动压缩触发百分比，范围 1-100 |
| `context_reserve_tokens` | `64000` | 主请求响应预留，同时限制主请求输出预算 |
| `context_compact_tail_tokens` | `256000` | 压缩后原样保留的热历史目标 |
| `context_compact_max_output_tokens` | `8192` | 摘要请求输出上限 |
| `context_compact_input_reduction` | `False` | 摘要前是否删除 thinking 并压缩工具噪声 |
| `max_search_files` | `5000` | Glob/Grep 最大遍历文件数 |
| `max_search_results` | `1000` | Grep 最大匹配结果行数 |
| `max_memory_mb` | `1024` | 内存限制（仅 nsjail cgroup） |
| `max_pids` | `64` | 进程数限制（仅 nsjail cgroup） |

### 调试

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `verbose` | `False` | 启用详细日志输出 |
| `signal_policy` | `None`（实际默认 `full`） | 信号策略：`off` / `evidence` / `state_ops` / `restart` / `full`；`None` 继承 `MINK_SIGNAL_POLICY` |
| `stream_events` | `True` | 是否让 Rust 侧输出过程事件；设为 `False` 时仅输出最终 `final`，适合非流式长任务 |

本地调试可以通过 `MINK_BINARY=/path/to/mink-core` 覆盖 SDK 使用的二进制。未设置时优先使用 wheel 内置二进制，然后查找 `PATH`。

默认发布的 SDK wheel 使用精简 `mink-core`，不包含 `PythonSandbox` 工具。需要该工具时可手动构建：

```bash
cargo build -p mink-cli --release --no-default-features --features "sdk-bin python-sandbox" --bin mink-core
MINK_BINARY=./target/release/mink-core python your_script.py
```

或构建带 `PythonSandbox` 的 wheel：

```bash
MINK_SDK_FEATURES="sdk-bin python-sandbox" python scripts/build_wheel.py
```

### 沙箱后端

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `sandbox_backend` | `"auto"` | 传递给 Rust 内部 (`MINK_LIMITS`)：`"auto"` / `"nsjail"` / `"bwrap"` / `"sandbox-exec"` / `"off"` |

沙箱由 Rust ``mink-core`` 二进制内部处理。Python SDK 不构造任何沙箱命令。

### API 配置

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `api_key` | `""` | DeepSeek API 密钥。也可通过 `DEEPSEEK_API_KEY` 环境变量设置 |
| `api_url` | `""` | DeepSeek API 地址。也可通过 `DEEPSEEK_BASE_URL` 环境变量设置 |
| `model` | `""` | 模型档位：`"flash"` / `"pro"`，也接受内部名 `"deepseek-v4-flash"` / `"deepseek-v4-pro"` |


## TOML 字段索引

以下字段来自当前分组解析器；所有分组拒绝未知字段。Option 未提供时继承上一层；映射逐键合并，列表整体替换。默认值及语义见本页前面的对应说明。

### `[provider]`

| 字段 | Rust 解析类型 |
|---|---|
| `api_key` | `Option<String>` |
| `base_url` | `Option<String>` |
| `model` | `Option<String>` |
| `model_aliases` | `Option<BTreeMap<String, String>>` |
| `openai_reasoning_effort` | `Option<String>` |
| `openai_include_usage` | `Option<bool>` |
| `openai_token_param` | `Option<String>` |
| `openai_tool_choice` | `Option<serde_json::Value>` |
| `openai_extra_body` | `Option<BTreeMap<String, serde_json::Value>>` |
| `http_timeout_secs` | `Option<u64>` |
| `image_input` | `Option<String>` |
| `vision_models` | `Option<Vec<String>>` |
| `image` | `ImageConfigFile` |

### `[provider.image]`

| 字段 | Rust 解析类型 |
|---|---|
| `detail` | `Option<String>` |
| `max_images_per_request` | `Option<usize>` |
| `max_image_bytes_per_request` | `Option<String>` |
| `max_image_bytes` | `Option<String>` |
| `max_dimension` | `Option<u32>` |
| `max_pixels` | `Option<String>` |

### `[generation]`

| 字段 | Rust 解析类型 |
|---|---|
| `max_tokens` | `Option<i32>` |
| `max_turns` | `Option<i32>` |
| `llm_first_event_timeout` | `Option<i32>` |
| `llm_idle_timeout` | `Option<i32>` |
| `llm_wait_heartbeat` | `Option<i32>` |
| `log_events` | `Option<bool>` |
| `output_format` | `Option<String>` |

### `[context]`

| 字段 | Rust 解析类型 |
|---|---|
| `max_context` | `Option<String>` |
| `context_compact_pct` | `Option<u8>` |
| `context_reserve_tokens` | `Option<usize>` |
| `context_compact_tail_tokens` | `Option<usize>` |
| `context_compact_max_output_tokens` | `Option<i32>` |
| `context_compact_input_reduction` | `Option<bool>` |

### `[tools]`

| 字段 | Rust 解析类型 |
|---|---|
| `approval_mode` | `Option<ToolApprovalMode>` |
| `approval` | `Option<BTreeMap<String, ToolApprovalPolicy>>` |
| `enabled_tools` | `Option<Vec<String>>` |
| `skills` | `Option<Vec<String>>` |
| `tool_timeout` | `Option<i32>` |
| `tool_timeout_max` | `Option<i32>` |
| `sub_agent_timeout` | `Option<i32>` |
| `max_search_files` | `Option<usize>` |
| `max_search_results` | `Option<usize>` |
| `edit` | `EditConfigFile` |

### `[tools.edit]`

| 字段 | Rust 解析类型 |
|---|---|
| `mode` | `Option<EditMode>` |
| `fuzzy_match` | `Option<bool>` |
| `fuzzy_threshold` | `Option<f64>` |
| `enforce_seen_lines` | `Option<bool>` |

### `[signal]`

| 字段 | Rust 解析类型 |
|---|---|
| `policy` | `Option<SignalPolicy>` |

### `[recovery]`

| 字段 | Rust 解析类型 |
|---|---|
| `format_window_size` | `Option<usize>` |
| `format_max_errors` | `Option<usize>` |
| `request_max_retries` | `Option<u32>` |
| `request_timeout_secs` | `Option<u64>` |

### `[sandbox]`

| 字段 | Rust 解析类型 |
|---|---|
| `enabled` | `Option<bool>` |
| `backend` | `Option<String>` |
| `read_dirs` | `Option<Vec<String>>` |
| `write_dirs` | `Option<Vec<String>>` |
| `allow_network` | `Option<bool>` |
| `max_memory_mb` | `Option<u64>` |
| `max_pids` | `Option<u32>` |
| `timeout_secs` | `Option<u64>` |

### `[sandbox_python]`

| 字段 | Rust 解析类型 |
|---|---|
| `wasm_path` | `Option<String>` |
| `stdlib_dir` | `Option<String>` |
| `timeout` | `Option<u64>` |
| `read_dirs` | `Option<Vec<String>>` |
| `write_dirs` | `Option<Vec<String>>` |
| `package_dirs` | `Option<Vec<String>>` |

## Python 字段索引

`SandboxConfig` 的完整字段和默认值如下；`None` 表示保留 Rust 默认或环境继承，映射见各分组说明。

| 字段 | 类型 | Python 默认 |
|---|---|---|
| `mink_home` | `Optional[str]` | `None` |
| `mission_file` | `Optional[str]` | `None` |
| `mission_content` | `Optional[str]` | `None` |
| `read_dirs` | `list[str]` | `[]` |
| `write_dirs` | `list[str]` | `[]` |
| `allow_network` | `bool` | `True` |
| `enabled_tools` | `Optional[list[str]]` | `None` |
| `edit_mode` | `str` | `'hashline'` |
| `edit_fuzzy_match` | `bool` | `True` |
| `edit_fuzzy_threshold` | `float` | `0.95` |
| `edit_enforce_seen_lines` | `bool` | `False` |
| `skills` | `list[str]` | `[]` |
| `inline_skills` | `list[InlineSkill]` | `[]` |
| `skill_discovery_policy` | `str` | `'defaults'` |
| `python_sandbox_wasm_path` | `str` | `'cpython-wasi/python.wasm'` |
| `python_sandbox_stdlib_dir` | `str` | `'cpython-wasi'` |
| `python_sandbox_read_dirs` | `list[str]` | `[]` |
| `python_sandbox_write_dirs` | `list[str]` | `[]` |
| `python_sandbox_package_dirs` | `list[str]` | `[]` |
| `python_sandbox_timeout` | `int` | `30` |
| `max_memory_mb` | `int` | `1024` |
| `max_pids` | `int` | `64` |
| `timeout_secs` | `int` | `600` |
| `tool_timeout` | `int` | `600` |
| `tool_timeout_max` | `int` | `600` |
| `sub_agent_timeout` | `int` | `300` |
| `llm_first_event_timeout` | `int` | `60` |
| `llm_idle_timeout` | `int` | `90` |
| `llm_wait_heartbeat` | `int` | `30` |
| `max_tokens` | `int` | `81920` |
| `max_turns` | `int` | `40` |
| `max_context` | `int` | `1000000` |
| `context_compact_pct` | `int` | `94` |
| `context_reserve_tokens` | `int` | `64000` |
| `context_compact_tail_tokens` | `int` | `256000` |
| `context_compact_max_output_tokens` | `int` | `8192` |
| `context_compact_input_reduction` | `bool` | `False` |
| `max_search_files` | `int` | `5000` |
| `max_search_results` | `int` | `1000` |
| `verbose` | `bool` | `False` |
| `stream_events` | `bool` | `True` |
| `sandbox_backend` | `str` | `'auto'` |
| `api_key` | `str` | `''` |
| `api_url` | `str` | `''` |
| `model` | `str` | `''` |
| `provider_http_timeout_secs` | `Optional[int]` | `None` |
| `session_id` | `str` | `''` |
| `session_layout` | `str` | `'home'` |
| `signal_policy` | `Optional[str]` | `None` |
| `format_window_size` | `Optional[int]` | `None` |
| `format_max_errors` | `Optional[int]` | `None` |
| `request_max_retries` | `Optional[int]` | `None` |
| `request_timeout_secs` | `Optional[int]` | `None` |
| `cwd` | `Optional[str]` | `None` |

`mink-server.toml` 的服务监听字段在 `[server]`，默认模型和 Mink home 在 `[agent]`；它与 CLI `--config` 的分组字符串是不同入口。
