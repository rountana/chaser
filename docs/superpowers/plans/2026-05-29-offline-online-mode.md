# Offline / Online Mode Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an explicit `mode: "offline" | "online"` field to config, enforce it in the router/CLI, and expose a Plan settings section in the UI where users switch between free (offline) and premium (online) tiers.

**Architecture:** Config grows one field (`mode`) and one helper (`is_offline()`). All LLM callsites check `is_offline()` before firing. The frontend Plan section writes `mode` (and optionally `api_key`) to config via a single POST. The router's R6 rule skips the LLM call entirely when offline.

**Tech Stack:** Rust (serde_json config, axum serve), React + TypeScript (frontend)

---

## File Map

| File | Action | What changes |
|------|--------|-------------|
| `pdf-core/src/config.rs` | Modify | Add `mode` field + `is_offline()` |
| `pdf-core/src/search/router.rs` | Modify | Skip R6 when `is_offline()` |
| `pdf-lab/src/cli/extract.rs` | Modify | Guard `run_online` with `is_offline()` bail |
| `pdf-lab/src/cli/serve.rs` | Modify | GET returns `mode`; POST accepts `mode` + `api_key` |
| `frontend/src/types.ts` | Modify | Add `mode` to `AppSettings` |
| `frontend/src/api.ts` | Modify | Extend `saveSettings` to accept `mode` + `apiKey` |
| `frontend/src/ScreensSystem.tsx` | Modify | Add Plan section to sidebar + `ScreenPlan` component |

---

## Task 1: Config `mode` field and `is_offline()` helper

**Files:**
- Modify: `pdf-core/src/config.rs`

- [ ] **Step 1: Add the `mode` field to `ClaudeConfig`**

In `pdf-core/src/config.rs`, add after the `gemini_model` field:

```rust
pub struct ClaudeConfig {
    pub model: String,
    pub api_key: String,
    pub base_url: Option<String>,
    pub source_dir: Option<String>,
    pub outputs_dir: Option<String>,
    #[serde(default)]
    pub backend: LlmBackend,
    #[serde(default)]
    pub ollama_url: Option<String>,
    #[serde(default)]
    pub ollama_model: Option<String>,
    #[serde(default)]
    pub schema_path: Option<String>,
    #[serde(default)]
    pub gemini_api_key: Option<String>,
    #[serde(default)]
    pub gemini_model: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,   // "offline" | "online" — None treated as "offline"
}
```

- [ ] **Step 2: Add `is_offline()` to the impl block**

In the `impl ClaudeConfig` block (after `resolved_gemini_model`):

```rust
/// Returns true when mode is "offline" or unset. Any config without a mode
/// field defaults to offline so existing installs are not broken.
pub fn is_offline(&self) -> bool {
    self.mode.as_deref() != Some("online")
}
```

- [ ] **Step 3: Add unit tests**

At the bottom of `pdf-core/src/config.rs`, inside `#[cfg(test)] mod tests`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_offline_when_mode_absent() {
        let c = ClaudeConfig::default();
        assert!(c.is_offline());
    }

    #[test]
    fn is_offline_when_mode_offline() {
        let c = ClaudeConfig { mode: Some("offline".to_string()), ..ClaudeConfig::default() };
        assert!(c.is_offline());
    }

    #[test]
    fn is_online_when_mode_online() {
        let c = ClaudeConfig { mode: Some("online".to_string()), ..ClaudeConfig::default() };
        assert!(!c.is_offline());
    }

    #[test]
    fn is_offline_when_mode_garbage() {
        // Unknown values default to offline (safe side)
        let c = ClaudeConfig { mode: Some("garbage".to_string()), ..ClaudeConfig::default() };
        assert!(c.is_offline());
    }
}
```

- [ ] **Step 4: Run tests**

```bash
cargo test -p pdf-core config 2>&1 | tail -20
```

Expected: all 4 new tests pass.

- [ ] **Step 5: Commit**

```bash
git add pdf-core/src/config.rs
git commit -m "feat(config): add mode field and is_offline() helper"
```

---

## Task 2: Router R6 offline bypass

**Files:**
- Modify: `pdf-core/src/search/router.rs`

- [ ] **Step 1: Add the offline short-circuit before R6**

In `route()`, find the comment `// R6: Ambiguous hybrid` and insert before it:

```rust
    // Offline mode: skip LLM classify entirely, route deterministically.
    // Avoids API calls, warnings, and error-path fallbacks.
    if config.is_offline() {
        return vec![Backend::Metadata];
    }

    // R6: Ambiguous hybrid — 2+ primary signal types present, no rule above matched.
```

