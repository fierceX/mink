# apps/web — mink web workbench

React 工作台：设计层与原子组件是直接拷入仓库的外部参考快照源码（vendored，MIT；不是 npm 包或运行时依赖），业务数据层由 mink 自己实现并接入 `mink-server` 的 REST + SSE 接口。
上游只作为初期设计参考，后续由 mink 独立演进；再同步是可选维护动作，不作为构建或 PR 前置条件。

## 现状

服务器默认服务本工作台（`crates/mink-server` 的 build.rs 构建 `dist` 并按字节嵌入二进制）。
已交付：三栏外壳与侧栏（目录、筛选/搜索、新建/删除、分组）、会话区（历史与分页、SSE 实时投影、
过程行与工具卡、逐轮改动文件卡、消息 chrome）、composer（发送/停止/运行中引导/附件/斜杠命令/模型位）、
目标条与目标边界卡、详情面板五页签（含与 TUI 同口径的诊断）、轮次选择器、设置与偏好、窄屏阅读 chrome。
开发时可用 `MINK_SERVER_DEV_WEB=<dir>/dist` 从磁盘服务（前端热迭代，改完强刷即可）。

按「只抄 mink 具有的功能」，上游的插件面板、Pin/Archive、权限 pill、Standard mode 标签、
Chat/Trajectory 页签与消息时间戳都不在这里出现。

## 命令

```bash
npm install
npm run dev        # 开发服务器，/api 代理到 127.0.0.1:18821 上的 mink-server
npm run build      # 产出 dist/
npm run test       # vitest（jsdom）
npm run typecheck  # tsc --noEmit
npm run e2e          # Playwright：fixtures（设计核对页）+ live（隔离 home 的真实服务端）
```

`playwright.config.ts` 自行拉起 Vite 与隔离服务端：`e2e/global-setup.ts` 建临时 home、确定性 provider
与 `target/debug/mink-server`（从磁盘服务刚构建的 dist），端口被占用直接失败。

## 偏好与主题

设置对话框（侧栏底部「设置」）按来源的机制落盘：`data-ds-dark-theme` 属性切换暗色调色板、
内容字号变量驱动全部正文/代码字号轴，`data-ds-theme-source` 记录偏好来源；主题三档
（浅色/深色/跟随系统，跟随系统时监听 `prefers-color-scheme`），字号 10–22px，过程展示简洁/标准/详细
（简洁只留结论，详细默认展开过程组）。偏好存 `localStorage`，并在挂载前应用以避免首帧闪烁。

## 设计核对页

`npm run dev` 后打开 `/fixtures.html`：用固定 transcript 把工具卡各家族（终端/文件/差异/搜索匹配/路径/通用
输入输出）一次渲染出来，供截图与对照；该页只在 dev server 存在，不进入生产构建。

## tsconfig 的两处放宽

`exactOptionalPropertyTypes` 与 `noUncheckedIndexedAccess` 关闭：本应用拷贝的来源包
（来源客户端 tsconfig 与拷贝来源包的 tsconfig）都没有开这两个开关，保留关闭可避免为通过检查而改写拷贝代码。其余严格检查保持开启。

## 目录

| 路径 | 来源 | 说明 |
| --- | --- | --- |
| `src/theme/styles/` | `packages/client/ui-theme/src/styles` | 逐字节拷贝：`design-platform.css` 等令牌表 + Montserrat 字体（OFL） |
| `src/ui/primitives/` | `packages/client/ui-primitives/src` | 逐字节拷贝的 128 个文件：按钮/标签/菜单/弹层/markdown/工具块等纯 React 原子 |
| `src/util/code-language.ts` | `packages/util/code-language/src/index.ts` | 代码高亮语言表 |
| `src/util/workspace-path/` | `packages/util/workspace-path/src` | 路径分段与文件地址解析 |
| `src/store/` | `packages/client/store/src` | 快照 store（zustand + immer），供设置类表单使用 |
| `src/app/` | mink + 上游快照 | 应用外壳、侧栏、会话区与数据接线为 mink 实现；部分屏幕 CSS module 与 `frame/columns.ts` 拷贝自上游（清单见 `THIRD_PARTY_NOTICES.md`） |
| `src/fixtures/`、`fixtures.html` | mink | 设计核对页（仅 dev server），固定 transcript 渲染工具卡各家族 |
| `src/data/` | 上一代 Vue 工作台的 `src/lib`（该前端已移除） | 逐字节拷贝的事件投影：`reducer`、`toolFormat`、`transcriptProjection`、`sse`、类型；`api`/hooks 为新写的接线 |

拷贝文件保留源文件的代码形态（含来源框架的 DOM 标记与注释）以便识别来源；导入落点指向本目录内的副本。
拷贝代码由 mink 维护，可直接按本项目需要修改，后续独立演进；来源与许可见 `THIRD_PARTY_NOTICES.md`。

## 依赖版本对齐

上游按 React 18 编写（`inert` 以空字符串下发等约定），因此本应用固定 `react@18.3.1` +
`@types/react@18`。升级到 React 19 可能需要在拷贝代码中做局部修改；这些文件由 mink 维护，不要求与上游逐字节一致。

第三方式声明见 [THIRD_PARTY_NOTICES.md](./THIRD_PARTY_NOTICES.md)。
