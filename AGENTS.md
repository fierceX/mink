# Agents Guide

> 更新日期：2026-11-28

## 项目概览

Mink 是用 Rust 1.99+ 实现的轻量 AI coding agent，面向 DeepSeek/OpenAI-compatible API：单二进制、终端优先，也可作为库嵌入（`mink::runtime`）。核心能力：

- LLM 流式请求 → 工具执行 → 决策的内循环；REPL / TUI 两种终端模式，`AgentRuntime::start() → run_turn()/stream_turn() → shutdown()` 无需子进程
- 信号驱动的信念系统：轨迹证据注入（`[trajectory]`/`[detector]`）、编辑循环复核、恢复首步守卫；`SignalPolicy` 分档，阈值/超参为内部常量，`MINK_SIGNAL_POLICY=off` 关闭
- 上下文自适应压缩：显式阈值/响应预留/热尾部/摘要预算，摘要以 `<compacted-summary>` checkpoint 投影；Todo（`todos.json` 权威快照 + revision/稳定 ID）、Artifact（超长输出落盘）、轻量资源读取（artifact/skill/rule/session URL）
- Goal（独立扩展包 `crates/mink-goal`）：`GoalPlugin::install_into` + `GoalEngine::attach(handle, plugin)` 组合于 `mink-core` 通用原语之上；草案确认、独立只读审核、按需读取，显式跨轮目标、默认 20 turn/60 分钟绝对期限、三种验收、持久恢复与未知副作用对账；core `SessionLease` 覆盖所有可写 runtime。不需要 Goal 的嵌入宿主可零开销跳过。
- 工具元数据与审批策略；Edit 双模式（Hashline snapshot / Replace exact+fuzzy，启动后固定）；Python 宿主 Programmatic Tool Calling（`RunCode`，支持 `PtcMode::Off` / `Tool` / `Exclusive` 与 Subprocess / 可选 WASI 后端）；子代理（isolated home，可目录级 fork）；TUI `Ctrl+V` 粘贴图片
- 网页搜索与读取由 `[web]` 独立开启（`WebSearch`/`WebFetch` 与原生联网声明）。公开历史和客户端事件只发布展示字段。

---

## Agent 工作规范

本文件每个任务都会注入；以下是本仓库对 agent 的全局行为约束，详细子系统契约见对应目录的 `AGENTS.md`。

### 技能纪律

- **先看技能再动手**：任务与 skill-index 中某技能的适用场景匹配时，先 `Read skill://<name>` 并遵循其步骤执行，不要等用户提醒。
- **时机对应**（技能须在 skill-index 中可见）：编辑代码前核查调用点与假设 → `pre-code-check`；出现 bug、测试失败或异常行为 → `debugging`；实现新功能或修 bug → `tdd`；声称完成、commit 前 → `verification`；结构性、调用关系或影响面问题 → `ripwire` / `codegraph`；处理 Office 文档 → `officecli`。

### 改动纪律

- **先搜后改**：先 `Grep`/`Glob` 定位、再 `Read` 精确目标范围；禁止凭印象猜路径、行号、字段名或 API。
- **因果自检**：每处改动前答出 cause（改什么行为）/ effect（预期观察）/ verify（如何验证）；答不出就不改。
- **独立小步**：因果独立的改动分开验证与提交，不捆绑无关修改。
- **不动不知道的**：先读调用链与既有测试再决定。
- **走公开协议**：计划用 `PlanDraft`/`PlanConfirm`/`PlanClear`，Todo 用 `TodoRead`/`TodoWrite`/`TodoAdvance`；编辑文件先读后写；**禁止直接写 `plan.md`/`todos.json`/`context-state.json` 等运行时状态文件**。
- **不伪造成功**：失败如实报告；不得绕过 `format_tool_result`/artifact 输出保护或任何 fail-closed 边界。

### 验证与失败纪律

- **改完必验**：跑相关测试并读完整输出（`cargo test -p mink-core` 起手；涉及 CLI 加 `-p mink-cli`；构建矩阵改动跑 `make feature-matrix`）。
- **提交前干净**：`cargo fmt --all -- --check` 与 `cargo clippy --workspace --all-targets --all-features -- -D warnings` 必须无输出（CI 同款命令）。
- **报告证据**：声称“完成/修复/通过”前必须有命令输出佐证；无法验证就说明无法验证。
- **期限测试**：正常 runtime 回归使用生产的日志提交与 shutdown 预算；短期限仅注入故障测试，不通过全局 `cfg(test)` 缩短健康路径的等待时间。
- **回归意识**：核心路径（压缩/持久化/编辑/信号）注意 `crates/mink-core/tests/invariants.rs` 与 regression 套件；删除或弱化被测试钉住的行为前先确认其已废弃。
- **停止-重析**：同一命令连续失败或输出异常时停下，重读代码与错误，换一种方法；禁止无修改重复相同命令。
- **假定不可信**：编译通过≠行为正确，测试全绿≠没有回归；用新证据（针对性测试/日志）确认因果，与先前结论矛盾时按新证据行事并明说。

### 架构与文档纪律

