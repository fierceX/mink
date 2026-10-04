# 图片输入

> 更新日期：2026-10-04

前置：模型后端支持图片，且会话创建时冻结了图片能力。配置开启方式及完整限额见[配置参考](../reference/configuration.md#多模态图片输入)。附件上传或粘贴只保存传输副本；只有模型调用 `Read` 才进入图片上下文。

## 本地图片

在输入中提供图片绝对路径并明确要求读取。成功时 Read 结果显示 image 引用；已看过的图片以后投影成文本，要求 `Read image://sha256:<hex64>` 可重新查看。远程 URL 直通尚未实现，须先保存本地文件。

### 粘贴图片（`Ctrl+V`，macOS）

终端不会把剪贴板图片交给程序，所以 TUI 自己读取剪贴板（macOS 通过 `osascript` 提取
pasteboard 的 `PNGf` 表示）：

- `Ctrl+V` 读取剪贴板 PNG，按当前会话能力与限额校验后写入
  `<session>/attachments/<sha256>.png`（内容寻址，重复粘贴复用同一文件；目录权限 `0700`）。
- 图片先进入输入框上方的待发送列表（如 `[image #1 1440x900 220KB]`）；输入框为空时按
  `Backspace` 移除最后一张，`Enter` 提交时在消息尾部追加
  `[Attached image: "<绝对路径>" - Read it to view.]`。slash 命令不携带图片，图片会留在待发送列表；
  重复粘贴同一张图不会重复入队（内容寻址到同一路径时提示已排队）。
  附件路径含引号或控制字符时拒绝附加并提示（marker 无法无歧义表示）。
- **送达依赖模型调用 `Read`**：附件文件只是“传输副本”，图片真正进入上下文仍走 `Read` 的捕获链
  （magic 校验 → 限额 → 内容寻址缓存 → 单次消费）。模型未读取时图片不会送达，可以直接要求它读取该路径。
- 仅当会话冻结的 `model-capabilities.json` 声明了图片能力时可用；文本会话按 `Ctrl+V`
  只会得到一条提示，不会读剪贴板也不会落盘。
- **为什么不是 `Cmd+V`**：macOS 的 `Cmd+V` 是终端自己的快捷键——终端拦截它做文本粘贴，既不会把按键事件交给程序，也不会把剪贴板里的图片字节交给程序，所以默认触发键是 `Ctrl+V`。若坚持用 `Cmd+V`，需要在终端里把它重绑为发送 `0x16`：
  iTerm2：Settings → Profiles → Keys → Key Mappings 添加 `⌘V` → “Send Hex Code” `0x16`；
  WezTerm/Ghostty/kitty 类似 `cmd+v` → `text:\x16`。代价是该终端下 `Cmd+V` 不再粘贴文本
  （可把文本粘贴改绑到 `⌘⇧V`）。终端若支持并转发 Kitty keyboard protocol 的 `Super+V`，
  TUI 也会识别。
- 目前仅支持 macOS，其它平台会明确报错。附件文件随 session 目录保留（不主动清理，随 session 一起删除）。
- 验证真实剪贴板路径：`cargo test -p mink-cli --features tui -- --ignored --nocapture clipboard_smoke`
  （需剪贴板里有图片；该测试默认被 `#[ignore]` 跳过，不会在常规测试中读取系统剪贴板）。


## Web 上传

选择、粘贴或拖放图片。上传成功后显示附件卡片；失败时移除或重选，失败和在途附件会阻止提交。可以只发送图片，服务端生成明确的查看请求。能力和格式取会话交集，Web 上传限额见[HTTP API](../reference/http-api.md#持久输入与附件)。刷新中断上传时需重选。

## 下一步

看[读图机制](../concepts/images.md)了解缓存、消费状态与失败降级，或回到[终端操作](terminal.md)。
