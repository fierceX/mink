# 终端操作

> 更新日期：2026-10-04

REPL、Full TUI、Inline TUI 与快捷键。


## 操作起点

前置：已完成[快速开始](../start/quickstart.md)。在项目目录用 `mink -i`、`mink --tui` 或 `mink --tui=inline` 启动。提交后看到模型回复与真实工具结果；运行中可继续编辑草稿。

## 终端模式与操作

项目提供 REPL、Full TUI、Inline TUI 和非交互 CLI 四种使用模式。

### REPL 模式（`-i`）

基于 rustyline 的行编辑器，适合日常编码交互：

```
mink interactive mode (type 'exit' or Ctrl+D to quit)
> scan this project for Rust errors
[tool] Bash(command="cargo check")
...
```

- 输入：rustyline 行编辑（历史、Tab 补全、Ctrl+W/Del）
- 输出：stderr 渲染（灰色 thinking、黄色 tool call、普通 text）
- 标题栏：ANSI escape 更新终端窗口标题
- 历史记录：持久化到 `~/.mink/history`

### Full TUI（`--tui` / `--tui=full`）

使用 alternate screen 和应用内 transcript，支持鼠标滚动、工具卡片点击、自动折叠和展开，适合需要完整结构化操作的编码会话。

### Inline TUI（`--tui=inline`）

将完成的结构化内容渐进写入终端原生 scrollback，底部保留流式尾部、状态栏和输入区。
通过 terminal scrolling region 写入稳定内容，避免宽字符占位空格和 viewport 整体重绘，适合 SSH 和长日志：

```
flash B:0.73 T:12 R:45 I:200K(50%) O:20K C:400K(40%) [idle]
────────────────────────────────────────────────────────────────
 原生 terminal scrollback（已完成对话和结构化工具卡片）
────────────────────────────────────────────────────────────────
 > 输入区域（多行输入）
```

**状态栏字段含义：**

| 字段 | 示例 | 含义 |
|------|------|------|
| `flash` | 模型名 | 当前模型 |
| `B:0.73` | 信念度 | 工具执行可靠性评分（0.0~1.0） |
| `T:12` | 对话轮次 | 当前对话的用户输入轮数 |
| `R:45` | API 请求 | 累计 LLM API 请求次数 |
| `I:200K` | 输入 tokens | 总输入 tokens，括号内为缓存命中率 |
| `O:20K` | 输出 tokens | 总输出 tokens |
| `C:400K` | 上下文 | 当前上下文 tokens，括号内为使用率 |
| `[idle]` | 工作状态 | idle / waiting / thinking / generating / tool / sub-agent / compacting / error |
| `·30s` | 等待心跳 | LLM 流式/首事件等待的精简标签（仅状态栏瞬时显示，流恢复/结束即清除） |

**B（信念度）值含义：**

| B 值 | 含义 |
|------|------|
| 0.75 | 初始（信任先验） |
| > 0.7 | 顺利 |
| 0.5~0.7 | 偶有小错 |
| 0.3~0.5 | 频繁出错 |
| < 0.3 | 严重 |

两种 TUI 共用：
- 多行输入和 grapheme 编辑（组合字符、Emoji、中文宽字符），Ctrl+C 中断当前 turn
- 运行中 Enter 提交当前 turn 的中途引导，在下一安全边界生效，不启动额外轮次
- 工具调用与结果按 ID 合并，显示退出状态、Plan/Todo 状态和 Artifact 元数据
- 语义工具着色、自动折叠和同一套 Markdown renderer（标题、列表、引用、代码块、表格、diff）
- Plan/Todo 详情从 session 状态文件加载；超长工具结果折叠后显示 `artifact://ID`
- `/artifact ID` 查看有界预览，Plan/Todo/Artifact 详情按内容宽度折行

模式差异：
- Full：鼠标捕获，应用内滚动，卡片可展开/折叠
- Inline：鼠标由终端处理，自动折叠后不可展开；详情临时使用 alternate screen

