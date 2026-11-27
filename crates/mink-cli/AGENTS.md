# CLI & TUI Invariants (`mink-cli`)

> 归属：`crates/mink-cli/`；从根目录 `AGENTS.md` 拆分的 CLI/REPL/TUI 契约，改完 CLI 或 TUI 代码时必守。

## Display、TUI 与输入契约

- macOS TUI 通知优先使用终端原生单条 OSC；不得用 `osascript display notification` 回退（点击属于脚本编辑器）。外部 `terminal-notifier` 必须携带终端 bundle 的 `-activate`，输出不能污染 TUI。
- TUI 主循环按事件唤醒，空闲不绘制/克隆 Inbox；正文暖缓存输入帧只访问视口，高度索引与消息 ID/revision/宽度缓存保持一致，宽度变化分批布局。详情按资源身份/revision/宽度缓存，滚动只切片，后台结果按 generation 拒绝过期查询。
- TUI 阅读锚点在追加、封口、折叠和 resize 后保留；只有明确提交或 Ctrl+L/`/latest` 恢复跟随。输入移动/删除按 grapheme，迟到准入回执只能清理对应草稿 revision；撤回/续发已有输入不消耗未发送草稿与图片，文本编辑不消耗新图片；停止等待权威 Final。
- Inline 每批最多提交 256 行，插入成功才推进消息/行位置；失败立即退出、不重试部分写入。已提交正文/presentation/行缓存释放，子代理详情经公开 Reader 按需读取；resize 不重写 committed 内容。
- Inbox 只读 watch 的初始快照与订阅同准入锁建立，持久发布成功后通知；watch 可合并状态，不能替代正式历史和权威终态。Reader 历史窗口流式保留完整轮次，子代理读取不得越出父 session。
- Display 实现必须完整转发 `ToolCallDisplay` / `PresentedToolResultDisplay` 结构化字段，不得丢失 `tool_use_id`、presentation 或 artifact 元数据。
- REPL/TUI 输出不可信 payload（模型文本/thinking、工具输出/摘要、错误）前必须经共享控制序列清洗（thinking/text 块内跨 chunk 保留解析状态、类型切换与消息边界 reset、每条错误 reset stderr、子代理 thinking/text 两段分别清洗、stdout/stderr 状态独立）；renderer 的颜色/标题码在清洗之后添加。
- TUI 光标必须落在 UTF-8 char boundary，输入/删除按 char boundary 处理。
- TUI 粘贴图片只产生 session `attachments/` 传输副本并把绝对路径写入用户消息；图片进入上下文的唯一入口仍是 `Read` 捕获；重复粘贴按内容寻址路径去重，路径不可无歧义表示时 fail closed；CLI/TUI/Web 共用 core `AttachmentStore`。
- Full/Inline TUI 任务与运行中 Enter 引导均走 `stream_input`，准入不排到下一轮；仅正式 `ConversationCommitted` 才回显引导，`Final` 才结束轮次。尚未应用输入由 Inbox 保留，`/resume ID` 明确续发，`/withdraw ID` 按 revision 撤回；拒绝提交保留草稿及图片，不封口当前流。
- Inline TUI 只提交连续且 sealed 的 transcript 前缀，committed 项不得修改或重复写入原生 scrollback；空闲保留最后一个 sealed item，新工作开始后才能推进 committed 边界。
- Inline 详情视图不得丢弃或重建主视图 terminal（否则 alternate screen 内容叠加残影）；Full TUI 保留主视图鼠标命中、可逆折叠与应用内 viewport，不受 Inline committed 边界影响。
- TUI 实时 signal 与 replay 必须经同一个 reducer，结构化工具状态不得从展示文本反向解析；Todo 增量 presentation 合并到当前完整状态；Plan/Todo/Artifact 详情先按内容宽度折行再算滚动范围。
- Goal 在 TUI 状态栏仅只读呈现：`goal:<状态> <剩余>/<上限>` 标记可点击打开只读详情（`/goals` 等效），会话打开从 `goal.json` 恢复、运行中由 `GoalUpdated` 更新，界面不得绕过 core 修改契约、额度或状态。
