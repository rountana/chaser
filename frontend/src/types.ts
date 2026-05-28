// Rust SearchResult shape (from pdf-core)
export interface RustSearchResult {
  filePath: string
  fileName: string
  sourcePath: string | null
  snippet: string
  pageNum: number | null
  backend: 'metadata' | 'keyword' | 'structural' | 'semantic'
  score: number | null
  meta: {
    person: string | null
    docType: string | null
    date: string | null
    pages: number | null
    words: number | null
    keyword: string | null
  }
}

// UI result shape (matches the design)
export interface UIResult {
  id: string
  kind: 'img' | 'doc' | 'pdf'
  filename: string
  path: string
  snippet: string
  score: number
  badge: string
  why: string[]
  // Extra fields from Rust (preserved for detail view)
  pageNum?: number | null
  backend?: string
  meta?: RustSearchResult['meta']
  sourcePath?: string | null
}

// Adapter: Rust → UI shape
export function toUIResult(r: RustSearchResult, idx: number): UIResult {
  const ext = r.fileName.split('.').pop()?.toLowerCase() ?? ''
  const kind: UIResult['kind'] =
    ['jpg', 'jpeg', 'png', 'gif', 'webp', 'heic'].includes(ext) ? 'img' :
    ext === 'pdf' ? 'pdf' : 'doc'

  const why: string[] = []
  if (r.meta.keyword) why.push(`keyword match: «${r.meta.keyword}»`)
  if (r.meta.person) why.push(`person: ${r.meta.person}`)
  if (r.meta.docType) why.push(`type: ${r.meta.docType}`)
  if (r.meta.date) why.push(`date: ${r.meta.date}`)
  if (r.backend === 'semantic') why.push('semantic similarity')
  if (r.backend === 'structural') {
    if (r.meta.pages) why.push(`${r.meta.pages} pages`)
    if (r.meta.words) why.push(`${r.meta.words} words`)
  }
  if (why.length === 0) why.push(`matched via ${r.backend}`)

  // Derive display path from sourcePath when available, otherwise filePath
  const pathForDisplay = r.sourcePath ?? r.filePath
  const parts = pathForDisplay.replace(/\\/g, '/').split('/')
  parts.pop()
  const displayPath = parts.slice(-3).join(' / ') || pathForDisplay

  return {
    id: `r${idx}`,
    kind,
    filename: r.fileName,
    path: displayPath,
    snippet: r.snippet,
    score: r.score ?? 0,
    badge: ext || kind,
    why,
    pageNum: r.pageNum,
    backend: r.backend,
    meta: r.meta,
    sourcePath: r.sourcePath,
  }
}

export interface SearchResponse {
  results: RustSearchResult[]
}

export interface IndexStatus {
  filesIndexed: number
  totalFiles: number
  sizeBytes: number
  lastSyncedAt: string | null
  running: boolean
}

export interface AppSettings {
  outputsDir: string
  apiKey: string
  schemaPath: string | null
}
