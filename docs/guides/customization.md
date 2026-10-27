# 定制规则与技能

> 更新日期：2026-10-04

选择工具、Skills 与 MISSION。


## 操作起点

先选择工具范围，再按任务加载 Skills 或 MISSION。启动后核对可用技能和工具；规则只能覆盖允许的 section，未知工具、保留 section 和非法参数在启动时拒绝。

## Skills（技能）

### 启用

```bash
# CLI 加载（可重复 --skill；与 .minkrc 的 [tools].skills 等价）
mink -m flash --skill debugging --skill tdd -i

# 查看可用
mink --list-skills
```

### 内置技能（编译时嵌入）

所有 `skills/<name>/SKILL.md` 在编译时嵌入二进制，零文件 I/O：

| 技能名 | 描述 | 适用场景 |
|--------|------|---------|
| `debugging` | 四阶段系统调试 | 遇到 bug、测试失败、非预期行为 |
| `verification` | 验证门控：禁止未验证就声称完成 | 完成任务、commit 前 |
| `tdd` | 红绿重构循环 | 新功能或修 bug |
| `pre-code-check` | 先搜索调用点、读上下文、验证假设 | 编辑文件前 |

### 搜索路径（优先级）

1. `<project>/.claude/skills/<name>/SKILL.md` — 项目级覆盖
2. `<project>/skills/<name>/SKILL.md` — 项目开发目录
3. `~/.claude/skills/<name>/SKILL.md` — 用户全局
4. **内置** — 编译时嵌入，兜底

### Frontmatter 字段

`SKILL.md` 顶部可选 YAML frontmatter（按 Agent Skills 最小子集解析）：

| 字段 | 作用 |
|------|------|
| `name` | 规范技能名；合法时优先于目录名（不一致会在 `skill://list/all` 的 Warnings 中提示） |
| `description` | 发现描述；缺省时回退正文首个非空行 |
| `when_to_use` | 附加触发语，拼接在 `description` 之后 |
| `hide` / `disable-model-invocation` | `true` 时不进入 skill index，仅可按名读取或 `--skill` 选中 |

单行值（可带引号）与块标量 `>`（折叠）/ `|`（保留换行）都支持；未闭合的 frontmatter 视为不存在。

### 索引渲染（模型可见面）

system prompt 的 skill index 与 `skill://list` 对技能名与描述做统一硬化：去控制字符、折叠为单行、转义尖括号（描述不能伪造 section 标记）；描述额外按 1,536 字符截断；条目总量有预算——有限窗口取 1%（clamp 2,000–8,000 字符），窗口不限时为 8,000，`MINK_SKILL_INDEX_BUDGET_CHARS` 可覆盖。超预算时保留全部技能名，按来源降级删除描述：用户级 → runtime → 项目 `.claude/skills` → 项目 `skills/` → 内置最后。`skill://list/all` 与 CLI/TUI 仍显示完整描述（诊断/宿主面）。

### 能力视图

每次 runtime 启动构建 `CapabilitySnapshot`。system prompt 的 skill index、selected skills、instruction files、rules 以及 `Read skill://` / `Read rule://` 都读取这份统一视图。CLI、Rust runtime、Python SDK 和子代理不各自重新扫描。

### 触发（推荐做法）

索引只是清单，不会自己触发。要让 agent 主动加载，把“时机→技能”写进项目常驻规则（如项目根 `AGENTS.md`，每个任务都会注入），例如：

```markdown
### 技能纪律

- **先看技能再动手**：任务与 skill-index 中某技能的适用场景匹配时，先 `Read skill://<name>` 并遵循其步骤执行，不要等用户提醒。
- **时机对应**（技能须在 skill-index 中可见）：编辑代码前核查调用点与假设 → `pre-code-check`；出现 bug、测试失败或异常行为 → `debugging`；实现新功能或修 bug → `tdd`；声称完成、commit 前 → `verification`。
```

备选：`.minkrc` 的 `[tools].skills = ["verification"]` 会在每次请求预载整段正文（无发现过程，适合强制执行的少数技能）。

## MISSION（自定义系统提示词）

通过 `--mission PATH` 加载。MISSION 可覆盖少量稳定的 core section，可追加自定义 section；
**不能**覆盖工具、workflow 或 runtime-owned 内容。

### Section 分类

MISSION.md 使用行首一级标题（`# section-id`）分段。section 分三类：

| 类型 | section ID | 行为 |
|------|------------|------|
| 可覆盖 core | `system-conventions`、`agent-identity`、`environment`、`execution-codes`、`belief-awareness`、`output-language` | 替换同名 core |
| runtime-reserved | 工具 prompt、workflow、`runtime-capabilities`、`tool-inventory`、`rules`、`instruction-files`、`rule-index`、`skill-index`、`selected-skills`、`current-plan` 等 | 启动时 fail fast |
| 普通自定义 | 不属于以上两类的唯一 ID | 作为 `mission:<section-id>` 原样追加 |

```markdown
# agent-identity
你是文档处理助手，负责根据素材文件生成结构化文档。

# mission-rules
- 严格遵循素材内容，不得额外杜撰

# process-flow
## Phase 1: 素材分析
...
```

### 用法

```bash
mink --mission ./my-task.mission.md -i
mink --mission ./my-task.mission.md --config $'[tools]\nskills=["debugging"]' -i
```

Python SDK 使用 `SandboxConfig(mission_file=...)` 或内联 `mission_content`，见
[嵌入与 SDK 使用](../integration/rust.md)。

### 迁移规则

- 旧 `# rules` → 改为 `# mission-rules` 或其他业务 ID
- 不再支持 `using-your-tools`、`anchored-edit-protocol`、`rationalization-table` 等旧 alias
- section ID 必须唯一；重复 heading 或占用 runtime-reserved ID 导致启动失败
- MISSION 只影响 prompt 文本，不改变工具 surface（工具选择仍用 `enabled_tools`）
- `MINK_SIGNAL_POLICY=off` 时 prompt 不存在 `belief-awareness`，MISSION 也不能创建它

## SubAgent（子代理）

### 参数

| 参数 | 说明 |
|------|------|
| `prompt` | 子代理任务描述（**必需**） |
| `description` | 日志标记（可选） |
| `fork` | 是否继承父会话上下文（可选，默认独立） |

### 模式

- **独立（默认）**：全新空会话，适合文件调查、搜索、隔离验证
- **Fork（`fork=true`）**：继承完整 session 状态（对话、压缩边界、摘要、计划、artifact），适合延续性任务

Fork 在子 runtime 初始化前复制父 session 目录，清除 child 的身份/事件/统计文件。
子代理从克隆的 `context-state.json` 恢复活跃投影；artifact 序号从克隆 index 继续。

### 输出

```
[sub-agent <id>] <status> (in=<n>, out=<n>)
Thinking: ...
Text: ...
```

Token 用量计入父会话统计。默认超时 1800 秒（30 分钟，`sub_agent_timeout` 可调）。超时后标记为 `failed`。

## 下一步

查看[配置参考](../reference/configuration.md)、[工具参考](../reference/tools.md)或返回[文档总览](../start/overview.md)。
