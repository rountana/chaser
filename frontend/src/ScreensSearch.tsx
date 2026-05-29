import { useState, useCallback, useRef, useEffect } from 'react'
import Icon from './icons'
import type { UIResult } from './types'
import { api, MOCK_RESULTS } from './api'
import { toUIResult } from './types'

// ── Primitives ──────────────────────────────────────────────────────────────

function ScoreBar({ value }: { value: number }) {
  return (
    <span className="score">
      <span className="bar"><i style={{ width: `${Math.round(value * 100)}%` }} /></span>
      <span className="n">{value.toFixed(2)}</span>
    </span>
  )
}

function Thumb({ kind, badge }: { kind: string; badge: string }) {
  return (
    <div className={`thumb ${kind}`}>
      {badge && <span className="badge">{badge}</span>}
      <span className="ic">{kind === 'doc' ? 'doc' : kind === 'img' ? 'img' : 'pdf'}</span>
    </div>
  )
}

const FOLDER_TREE = [
  { ind: '▾', name: 'Documents', depth: 0 },
  { ind: '▸', name: 'personal', depth: 1, count: 8 },
  { ind: '▸', name: 'pets', depth: 1, count: 4 },
  { ind: '▸', name: 'writing', depth: 1, count: 22 },
  { ind: '▾', name: 'Pictures', depth: 0 },
  { ind: '●', name: '2025', depth: 1, count: 184, active: true },
  { ind: '▸', name: 'Downloads', depth: 0 },
]

// ── Sidebar ─────────────────────────────────────────────────────────────────

function Sidebar() {
  return (
    <aside className="sb">
      <h4>scope</h4>
      <div className="chip-row" style={{ padding: '0 4px' }}>
        <span className="chip on">all files</span>
        <span className="chip">docs</span>
        <span className="chip">pdf</span>
      </div>

      <h4>library</h4>
      <ul className="tree">
        {FOLDER_TREE.map((n, i) => (
          <li key={i} className={n.active ? 'is-on' : ''} style={{ paddingLeft: 8 + n.depth * 12 }}>
            <span className="twig">{n.ind}</span>
            <Icon name={n.depth === 0 ? 'folderOpen' : 'folder'} size={13} />
            <span className="nm">{n.name}</span>
            {n.count && <span className="ct">{n.count}</span>}
          </li>
        ))}
      </ul>

      <h4>saved searches</h4>
      <ul className="tree">
        <li>
          <span className="twig"><Icon name="star" size={12} color="var(--warn)" /></span>
          <span className="nm">invoices · this year</span>
        </li>
        <li>
          <span className="twig"><Icon name="star" size={12} color="var(--warn)" /></span>
          <span className="nm">untagged docs</span>
        </li>
      </ul>

      <div className="index-stat">
        <div><b>3,481</b> files · <b>642 MB</b></div>
        <div className="last"><span className="dot" />{' '}synced 2 min ago</div>
      </div>
    </aside>
  )
}

// ── SearchBar ────────────────────────────────────────────────────────────────

interface SearchBarProps {
  query: string
  setQuery: (q: string) => void
  onSearch: (q: string) => void
  intent?: string
  searchMode: 'text' | 'images'
  setSearchMode: (m: 'text' | 'images') => void
}

function SearchBar({ query, setQuery, onSearch, intent = 'keyword', searchMode, setSearchMode }: SearchBarProps) {
  return (
    <div className="searchbar">
      <div className="input-wrap">
        <Icon name="search" size={16} />
        <input
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          onKeyDown={(e) => { if (e.key === 'Enter') onSearch(query) }}
          placeholder="ask in your own words…"
        />
        <span className="intent">
          <span className="dot" />
          <span>intent · {intent}</span>
        </span>
      </div>
      <div className="mode-toggle" role="group" aria-label="Search pool">
        <label className={`mode-opt${searchMode === 'text' ? ' on' : ''}`}>
          <input
            type="radio"
            name="searchMode"
            value="text"
            checked={searchMode === 'text'}
            onChange={() => setSearchMode('text')}
          />
          Documents (PDFs)
        </label>
        <label className={`mode-opt${searchMode === 'images' ? ' on' : ''}`}>
          <input
            type="radio"
            name="searchMode"
            value="images"
            checked={searchMode === 'images'}
            onChange={() => setSearchMode('images')}
          />
          Images &amp; Scans
        </label>
      </div>
    </div>
  )
}

// ── Crumbs toolbar ───────────────────────────────────────────────────────────

interface CrumbsProps {
  layout: string
  setLayout: (l: string) => void
  showScores: boolean
  setShowScores: (v: boolean) => void
  count: number
  total: number
}

