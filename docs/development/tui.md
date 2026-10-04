# TUI 实现与维护

> 更新日期：2026-10-05

仅供仓库维护，不进入官网发布产物。

## TUI 调度与布局

Full 仍为默认模式；Full/Inline 共用 reducer、结构化工具状态和 Markdown。主循环以
Crossterm EventStream、runtime signal、后台结果、Inbox watch 和绘制期限做异步选择；文本
按 33ms 合并，键盘、停止与终态立即请求绘制，空闲不绘制。signal 批次最多 512 项/4ms，
Inline 每批最多插入 256 行，批次之间让出执行。同步 Display 通道通过有界异步转发唤醒。
文件选择器、历史恢复、Artifact、子代理详情与持久 Inbox 操作在后台执行；查询结果带
operation generation，准入回执捕获草稿 revision，迟到结果不能清理新草稿。撤回/续发
已有 Inbox 项不消耗未发送草稿和图片；编辑只消耗对应文本，新图片仅由新提交消耗。

`TranscriptItem` 保存稳定 ID、revision 与按宽度缓存的行；Fenwick 高度索引定位可见消息，
Full 只复制视口行并构建可见点击区域，不保留完整历史的展平副本。追加/结果/折叠只标脏
对应 item；宽度变化每次最多布局 2048 项/4ms。详情按资源身份、revision、宽度缓存折行
结果，滚动只切片，其他子代理更新不使当前 Artifact 失效。输入布局独立按草稿版本与宽度
缓存，编辑按 grapheme 处理。

流式 Markdown 增量检查完整行，匹配围栏字符和长度，列表、引用、表格及缩进结构不确定
时保留可变尾部；不再按任意字节裁掉前文。长未闭合代码块复用已折行完整行，闭合围栏
必须收到完整换行后才停止增量代码布局，防止后续字符改变该行语义。阅读状态为
跟随最新或消息 ID/内容偏移锚点，封口、追加、折叠与 resize 保留阅读意图。
Inline 插入成功才推进 item/行位置，释放已提交正文、presentation 与行缓存，保留轻量
身份/Artifact ledger；完成子代理详情经公开 Reader 按需恢复。写入失败退出，不重试可能
部分成功的 scrollback。详情返回继续使用原 Inline Terminal，初始化前建立恢复 guard。

macOS 通知模块优先发送单条终端原生 OSC 9，由终端保留点击与 surface 的关联。
Ghostty/iTerm2/WezTerm 不另发平台通知；Terminal.app 或其他已提供 bundle 身份的终端
可通过可选 `terminal-notifier -activate <bundle>` 激活应用，外部命令输出隔离。禁止
AppleScript 通知回退，其通知属于脚本编辑器；外部工具缺失时保留 OSC 与铃声。

性能数据与验证脚本见 [TUI 性能记录](performance.md)。

## TUI 阅读、编辑与异步反馈

TUI 不把流式封口或工具边界视为用户恢复跟随的指令。Full 用稳定消息 ID 加内容偏移保存
阅读锚点；分批重新布局期间保留原锚点，布局完成后再夹到有效位置。用户提交任务、
`Ctrl+L` 或 `/latest` 明确恢复正文跟随；详情位置独立保存。Inline 原生 scrollback 不可
修改，稳定边界宁可延迟提交也不能提前封口未完整的围栏行或仍增长的容器。

暖缓存输入帧不能扫描所有历史；高度索引和消息缓存承担正文布局，可见区域单独生成点击
映射。详情缓存按资源而非任意全局事件失效。宽度变化允许冷布局，但必须分批让出输入。
已提交 Inline item 只保留轻量身份和详情引用；256 行提交以 terminal 成功为位置推进边界。

准入不等待 broker 的当前 turn。后台调用同一持久 Inbox API，回执只能清理对应草稿版本，
后来输入保留；失败原稿成为 `/inputs` 的可恢复项。停止在准入期间到达时，接收回执后仍
转交 stream 并中断，等待权威 Final，不将暂态通知当作完成。Inbox watch 在持久发布后
通知，订阅和初始快照同锁，不在每帧克隆比较全部输入。撤回/续发只操作选中的 Inbox
项，不清空另一个未发送草稿；匹配的斜杠命令可清空命令文本。只有新提交消耗待发送图片，
编辑已有项只更新文本。续发历史记录使用实际续发内容，失败管理操作不生成失败提交。

