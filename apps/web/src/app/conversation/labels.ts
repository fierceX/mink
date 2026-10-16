/**
 * Reference-stable chrome labels for the copied Markdown renderer and the
 * disclosure rows. Identity must stay stable across renders: a new labels
 * object discards the streaming render cache mid-message.
 */

import type { MarkdownLabels } from '../../ui/primitives/index.ts'

/** Markdown chrome: fence copy controls and the footnote section title. */
export const MARKDOWN_LABELS: MarkdownLabels = {
  code: {
    copyLabel: '复制',
    copiedLabel: '已复制',
    toolbarLabels: {
      codeLabel: '代码',
      wrapLabel: '自动换行',
      unwrapLabel: '保留原样',
    },
  },
  footnotes: '脚注',
} as MarkdownLabels
