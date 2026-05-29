import { useState, useEffect } from 'react'
import Icon from './icons'
import { ScreenEmpty, ScreenResults } from './ScreensSearch'
import { ScreenIndexing, ScreenSettings } from './ScreensSystem'
import { api } from './api'

type ScreenId = 'empty' | 'results' | 'preview' | 'indexing' | 'settings'

const SCREENS: { id: ScreenId; n: string; label: string; icon: string }[] = [
  { id: 'empty',    n: '01', label: 'Search',    icon: 'command' },
  { id: 'results',  n: '02', label: 'Results',   icon: 'search' },
  { id: 'preview',  n: '03', label: 'Preview',   icon: 'eye' },
  { id: 'indexing', n: '04', label: 'First-run', icon: 'layers' },
  { id: 'settings', n: '05', label: 'Settings',  icon: 'settings' },
]

const LS_OUTPUTS_DIR = 'chaser.outputsDir'
const LS_SCHEMA_PATH = 'chaser.schemaPath'
const LS_SEARCH_MODE = 'chaser.searchMode'
type SearchMode = 'text' | 'images'
function loadSearchMode(): SearchMode {
  const v = localStorage.getItem(LS_SEARCH_MODE)
  return v === 'images' ? 'images' : 'text'
}
function saveSearchMode(m: SearchMode) {
  localStorage.setItem(LS_SEARCH_MODE, m)
}

function loadOutputsDir(): string {
  return localStorage.getItem(LS_OUTPUTS_DIR) ?? ''
}

function saveOutputsDir(dir: string) {
  localStorage.setItem(LS_OUTPUTS_DIR, dir)
}

function loadSchemaPath(): string {
  return localStorage.getItem(LS_SCHEMA_PATH) ?? ''
}

function saveSchemaPathLocal(p: string) {
  localStorage.setItem(LS_SCHEMA_PATH, p)
}

function WindowFrame({ title, right, children }: {
  title: React.ReactNode
  right?: string
  children: React.ReactNode
}) {
  return (
    <div className="frame">
      <div className="titlebar">
        <div className="tl-dots">
          <span className="tl-dot r" />
          <span className="tl-dot y" />
          <span className="tl-dot g" />
        </div>
        <div className="tl-title">{title}</div>
        <div className="tl-right">{right}</div>
      </div>
      <div className="window-body">{children}</div>
    </div>
  )
}

export default function App() {
  const [active, setActive] = useState<ScreenId>('empty')
  const [query, setQuery] = useState('')
  const [selectedId, setSelectedId] = useState<string | null>(null)
  const [layout, setLayout] = useState('list')
  const [showScores, setShowScores] = useState(true)
  const [outputsDir, setOutputsDirState] = useState<string>(loadOutputsDir)
  const [schemaPath, setSchemaPathState] = useState<string>(loadSchemaPath)
  const [fileCount, setFileCount] = useState<number | null>(null)
  const [searchMode, setSearchModeState] = useState<SearchMode>(loadSearchMode)
  function setSearchMode(m: SearchMode) {
    setSearchModeState(m)
    saveSearchMode(m)
  }

  function setOutputsDir(dir: string) {
    setOutputsDirState(dir)
    saveOutputsDir(dir)
    api.saveSettings({ outputsDir: dir }).catch(() => {})
  }

  function setSchemaPath(p: string) {
    setSchemaPathState(p)
    saveSchemaPathLocal(p)
    api.saveSettings({ schemaPath: p }).catch(() => {})
  }

  // Fetch initial settings from server (if running)
  useEffect(() => {
    api.settings()
      .then(s => {
        if (s.outputsDir && !outputsDir) setOutputsDir(s.outputsDir)
        if (s.schemaPath && !schemaPath) setSchemaPathState(s.schemaPath)
      })
      .catch(() => {})
  }, []) // eslint-disable-line react-hooks/exhaustive-deps

  // Poll file count for the status badge
  useEffect(() => {
    if (!outputsDir) return
    api.indexStatus(outputsDir)
      .then(s => setFileCount(s.filesIndexed))
      .catch(() => {})
  }, [outputsDir])

  function handleSearch(q: string) {
    setQuery(q)
    setActive('results')
  }

  function handleSelect(id: string) {
    setSelectedId(id)
    setActive('preview')
  }

  const frameTitle =
    active === 'empty'    ? <><b>chaser</b> · idle</> :
    active === 'indexing' ? <><b>chaser</b> · first run</> :
    active === 'settings' ? <><b>chaser</b> · settings</> :
    <><b>chaser</b> · "{query}"</>

  const frameRight =
    active === 'indexing' ? 'step 2 of 4' :
    active === 'empty'    ? '⌘K' : 'v0.1'

  let screen: React.ReactNode
  if (active === 'empty') {
    screen = <ScreenEmpty onSearch={handleSearch} />
  } else if (active === 'results') {
    screen = (
      <ScreenResults
        query={query} setQuery={setQuery}
        selectedId={null} onSelect={handleSelect}
        layout={layout} setLayout={setLayout}
        showScores={showScores} setShowScores={setShowScores}
        onSearch={handleSearch}
        outputsDir={outputsDir}
        searchMode={searchMode}
        setSearchMode={setSearchMode}
      />
    )
  } else if (active === 'preview') {
    screen = (
      <ScreenResults
        query={query} setQuery={setQuery}
        selectedId={selectedId} onSelect={setSelectedId}
        layout={layout} setLayout={setLayout}
        showScores={showScores} setShowScores={setShowScores}
        withPreview onSearch={handleSearch}
        outputsDir={outputsDir}
        searchMode={searchMode}
        setSearchMode={setSearchMode}
      />
    )
  } else if (active === 'indexing') {
    screen = (
      <ScreenIndexing
        outputsDir={outputsDir}
        onFolderChange={setOutputsDir}
      />
    )
  } else {
    screen = (
      <ScreenSettings
        outputsDir={outputsDir}
        onFolderChange={setOutputsDir}
        schemaPath={schemaPath}
        onSchemaPathChange={setSchemaPath}
      />
    )
  }

  const statusLabel = fileCount !== null
    ? `${fileCount.toLocaleString()} files indexed`
    : outputsDir ? 'loading…' : 'no folder set'

  return (
    <div className="page">
      <header className="masthead">
        <div className="brand">
          <div className="logo">c</div>
          <div>
            <h1>chaser</h1>
            <div className="sub">local search · v0.1</div>
          </div>
        </div>
        <div className="meta">
          <span className="badge">
            <span className={fileCount !== null && fileCount > 0 ? 'dot' : 'dot warn'} />
            {statusLabel}
          </span>
        </div>
      </header>

      <nav className="tabstrip" role="tablist">
        {SCREENS.map(s => (
          <button
            key={s.id}
            role="tab"
            aria-selected={active === s.id}
            className={`tab${active === s.id ? ' is-active' : ''}`}
            onClick={() => setActive(s.id)}
          >
            <span className="n">{s.n}</span>
            <Icon name={s.icon} size={13} />
            <span>{s.label}</span>
          </button>
        ))}
      </nav>

      <main>
        <WindowFrame title={frameTitle} right={frameRight}>
          {screen}
        </WindowFrame>
      </main>

      <footer style={{
        marginTop: 18, color: 'var(--text-faint)', fontSize: 12,
        display: 'flex', justifyContent: 'space-between', flexWrap: 'wrap',
        gap: 12, fontFamily: 'var(--font-mono)', letterSpacing: '0.02em',
      }}>
        <div>⌘K opens palette · click results to preview · data stays local</div>
        <div>chaser 0.1 · dark</div>
      </footer>
    </div>
  )
}
