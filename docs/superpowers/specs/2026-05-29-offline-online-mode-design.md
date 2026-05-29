# Offline / Online Mode — Design Spec

**Date:** 2026-05-29  
**Status:** Approved

---

## Context

pdf-lab has two extraction pipelines:

- **Offline** — pdfium + Tesseract + BGE embeddings. Runs entirely on CPU. No LLM, no network.
- **Online** — everything in offline, plus LLM enrichment (Claude / Gemini / Ollama) for structured metadata extraction.

Currently the system has no explicit mode flag. The router's R6 rule tries to call the Claude API for ambiguous hybrid queries, then silently falls back when no key is set — printing a warning on every search. `pdf-lab extract online` has no guard against running in a non-LLM context.

The goal: make offline the default first-class mode, expose the choice in the UI as Free vs Premium, and write the selected mode to config so every CLI command and the serve API respect it without making unintended LLM calls.

---

## Definitions

| Term | Meaning |
|------|---------|
| **Offline mode** | CPU libraries only: fastembed (BGE ONNX), Tesseract, pdfium, regex. No LLM of any kind — not Claude, not Gemini, not Ollama. |
| **Online mode** | Everything in offline, plus LLM calls via the configured backend (Claude / Gemini / Ollama). |

Ollama is **not** offline — it runs a model and is treated as an LLM backend.

---

## Config Schema Change

Add one field to `ClaudeConfig` (`pdf-core/src/config.rs`):

```rust
#[serde(default)]
pub mode: Option<String>,  // "offline" | "online" — None treated as "offline"
```

Add one helper:

```rust
pub fn is_offline(&self) -> bool {
    self.mode.as_deref() != Some("online")
}
```

**Defaults:** `None` → offline. Existing configs without the field remain in offline mode automatically.

**Written values:**
```json
{ "mode": "offline" }   // free tier
{ "mode": "online"  }   // premium, api_key must also be set
```

---

## Frontend — Plan Section

### Settings sidebar

Add `Plan` between `General` and `Indexing` in the `SECTIONS` array in `ScreensSystem.tsx`.

### ScreenPlan component — two states

**State 1 — Offline active (default)**

Two tier cards side by side. Offline card has a green active ring. Online card is dim with a "Click to activate" label.

Below the cards: a "What runs where" table showing:
- `extract offline` → local
- `extract online` → blocked  
- `search` → local (BGE + index scan, no routing LLM)

**State 2 — User clicks Online card**

Offline card dims to 60% opacity. Online card gets a purple active ring and expands a key zone below the feature list:

```
Claude API key
Get yours at console.anthropic.com. Stored locally in config.json.
[ sk-ant-api03-…                    ] [Cancel] [Activate Online]
```

Clicking **Cancel** collapses the key zone; Offline stays active.  
Clicking **Activate Online** fires one POST `/settings` with `{ mode: "online", api_key: "..." }`.

After activation, "What runs where" updates: `extract online` → network, `search` → local+.

### Types and API

```typescript
// types.ts
interface AppSettings {
  outputsDir: string
  apiKeySet: boolean       // boolean — key is never echoed back
  schemaPath: string | null
  mode: 'offline' | 'online'
}

// api.ts
saveSettings: (s: {
  outputsDir?: string
  schemaPath?: string
  mode?: string
  apiKey?: string          // only sent on Online activation
}) => post<void>('/settings', s)
```

---

## Backend — serve.rs

### GET `/settings`

Add `mode` to `SettingsResponse`:

```rust
struct SettingsResponse {
    outputs_dir: String,
    api_key_set: bool,
    model: String,
    schema_path: Option<String>,
    mode: String,           // "offline" | "online"
}
```

Return `config.mode.clone().unwrap_or_else(|| "offline".to_string())`.

### POST `/settings`

Extend `SaveSettingsBody`:

```rust
struct SaveSettingsBody {
    outputs_dir: Option<String>,
    schema_path: Option<String>,
    mode: Option<String>,       // new
    api_key: Option<String>,    // new — only sent on Online activation
}
```

Handler additions:

```rust
if let Some(m) = body.mode    { config.mode    = Some(m); }
if let Some(k) = body.api_key { config.api_key = k; }
```

Both fields are optional — existing callers sending only `outputs_dir` or `schema_path` are unaffected.

---

## CLI — Extract Guard

`pdf-lab/src/cli/extract.rs` — top of `run_online`:

```rust
pub async fn run_online(args: ExtractOnlineArgs) -> anyhow::Result<()> {
    let config = ClaudeConfig::load()?;
    if config.is_offline() {
        anyhow::bail!(
            "online extraction is disabled in Offline mode.\n\
             Switch to Online mode in Settings → Plan to enable it."
        );
    }
    // rest unchanged
}
```

`pdf-lab extract offline` is unaffected — it never calls `is_offline()`.

---

## Search — Router Fix (Fix 2a)

`pdf-core/src/search/router.rs` — before the R6 block:

```rust
// Offline mode: skip LLM classify entirely — route deterministically
if config.is_offline() {
    return vec![Backend::Metadata];
}

if primary_signal_count >= 2 {
    match classify::classify_backends(...).await { ... }
}
```

**Before:** R6 fires, `classify_backends` bails for Ollama/no-key, logs a warning, returns Metadata.  
**After:** `is_offline()` short-circuits before any network attempt. No warning, no error, same result.

The router function signature already takes `config: &ClaudeConfig` — no signature change needed.

---

## Files Changed

| File | Change |
|------|--------|
| `pdf-core/src/config.rs` | Add `mode: Option<String>` field + `is_offline()` method |
| `pdf-lab/src/cli/serve.rs` | GET `/settings` returns `mode`; POST `/settings` accepts `mode` + `api_key` |
| `pdf-lab/src/cli/extract.rs` | Guard `run_online` with `is_offline()` bail |
| `pdf-core/src/search/router.rs` | Skip R6 when `is_offline()` |
| `frontend/src/ScreensSystem.tsx` | Add `Plan` to sidebar; add `ScreenPlan` component with tier cards + inline key zone |
| `frontend/src/types.ts` | Add `mode: 'offline' \| 'online'` to `AppSettings` |
| `frontend/src/api.ts` | Extend `saveSettings` to accept `mode` and `apiKey` |

---

## What This Does Not Include

- Fix 2b (intent.rs exact-match + stemming for doc_type extraction) — separate task
- Semantic search backend (future)
- API key validation on entry (validate on first actual LLM call, not at save time)
- Per-operation mode override (e.g., force online for one search) — not needed now