输入移动和删除按 grapheme，视觉上下移动保持目标列；Home/End 为逻辑行，Ctrl+A/E 为
整个草稿。Ctrl+Z/Alt+Z 撤销重做，整次粘贴是一项编辑。Esc 关闭当前覆盖层或返回主界面，
主界面不退出；Ctrl+D 仅在文字、附件、上传及准入都空闲时退出。帮助、统计、输入和详情
列表为本地面板，不向对话追加正文。工具状态 live/replay 保留完整 `ToolStatus`，旧记录
缺少状态显示“状态未记录”，不以旧成功布尔值猜测阻止或中断。

macOS 通知点击归属必须保留终端身份。Ghostty/iTerm2/WezTerm 用单条 OSC 9，
避免同时发送两种 OSC 和 AppleScript 通知；后者点击属于脚本编辑器。其他已识别终端
的可选 `terminal-notifier` 使用 `-activate` 指向终端 bundle，仅激活应用，不执行脚本。
原生通知是否显示由终端与系统通知设置决定；缺少外部工具时只保留终端通知/铃声。

## 定位

TUI 是 `AgentEventStream` 结构化事件的两种终端 surface：

- `--tui` / `--tui=full`：全屏应用内 transcript，保留鼠标滚动、卡片点击、可逆折叠和详情视图。
- `--tui=inline`：原生 terminal scrollback，完成内容不可修改，面向 SSH 和长日志。

两种模式共用 `TuiSignal`、transcript reducer、session replay、Markdown、语义工具卡片、
自动折叠策略、输入编辑、状态栏和结构化详情数据。模式差异只存在于终端生命周期、viewport、
鼠标路由和最终输出方式。

## 数据流

任务与中途引导通过 `TuiRuntime.handle.stream_input` 进入 session 唯一持久 Inbox；引导直接准入，不能等待 broker 的当前 stream 结束。输入区回执与 transcript 分离，正式 `ConversationCommitted` 才发出 `GuidanceApplied`，权威 outcome 才发出 `TurnFinished`。运行中 Enter 不封口文本、不新建计费 turn；停止期间保留草稿，`/inputs` 展示待处理输入，`/resume ID` 明确续发，`/withdraw ID` 按 revision 撤回。含引导的历史窗口从公开 Reader 的完整 conversation 轮次恢复，随后复用同一 reducer。

```text
LLM / ToolRunner
  -> AgentEvent
  -> TuiSignal
  -> shared transcript reducer
  -> TranscriptItem / Plan / Todo / Artifact / SubAgent state
        ├── Full projection   -> application viewport + click map
        └── Inline projection -> sealed/stable output + insert_before

events.jsonl
  -> replay decoder
  -> same transcript reducer

plan.draft / plan.md / todos.json
  -> startup state loader
  -> current Plan / Todo detail baseline
```

主循环以 EventStream、runtime signal、后台结果、Inbox watch 与绘制期限唤醒，文本以 33ms 合并；键盘/停止/终态立即请求绘制，空闲不重绘。signal 批次最多 512 项或 4ms，终端输入不再无限排空。文件扫描、恢复、详情读取与 Inbox 准入放入后台；generation 与草稿 revision 拒绝过期结果。

工具调用和结果通过 `tool_use_id` 合并。`PresentedToolResultDisplay` 携带 `ToolStatus`、
`ToolResultKind`、Plan/Todo presentation 和 Artifact 元数据；两个 surface 均不得从展示文本
反向解析结构化状态。旧记录缺少状态显示“状态未记录”，不折成成功布尔值。

## 共用渲染

工具卡片由同一 renderer 产生：

- `ToolResultKind` 决定 Read/Search/Edit/Command/Control/SubAgent 的语义颜色。
- `ToolStatus` 决定 pending/success/failure/blocked/interrupted 状态标记。
- `CollapsePolicy` 决定初始自动折叠。
- Plan、Todo、Artifact 和普通工具正文使用同一份 presentation。
- 含 Artifact 元数据的折叠卡片保留首个 `artifact://ID`，详情按 ID 有界读取正文。
- Markdown normalize、block、inline、table、diff 和 wrap 逻辑只保留一套。
- Plan、Todo 和 Artifact 详情使用扣除水平 padding 后的内容宽度折行，滚动范围按折行后的
  可视行计算。

Full projection 可通过 `collapse_overridden` 改变自动折叠结果；Inline projection 只输出自动
折叠后的最终形态，不显示可展开标识。

## Full TUI

Full 模式使用 alternate screen 和 mouse capture。内容区保存完整 transcript：

- 鼠标滚轮和 PageUp/PageDown 操作应用内 viewport。
- 点击普通可折叠卡片切换展开状态。
- 点击 Plan、Todo、Artifact 或 SubAgent 卡片进入对应详情。
- 输入区、文件选择器和状态栏固定在主视图底部。
- 实时 signal 和 replay 使用完全相同的 item 模型。

