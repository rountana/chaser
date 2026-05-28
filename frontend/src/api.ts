import type { RustSearchResult, IndexStatus, AppSettings } from './types'

const BASE = '/api'

async function get<T>(path: string): Promise<T> {
  const res = await fetch(BASE + path)
  if (!res.ok) throw new Error(`GET ${path} → ${res.status}`)
  return res.json() as Promise<T>
}

async function post<T>(path: string, body: unknown): Promise<T> {
  const res = await fetch(BASE + path, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  })
  if (!res.ok) throw new Error(`POST ${path} → ${res.status}`)
  return res.json() as Promise<T>
}

export const api = {
  search: (query: string, top = 12, outputsDir?: string) => {
    const params = new URLSearchParams({ q: query, top: String(top) })
    if (outputsDir) params.set('outputs_dir', outputsDir)
    return get<RustSearchResult[]>(`/search?${params}`)
  },

  indexStatus: (outputsDir?: string) => {
    const params = outputsDir ? `?outputs_dir=${encodeURIComponent(outputsDir)}` : ''
    return get<IndexStatus>(`/index/status${params}`)
  },

  settings: () => get<AppSettings>('/settings'),

  saveSettings: (s: Partial<AppSettings>) => post<void>('/settings', s),
}

// Mock data when the server isn't running
export const MOCK_RESULTS: RustSearchResult[] = [
  {
    filePath: '/Users/demo/Documents/personal/vacation-notes.md',
    fileName: 'vacation-notes.md', sourcePath: null, snippet: '…the beach trip with Milo, the dog. We collected shells in the morning and watched the sunset…',
    pageNum: null, backend: 'keyword', score: 0.82,
    meta: { person: null, docType: 'note', date: '2025-08', pages: null, words: 420, keyword: 'dog' },
  },
  {
    filePath: '/Users/demo/Documents/pets/milo-vet-records.pdf',
    fileName: 'milo-vet-records.pdf', sourcePath: null, snippet: 'Annual exam — Milo, golden retriever, 4 years old. Coat condition: excellent…',
    pageNum: 1, backend: 'keyword', score: 0.71,
    meta: { person: null, docType: 'medical', date: '2025-03', pages: 3, words: 840, keyword: 'dog' },
  },
  {
    filePath: '/Users/demo/Documents/writing/summer-2025-blog-draft.txt',
    fileName: 'summer-2025-blog-draft.txt', sourcePath: null, snippet: 'Three weeks on the coast — what I learned about traveling slowly with a dog…',
    pageNum: null, backend: 'keyword', score: 0.61,
    meta: { person: null, docType: null, date: '2025-08', pages: null, words: 1820, keyword: 'dog' },
  },
  {
    filePath: '/Users/demo/Documents/personal/packing-list.md',
    fileName: 'packing-list.md', sourcePath: null, snippet: 'leash, dog food, beach towel, sunscreen, two extra t-shirts, the small camera bag…',
    pageNum: null, backend: 'keyword', score: 0.41,
    meta: { person: null, docType: null, date: null, pages: null, words: 180, keyword: 'dog' },
  },
]
