# Third-party notices — apps/web

本目录的下列文件拷贝自 deepseek-harness（<https://github.com/deepseek-ai/deepseek-harness>），
版权归 DeepSeek 所有，按 MIT 许可使用。除导入落点（指向本目录内副本）外未作改动。

对照版本（`npm run check:design` 的比对基线，升级拷贝时同步更新）：
`5badb15009ae1756c3afe0ae0cef1faafc290ccc`（CI 按此 revision 浅克隆上游后逐字节比对）。

| 本目录路径 | 上游路径 |
| --- | --- |
| `src/theme/styles/*.css`、`src/ui/primitives/**/*.css` | `packages/client/ui-theme/src/styles`、`packages/client/ui-primitives/src` |
| `src/theme/styles/montserrat-*.woff2` | 同上目录，字体本体按 SIL Open Font License 1.1 分发（见 `src/theme/styles/Montserrat-OFL.txt`） |
| `src/app/conversation/running-whale@2x.png` | `packages/client/ui-chat/src/client/chat/running-whale@2x.png` |
| `src/ui/primitives/**` | `packages/client/ui-primitives/src`（含 `icons/`、`markdown/`、`settings-form/`） |
| `src/util/code-language.ts` | `packages/util/code-language/src/index.ts` |
| `src/util/workspace-path/{index.ts,file-address.ts}` | `packages/util/workspace-path/src/{index.ts,file-address.ts}` |
| `src/store/{index.ts,contract.ts}` | `packages/client/store/src/{index.ts,contract.ts}` |

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
