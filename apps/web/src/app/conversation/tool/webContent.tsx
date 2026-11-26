import { MarkdownText, WebBlock } from '../../../ui/primitives/index.ts'
import type { WebSourceView } from '../../../ui/primitives/WebBlock.tsx'
import { isProviderWebTool } from '../../../data/toolFormat.ts'
import type { ToolItem, WebCitation } from '../../../data/types.ts'
import { MARKDOWN_LABELS } from '../labels.ts'

const LABELS = {
  noResults: '没有搜索结果',
  sourcesTruncated: '部分来源已省略',
  http: 'HTTP',
  contentTruncated: '正文已截断',
  markdown: MARKDOWN_LABELS,
}

function source(value: unknown): WebSourceView[] {
  if (value === null || typeof value !== 'object') return []
  const { url, title, snippet, published_at } = value as Record<string, unknown>
  return typeof url === 'string' ? [{
    url,
    title: typeof title === 'string' ? title : undefined,
    snippet: typeof snippet === 'string' ? snippet : undefined,
    publishedAt: typeof published_at === 'string' ? published_at : undefined,
  }] : []
}

export function webToolBody(item: ToolItem) {
  if (item.result === undefined) return null
  if (item.name === 'WebFetch' && item.success === true) {
    const match = /^# ([^\n]*)\n\nSource: (\S+)\n\n([\s\S]*)$/.exec(item.result)
    if (match === null) return null
    return <>
      <WebBlock kind="fetch" url={match[2]!} title={match[1]} labels={LABELS} truncated={(item.artifacts?.length ?? 0) > 0} />
      <MarkdownText text={match[3]!} labels={MARKDOWN_LABELS} variant="compact" inertImages />
    </>
  }
  if (item.name !== 'WebSearch' && !isProviderWebTool(item.name)) return null
  try {
    const result = JSON.parse(item.result) as Record<string, unknown> | null
    if (result === null) return null
    if (typeof result.error === 'string') return <p>{result.error}</p>
    if (item.success !== true) return null
    const page = source(result.page)[0]
    if (page !== undefined) {
      return <WebBlock kind="fetch" url={page.url} title={page.title} labels={LABELS} truncated={false} />
    }
    const sources = result.results ?? result.sources
    return Array.isArray(sources)
      ? <WebBlock kind="search" sources={sources.flatMap(source)} labels={LABELS} truncated={false} />
      : null
  } catch {
    return null
  }
}

export function WebCitations({ text, citations }: { text: string; citations: WebCitation[] }) {
  const codePoints = Array.from(text)
  const sources = citations.flatMap(citation => {
    if (!citation || typeof citation !== 'object') return []
    const { start_index: start, end_index: end } = citation
    return source({
      ...citation,
      snippet: Number.isInteger(start) && Number.isInteger(end) && start >= 0 && end > start && end <= codePoints.length
        ? codePoints.slice(start, Math.min(start + 240, end)).join('')
        : undefined,
    })
  })
  if (sources.length === 0) return null
  return <section aria-label="网页来源"><WebBlock kind="search" sources={sources} labels={LABELS} truncated={false} /></section>
}