**TUI / REPL 操作：**（标注 TUI 的操作仅用于 Full/Inline）
| 操作 | 行为 |
|------|------|
| `Ctrl+C` | TUI 工作中停止并等待权威终态，2 秒内再按退出；空闲退出；REPL 保持原行为 |
| `Enter`（TUI） | 空闲发送新任务；运行中提交引导；停止期间保留草稿 |
| `/inputs`（TUI） | 打开输入面板；↑↓ 选择，e 编辑，w 撤回，r 明确续发或恢复失败草稿；applying 项只读 |
| `/resume ID`（TUI） | 空闲时明确将未应用输入用于新轮次 |
| `/withdraw ID`（TUI） | 按 revision 撤回尚未消费的输入 |
| `Ctrl+V` | 粘贴剪贴板图片（macOS）：暂存到 session `attachments/`，随下一条消息附带绝对路径 |
| `Shift+Enter` | 输入框内换行（需终端支持 Kitty keyboard protocol） |
| `Ctrl+J` / `Alt+Enter` | 换行（不依赖终端协议的兜底键） |
| `/flash` / `/pro` | 切换模型 |
| `/compact` | 手动触发上下文压缩 |
| `/help` / `/status`（TUI） | 打开本地帮助/完整统计面板，不向对话追加正文 |
| `/details`（TUI） | 键盘选择卡片折叠、Plan/Todo/Artifact/子代理详情 |
| `/skills` | 显示 skill 列表 |
| `/plan` / `/todos` | 打开 Plan/Todo 详情 |
| `/artifact ID` | 打开最多 256 KiB 的 Artifact 预览 |
| `/sub-agent ID` | 打开子代理详情 |
| `/exit` / `/quit` / `/q` | 退出 |
| 未知 `/xxx` | 本地提示，不发送给 LLM |
| 行首空格 + `/xxx` | 作为普通文本发送 |
| `Esc`（TUI） | 关闭补全、面板或返回主界面；主界面不退出 |
| `↑↓`（TUI） | 非空草稿按视觉行移动并保留目标列；空输入或历史浏览时切换历史 |
| `Home` / `End`（TUI） | 当前逻辑行首尾；Ctrl+A / Ctrl+E 为整个草稿首尾 |
| `Ctrl+Z` / `Alt+Z`（TUI） | 撤销 / 重做；整次粘贴作为一项编辑 |
| `PgUp` / `PgDn`（TUI） | 按当前正文或详情视口翻页 |
| `Ctrl+L` / `/latest`（TUI） | 恢复正文跟随最新，不重置详情阅读位置 |
| `Ctrl+D` | REPL 中退出；TUI 仅在文字、图片、上传和准入均为空闲时退出 |

`/flash` / `/pro` 不会发送给 LLM；TUI 在任务执行期间拒绝切换模型和手动压缩，保留命令草稿。

TUI 阅读历史后，新输出、工具边界、封口和 resize 保留阅读位置。异步发送后可继续编辑；迟到回执只清理发送时的草稿，失败原稿可在 `/inputs` 恢复。窄终端优先显示工作状态，完整模型名和统计见 `/status`。撤回或续发已有输入保留未发送草稿和图片；编辑已有输入只修改文本，待发送的新图片留给下一次新提交。

TUI 输入框在运行中仍可编辑，图片可随引导一起提交。输入上方先显示 `Accepted; waiting for safe boundary`，只有写入正式历史后才回显 `[Added to context]`；不表示模型已经遵循。提交失败保留文字和图片；停止或重启后的未应用指令仍保留，可用 `/inputs` 查看并明确续发或撤回，不会自动执行。含引导的历史按完整正式轮次恢复；只有权威 outcome 才结束任务，停止通知与完成、失败通知区分。

macOS 任务通知：Ghostty/iTerm2/WezTerm 使用终端原生通知，点击由终端定位对应
窗口/会话。Terminal.app 可用已安装的 `terminal-notifier`，点击激活 Terminal；其他终端
可使用环境中的 `__CFBundleIdentifier` 作为激活目标。工具缺失时保留终端通知与铃声，
不再发出点击打开脚本编辑器的 AppleScript 通知。显示通知还需允许终端的系统通知。

`Shift+Enter` 需要终端上报修饰键（Kitty keyboard protocol 的 `DISAMBIGUATE_ESCAPE_CODES`）：
TUI 启动时探测并启用，退出时自动还原，panic 时由 panic hook 还原；Ghostty / kitty /
WezTerm / Alacritty / iTerm2 3.5+ 等终端支持。`Ctrl+J` 与 `Alt+Enter` 在任何终端都能换行。
`MINK_KEYBOARD_ENHANCEMENT=off` 可跳过探测与启用（不响应探测的终端上探测最多等待 2 秒）。

图片操作见[图片输入](images.md)。

### 标题栏（REPL/CLI 模式）

终端窗口标题显示相同统计：`flash B:0.73 T:12 R:45 I:200K(50%) O:20K C:400K(40%)`
信念度在每次工具调用后实时更新，低于阈值时在同轮的下次 LLM 调用前注入提示或中止。

### 非交互 CLI 模式

```bash
# 单次查询
./target/release/mink -m flash "explain this"

# 管道输入
cat main.rs | ./target/release/mink -m flash "review"

# ndjson 结构化输出
./target/release/mink --print "list files"
```

prompt 为空且 stdin 是终端时自动进入交互模式。非终端 stdin 时读取 stdin 作为 prompt。

## TUI 本地面板与工具状态

`/help`、`/status`、`/inputs`、`/details` 是本地面板，不是模型工具，也不向正式对话
追加帮助或状态正文。`/inputs` 的查看/编辑/撤回/明确续发调用持久 Inbox 的公开协议，
applying 项不能修改。撤回/续发已有项不消耗新草稿和图片，编辑已有项只修改文本；
新图片仅随新提交发送。Plan/Todo 修改仍通过现有工具。
Full/Inline 和 replay 保存完整 `ToolStatus`（Succeeded / Failed / Blocked / Interrupted）；
缺少状态的旧事件显示“状态未记录”。JSONL/REST/SSE 字段保持兼容，正文不用于反推状态。

macOS TUI 完成/失败/停止通知属于本地展示，不是模型工具。支持原生通知的终端
只发单条 OSC；可选 `terminal-notifier` 点击激活终端应用，不执行脚本，不使用
AppleScript 通知回退。

## 下一步

查看[配置参考](../reference/configuration.md)、[工具参考](../reference/tools.md)或返回[文档总览](../start/overview.md)。
