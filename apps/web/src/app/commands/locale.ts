/**
 * Locale seat of the copied command surfaces. Upstream resolves it through the
 * slots package; mink has one language, so the strings live here and the view
 * keeps its own `t('…')` calls unchanged.
 */

import type { ReactNode } from 'react'

/** Copy the popupSelect shell asks for. */
export interface CommandLocale {
  'search.placeholder': string
  'search.aria': string
  'status.loading': string
  'status.applying': string
  'status.empty': string
  'overlay.aria': string
  'listbox.aria': string
  'notice.attachmentsUnsupported': string
  'retry': string
  'close': string
}

/** Props one owner passes to a copied surface: its children plus the locale seat. */
export interface PropsLocale<K extends string> {
  /** Copy resolver of the surface's own key space (required: the surfaces call it directly). */
  readonly t: (key: string, params?: Record<string, string>) => string
  readonly children?: ReactNode
  /** Present so a surface can be typed without the seat being supplied. */
  readonly _key?: K
}

/** mink's copy for the popupSelect shell. */
export const COMMAND_LOCALE: CommandLocale = {
  'search.placeholder': '搜索命令…',
  'search.aria': '筛选命令',
  'status.loading': '正在加载选项…',
  'status.applying': '正在应用…',
  'status.empty': '无可用命令',
  'overlay.aria': '/{command} 选项',
  'listbox.aria': '/{command} 匹配项',
  'notice.attachmentsUnsupported': '/{command} 不接受附件，请先移除附件',
  'retry': '重试',
  'close': '关闭',
}

/**
 * Resolve one key, substituting `{name}` placeholders.
 * @param locale - the command copy.
 * @param key - the requested key.
 * @param params - placeholder values.
 * @returns the resolved string, or the key itself when it is unknown.
 */
export function commandText(
  locale: CommandLocale,
  key: string,
  params?: Record<string, string>,
): string {
  const template = (locale as unknown as Record<string, string>)[key] ?? key
  if (params === undefined) return template
  return template.replace(/\{(\w+)\}/g, (match, name: string) => params[name] ?? match)
}
