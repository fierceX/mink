/**
 * Locale copy for the copied tool-card primitives. Kept in one reference-stable
 * object per primitive so a row re-render never rebuilds the chrome identity.
 */

import type { DiffBlockLabels } from '../../../ui/primitives/DiffBlock.tsx'
import type { ReadBlockLabels } from '../../../ui/primitives/ReadBlock.tsx'
import type { SearchBlockLabels } from '../../../ui/primitives/SearchBlock.tsx'
import type { TerminalBlockLabels } from '../../../ui/primitives/TerminalBlock.tsx'

const CODE_TOOLBAR = {
  codeLabel: '代码',
  wrapLabel: '自动换行',
  unwrapLabel: '保留原样',
}

/** Command card chrome. */
export const TERMINAL_LABELS: TerminalBlockLabels = {
  signal: (signal: string) => `信号 ${signal}`,
  exitCode: (exitCode: number) => `退出码 ${exitCode}`,
  noExitCode: '无退出码',
  running: '运行中',
  failed: '失败',
  done: '完成',
  copy: '复制输出',
  copied: '已复制',
  noOutput: '无输出',
  collapseAria: '收起输出',
  collapse: '收起',
  expandAria: (hidden: number) => `展开其余 ${hidden} 行`,
  expand: (hidden: number) => `展开其余 ${hidden} 行`,
}

/** File card chrome. */
export const READ_LABELS: ReadBlockLabels = {
  ...CODE_TOOLBAR,
  window: (shown: number, total: number) => `显示 ${shown} / 共 ${total} 行`,
  copy: '复制内容',
  copied: '已复制',
  collapseAria: '收起内容',
  expandAria: (hidden: number) => `展开其余 ${hidden} 行`,
  collapse: '收起',
  expand: (hidden: number) => `展开其余 ${hidden} 行`,
}

/** Diff card chrome. */
export const DIFF_LABELS: DiffBlockLabels = {
  ...CODE_TOOLBAR,
  copy: '复制差异',
  copied: '已复制',
  collapseAria: '收起差异',
  expandAria: (hidden: number) => `展开其余 ${hidden} 行`,
  collapse: '收起',
  expand: (hidden: number) => `展开其余 ${hidden} 行`,
}

/** Search card chrome. */
export const SEARCH_LABELS: SearchBlockLabels = {
  pathsSummary: (shown: number, total: number, truncated: boolean) => (truncated
    ? `显示 ${shown} / 共 ${total} 个路径`
    : `${shown} 个路径`),
  matchesSummary: (shown: number, total: number, files: number, truncated: boolean) => (truncated
    ? `显示 ${shown} / 共 ${total} 处匹配 · ${files} 个文件`
    : `${shown} 处匹配 · ${files} 个文件`),
  copy: '复制结果',
  copied: '已复制',
  noResults: '没有匹配',
  collapseAria: '收起结果',
  expandAria: (hidden: number) => `展开其余 ${hidden} 行`,
  collapse: '收起',
  expand: (hidden: number) => `展开其余 ${hidden} 行`,
}
