import { useState } from 'react'
import Icon from './icons'

// ── Toggle ───────────────────────────────────────────────────────────────────

function Toggle({ on, onClick }: { on: boolean; onClick: () => void }) {
  return <button className={`swt${on ? ' on' : ''}`} onClick={onClick} />
}

// ── Folder picker (path input + native dialog hint) ──────────────────────────

interface FolderPickerProps {
  label: string
  value: string
  onChange: (path: string) => void
  hint?: string
}

function FolderPicker({ label, value, onChange, hint }: FolderPickerProps) {
  const [editing, setEditing] = useState(false)
  const [draft, setDraft] = useState(value)

  function commit() {
    const trimmed = draft.trim()
    if (trimmed) onChange(trimmed)
    setEditing(false)
  }

  // Use File System Access API when available (Chrome/Edge/Brave)
  async function pickNative() {
    if ('showDirectoryPicker' in window) {
      try {
        // @ts-expect-error - non-standard API
        const handle = await window.showDirectoryPicker({ mode: 'read' })
        // showDirectoryPicker gives a handle, not a path — show name as confirmation
        // When Tauri is added this will be replaced by a native dialog returning the real path
        onChange(handle.name)
        setDraft(handle.name)
      } catch {
        // user cancelled
      }
    } else {
      setEditing(true)
      setDraft(value)
    }
  }

  return (
    <div className="folder-picker">
      <div className="fp-label">{label}</div>
      {editing ? (
        <div className="fp-edit">
          <input
            className="fp-input"
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => { if (e.key === 'Enter') commit(); if (e.key === 'Escape') setEditing(false) }}
            placeholder="/absolute/path/to/outputs"
            autoFocus
          />
          <button className="btn primary" onClick={commit}>Set</button>
          <button className="btn ghost" onClick={() => setEditing(false)}>Cancel</button>
        </div>
      ) : (
        <div className="fp-row">
          <div className="fp-path" title={value}>
            {value ? (
              <><Icon name="folder" size={14} color="var(--accent)" /><span>{value}</span></>
            ) : (
              <span className="fp-empty">no folder selected</span>
            )}
          </div>
          <button className="btn" onClick={pickNative}>
            <Icon name="folder" size={13} />
            Browse…
          </button>
          <button className="btn ghost" onClick={() => { setEditing(true); setDraft(value) }}>
            <Icon name="fileText" size={13} />
            Type path
          </button>
        </div>
      )}
      {hint && <div className="fp-hint">{hint}</div>}
    </div>
  )
}

// ── Screen 5: First-run indexing ─────────────────────────────────────────────

interface ScreenIndexingProps {
  outputsDir: string
  onFolderChange: (dir: string) => void
}