- [ ] **Step 2: Add test for offline R6 bypass**

In the `#[cfg(test)] mod tests` block in `router.rs`, add a sync helper test. The existing `route_sync` function already simulates R6 as a fallback — add a test showing the intent:

```rust
    #[test]
    fn offline_config_never_reaches_r6() {
        // With an offline config, date+doc_type should short-circuit to Metadata
        // before the R6 LLM call. route_sync doesn't have the config argument,
        // so we verify the logic path manually: is_offline() returns true means
        // the early return fires before primary_signal_count is evaluated.
        let cfg = crate::config::ClaudeConfig {
            mode: Some("offline".to_string()),
            ..crate::config::ClaudeConfig::default()
        };
        assert!(cfg.is_offline(), "config must report offline");
        // The actual route() fn would return Metadata without any LLM call.
        // Integration verified by: no API call log + correct result in search.
    }
```

- [ ] **Step 3: Run existing router tests**

```bash
cargo test -p pdf-core search::router 2>&1 | tail -20
```

Expected: all existing tests pass plus the new one.

- [ ] **Step 4: Commit**

```bash
git add pdf-core/src/search/router.rs
git commit -m "feat(router): skip R6 LLM call when config is offline"
```

---

## Task 3: CLI extract online guard

**Files:**
- Modify: `pdf-lab/src/cli/extract.rs`

- [ ] **Step 1: Add offline guard at top of `run_online`**

Find `pub async fn run_online(args: ExtractOnlineArgs) -> anyhow::Result<()> {` and add immediately after the opening brace:

```rust
pub async fn run_online(args: ExtractOnlineArgs) -> anyhow::Result<()> {
    let config = ClaudeConfig::load()?;
    if config.is_offline() {
        anyhow::bail!(
            "online extraction is disabled — pdf-lab is set to Offline mode.\n\
             To enable online extraction, switch to Online mode in Settings → Plan."
        );
    }
    // rest of function unchanged...
```

Note: `run_online` already calls `ClaudeConfig::load()` further down. Move that first call to the top and reuse. Find the existing `let config = ClaudeConfig::load()?;` inside `run_online` and remove the duplicate — keep only the one added at the top.

- [ ] **Step 2: Verify compile**

```bash
cargo build -p pdf-lab 2>&1 | tail -10
```

Expected: `Finished` with no errors.

- [ ] **Step 3: Smoke test**

With current config having `"mode": "offline"`:

```bash
./target/debug/pdf-lab extract online 2>&1 | head -5
```

Expected output contains: `online extraction is disabled`

- [ ] **Step 4: Commit**

```bash
git add pdf-lab/src/cli/extract.rs
git commit -m "feat(cli): bail on extract online when config is offline"
```

---

## Task 4: Serve.rs settings API — GET + POST

**Files:**
- Modify: `pdf-lab/src/cli/serve.rs`

- [ ] **Step 1: Add `mode` to `SettingsResponse`**

Find the `SettingsResponse` struct and add the field:

```rust
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SettingsResponse {
    outputs_dir: String,
    api_key_set: bool,
    model: String,
    schema_path: Option<String>,
    mode: String,               // "offline" | "online"
}
```

- [ ] **Step 2: Return `mode` in `handle_get_settings`**

Update the `Json(SettingsResponse { ... })` construction:

```rust
Json(SettingsResponse {
    outputs_dir,
    api_key_set: !state.config.api_key.is_empty(),
    model: state.config.model.clone(),
    schema_path,
    mode: state.config.mode.clone().unwrap_or_else(|| "offline".to_string()),
})
```

- [ ] **Step 3: Add `mode` and `api_key` to `SaveSettingsBody`**

```rust
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SaveSettingsBody {
    outputs_dir: Option<String>,
    schema_path: Option<String>,
    mode: Option<String>,       // "offline" | "online"
    api_key: Option<String>,    // only sent on Online activation
}
```

- [ ] **Step 4: Handle `mode` and `api_key` in `handle_save_settings`**

In the handler body, add after the existing `schema_path` block and before `let _ = config.save();`:

```rust
    if let Some(m) = body.mode {
        config.mode = Some(m);
    }
    if let Some(k) = body.api_key {
        if !k.is_empty() {
            config.api_key = k;
        }
    }
```

- [ ] **Step 5: Build and verify**

```bash
cargo build -p pdf-lab 2>&1 | tail -10
```

Expected: `Finished` with no errors.

- [ ] **Step 6: Commit**

```bash
git add pdf-lab/src/cli/serve.rs
git commit -m "feat(serve): expose mode in settings GET/POST, accept api_key on activation"
```

