# Web 与官网维护

> 更新日期：2026-10-05

仅供仓库维护，不进入官网发布产物。

## 发布候选检查

`python3 scripts/check-release.py --tag v0.6.7` 核对 Cargo、Python、Web、锁文件、当前安装示例、官网版本和发布记录；历史回放与性能记录保留原版本。`python3 -m unittest discover -s scripts -p 'test_release.py'` 检查版本漂移及 registry/发布错误分支，不发送真实发布请求。

CI 在 Rust 1.99 上检查 workspace/all-features 与精简 runtime/SDK，Web 运行单元、类型及隔离 provider E2E；文档 job 使用统一组装命令与五档宽度验收。publish 必须等待 binaries、wheels、pages-check 和 msrv，全渠道版本须与 tag 一致。

每个 wheel 安装到独立环境后运行 `scripts/verify-wheel.py`：检查包元数据、内置二进制、两个本地 fixture 回合及会话复用。GNU/macOS 使用原生环境，musl 使用 Alpine 容器；不强制把 musl wheel 安装到不兼容的 GNU Python 中。crates.io 发布只对明确存在的相同未撤回版本跳过，网络、认证、异常响应或 cargo 失败均传播并阻止后续发布。

文档示例检查通过 `PostInitHook` 导出 Hashline/Replace 的实际工具 schema，核对工具 JSON 的必填字段、类型和未知字段；没有模型请求，不能称为真实任务验证。Rust、Python、CLI 和 TOML 示例的统一检查入口为 `make docs-examples`。

## 官网与文档

[官网主页](../index.html) 先介绍轻量可嵌入、长任务上下文与可靠编辑，再提供 CLI、Python SDK、
Rust 与 Server 快速开始。顶部索引可跳到各章节，标签页支持方向键、Home/End。
复制保留多行代码，失败时可手动选择。

终端回放来自[独立实跑的 .env 解析器案例](../examples/guided-env-parser.md)，包含运行中引导、
测试失败和修复结果。点击“看中途引导”跳到补充要求，可以暂停/重播；遵循减少动画偏好。
输入动画经过重建，播放节奏加速；结束帧统计来自实际保存的最终快照。
底栏与 TUI 共用字段顺序、k/m 单位和窄屏优先级，中间 usage 按实录事件恢复。
例如引导后为 `R:1 I:2.6k(4%) O:93 C:3.6k(0%)`；`B:—` 表示该阶段没有记录
信念值，保留字段但不推算数值。

阅读器地址保留当前文档，如 `index.html#docs/start/quickstart.md`；刷新、浏览器前后导航均可
恢复。开发时在仓库根目录先运行 `make docs-site`，再运行
`python3 -m http.server 8026 --bind 127.0.0.1 --directory target/docs-site`
预览，解析与高亮资源无需外部 CDN。组装命令同时复制根目录 CHANGELOG，本地与发布产物一致。

## 官网与文档站

GitHub Pages 发布 `target/docs-site` 白名单组装产物；`docs/index.html` 提供官网与 Markdown 阅读器，
`assets/home.css` 负责主页及共享视觉样式，`assets/vendor/` 保存解析/高亮资源及版本许可。
官网围绕轻量可嵌入、长任务上下文、可靠编辑组织三个章节，以机制图说明能力。
官网自身不启动 runtime；`assets/hero-replay.json` 来自独立实跑案例，包含同轮引导，
`examples/guided-env-parser.md` 保存原始要求、测试结果与代码。输入动画由正式引导重建，
长正文节选并加速播放；暂停/离开/隐藏保留位置，加载失败显示错误。

`scripts/build_hero_replay.py` 从 conversation 导出来源行号与类型化工具状态，忽略 internal
输入，HTML 转义、控制序列清洗与路径匿名化；旧未知状态不标成功。中途不填造统计，
结束帧可用 `--stats` 读取最终快照；`--events` 按 usage 事件重建已结算计数，
以调用 ID 或完整回复文本匹配正式消息，保留被废弃响应的真实用量，并与最终总量核对。
该路径限定独立单轮、主 agent 案例，不匹配或不完整即拒绝导出；未记录信念值显示 —。`--case-title` / `--case-doc` / `--recorded-date` /
`--mink-version` 指定公开案例身份，`--recording-root` 匿名化独立录制目录。
导出结构化状态项（字段、文本、TUI 优先级）、状态事件行号与 workState；播放器按实际宽度逐项裁剪，
resize 后恢复可容纳字段，模型与工作状态优先保留。Python 模块测试验证引导绑定、
真实/未知状态、TUI 数字格式、输入 Token 口径及导出清洗。