function Crumbs({ layout, setLayout, showScores, setShowScores, count, total }: CrumbsProps) {
  return (
    <div className="crumbs">
      <div className="left">
        <span style={{ fontFamily: 'var(--font-mono)' }}>showing top</span>
        <span className="pill solid mono">k={count}</span>
        <span>of {total} ranked</span>
      </div>
      <div className="right">
        <div className="toggle">
          <button className={layout === 'list' ? 'on' : ''} onClick={() => setLayout('list')}>
            <Icon name="layoutList" size={13} />
          </button>
          <button className={layout === 'grid' ? 'on' : ''} onClick={() => setLayout('grid')}>
            <Icon name="layoutGrid" size={13} />
          </button>
        </div>
        <button className={`btn ghost${showScores ? ' on' : ''}`} onClick={() => setShowScores(!showScores)}>
          {showScores && <Icon name="check" size={13} />}
          Scores
        </button>
      </div>
    </div>
  )
}

// ── Result card ──────────────────────────────────────────────────────────────

interface CardProps {
  r: UIResult
  selected: boolean
  onClick: () => void
  layout: string
  showScores: boolean
}

function ResultCard({ r, selected, onClick, layout, showScores }: CardProps) {
  return (
    <div className={`card${selected ? ' is-selected' : ''}`} onClick={onClick}>
      <Thumb kind={r.kind} badge={r.badge} />
      <div className="body">
        <div className="filename">
          <span className="name">{r.filename}</span>
          {showScores && layout === 'list' && <ScoreBar value={r.score} />}
        </div>
        <div className="path">{r.path}</div>
        {layout === 'list' && <div className="snippet">{r.snippet}</div>}
        {layout === 'grid' && showScores && <div style={{ marginTop: 6 }}><ScoreBar value={r.score} /></div>}
      </div>
    </div>
  )
}

// ── Document embed ───────────────────────────────────────────────────────────

function DocumentEmbed({ result }: { result: UIResult }) {
  if (!result.sourcePath) {
    return (
      <div className="hero" style={{ color: 'var(--text-faint)', fontSize: 12 }}>
        {result.snippet || 'no preview available'}
      </div>
    )
  }

  const fileUrl = `/api/file?path=${encodeURIComponent(result.sourcePath)}`

  if (result.kind === 'img') {
    return (
      <div className="hero" style={{ padding: 0, overflow: 'hidden' }}>
        <img
          src={fileUrl}
          alt={result.filename}
          style={{ width: '100%', height: '100%', objectFit: 'contain', display: 'block' }}
        />
      </div>
    )
  }

  if (result.kind === 'pdf') {
    return (
      <div className="hero" style={{ padding: 0, overflow: 'hidden' }}>
        <embed
          src={fileUrl}
          type="application/pdf"
          style={{ width: '100%', height: '100%', display: 'block' }}
        />
      </div>
    )
  }

  // text / doc: show snippet
  return (
    <div className="hero" style={{ fontSize: 12, whiteSpace: 'pre-wrap', overflow: 'auto', textAlign: 'left', padding: '12px' }}>
      {result.snippet || 'no preview available'}
    </div>
  )
}

// ── Preview panel ────────────────────────────────────────────────────────────

function PreviewPanel({ result }: { result: UIResult }) {
  return (
    <aside className="preview">
      <DocumentEmbed result={result} />
      <h3>{result.filename}</h3>
      <div className="pth">{result.path}</div>

      <div className="why">
        <h5>Why this matched</h5>
        <ul>
          {result.why.map((w, i) => <li key={i}>{w}</li>)}
        </ul>
        <div className="conf">
          <span>Confidence</span>
          <b>{(result.score * 100).toFixed(0)}%</b>
        </div>
      </div>

      <dl className="kv">
        {result.meta?.docType && <><dt>Type</dt><dd>{result.meta.docType}</dd></>}
        {result.meta?.date && <><dt>Date</dt><dd>{result.meta.date}</dd></>}
        {result.pageNum && <><dt>Page</dt><dd>{result.pageNum}</dd></>}
        {result.meta?.pages && <><dt>Pages</dt><dd>{result.meta.pages}</dd></>}
        {result.meta?.words && <><dt>Words</dt><dd>{result.meta.words.toLocaleString()}</dd></>}
        <dt>Backend</dt><dd className="mono">{result.backend}</dd>
      </dl>

      <div className="actions">
        <button className="btn primary" onClick={() => result.sourcePath && window.open(`/api/file?path=${encodeURIComponent(result.sourcePath)}`, '_blank')}>
          <Icon name="externalLink" size={13} />
          Open
        </button>
        <button className="btn" onClick={() => result.sourcePath && navigator.clipboard.writeText(result.sourcePath)}>
          <Icon name="copy" size={13} />
          Copy path
        </button>
        <button className="btn icon" title="Bookmark">
          <Icon name="bookmark" size={13} />
        </button>
      </div>
    </aside>
  )
}

// ── Screen 1: Command palette ─────────────────────────────────────────────────

interface ScreenEmptyProps {
  onSearch: (q: string) => void
}