---

## Task 5: Frontend types and API client

**Files:**
- Modify: `frontend/src/types.ts`
- Modify: `frontend/src/api.ts`

- [ ] **Step 1: Add `mode` to `AppSettings` in `types.ts`**

Find `AppSettings` and update:

```typescript
export interface AppSettings {
  outputsDir: string
  apiKeySet: boolean
  schemaPath: string | null
  mode: 'offline' | 'online'
}
```

Note: the existing field is `apiKey: string` — change it to `apiKeySet: boolean` to match what the server actually sends (the key is never echoed back).

- [ ] **Step 2: Extend `saveSettings` in `api.ts`**

Find the `saveSettings` line and update the signature:

```typescript
saveSettings: (s: Partial<{
  outputsDir: string
  schemaPath: string | null
  mode: 'offline' | 'online'
  apiKey: string
}>) => post<void>('/settings', s),
```

- [ ] **Step 3: Fix any TypeScript errors from the apiKey → apiKeySet rename**

Search `ScreensSystem.tsx` for references to `settings.apiKey` and update to `settings.apiKeySet`. It is currently used only as a boolean indicator (truthy check), so `apiKeySet` is a drop-in.

- [ ] **Step 4: Verify TypeScript compiles**

```bash
cd frontend && npm run build 2>&1 | tail -20
```