阅读器用 `#docs/<path>#<anchor>` 保存文档位置，fetch 的 generation 拒绝过期结果，
主页入口使用 `#home`、`#features`、`#start`，三个章节各有 `#runtime` / `#context` /
`#reliability`，引导案例为 `#guidance`。组装命令将根目录 CHANGELOG 复制到
站点。`scripts/test-homepage.mjs` 复用现有 Web
测试环境的 jsdom，CI 验证资源、版本、导航、键盘、复制与异步加载行为。

## 会话工作台与人类输入

`mink-server` 继续复用 Axum、Registry lease 与 `AgentRuntime`，Vue/Vite 产物嵌入二进制。Web 数据流为 REST/SSE → SessionController / CatalogController → 会话模型 → 纯轮次投影 → 展示组件。`workbench.ts` 单独保存按 `(project_key, session_id)` 隔离的草稿、附件引用、展开选择和阅读锚点；异步操作捕获原会话身份，过期响应不能写入当前视图。

`catalogFilter.ts` 是已发现目录的纯匹配投影：按项目名称、会话标题/别名/ID、完整工作目录分别匹配；SessionSidebar 持有关键词与范围，不改 CatalogController 的稳定顺序或当前会话身份。

App 以 URL 的 `session` / `project` 作为刷新目标；根地址直接显示首页，不恢复旧的 localStorage 会话选择。启动时记录 SessionController 导航 revision，目录响应不得覆盖中途明确导航；指定会话在目录读取期间显示中性加载界面。当前视图通过 replaceState 同步完整 URL 身份，离开时清除会话参数。

`SettingsDialog.vue` 使用原生模态 dialog，设置由顶栏菜单按需打开；菜单先恢复触发器焦点，模态面板随后捕获焦点。设置与会话生命周期分离，偏好仍由 `workbench.ts` 持久化。

Web 的 `ActionMenu.vue` 和 `IconButton.vue` 分别封装 Reka UI DropdownMenu / Tooltip，使用 Lucide 图标；组件库负责菜单定位、碰撞避让、方向键、外部点击和焦点交接。输入区将草稿、附件与操作收进单一容器，轮次跳转浮于对话内；`useOverlay.ts` 维护响应式媒体条件与移动覆盖层焦点范围。可视窗口 resize 只更新布局高度，不触发会话命令。详情请求以会话身份和加载 revision 丢弃过期结果，目录与文件正文互斥显示。

`clipboard.ts` 为对话和文件路径提供统一文本复制：优先 Clipboard API，不可用或拒绝时使用用户点击中的选区复制兼容路径；完成后恢复输入焦点、光标与文本选区，只有真实复制成功才显示成功。

`ToolInput.vue` 按工具语义展示命令、脚本、文件内容、子任务与参数字段，`EditCall.vue` 同时处理 Replace 和 Hashline；原始参数仅在独立折叠入口查看。Plan/Todo 结果优先读取类型化 presentation，普通文本结果与对话共用 Markdown 渲染，PythonSandbox 归入命令视图。调用与结果分别展示，不以结果覆盖调用参数，不改工具执行与输出保护。

`ActionMenu` 的轮次变体使用单列弹层，宽度不超过 300px、高度不超过 260px/40dvh 和可用空间；条目保留独立行、截断长标题并支持键盘滚动定位。

阅读区与过程组使用 `minmax(0,1fr)` 和明确的最小/最大宽度约束；`preferences.wrapText` 持久保存并发布 `data-wrap`，代码/工具/文件正文和 Markdown 表格的换行只改变 CSS，不改源文本。换行切换保留消息阅读锚点。