export function ScreenEmpty({ onSearch }: ScreenEmptyProps) {
  const [input, setInput] = useState('')
  const recent = [
    { ic: 'fileText', q: 'invoices from last quarter' },
    { ic: 'fileText', q: 'vet records Milo' },
    { ic: 'fileText', q: 'summer 2025 trip notes' },
    { ic: 'fileText', q: 'marketing plan q2 final' },
  ]
  return (
    <div className="empty">
      <div className="palette">
        <div className="ph">
          <span style={{ fontWeight: 500, color: 'var(--text-muted)' }}>chaser</span>
          <span style={{ color: 'var(--text-faint)' }}>·</span>
          <span>command palette</span>
          <span style={{ marginLeft: 'auto' }}>
            <span className="kbd">⌘ K</span>
          </span>
        </div>
        <div className="pin">
          <Icon name="search" size={20} />
          <input
            value={input}
            onChange={(e) => setInput(e.target.value)}
            placeholder="Ask in your own words…"
            autoFocus
            onKeyDown={(e) => { if (e.key === 'Enter' && input.trim()) onSearch(input.trim()) }}
          />
          <span className="pill accent">
            <Icon name="sparkles" size={11} />
            smart search
          </span>
        </div>
        <div className="recents">
          <div className="section-label" style={{ padding: '10px 12px 4px' }}>Recent</div>
          {recent.map((r, i) => (
            <div key={i} className="recent-row" onClick={() => onSearch(r.q)}>
              <div className="ic"><Icon name={r.ic} size={12} /></div>
              <div className="q">{r.q}</div>
            </div>
          ))}
        </div>
        <div className="palette-foot">
          <div className="left">
            <span className="group"><span className="kbd">↑</span><span className="kbd">↓</span> navigate</span>
            <span className="group"><span className="kbd">↵</span> search</span>
          </div>
          <div>3,481 files indexed</div>
        </div>
      </div>
    </div>
  )
}

// ── Screen 2/3: Results + optional preview panel ──────────────────────────────

interface ScreenResultsProps {
  query: string
  setQuery: (q: string) => void
  selectedId: string | null
  onSelect: (id: string) => void
  layout: string
  setLayout: (l: string) => void
  showScores: boolean
  setShowScores: (v: boolean) => void
  withPreview?: boolean
  onSearch: (q: string) => void
  outputsDir: string
  searchMode: 'text' | 'images'
  setSearchMode: (m: 'text' | 'images') => void
}

export function ScreenResults({
  query, setQuery, selectedId, onSelect,
  layout, setLayout, showScores, setShowScores,
  withPreview = false, onSearch, outputsDir,
  searchMode, setSearchMode,
}: ScreenResultsProps) {
  const [results, setResults] = useState<UIResult[]>([])
  const [loading, setLoading] = useState(false)
  const abortRef = useRef<AbortController | null>(null)

  const runSearch = useCallback(async (q: string) => {
    if (!q.trim()) return
    abortRef.current?.abort()
    abortRef.current = new AbortController()
    setLoading(true)
    try {
      const raw = await api.search(q, 12, outputsDir || undefined, searchMode)
      setResults(raw.map((r, i) => toUIResult(r, i)))
    } catch {
      // Server not running — fall back to mock data
      setResults(MOCK_RESULTS.map((r, i) => toUIResult(r, i)))
    } finally {
      setLoading(false)
    }
  }, [outputsDir, searchMode])

  useEffect(() => { void runSearch(query) }, [query, runSearch, searchMode])

  const selected = results.find(r => r.id === selectedId) ?? results[0] ?? null

  return (
    <div className={`shell${withPreview ? ' has-preview' : ''}`}>
      <Sidebar />
      <div className="main">
        <SearchBar query={query} setQuery={setQuery} onSearch={onSearch}
          searchMode={searchMode} setSearchMode={setSearchMode} />
        <Crumbs
          layout={layout} setLayout={setLayout}
          showScores={showScores} setShowScores={setShowScores}
          count={results.length} total={results.length}
        />
        {loading ? (
          <div style={{ padding: 32, color: 'var(--text-muted)', fontFamily: 'var(--font-mono)', fontSize: 13 }}>
            searching…
          </div>
        ) : (
          <div className={`results ${layout}`}>
            {results.map((r) => (
              <ResultCard
                key={r.id} r={r}
                selected={r.id === selectedId}
                layout={layout} showScores={showScores}
                onClick={() => onSelect(r.id)}
              />
            ))}
            {results.length === 0 && (
              <div style={{ padding: 32, color: 'var(--text-muted)', fontFamily: 'var(--font-mono)', fontSize: 13 }}>
                no results for "{query}"
              </div>
            )}
          </div>
        )}
      </div>
      {withPreview && selected && <PreviewPanel result={selected} />}
    </div>
  )
}