export function ScreenIndexing({ outputsDir, onFolderChange }: ScreenIndexingProps) {
  const [folders, setFolders] = useState([
    { name: '~/Documents', size: '412 MB', count: '1,840 files', on: true },
    { name: '~/Pictures', size: '8.4 GB', count: '12,304 files', on: true },
    { name: '~/Desktop', size: '21 MB', count: '84 files', on: true },
    { name: '~/Downloads', size: '2.1 GB', count: '612 files', on: false },
    { name: '~/Code', size: '1.1 GB', count: '9,402 files (filtered)', on: true },
  ])

  const toggle = (i: number) =>
    setFolders(prev => prev.map((f, j) => j === i ? { ...f, on: !f.on } : f))

  return (
    <div className="wizard">
      <div className="steps">
        <div className="step is-done"><span className="nu"><Icon name="check" size={9} /></span> Welcome</div>
        <div className="step is-now"><span className="nu">2</span> Choose folders</div>
        <div className="step"><span className="nu">3</span> Indexing</div>
        <div className="step"><span className="nu">4</span> Ready</div>
      </div>

      <h2>What should Chaser look at?</h2>
      <p className="lead">
        Point Chaser at the folder containing your extracted <code>.md</code> files. Sub-folders are included automatically.
      </p>

      <FolderPicker
        label="Source folder (.md output files)"
        value={outputsDir}
        onChange={onFolderChange}
        hint="Chaser searches all .md files in this folder and its sub-folders recursively."
      />

      <div className="idx-grid" style={{ marginTop: 20 }}>
        <div>
          <div className="section-label" style={{ marginBottom: 8 }}>Folders</div>
          <div className="idx-block">
            <ul className="folder-list">
              {folders.map((f, i) => (
                <li key={i}>
                  <span className={`ck${f.on ? ' on' : ''}`} onClick={() => toggle(i)} style={{ cursor: 'pointer' }}>
                    {f.on && <Icon name="check" size={11} stroke={2.5} />}
                  </span>
                  <div>
                    <div className="nm">{f.name}</div>
                    <div className="info">{f.count}</div>
                  </div>
                  <span className="size">{f.size}</span>
                  <button className="btn ghost" style={{ fontSize: 12 }}>exclude</button>
                </li>
              ))}
            </ul>
          </div>
          <button className="btn subtle" style={{ marginTop: 12 }}>
            <Icon name="plus" size={13} />
            Add folder
          </button>
        </div>

        <div>
          <div className="section-label" style={{ marginBottom: 8 }}>Live preview</div>
          <div className="progress-card">
            <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
              <span className="section-label" style={{ margin: 0 }}>Indexing</span>
              <span className="pill accent mono">ready</span>
            </div>
            <div className="big">
              {folders.filter(f => f.on).length * 2000 + 140}
              <span className="unit">files estimated</span>
            </div>
            <div className="progress-bar"><i style={{ width: '0%' }} /></div>
            <div style={{ fontSize: 12, color: 'var(--text-muted)' }}>
              select folders, then click Start indexing
            </div>
          </div>

          <div style={{ marginTop: 14, display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
            <button className="btn ghost">← Back</button>
            <div style={{ display: 'flex', gap: 8 }}>
              <button className="btn subtle">Skip — search live</button>
              <button className="btn primary">
                Start indexing
                <Icon name="arrowRight" size={13} />
              </button>
            </div>
          </div>
        </div>
      </div>
    </div>
  )
}

// ── Screen 6: Settings ───────────────────────────────────────────────────────

const SECTIONS = [
  { id: 'General',   icon: 'monitor' },
  { id: 'Indexing',  icon: 'layers' },
  { id: 'Search',    icon: 'search' },
  { id: 'Privacy',   icon: 'shield' },
  { id: 'Shortcuts', icon: 'keyboard' },
  { id: 'About',     icon: 'info' },
] as const

interface ScreenSettingsProps {
  outputsDir: string
  onFolderChange: (dir: string) => void
  schemaPath: string
  onSchemaPathChange: (p: string) => void
}

export function ScreenSettings({ outputsDir, onFolderChange, schemaPath, onSchemaPathChange }: ScreenSettingsProps) {
  const [active, setActive] = useState<string>('Indexing')

  return (
    <div className="set-shell">
      <aside className="set-sb">
        <div className="section-label" style={{ padding: '0 12px 8px' }}>Settings</div>
        <ul>
          {SECTIONS.map((s) => (
            <li key={s.id} className={s.id === active ? 'on' : ''} onClick={() => setActive(s.id)}>
              <Icon name={s.icon} size={14} />
              {s.id}
            </li>
          ))}
        </ul>
        <div className="set-version">chaser 0.1.0<br />local index · 642 MB</div>
      </aside>

      <div className="set-main">
        {active === 'Indexing'  && <SettingsIndexing outputsDir={outputsDir} onFolderChange={onFolderChange} schemaPath={schemaPath} onSchemaPathChange={onSchemaPathChange} />}
        {active === 'Search'    && <SettingsSearch />}
        {active === 'Privacy'   && <SettingsPrivacy />}
        {active === 'Shortcuts' && <SettingsShortcuts />}
        {active === 'About'     && <SettingsAbout />}
        {active === 'General'   && <SettingsGeneral />}
      </div>
    </div>
  )
}

function SettingsIndexing({ outputsDir, onFolderChange, schemaPath, onSchemaPathChange }: { outputsDir: string; onFolderChange: (d: string) => void; schemaPath: string; onSchemaPathChange: (p: string) => void }) {
  const [auto, setAuto] = useState(true)
  const [ocr, setOcr] = useState(true)
  const [archives, setArchives] = useState(false)

  return (
    <>
      <h2>Indexing</h2>
      <p className="lead">What we read, where embeddings live, and how often we re-scan.</p>

      <div className="set-block">
        <h4>Source folder</h4>
        <FolderPicker
          label="Outputs directory (.md files)"
          value={outputsDir}
          onChange={onFolderChange}
          hint="Chaser searches this folder and all sub-folders recursively for .md files."
        />
      </div>

      <div className="set-block">
        <h4>Schema folder</h4>
        <FolderPicker
          label="Schema directory (schema.toml)"
          value={schemaPath}
          onChange={onSchemaPathChange}
          hint="Point to a folder containing schema.toml. Sub-folders are scanned recursively — the first schema.toml found (sorted by path) is used. Leave empty to use the default at ~/.config/pdf-lab/schema.toml."
        />
      </div>

      <div className="set-block">
        <h4>Watched folders</h4>
        {[
          { nm: '~/Documents', meta: '412 MB · 1,840 files', desc: 'Scanned 2 min ago' },
          { nm: '~/Pictures',  meta: '8.4 GB · 12,304 files', desc: 'Scanned 2 min ago' },
        ].map((f) => (
          <div className="row" key={f.nm}>
            <div className="lbl">
              <div className="nm">{f.nm} <span className="muted" style={{ fontWeight: 400 }}>· {f.meta}</span></div>
              <div className="desc">{f.desc}</div>
            </div>
            <div className="ctl">
              <button className="btn ghost">Exclude</button>
              <button className="btn ghost">Remove</button>
            </div>
          </div>
        ))}
        <div className="row">
          <button className="btn subtle">
            <Icon name="plus" size={13} />
            Add folder
          </button>
        </div>
      </div>

      <div className="set-block">
        <h4>How we read files</h4>
        <div className="row">
          <div className="lbl">
            <div className="nm">Auto re-index on change</div>
            <div className="desc">watch the filesystem and update the index in the background</div>
          </div>
          <div className="ctl"><Toggle on={auto} onClick={() => setAuto(!auto)} /></div>
        </div>
        <div className="row">
          <div className="lbl">
            <div className="nm">OCR scanned PDFs &amp; images</div>
            <div className="desc">extracts text from receipts, screenshots, photos of whiteboards…</div>
          </div>
          <div className="ctl"><Toggle on={ocr} onClick={() => setOcr(!ocr)} /></div>
        </div>
        <div className="row">
          <div className="lbl">
            <div className="nm">Open zips / archives</div>
            <div className="desc">slower indexing, larger embedding store</div>
          </div>
          <div className="ctl"><Toggle on={archives} onClick={() => setArchives(!archives)} /></div>
        </div>
      </div>
    </>
  )
}

function SettingsSearch() {
  return (
    <>
      <h2>Search</h2>
      <p className="lead">Tune ranking and what you see in results.</p>
      <div className="set-block">
        <h4>Default behavior</h4>
        <div className="row">
          <div className="lbl">
            <div className="nm">Top-k results</div>
            <div className="desc">how many results to keep per query</div>
          </div>
          <div className="ctl">
            <div className="toggle">
              <button>8</button>
              <button className="on">12</button>
              <button>20</button>
            </div>
          </div>
        </div>
        <div className="row">
          <div className="lbl"><div className="nm">Show relevance scores</div></div>
          <div className="ctl"><Toggle on={true} onClick={() => {}} /></div>
        </div>
        <div className="row">
          <div className="lbl"><div className="nm">"Why this matched" callout</div></div>
          <div className="ctl"><Toggle on={true} onClick={() => {}} /></div>
        </div>
      </div>
    </>
  )
}

function SettingsPrivacy() {
  return (
    <>
      <h2>Privacy</h2>
      <p className="lead">
        Chaser runs entirely on this machine. No queries or files leave the device unless you turn something on below.
      </p>
      <div className="set-block">
        <h4>Local-only mode</h4>
        <div className="row">
          <div className="lbl">
            <div className="nm">Disable all network features</div>
            <div className="desc">strict local — no telemetry, no model updates, no link previews</div>
          </div>
          <div className="ctl"><Toggle on={true} onClick={() => {}} /></div>
        </div>
      </div>
      <div className="set-block">
        <h4>Index data</h4>
        <div className="row">
          <div className="lbl">
            <div className="nm">Embedding store</div>
            <div className="desc mono">~/Library/Application Support/chaser/index.db · 642 MB</div>
          </div>
          <div className="ctl">
            <button className="btn ghost">Reveal</button>
            <button className="btn">Clear…</button>
          </div>
        </div>
      </div>
    </>
  )
}

function SettingsShortcuts() {
  const rows = [
    ['Open palette anywhere', '⌘ K'],
    ['Focus search', '/'],
    ['List / grid', 'G'],
    ['Open in finder', '⌘ ⇧ R'],
    ['Filters drawer', '⌥ F'],
  ]
  return (
    <>
      <h2>Shortcuts</h2>
      <p className="lead">All keys are remappable.</p>
      <div className="set-block">
        <h4>Default bindings</h4>
        {rows.map(([k, v], i) => (
          <div className="shortcut-row" key={i}>
            <div>{k}</div>
            <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
              <span className="kbd">{v}</span>
              <button className="btn ghost" style={{ fontSize: 12 }}>change</button>
            </div>
          </div>
        ))}
      </div>
    </>
  )
}

function SettingsAbout() {
  return (
    <>
      <h2>About</h2>
      <div className="set-block">
        <div style={{ padding: 24, display: 'flex', gap: 18, alignItems: 'center' }}>
          <div style={{
            width: 64, height: 64, borderRadius: 16,
            background: 'linear-gradient(135deg, var(--accent), var(--accent-dim))',
            display: 'grid', placeItems: 'center', color: '#fff',
            fontSize: 32, fontWeight: 700,
            boxShadow: '0 8px 24px -8px var(--accent)',
          }}>c</div>
          <div>
            <div style={{ fontSize: 22, fontWeight: 600, letterSpacing: '-0.02em' }}>chaser</div>
            <div className="muted" style={{ fontSize: 13 }}>Local search · v0.1.0 · pdf-lab</div>
          </div>
        </div>
      </div>
    </>
  )
}

function SettingsGeneral() {
  return (
    <>
      <h2>General</h2>
      <p className="lead">App-level preferences.</p>
      <div className="set-block">
        <h4>Appearance</h4>
        <div className="row">
          <div className="lbl"><div className="nm">Theme</div></div>
          <div className="ctl">
            <div className="toggle">
              <button>Light</button>
              <button className="on">Dark</button>
              <button>Auto</button>
            </div>
          </div>
        </div>
      </div>
    </>
  )
}