手机阅读模式由 App 的临时 UI 状态驱动：Transcript 在外层用户滚动达到方向阈值后请求隐藏，布局/流式滚动与内层卡片滚动不参与；顶栏和输入卡片使用 `v-show` 保留组件，输入区发布锁定状态。焦点输入、附件、待处理/失败输入和恢复操作优先保持可见，阅读锚点在控件高度变化时恢复。

Registry 的扫描入口统一排除目录名或 metadata.id 带 `sub_`/`replan_` 前缀、或 metadata.parent 存在的子代理；列表与会话查找复用同一规则，旧子代理元数据缺失、损坏或继承父身份时仍按目录排除。

Core 的 `session/input.rs` 拥有 session 唯一 `InputInbox`。`AgentRuntimeHandle::stream_input(HumanInput)` 为新任务返回 `(InputReceipt, Some(AgentEventStream))`，为目标 turn 的引导或重复提交返回回执与 `None`；`resume_input(id, revision)` 明确续发未应用输入。旧 Rust `stream_turn` 保持原执行契约，server 的旧 `/turn` 转入持久输入路径。HTTP handler 不直接追加 conversation；TurnExecutor 在已接受响应、完整工具交换之后、下一请求的压缩检查之前消费 Inbox。

`inputs.json` 经现有原子状态发布与 persistence fault 闩锁更新；`applying` 回执与正式消息的 `_mink.input_id` 构成可恢复交接。ConversationStore 的 commit observer 在追加完成后向可靠 runtime 事件流发出 `ConversationCommitted`，身份使用物理 JSONL 行号，初始化只扫描一次，追加增量计数，与历史分页一致。

Full/Inline TUI 通过 `TuiRuntime` 调用同一 `stream_input`；引导准入不经过等待当前 turn 的 broker 工作队列。新任务返回的 stream 交给 broker 可靠消费，正式引导提交转成 `GuidanceApplied`，权威 outcome 转成 `TurnFinished`，共用 TuiState reducer。输入上方只读映射 Inbox 状态；`/inputs`、`/resume ID`、`/withdraw ID` 调用公开接口。含引导的历史窗口经公开 SessionReader 读取完整正式轮次后走相同 reducer，保留引导顺序和工具元数据。

server `Projection` 是可重建只读镜像：启动经公开 `SessionReader` 读取历史、Plan、Todo、Artifact；运行中消费可靠 `AgentEventStream` 更新。快照与广播订阅在同一 mirror 发布锁下建立，不使用 best-effort EventSink 作为事实来源。`diagnostics.ts` 按 TUI `StatsSnapshot` 口径展示完整状态栏指标；镜像单独保持可靠 activity（工作阶段和等待秒数），快照重连不必依赖历史文本猜测正在执行的动作。新版 SSE 使用 runtime generation 与传输水位；正式消息接管暂态文本，Plan/Todo presentation 增量合并，终态由 `turn_final` 决定。

`session/attachments.rs` 是 CLI/TUI/Web 共享的内容寻址存储。Web 上传保存原始字节到 session `attachments/`，校验真实格式、完整解码、尺寸、能力、大小、权限和校验和。附件预览是 session 限定接口；输入只携带绝对路径，模型图片仍由 `Read` 捕获。

## 官网的信息与交互

官网以轻量可嵌入 runtime、长任务上下文、可靠编辑为核心叙事，每章解释收益、机制与
接入路径。共用内核、历史与活跃投影、Hashline/Replace 边界分别用图示说明；运行中
引导以独立实跑案例展示要求如何进入同一轮任务，再提供快速开始。

回放保留真实的任务、引导、调用、测试失败与修复结果，工具结果按 ID 更新对应卡片，
不以调用开始代表成功。长正文节选、输入动画重建和播放加速在案例记录中说明。
中途按事件日志重建已结算 usage，最终文本播放期间保留上一笔统计，结束后使用核对
总量的最终快照；未记录值显示 —，不估算中间信念。外框四边等距包裹终端。
输入框使用 TUI 的边框/标题，正文、标题与底栏共用等宽字号及行高。
底栏按 TUI 字段顺序紧邻排列，使用 k/m 单位、缓存读取占比和上下文占比；未记录的
上下文上限显示 —。宽度不足按 TUI 优先级移除整项，最后才截短模型，保持工作状态。
CLI、Python、Rust、Server 示例共用可键盘操作的标签页，复制保留原始换行。

