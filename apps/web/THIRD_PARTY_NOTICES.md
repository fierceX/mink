# Third-party notices — apps/web

本目录的下列文件是直接拷入仓库的源码（vendored，不通过 npm 包引用），拷贝自 deepseek-harness（<https://github.com/deepseek-ai/deepseek-harness>），
版权归 DeepSeek 所有，按 MIT 许可使用。上游只作为初期设计参考；这些文件由 mink 维护、可按本项目需要独立演进，MIT 署名随文件保留，
与上游再同步是可选维护动作，不是构建或 PR 前置条件。

拷贝时参考的快照：`5badb15009ae1756c3afe0ae0cef1faafc290ccc`。普通构建、测试与 PR CI 不依赖参考实现。

| 本目录路径 | 上游路径 |
| --- | --- |
| `src/theme/styles/*.css`、`src/ui/primitives/**/*.css` | `packages/client/ui-theme/src/styles`、`packages/client/ui-primitives/src` |
| `src/theme/styles/montserrat-*.woff2` | 同上目录，字体本体按 SIL Open Font License 1.1 分发（见 `src/theme/styles/Montserrat-OFL.txt`） |
| `src/app/conversation/running-whale@2x.png` | `packages/client/ui-chat/src/client/chat/running-whale@2x.png` |
| `src/ui/primitives/**` | `packages/client/ui-primitives/src`（含 `icons/`、`markdown/`；`settings-form/` 与 `ConfigField` 已移除） |
| `src/util/code-language.ts` | `packages/util/code-language/src/index.ts` |
| `src/util/workspace-path/{index.ts,file-address.ts}` | `packages/util/workspace-path/src/{index.ts,file-address.ts}` |
| `src/store/{index.ts,contract.ts}` | `packages/client/store/src/{index.ts,contract.ts}` |

以下屏幕级与布局文件同样来自上述快照（MIT）；它们可以按 mink 需要修改，不要求逐字节同步：

| 本目录路径 | 上游路径 |
| --- | --- |
| `src/app/conversation/AssistantMarkdown.module.css` | `packages/client/ui-chat/src/client/chat/AssistantMarkdown.module.css` |
| `src/app/conversation/ChangedFiles.module.css` | `packages/client/ui-deliverables/src/client/ChangedFiles.module.css` |
| `src/app/conversation/ComposerAttachments.module.css` | `packages/client/ui-attachment/src/client/ComposerAttachments.module.css` |
| `src/app/conversation/DropOverlay.module.css` | `packages/client/ui-attachment/src/DropOverlay.module.css` |
| `src/app/conversation/HeroShell.module.css` | `packages/client/ui-conversation/src/client/skeleton/HeroShell.module.css` |
| `src/app/conversation/MessageIconActions.module.css` | `packages/client/ui-chat/src/client/chat/MessageIconActions.module.css` |
| `src/app/conversation/ReasoningRow.module.css` | `packages/client/ui-chat/src/client/chat/ReasoningRow.module.css` |
| `src/app/conversation/TurnProcessNodeView.module.css` | `packages/client/ui-chat/src/client/chat/TurnProcessNodeView.module.css` |
| `src/app/details/ExpandButton.module.css` | `packages/client/ui-sidebar-right/src/client/shell/ExpandButton.module.css` |
| `src/app/settings/AppearanceRow.module.css` | `packages/client/ui-theme/src/client/AppearanceRow.module.css` |
| `src/app/settings/FontSizeRow.module.css` | `packages/client/ui-theme/src/client/FontSizeRow.module.css` |
| `src/app/sidebar/Rows.module.css` | `packages/client/ui-workspace/src/client/rows/Rows.module.css` |
| `src/app/tool/ToolCallTree.module.css` | `packages/client/ui-tool/src/client/tool/ToolCallTree.module.css` |
| `src/app/frame/columns.ts` | `packages/client/ui-layout/src/client/columns.ts` |
| `src/ui/commands/PopupSelectView.module.css` | `packages/client/ui-commands/src/client/PopupSelectView.module.css` |

上游许可全文（MIT）：

```
MIT License

Copyright (c) 2026 DeepSeek

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

npm 依赖（React、shiki、katex、simple-icons、mdast/micromark、zustand、immer 等）按其各自许可分发，
见 `package.json` 与 `node_modules/*/LICENSE`。
