# Web Workbench & Server Invariants (`apps/web` & `crates/mink-server`)

> 归属：`apps/web/` 与 `crates/mink-server/`；从根目录 `AGENTS.md` 拆分的 Web 工作台与服务端嵌入契约。

## 人类输入与 Web 工作台

- **服务器默认前端是 `apps/web`（React 工作台）**：`crates/mink-server` 的 build.rs 构建 `apps/web` 并把 `dist` 按字节嵌入二进制（资源含字体等非 UTF-8 文件，不得改回 `include_str!`）；开发时可用 `MINK_SERVER_DEV_WEB=<dir>/dist` 从磁盘服务，前端热迭代不重启 server。
- **server 默认监听 loopback `127.0.0.1`**；远程访问必须通过显式 `MINK_SERVER_HOST`/`--host` 配置并由部署侧限制网络访问。Rust CLI/server 共用 `mink::config_file` 的 `.minkrc` schema/parser；只忽略 NotFound，已有配置读取或解析失败必须向调用方报错，不得回退到空配置。
- **新前端的设计层基于外部设计快照**：`apps/web/src/{theme,ui/primitives,util,store}` 与部分屏幕 CSS module 是直接拷入仓库的源码（vendored，不是 npm/库依赖；来源与许可清单见 `apps/web/THIRD_PARTY_NOTICES.md`），样式只消费拷贝的 `--dsw-*` token 层。快照只作为初期设计参考；这些文件由 mink 维护，可按本项目需要独立优化，与参考实现再同步是可选维护动作，不是构建或 PR 门禁。
- **控制面细节**：composer 的模型位是可点击菜单（清单来自运行时解析器，经 `GET/POST /api/sessions/{id}/model`，切换以运行时接受的 label 为准）；「+」打开命令面板（添加图片 + 契约里的全部命令，选中只写入草稿而不直接执行）；图标控件经拷贝 `Tooltip` 在 hover/聚焦时显示名称；打开失败的错误行带「重试」（只重跑打开，不重建 turn 动作）。
- **卡片家族与预览**：工具卡覆盖终端/文件/差异/搜索/待办/计划/委派（`tasks[]` 一行一个任务）/目标报告（类型、轮次、摘要、下一步、验收条件与证据），解析失败一律回退通用 in/out 卡；变更文件卡的每一行 hover 显示该文件的 hunks（取自转录，不二次请求）；设计核对页对每个家族都有样本。
- **草稿与附件按会话身份隔离**：`Conversation` 以会话身份为 key，草稿（`mink.draft:<project>:<id>`）与已上传附件条目（`mink.attachments:<project>:<id>`，每项含草稿条目身份 `key` 与内容 id）随会话保存与恢复，切换会话不串、返回即可继续；存储不可用时退化为内存草稿。草稿必须按 key 在渲染期同步读取（keyed composer 首次挂载即拿到目标会话正文），不得靠 effect 延后换内容。
- **会话级异步身份与提交幂等**：投影、草稿、附件与在途提交按完整会话身份维护，提交记录（`mink.pending:<project>:<id>`）与草稿同生命周期：离开会话、刷新页面都不丢，只有服务端明确受理或拒绝才删除。发送/引导/回执合并/详情加载/历史分页/命令副作用只在发起它的会话仍是当前视图时落地；发送与 stop 的完成还必须验发起时的视图 epoch + 投影 runtime generation + 当前 turn（新的 `session_snapshot` 是更新的真相，旧 HTTP 失败不得回滚 running/写旧错误）。历史以投影 `messages` 为唯一已提交集合，按 `seq` 合并去重排序，分页不得覆盖快照之后的 committed 消息。提交以稳定 request ID 对账：答复丢失（网络）时先查回执或以同一 ID 重试，已受理不得再当失败恢复草稿；附件上传的非 200 拒绝与网络失败都落到失败条目及原因。附件消费按提交记录中冻结的草稿条目快照（`attachment_snapshot`，只存本地）执行：内容 id 仅用于请求引用与资源读取，不得单独决定移除哪些草稿条目（内容寻址下同一图片重选会有相同 id）；旧记录在恢复边界一次性迁移为显式 `known:false` 并持久化，确认时保守保留草稿、仍用原 request_id 对账；“无快照”与“已知空快照”必须区分，存储与已挂载 rail 共用同一纯消费判据；上传合并必须同时更新存储与已挂载 rail，禁止只写存储。Composer 只报告操作状态，确认消费只由会话视图或首次发送流程执行；首条发送成功同样只能按冻结快照消费附件，失败恢复旧正文仅在会话尚无草稿时写入，不得覆盖后来输入。
- **模型座位与目标详情的版本顺序**：模型读取与切换共享单调 request revision（每次 refresh、每次已接受切换与每次视图生命周期都推进），只有最新一次可写，A→B→A 与“先切换后回来”的旧响应一律作废；`/model`、`/flash`、`/pro` 成功后重读运行时。目标页签的 REST seed 在发起时记录投影目标更新计数，计数变化则丢弃 seed（清空/新建也不靠 null 或 ID 猜顺序），revision 相等时仍独立更新 `driven`。
- **新建任务面板**：正常路径不再弹目录/名称表单。项目行 ➕ 直接进入 composing 视图，**首次发送才创建服务端会话**：客户端用稳定 `draft_id` 幂等创建，`first_prompt`（≤128 KiB）生成标题；无文本会话标记 `untitled`，后续首个非空文本用户消息回填。空名称不得复用 `unnamed` 会话。composing 复用 `ConversationRoot` 的内容宽度轴并水平/垂直居中，项目名与目录收成输入卡上的胶囊，禁止让 hero 在左缘 shrink-wrap。首条 `request_id` 与 `draft_id` 一起写入 compose draft，刷新后重试仍是同一次提交；首条消息得到答复后必须按**身份**结算该记录（`draftId`+`requestId` 相等；正文相等不能作判据，composer 提交前已清空输入框），使这对身份不可再复用：空正文或与提交正文相同的记录删除，发送开始后新输入的正文换成新身份保留；暂存附件总字节≤16 MiB，首次发送前图片刷新会丢失，文本与 ids 保留。「新建项目」只校验/选择 server 工作目录，不创建会话；项目仍由会话工作目录派生。
- **新前端已实现的契约**：会话目录与打开（筛选/搜索四种范围、切换保留关键词、新建与删除）、`/conversation` 历史 + SSE 实时投影（`src/data` 的纯 reducer/SSE 客户端，`/open` 的 snapshot 提议优先）、历史分页（`before_seq` 回溯，翻页保持阅读位置；`turns=true` 批体积上限 512 KiB）、composer 的 `/turn` 与 `/interrupt`、运行中引导（`/inputs` 队列：按 revision 编辑/撤回/续发，未取得 turn id 时拒绝并保留草稿）、附件与图片（粘贴/拖放/选择 → `/attachments` 会话副本，id 随输入提交，图片仍只经 Read 进上下文；新对话首发送前暂存于浏览器）、斜杠命令（与终端共用 `crates/mink-cli/src/tui/slash_commands.json` 契约；无会话的 composing 视图隐藏命令和模型位）、工具卡家族（终端/文件/差异/搜索/通用）与逐轮「已编辑 N 个文件」摘要卡、消息 chrome（复制与该轮用量；旧轮次悬停显现）、目标条与目标边界卡（拷贝的 GoalBar 样式，暂停/恢复/取消/清除全部按 revision CAS；流只传摘要，详情按需 full）、详情面板五页签（任务/目标/文件/输出/诊断，诊断与 TUI StatsSnapshot 同口径）、轮次选择器与「返回最新内容」、移动端阅读 chrome（仅窄布局生效，宽屏点击不得隐藏；运行中/待处理强制可见，隐藏不 close/interrupt）、断线连接按钮（重新 `/open` + 权威 snapshot 重载，不靠浏览器刷新）、主区宽度随中间列发布与宽表列内横向滚动、设置与偏好（外观/字号/过程展示）。
- **本地视觉对照**：在本地参考客户端上截取的对照图可能含用户私有会话，不得入库。
- **按「mink 没有的功能不抄」排除**：插件面板、Pin/Archive、权限 pill、`Standard mode` 模式标签、`Chat/Trajectory` 页签、消息时间戳（mink 对话行没有时间数据）。新增这类能力时先补运行时契约，再对齐界面。
- `InputInbox` 是 session 唯一持久输入准入边界；request ID 绑定原始内容，编辑/撤回比较 revision，仅 pending/unapplied 可修改。applying 先发布，正式历史以 `_mink.input_id` 接管，追加不确定或历史已写但回执发布失败必须闩锁；恢复核对完整预期消息，禁止重复追加。
- 引导绑定父 turn，在已接受工具交换完整持久化后、下一请求压缩检查前顺序消费；Stop 与准入共享锁。取消/故障/恢复耗尽/轮次上限优先，引导不重置格式窗口、账单 turn 或请求期限，不自动传给子代理；未应用项仅由用户明确续发。
- server 全局运行会话上限由唯一容量账本（`CapacityPool`）覆盖所有新 turn（含恢复旧引导）与目标驱动：普通 turn 的 permit 随 turn 任务结束释放，目标 run 的 permit 随 `GoalRunLease`/RunGuard 释放，且只在 run 结束且 turn gate 空闲后归还（暂停/取消后仍在退出的检查器或工具继续占容量）；同一 session 实例同时只有一个 permit，被拒绝的创建不占容量也不启动无关旧目标；等待人工验收没有执行任务时释放容量，恢复须重新申请。当前运行 turn 的引导与既有 request ID 的幂等重试不额外占用名额。配置路径是首个可选位置参数，未知 flag/多余参数须拒绝。
- Web mirror 经公开 Reader 初始化、可靠 runtime 事件更新，快照与订阅位置在同一发布锁建立；generation/水位拒绝旧事件。Goal 随投影与水位同一临界区镜像，快照/详情渲染不得在持有投影锁时获取会话状态锁（锁序固定为 state → projection）。离开视图只断订阅，不调用 close。草稿/附件/展开/阅读锚点按完整会话身份隔离，迟到响应不得污染当前视图；草稿写入必须递增 revision（改回原文不算未改），每个视图同时只允许一条在途命令，结果与提示/详情副作用只从发起尝试的 pending 上下文（identity/generation/revision）取判据并仅在视图与 runtime generation 仍当前时应用，成功且 revision 未变才清空草稿，服务器拒绝或传输失败保留正文供重试。
- Web 会话筛选只搜索已发现目录，项目名称（路径末段）与完整项目路径必须区分；会话范围包含标题、别名和 ID，切换范围保留关键词，不改变当前执行或会话排序。
- Web 根地址默认首页，会话刷新目标来自完整 URL 身份，不回退旧的本地选择；启动请求不得覆盖期间的明确导航，返回首页只断订阅。
- Web 设置使用按需打开的模态面板，菜单焦点交接完成后再捕获焦点；关闭须恢复入口，偏好修改不得重启任务或重置阅读状态。
- Web 菜单与移动覆盖层必须支持键盘关闭和焦点恢复；请求在途禁用重复操作，不以加载或 HTTP 回执覆盖更晚终态、runtime generation 或正式已应用输入。文件预览加载结果与滚动恢复均受完整身份及请求 revision 约束。
- Web 诊断必须与 TUI StatsSnapshot 口径一致：缓存命中只计 read、分母含 input/read/creation；缺失旧统计显示未知。完整 stats 与 activity 同镜像发布边界恢复，正式历史提交不得清除等待计时。
- Web 图标控件使用场景尺寸（32px/触控 36px、会话行 44px），禁止统一放大所有按钮；Reka UI 菜单优先处理方向键/Esc，手机覆盖层不得抢走菜单键盘事件或因响应式切换自动遮挡输入。
- Web 文本复制共用 Clipboard API/选区兼容路径，清理临时控件并恢复焦点、光标与选区；两条路径均失败不得显示成功。
- Web 工具卡片按实际参数与 ToolPresentation 展示，Replace/Hashline 均须支持；JSON 参数保留手动核对入口，正文不通用解码为参数，旧未知状态不得渲染为成功，Artifact 入口不得丢失。
- Web 轮次选择须为限高纵向列表，标题不得撑宽弹层；支持菜单内滚动、方向键/Home/End、选择关闭与焦点恢复。
- Web 正文/过程组须限制在可用列宽；自动换行只改展示并保存偏好，不改复制/发送/文件/工具结果的原始字节；关闭换行后的横向滚动只属于内部内容块。
- 手机操作栏只由外层明确手势（用户滚动或轻点正文）隐藏，内层/布局/流式滚动不得触发；运行中的轮次、输入焦点（含输入卡内菜单与附件控件）、草稿附件、待处理或失败回执、在途提交或在途命令、打开/发送/命令失败与连接中断/恢复提示一律阻止隐藏（单一派生判据，状态由 Composer 汇报而非解析提示文本；成功提示不锁定），解除锁定不自动隐藏，隐藏只改展示、保留恢复入口、草稿与阅读锚点，不调用 close/interrupt。
- Web 会话扫描与查找共用子代理过滤：目录名或 metadata.id 为 sub_/replan_ 前缀、或 metadata.parent 存在时排除；不能因缺失、损坏或继承 metadata 将旧子代理展示为用户会话。
- Web 附件预览仅保存 session attachments 原始字节与绝对路径，图片上下文唯一入口仍为 Read；会话限定预览不得扩大工作目录文件访问范围。
- Web 目标呈现：经 `GET /api/sessions/{id}/goal` 读取当前快照（活运行时优先，回退 `goal.json` 的 `current`，不含 history；默认流摘要，`?full=true` 取完整契约），经 `POST .../goal` 与 `POST .../goal/{action}` 以宿主身份创建与控制，每次变更必须是当前 `revision` 的 compare-and-set，且一律经 `AgentRuntimeHandle` / `goals()` 落到 core；SSE 转发 `goal_updated` 摘要（不含 usage_records/完整 reviews；保留 `review_candidate` 供候选匹配），详情按 revision 强制 full 重拉；状态条与详情显示审核进行中或可重试状态。界面不得绕过 core 修改契约、额度或状态。