终端演示可暂停，遵循减少动画偏好，隐藏页面停止推进。文档路由可刷新和前后导航，
旧 fetch 不得覆盖后来的选择或改变主页阅读位置。复制共用 Clipboard API 与选区兼容
路径，恢复焦点/选区，仅实际成功才提示。静态依赖随站点提供，代码与表格的横向滚动
限制在内容块内部。

## 6. 静态资源与嵌入

- **自动构建**：`build.rs` 在 `cargo build` 时自动执行 `npm run build`（web/），产物复制到 `OUT_DIR/assets` 并生成 `assets.rs`（`include_str!` 内容表）
- **嵌入服务**：默认从二进制内容服务静态资源（content-type 映射、`index.html` no-cache、静态资源 immutable 缓存、SPA fallback 到 index.html）
- **开发模式**：`MINK_SERVER_DEV_WEB=1` 时回退磁盘 `web/dist`（前端热迭代，改完强刷即可）
- E2E 使用 `MINK_SERVER_DEV_WEB=1` 保证测试服务最新构建产物

## 9. 测试

```bash
cd crates/mink-server/web
npm run e2e             # 真实浏览器 + 真实 server + 本地模拟模型
npm run test            # reducer/controller/SSE/组件状态测试
npm run typecheck && npm run build
cargo test -p mink-core -p mink-cli -p mink-server
```

E2E 通过 global-setup 构造隔离临时 home + 模板会话与本地模拟 OpenAI-compatible provider，覆盖 1440/1024/768/390/320px、引导/续发/附件/分页锚点；失败产出
trace/error-context 供定位。截图通过 `testInfo.outputPath()` 写入每个测试独立的产物目录，默认
`test-results/`（已加入 .gitignore），也可用 `--output=<目录>` 指定；不依赖平台临时目录，
并发测试不会互相覆盖。global-setup 从 `CARGO_TARGET_DIR`（默认仓库 `target/`）启动刚构建的 server。

手机滑动测试使用 CDP `Input.dispatchTouchEvent` 发送可信的 touchStart/move/end，逐帧移动，
释放前停留以避免惯性改变阅读锚点。Linux headless Chromium 的 `Input.synthesizeScrollGesture`
在相同 fixture 中可能只产生开始/结束事件、没有实际滚动，不能作为阅读模式验收依据。

## 文档清单、校验与预览

`docs/manifest.json` 是公开文档唯一清单，只含 path/title/group/description/source，数组顺序用于分组导航、总览、上一篇/下一篇和加载白名单。根 CHANGELOG 映射到站点 CHANGELOG.md。`assets/docs-shared.js` 同时供阅读器和校验器使用标题 ID 及链接解析；先经 marked 得到 DOM，再从源路径解析文档、图片及仓库源码链接，支持 Pages 子路径。

`make docs-check` 检查 Markdown 文件、链接、章节锚点、图片、本地依赖和旧正式引用；`make docs-site` 使用同一校验并将清单中的文档、根 CHANGELOG、必要静态资源复制到 `target/docs-site`。development、迁移记录和实验材料不进入产物。`make docs-examples` 语法校验 Python、检查 SDK 字段及 TOML/CLI 示例，编译完整 Rust fenced 示例和现有 core examples；它不表示真实模型任务成功。

手机导航是可关闭抽屉，Esc/Tab 焦点约束、关闭恢复入口；本文目录单独展开。导航和目录分别限高；代码与表格只内部横向滚动。阅读进度只计算当前正文，章节高亮使用视口位置，异步 generation 拒绝旧文档结果。

### Web 交互

项目导航常驻桌面，右侧详情按需打开；1024–1279px 打开详情时隐藏导航，768–1023px 侧栏覆盖，手机详情为全屏，使用 100dvh/safe-area，body 不承担对话滚动。轮次中思考/工具组成紧凑过程组，回复、重试、错误与引导有独立边界；支持简洁/标准/详细模式与手动展开优先。输入区使用一体容器，轮次跳转浮于对话内；图标按钮 32px/触控 36px、手机会话行 44px，进入窄屏收起桌面导航。菜单/提示使用 Reka UI，键盘、碰撞定位与焦点恢复由组件库处理。

