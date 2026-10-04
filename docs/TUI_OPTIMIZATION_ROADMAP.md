# TUI 实现说明与维护建议

> 更新日期：2026-10-04

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
初始化失败及 tmux scrollback。数据和限制见 [性能记录](TUI_PERFORMANCE-2026-10-04.md)。

普通暖缓存输入帧 p95 ≤16ms、输入到绘制 p95 ≤50ms 是目标；本地及实际 SSH 的
Full/Inline、tmux 样本已满足。SSH 输入 p95 为 24.75–42.29ms，空闲无输出、终端恢复
及 Inline scrollback 单次写入均通过；其他链路与负载仍需各自测量。
1 MiB 连续段落/增长表格更新绘制 p95 为 21.924/28.671ms，尚不能保证所有运行态帧
低于 16ms；这组样本未同时测量重输出期间的输入延迟。Artifact 详情当前最多读取
256 KiB，未提供后续区段分页。