Full 采用稳定消息 ID、revision 与宽度缓存，Fenwick 高度索引只定位可见消息，不克隆整份展平历史或遍历全部点击区域。追加、工具结果、折叠仅更新对应 item；resize 按 2048 项/4ms 分批重布局。详情按资源身份/revision/宽度缓存，纯滚动只切片。阅读采用跟随或消息 ID/偏移锚点，追加、封口、折叠、返回详情与 resize 不恢复跟随。

Full 模式不读取 Inline 的 committed 边界。

## Inline TUI

Inline 模式使用 ratatui inline viewport，主视图不启用 mouse capture：

- 只能提交从 `inline.committed` 开始的连续 sealed 前缀。
- committed item 不得再次修改或重复写入；每批最多 256 行，插入成功才推进消息 ID/行位置，批次之间允许输入，失败直接退出。
- 已提交正文、presentation、行缓存释放，保留轻量 ledger；完成子代理详情经公开 Reader 按需恢复。resize 仅重排未提交余行。
- 流式 assistant text 增量扫描完整行；围栏必须匹配字符、长度和完整闭合行，列表/引用/表格/缩进结构不确定时保留尾部；完整展示前文，不做任意 64 KiB 截尾。
- 空闲时保留最后一个 sealed item；新工作开始后再将它提交到 scrollback。
- `insert_before` 使用 terminal scrolling region，宽字符占位 cell 不得输出为可见空格。
- 动态 viewport 高度依据终端尺寸在 8–12 行内计算。
- Plan、Todo、Artifact 和 SubAgent 详情可临时进入 alternate screen，退出后恢复 inline viewport。
- alternate screen 生命周期保存并恢复同一个 inline terminal；禁止在返回主视图时重新创建
  inline viewport。

Inline 写入不额外插入 item 间空行；Markdown parser 只保留原始内容实际表达的段落间距。

## 不变式

- 实时和 replay 必须经过同一个 reducer。
- 工具 call/result 必须按稳定 `tool_use_id` 合并。
- AgentEvent 投影必须保留详细调用 ID 和结构化 result presentation；缺少或不匹配
  `tool_use_id` 时不得根据工具名推断关联。
- Todo mutation presentation 必须归并到当前完整 Todo 状态。
- 缺失结果的 replay 工具调用和终止信号前的未完成项必须封口。
- Full 与 Inline 必须调用同一个工具卡片和 Markdown renderer。
- Full 的折叠覆盖只影响交互投影；Inline committed 状态只影响原生输出投影。
- Artifact 详情最多读取 256 KiB。
- Plan/Todo/Artifact 详情必须先折行再计算垂直滚动边界。
- 输入 cursor 始终位于 grapheme/UTF-8 char boundary；粘贴整次撤销，迟到准入回执不覆盖新草稿。

## 验证

```bash
cargo test -p mink-cli --features tui
cargo test -p mink-core
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
make feature-matrix
python3 scripts/bench-tui.py
cargo build -p mink-cli --no-default-features --features tui
python3 scripts/verify-tui-pty.py target/debug/mink
python3 scripts/verify-tui-pty.py target/debug/mink --tmux
```

测试至少覆盖：

- `--tui`、`--tui=full` 和 `--tui=inline` 参数解析。
- 工具调用/结果合并和 v2 replay。
- ToolResultKind 语义颜色在共享 renderer 中保持稳定。
- Full click map、鼠标折叠和结构化详情动作。
- Inline sealed boundary、稳定 Markdown 提交和无重复输出。
- Markdown 结尾换行不产生额外空白行。
- UTF-8 输入、Ctrl+C、文件选择器、表格、diff、详情长行折行和滚动。

## 性能验证范围与限制

release 基准覆盖 1千/1万条消息、40/80/120/200 列、12/40 行、长 Markdown、256 KiB
详情、多子代理和 32 项输入。计数断言暖缓存输入帧不访问历史，详情滚动不重新解析；PTY
记录输入到绘制、输出字节和空闲输出，并验证 Full/Inline、详情返回、resize、停止、退出、
初始化失败及 tmux scrollback。数据和限制见 [性能记录](performance.md)。

普通暖缓存输入帧 p95 ≤16ms、输入到绘制 p95 ≤50ms 是目标；本地及实际 SSH 的
Full/Inline、tmux 样本已满足。SSH 输入 p95 为 24.75–42.29ms，空闲无输出、终端恢复
及 Inline scrollback 单次写入均通过；其他链路与负载仍需各自测量。
1 MiB 连续段落/增长表格更新绘制 p95 为 21.924/28.671ms，尚不能保证所有运行态帧
低于 16ms；这组样本未同时测量重输出期间的输入延迟。Artifact 详情当前最多读取
256 KiB，未提供后续区段分页。