草稿、附件、失败提交、展开、消息锚点、内层/详情滚动按完整会话身份持久保留。切换、返回首页、关闭浏览器只停止正文订阅，不调用 close、不停止任务。断线/停止中禁用提交但仍可编辑；明确发送与“返回最新内容”恢复外层跟随。任务详情只读，文件是当前磁盘内容，工具/Artifact 是当时记录。

对话与文件路径复制共用客户端 Clipboard API/选区兼容路径，适配未提供 Clipboard API 的普通 HTTP 页面；浏览器拒绝两条路径时显示失败，不能伪造成功，不新增 REST 接口。

工具卡片按真实参数显示命令、脚本、写入内容与子任务；Replace/Hashline 分别展示当次编辑指令，Plan/Todo 使用正式 presentation。完整 JSON 参数保留在折叠入口，调用参数不会随结果到达消失；历史刷新与实时结果使用相同渲染器，保留旧记录兼容与 Artifact 入口。

轮次选择弹层使用纵向限高列表（300px 宽、260px/40dvh 高及可用空间的交集），长标题截断；点击与键盘均可定位已加载轮次。

正文列宽受当前可用空间约束，代码/工具输出/文件和表格默认自动换行；导航与轮次菜单可关闭并持久保存偏好。关闭后横向滚动限于内部内容块，不撑宽页面；不改变 REST 文本和工具输出原始字节。

手机外层上滑达到 48px 时可隐藏操作栏，下滑、轻点正文、恢复入口或 Esc 重新显示；保留草稿和消息锚点，运行中停止始终可达。待处理/失败输入、附件和断线/停止/恢复状态禁用隐藏；该 UI 状态不改变 SSE 订阅和 runtime 生命周期。

页面可见且有运行任务时目录每 2s 对账，否则每 15s；隐藏停止轮询，恢复可见立即刷新，只当前会话保持正文 SSE。新建任务在应用内填写 server 工作目录并生成唯一 alias；删除失败留在原位报告。


## 本轮验证记录（2026-10-04）

- `make docs-site`：27 个公开条目、37 个 Markdown 来源及 11 个必要静态资源通过；产物不含 development、migration 与未跟踪材料。
- `node scripts/test-homepage.mjs`：18 项交互回归；`test_hero_replay.py`：8 项来源与统计回归。
- `make docs-examples`：4 个完整 Rust fenced 示例编译，.env 案例 6 项测试；8 个 Python 示例语法/字段、11 个 TOML 示例语法/分组字段、29 个 CLI 参数引用通过（含续行）。已有 core examples 全 feature 编译通过。
- `cargo run -p mink-core --example custom_llm_backend`：无网络 echo backend 实际运行，返回 model=private-model-v1、alias=local 与一笔 fixture usage；不是生产模型任务。
- `cargo test -p mink-core`：991 单元 + 5 集成测试通过，12 项默认忽略；`cargo test -p mink-cli`：251 单元 + 3 兼容测试通过，4 项默认忽略。
- `cargo fmt --all -- --check` 无输出；workspace all-targets/all-features clippy 成功，无 clippy 诊断（build.rs 仍输出已有 embedded skills 生成通知）。
- 真实 Chromium 在根地址及 Pages `/mink/` 子路径验收 320/390/768/1024/1440px：总览与长配置页无页面级溢出；导航/目录、Esc/Tab 焦点与恢复、跨目录中文锚点、刷新/前后历史、Clipboard API 原字节复制通过。截图在 `target/docs-browser/`，未入库。复制拒绝/兼容回退由 jsdom 回归覆盖。

预览：先 `make docs-site`，再 `python3 -m http.server 8026 --bind 127.0.0.1 --directory target/docs-site`，打开 `http://127.0.0.1:8026/#docs/start/overview.md`。`make docs-browser` 对该预览运行验收；可用 `MINK_DOCS_URL` 指定 Pages 子路径预览。