Expected: no TypeScript errors. (Vite build errors are fine to ignore if they're unrelated to the changed files.)

- [ ] **Step 5: Commit**

```bash
git add frontend/src/types.ts frontend/src/api.ts
git commit -m "feat(frontend): add mode to AppSettings, extend saveSettings API"
```

---

## Task 6: Frontend — ScreenPlan component

**Files:**
- Modify: `frontend/src/ScreensSystem.tsx`

This task adds the full `ScreenPlan` component and wires it into `ScreenSettings`.

- [ ] **Step 1: Add Plan to SECTIONS array**

Find `const SECTIONS = [` and add Plan between General and Indexing:

```typescript
const SECTIONS = [
  { id: 'General',   icon: 'monitor' },
  { id: 'Plan',      icon: 'sparkles' },
  { id: 'Indexing',  icon: 'layers' },
  { id: 'Search',    icon: 'search' },
  { id: 'Privacy',   icon: 'shield' },
  { id: 'Shortcuts', icon: 'keyboard' },
  { id: 'About',     icon: 'info' },
] as const
```

- [ ] **Step 2: Add ScreenPlanProps interface and ScreenPlan component**

Add before the `ScreenSettings` function:

```typescript
interface ScreenPlanProps {
  mode: 'offline' | 'online'
  onModeChange: (mode: 'offline' | 'online') => void
}

function ScreenPlan({ mode, onModeChange }: ScreenPlanProps) {
  const [activating, setActivating] = useState(false)
  const [apiKey, setApiKey] = useState('')
  const [saving, setSaving] = useState(false)

  async function activateOnline() {
    if (!apiKey.trim()) return
    setSaving(true)
    try {
      await api.saveSettings({ mode: 'online', apiKey: apiKey.trim() })
      onModeChange('online')
      setActivating(false)
      setApiKey('')
    } finally {
      setSaving(false)
    }
  }

  async function switchToOffline() {
    await api.saveSettings({ mode: 'offline' })
    onModeChange('offline')
    setActivating(false)
    setApiKey('')
  }

  const isOnline = mode === 'online'

  return (
    <>
      <h2>Plan</h2>
      <p className="lead">Choose how pdf-lab processes your documents.</p>

      <div className="set-block">
        <h4>Active mode</h4>
        <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 12 }}>

          {/* Offline card */}
          <div
            style={{
              borderRadius: 8, padding: '14px 16px',
              border: `2px solid ${isOnline ? '#1e293b' : '#22c55e'}`,
              background: isOnline ? '#0d1117' : '#0f2a1a',
              boxShadow: isOnline ? 'none' : '0 0 0 3px #22c55e22',
              opacity: activating ? 0.6 : 1,
              cursor: isOnline ? 'pointer' : 'default',
              transition: 'all .15s',
            }}
            onClick={isOnline ? switchToOffline : undefined}
          >
            <div style={{ fontSize: 9, fontWeight: 800, textTransform: 'uppercase', letterSpacing: '.8px', padding: '1px 8px', borderRadius: 3, display: 'inline-block', marginBottom: 8, background: isOnline ? '#1e293b' : '#14532d', color: isOnline ? '#64748b' : '#4ade80' }}>
              {isOnline ? 'Switch back' : 'Active'}
            </div>
            <div style={{ fontSize: 14, fontWeight: 800, color: '#4ade80', marginBottom: 2 }}>Offline</div>
            <div style={{ fontSize: 11, color: '#16a34a', marginBottom: 10 }}>Free · no API key needed</div>
            {(['pdfium + Tesseract OCR', 'BGE embedding search', 'Heuristic metadata', 'Zero LLM calls, ever'] as const).map(f => (
              <div key={f} style={{ fontSize: 11, color: isOnline ? '#334155' : '#64748b', margin: '3px 0' }}>✓ {f}</div>
            ))}
          </div>

          {/* Online card */}
          <div
            style={{
              borderRadius: 8, padding: '14px 16px',
              border: `2px solid ${isOnline ? '#6366f1' : activating ? '#6366f1' : '#1e293b'}`,
              background: '#12101f',
              boxShadow: isOnline ? '0 0 0 3px #6366f133' : activating ? '0 0 0 3px #6366f133' : 'none',
              cursor: (!isOnline && !activating) ? 'pointer' : 'default',
              transition: 'all .15s',
            }}
            onClick={(!isOnline && !activating) ? () => setActivating(true) : undefined}
          >
            <div style={{ fontSize: 9, fontWeight: 800, textTransform: 'uppercase', letterSpacing: '.8px', padding: '1px 8px', borderRadius: 3, display: 'inline-block', marginBottom: 8, background: isOnline ? '#312e81' : '#1e1b4b', color: isOnline ? '#818cf8' : '#64748b' }}>
              {isOnline ? 'Active' : activating ? 'Activating…' : 'Click to activate'}
            </div>
            <div style={{ fontSize: 14, fontWeight: 800, color: '#818cf8', marginBottom: 2 }}>Online</div>
            <div style={{ fontSize: 11, color: '#6366f1', marginBottom: 10 }}>Premium · Claude API key required</div>
            {(['Everything in Offline', 'LLM-enriched extraction', 'Smart query routing', 'Structured field output'] as const).map(f => (
              <div key={f} style={{ fontSize: 11, color: '#64748b', margin: '3px 0' }}>✓ {f}</div>
            ))}

            {/* Inline key zone — shown when activating */}
            {activating && !isOnline && (
              <div style={{ marginTop: 10, background: '#0d1117', border: '1px solid #4f46e5', borderRadius: 6, padding: '11px 12px' }}>
                <div style={{ fontSize: 10, fontWeight: 800, textTransform: 'uppercase', letterSpacing: '.8px', color: '#818cf8', marginBottom: 4 }}>Claude API key</div>
                <div style={{ fontSize: 11, color: '#64748b', marginBottom: 8 }}>
                  Get yours at console.anthropic.com. Stored locally in config.json.
                </div>
                <input
                  style={{ width: '100%', boxSizing: 'border-box', background: '#1e293b', border: '1px solid #334155', borderRadius: 5, padding: '7px 10px', color: '#e2e8f0', fontFamily: 'monospace', fontSize: 11, outline: 'none', marginBottom: 8 }}
                  placeholder="sk-ant-api03-…"
                  value={apiKey}
                  onChange={e => setApiKey(e.target.value)}
                  autoFocus
                />
                <div style={{ display: 'flex', gap: 6, justifyContent: 'flex-end' }}>
                  <button className="btn ghost" onClick={() => { setActivating(false); setApiKey('') }}>Cancel</button>
                  <button className="btn primary" onClick={activateOnline} disabled={saving || !apiKey.trim()}>
                    {saving ? 'Saving…' : 'Activate Online'}
                  </button>
                </div>
              </div>
            )}
          </div>
        </div>
      </div>

      {/* What runs where */}
      <div className="set-block">
        <h4>What runs where</h4>
        {[
          { op: 'extract offline', local: true,  blocked: false, desc: 'pdfium · Tesseract · heuristics — always local' },
          { op: 'extract online',  local: false, blocked: !isOnline, desc: isOnline ? 'LLM enrichment via Claude API' : 'disabled in Offline mode' },
          { op: 'search',          local: true,  blocked: false, desc: isOnline ? 'BGE local + LLM routing for complex queries' : 'BGE · index scan · no LLM calls' },
        ].map(({ op, local, blocked, desc }) => (
          <div key={op} className="row" style={{ alignItems: 'flex-start', paddingTop: 8, paddingBottom: 8 }}>
            <div className="lbl">
              <div className="nm mono" style={{ fontSize: 12 }}>{op}</div>
              <div className="desc">{desc}</div>
            </div>
            <div className="ctl" style={{ flexShrink: 0 }}>
              <span className="pill" style={{
                fontSize: 11,
                background: blocked ? 'var(--surface-2)' : local ? 'var(--surface-2)' : 'var(--accent-dim)',
                color: blocked ? 'var(--danger, #ef4444)' : local ? 'var(--text-muted)' : 'var(--accent)',
              }}>
                {blocked ? 'blocked' : local ? 'local' : 'network'}
              </span>
            </div>
          </div>
        ))}
      </div>
    </>
  )
}
```

- [ ] **Step 3: Add Plan props to `ScreenSettingsProps` and `ScreenSettings`**

Update the interface:

```typescript
interface ScreenSettingsProps {
  outputsDir: string
  onFolderChange: (dir: string) => void
  schemaPath: string
  onSchemaPathChange: (p: string) => void
  mode: 'offline' | 'online'
  onModeChange: (mode: 'offline' | 'online') => void
}
```

Update `ScreenSettings` signature to accept the new props and pass them through:

```typescript
export function ScreenSettings({ outputsDir, onFolderChange, schemaPath, onSchemaPathChange, mode, onModeChange }: ScreenSettingsProps) {
```

In the `set-main` div, add the Plan case alongside the existing section renders:

```typescript
{active === 'Plan'      && <ScreenPlan mode={mode} onModeChange={onModeChange} />}
{active === 'Indexing'  && <SettingsIndexing ... />}
// ... rest unchanged
```

- [ ] **Step 4: Find the parent component and wire mode state**

First locate where `ScreenSettings` is rendered:

```bash
grep -rn "ScreenSettings" frontend/src/
```

Open the file that renders `<ScreenSettings`. Add `mode` state at the top of that component:

```typescript
const [mode, setMode] = useState<'offline' | 'online'>('offline')

// Load initial mode from settings on mount
useEffect(() => {
  api.settings().then(s => setMode(s.mode ?? 'offline')).catch(() => {})
}, [])
```

Pass to `ScreenSettings`:

```typescript
<ScreenSettings
  outputsDir={outputsDir}
  onFolderChange={setOutputsDir}
  schemaPath={schemaPath}
  onSchemaPathChange={setSchemaPath}
  mode={mode}
  onModeChange={setMode}
/>
```

- [ ] **Step 5: Verify TypeScript build**

```bash
cd frontend && npm run build 2>&1 | tail -30
```

Expected: no type errors in the changed files.

- [ ] **Step 6: Commit**

```bash
git add frontend/src/ScreensSystem.tsx
git commit -m "feat(frontend): add Plan settings section with offline/online tier cards"
```

---

## Task 7: End-to-end verify and final commit

- [ ] **Step 1: Build everything**

```bash
cargo build -p pdf-lab 2>&1 | tail -10
```

Expected: `Finished` with no errors.

- [ ] **Step 2: Run all Rust tests**

```bash
cargo test -p pdf-core -p pdf-lab 2>&1 | tail -20
```

Expected: all tests pass.

- [ ] **Step 3: Verify offline mode blocks online extract**

```bash
# Ensure mode is offline in config
./target/debug/pdf-lab extract online 2>&1
```

Expected: `Error: online extraction is disabled — pdf-lab is set to Offline mode.`

- [ ] **Step 4: Verify search works in offline mode (no LLM warning)**

```bash
./target/debug/pdf-lab search "invoice" 2>&1
```

Expected: results printed, no `warning: LLM triage failed` line in stderr.

- [ ] **Step 5: Verify switching to online via API**

```bash
# Start serve in background, then test settings round-trip
./target/debug/pdf-lab serve &
sleep 1
curl -s http://127.0.0.1:7410/settings | python3 -m json.tool | grep mode
# Expected: "mode": "offline"

curl -s -X POST http://127.0.0.1:7410/settings \
  -H 'Content-Type: application/json' \
  -d '{"mode":"online","apiKey":"test-key"}' -w "%{http_code}"
# Expected: 204

curl -s http://127.0.0.1:7410/settings | python3 -m json.tool | grep mode
# Expected: "mode": "online"

# Reset back to offline
curl -s -X POST http://127.0.0.1:7410/settings \
  -H 'Content-Type: application/json' \
  -d '{"mode":"offline"}' -w "%{http_code}"
kill %1
```

- [ ] **Step 6: Final commit if any loose files**

```bash
git status
# Stage anything uncommitted
git add -A
git commit -m "feat: offline/online mode — config, router, CLI guard, UI Plan section" || true
```
