# 官网文档阅读器依赖

主页与文档使用本地静态资源，不依赖访问第三方 CDN。保留原有依赖主版本：

| 资源 | 版本 | 来源与许可 |
| --- | --- | --- |
| `marked.min.js` | 15.0.12 | [Marked](https://github.com/markedjs/marked)，MIT，见 `marked.LICENSE.md` |
| `highlight.min.js`、`github.min.css` | 11.9.0 | [Highlight.js](https://github.com/highlightjs/highlight.js)，BSD-3-Clause，见 `highlight.LICENSE` |

资源来自 jsDelivr 的 `npm/marked@15.0.12`、`gh/highlightjs/cdn-release@11.9.0/build`；
Highlight.js 许可来自 `npm/highlight.js@11.9.0/LICENSE`。更新时一并检查文档渲染与许可。