- **文档范围**：正式文档只描述已实现行为、设计约束、验证方法与测量证据；实施计划、临时审计及评审报告仅本地保留，不入库。
- **契约必守**：「关键不变式」不可逾越；确需变更协议时先更新对应 `AGENTS.md` 与 `docs/` 章节，并保持全仓一致。
- **同步文档**：架构/行为变更更新 `docs/concepts/` 的对应机制、`docs/guides/` 的操作说明及 `docs/reference/` 的唯一权威定义；集成契约更新 `docs/integration/`，新增公开页登记 `docs/manifest.json`。同步本文件或子目录 `AGENTS.md`（含更新日期）；用户可见变更记入 `CHANGELOG.md` Unreleased。维护记录放 `docs/development/`，不发布。
- **测试锚定**：新增行为必有测试；不变式守护优先落在 `tests/invariants.rs` 或模块级回归测试。
- **性能证据**：`docs/development/performance.md` 维护测量方法与索引，日期结果及脱敏 JSON/日志存 `docs/development/benchmarks/`；保留版本、dirty 标记、样本口径及失败/跳过，不把历史结果称为当前 HEAD 实测。日常输出留 `target/bench/`。
- **官网展示**：`docs/index.html` 是静态官网，与 server 工作台分离；以轻量可嵌入、长任务上下文、可靠编辑为主线。改动运行 `node scripts/test-homepage.mjs`、`python3 -m unittest discover -s scripts -p 'test_hero_replay.py'` 并检查桌面/窄屏渲染。

### 提交纪律

- **说明范围**：提交说明与代码注释描述行为、原因和约束，不包含实施计划中的阶段编号或审计优先级标记。
- **卫生**：不提交敏感数据（模型轨迹、密钥、用户目录、内网地址）与实验性未跟踪产物；拿不准先问。
- **message**：一个主题一个提交，完整概括 what + why + 影响面；「版本号 + 文档同步」这类准备性改动可单独成提交。

---

## 运行时分层（概览）

```
main.rs → OrchActor (agent/orchestrator.rs) → TurnExecutor (agent/turn.rs)
  TurnExecutor 内循环（同一用户输入可多轮 tool_use）：
  1. 压缩检查（同一输入内可重复压缩，严格下降终止，无次数上限）
  2. LLM 流式请求（SSE → Event；工具层解析）
  3. Scavenge 回收遗漏工具调用 → 持久化 assistant 消息
  4. ToolRunner::execute_all（surface gate → StormBreaker → dispatch → 格式化/artifact）
  5. 持久化 tool results → Display 输出 → 信号采集 → belief → decision
```

分层：LLM 层（`llm/client.rs`、`llm/transport.rs`、`sse/*`）、工具层（`tools/runner.rs`、`tools/file.rs`、`tools/{bash,python,todo,plan}.rs`）、资源与能力层（`resources/router.rs`、`capabilities/*`）、信号层（`guard/*`、`agent/{belief,decision}.rs`）、持久化层（`session/{store,compaction,plan,todo,prefix,artifacts}.rs`）、UI 层（`crates/mink-cli/{ui,tui}/*`）。完整模块职责见 `docs/concepts/architecture.md`，REPL/TUI 细节见 `docs/concepts/runtime.md` 与 `docs/development/tui.md`。

---

## 子目录契约索引

修改对应子系统前请先阅读其本地 `AGENTS.md` 中的全部不变式与协议边界：

- `crates/mink-core/AGENTS.md`：压缩与持久化、LLM 有界恢复、通用扩展原语、Plan 与 Todo、信号系统、嵌入事件与失败契约、工具面与执行、Read/Edit 协议、新增工具步骤。
- `crates/mink-goal/AGENTS.md`：Goal 插件装配、生命周期、独立审核、验收门槛、恢复与 `GoalStore` 契约。
- `crates/mink-cli/AGENTS.md`：Display、TUI、Inline/Full 渲染与输入/剪贴板/附件/REPL 契约。
- `apps/web/AGENTS.md`：人类输入与 Web 工作台、`mink-server` 内嵌前端、卡片/草稿/附件/目标/移动阅读契约。

---

## 开发提示

```bash
cargo test              # 日常测试（跳过重型测试）
cargo test -p mink-core --lib           # 仅 core 单元测试
cargo test --workspace                  # 全 workspace
cargo test --features slow-tests -- --include-ignored   # 全量（含 WASM 沙箱）
make build / make check / make test / make feature-matrix / make regression-all / make bench-check
cargo fmt --all -- --check && cargo clippy --workspace --all-targets --all-features -- -D warnings
```

`crates/mink-core/src/tools/sandbox_python.rs` 的重型测试默认跳过，CI 用 `--features slow-tests -- --include-ignored` 全量覆盖。

---

## 文档索引

- `docs/start/overview.md`：三条阅读路径与完整清单。
- `docs/concepts/architecture.md`：模块查找首选；运行契约见 `runtime.md`、`state-and-context.md`、`tools-and-capabilities.md`、`recovery-and-signals.md`、`images.md`。
- `docs/guides/`：终端、Web、会话与引导、Plan/Todo、上下文、图片、定制、安全和排查。
- `docs/integration/`：Rust、Python、扩展契约。
- `docs/reference/`：配置、工具、机器协议、HTTP API 和用量的唯一完整定义。
- `docs/development/`：TUI、性能、Web 与官网维护；`docs/migration.md` 为迁移对照。
- `make docs-check` / `make docs-site`：统一校验与组装，产物 `target/docs-site`。
