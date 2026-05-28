# pdf-lab

Local document extraction and search — extracts PDFs and images into structured Markdown using an LLM backend (Claude, Gemini, or Ollama), then lets you search them via a dark-themed web UI or the CLI.

## Overview

`pdf-lab` processes your documents in two steps:

1. **Extract** — renders each PDF page with pdfium (text layer, or PNG at 300 DPI for scanned pages), then calls the configured LLM backend to produce structured output: page text + YAML metadata (person, doc type, date, institution). Results are written as `.md` files in an `outputs/` directory.

2. **Search** — reads the `.md` files at query time. Metadata queries (person names, document types, dates) hit an in-memory index; keyword queries use ripgrep-style search. Sub-folders are searched recursively.

No database. No embedding server. The only network call is to the configured LLM during extraction.

## Quick start

```bash
# 1. Build and install the CLI
cargo install --path pdf-lab --force

# 2. Configure
pdf-lab config set --api-key sk-ant-...
pdf-lab config set --backend claude          # claude | gemini | ollama
pdf-lab config set --outputs-dir ~/Documents/pdf-lab-outputs

# 3. Extract some documents
pdf-lab extract agreement.pdf receipt.jpg

# 4. Search from the CLI
pdf-lab search "stamp duty"

# 5. Or open the web UI  ↓ see section below
```

## Running the web UI

The UI is a Vite + React app in `frontend/`. It connects to a local HTTP server (`pdf-lab serve`) for search.

### Prerequisites

- Node.js 18+ and npm

### Start the backend API server

```bash
# From the workspace root:
pdf-lab serve --outputs-dir /path/to/your/outputs

# Or use the configured default (set via `pdf-lab config set --outputs-dir ...`):
pdf-lab serve
```

The server starts on `http://127.0.0.1:7410`. Keep this terminal open.

### Start the frontend dev server

```bash
cd frontend
npm install        # first time only
npm run dev
```

Open **http://localhost:5173** in your browser.

The frontend proxies `/api/*` → `http://127.0.0.1:7410` automatically. If the backend is not running, the UI falls back to mock data so you can still browse the design.

### Configure the source folder in the UI

Once open, go to **Settings → Indexing** and set the outputs directory path. This persists across sessions (saved in `localStorage` and synced to the server config).

### Build for production

```bash
cd frontend
npm run build
# Output in frontend/dist/ — serve with any static file server
```

## Installation

### Prerequisites

- Rust 1.78+ (`rustup` recommended)
- `libpdfium` shared library — required for PDF rendering. Place `libpdfium.dylib` (macOS), `pdfium.dll` (Windows), or `libpdfium.so` (Linux) alongside the binary or in a system library path.
- An API key for your chosen backend (Anthropic, Google Gemini, or a running Ollama instance)

### Build from source

```bash
cargo build --release
# binary at: target/release/pdf-lab
```

### Install the CLI globally

```bash
cargo install --path pdf-lab --force
```

Run this after any code change to update `~/.cargo/bin/pdf-lab`.

## Setup

```bash
# Source and output directories
pdf-lab config set --source-dir ~/Documents/pdfs
pdf-lab config set --outputs-dir ~/Documents/pdf-lab-outputs

# Verify
pdf-lab config get
pdf-lab config test      # verify API key + measure latency
```

Config is stored at `config/pdf-lab/config.json` relative to the directory where you run `pdf-lab`.

### Choose a backend

All LLM calls — extraction, classification, and search routing — use the single backend set in config. There are no per-command flags for switching backends.

```bash
pdf-lab config set --backend claude    # Anthropic Claude (default)
pdf-lab config set --backend gemini    # Google Gemini
pdf-lab config set --backend ollama    # Local Ollama
```

**Claude** (default):
```bash
pdf-lab config set --api-key sk-ant-...
# Model is fixed to claude-haiku-4-5-20251001
pdf-lab config test
```

**Gemini**:
```bash
pdf-lab config set --backend gemini
pdf-lab config set --gemini-api-key AIza...
pdf-lab config set --gemini-model gemini-2.0-flash   # optional, this is the default
pdf-lab config test --gemini
```

**Ollama** (local, no API key needed):
```bash
pdf-lab config set --backend ollama
pdf-lab config set --ollama-url http://localhost:11434   # optional, this is the default
pdf-lab config set --ollama-model qwen3.5:9b            # optional, this is the default
pdf-lab config test --local
```

> Note: when using Ollama as the backend, the search query router (R6 — ambiguous hybrid queries) falls back to keyword search, since Ollama does not support the structured tool-calling interface used for query classification.

## CLI usage

### Extract documents

```bash
# Extract specific files
pdf-lab extract agreement_to_sell.pdf Hema_PAN.jpg Receipt_30_10_2025.pdf

# Extract everything in source_dir
pdf-lab extract

pdf-lab extract <relative source path>
```


Options:
```
pdf-lab extract [paths...] [--source-dir DIR] [--outputs-dir DIR] [--json] [--auto-schema]
```

The backend used for extraction is whatever is configured via `pdf-lab config set --backend`. No per-run flags.

JSON lines output (with `--json`):
```json
{"event":"started","file":"agreement_to_sell.pdf"}
{"event":"complete","file":"agreement_to_sell.pdf","output":"outputs/agreement_to_sell.md","chars_extracted":2341}
{"event":"error","file":"bad.pdf","message":"Claude API error: 429 rate limit"}
```

### Search

```bash
pdf-lab search "Hema's PAN"
pdf-lab search "receipts from October 2025"
pdf-lab search "stamp duty"
```

Options:
```
pdf-lab search <query> [--top N] [--outputs-dir DIR] [--json]
```

- `--top N` — number of results (default: 5)
- `--outputs-dir` — where to find `.md` files (searched recursively; default: `./outputs`)

Human-readable output:
```
[Hema_PAN.md]  (metadata)
  Person: Hema | Type: pan | Date: —
  "PAN CARD  Income Tax Department..."

[agreement_to_sell.md] — Page 2  (keyword)
  Keyword: duty
  "...The stamp duty payable on this agreement shall be..."
```

### HTTP API server

```bash
pdf-lab serve [--port 7410] [--outputs-dir DIR]
```

Starts a local HTTP server used by the web UI. Endpoints:

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/search?q=<query>&top=<n>&outputs_dir=<path>` | Run search, returns JSON array |
| `GET` | `/settings` | Current `outputs_dir`, model, API key status |
| `POST` | `/settings` | Update `outputs_dir` (persisted to config file) |
| `GET` | `/index/status` | File count and index size for `outputs_dir` |

### MCP server

```bash
pdf-lab mcp
```

Starts a stdio JSON-RPC 2.0 MCP server. Exposes three tools:

**`extract_document`** — extract text and metadata from a PDF or image:
```json
{ "file_path": "/path/to/file.pdf", "file_type": "pdf" }
```

**`ocr_scan`** — run local Tesseract OCR on an image:
```json
{ "file_path": "/path/to/image.png" }
```

**`classify_query`** — classify a search query to backends using the configured LLM:
```json
{ "query": "Hema's invoices", "known_persons": ["Hema"], "known_doc_types": ["invoice"] }
```

**Claude Desktop config** (`~/Library/Application Support/Claude/claude_desktop_config.json`):
```json
{
  "mcpServers": {
    "pdf-lab": {
      "command": "/path/to/pdf-lab",
      "args": ["mcp"]
    }
  }
}
```

## Output format

Every extracted file is a Markdown file with a YAML frontmatter block:

```markdown
---
title: AGREEMENT - Shyam
person: Shyam
doc_type: agreement
date: 2025-10-30
institution: ""
source_file: /path/to/agreement_to_sell.pdf
pages: 4
ocr_method: llm-vision
extracted_at: 2026-05-22T10:00:00Z
---
[Page 1]
THIS AGREEMENT TO SELL...

[Page 2]
...
```

Supported `doc_type` values: `pan` · `aadhaar` · `passport` · `cheque` · `receipt` · `agreement` · `deed` · `khata` · `oc` · `ecc` · `layout` · `unknown`

## Project structure

```
pdf-lab/claude-sdk/
├── Cargo.toml                   (workspace)
├── config/pdf-lab/
│   ├── config.json              (runtime config — created by `pdf-lab config set`)
│   └── schema.toml              (custom field schema)
├── frontend/                    (Vite + React web UI)
│   ├── src/
│   │   ├── App.tsx              (app shell, routing, outputsDir state)
│   │   ├── ScreensSearch.tsx    (search palette, results, preview)
│   │   ├── ScreensSystem.tsx    (indexing wizard, settings + folder picker)
│   │   ├── icons.tsx            (inline SVG icon library)
│   │   ├── api.ts               (fetch wrapper → /api/*)
│   │   ├── types.ts             (Rust ↔ UI data shapes + adapter)
│   │   └── app.css              (dark design system tokens + components)
│   └── vite.config.ts           (proxy /api → localhost:7410)
├── pdf-core/                    (shared library crate)
│   └── src/
│       ├── config.rs            (ClaudeConfig, LlmBackend)
│       ├── extraction/          (pdfium rendering + LLM backends)
│       ├── frontmatter/         (YAML generation + filename fallback)
│       └── search/
│           ├── intent.rs        (query → IntentSignals parser)
│           ├── router.rs        (7-rule deterministic cascade)
│           ├── classify.rs      (LLM triage for R6 — Claude only)
│           ├── index.rs         (in-memory metadata index, recursive walk)
│           ├── metadata.rs      (metadata backend)
│           ├── keyword.rs       (ripgrep-style keyword backend, recursive)
│           ├── structural.rs    (page/word count backend, recursive)
│           ├── semantic.rs      (stub — Phase 4)
│           └── merge.rs         (dedup + rank + truncate)
└── pdf-lab/                     (CLI binary crate)
    └── src/cli/
        ├── extract.rs
        ├── search.rs
        ├── serve.rs             (Axum HTTP server for the UI)
        ├── config.rs
        ├── index.rs
        └── mcp.rs               (rmcp stdio MCP server)
```

## Phased roadmap

| Phase | Status | Scope |
|-------|--------|-------|
| 1 — Extraction + Metadata/Keyword | **Done** | pdfium rendering, Claude extraction, metadata + keyword search, MCP server |
| 2 — Query Router + Search UI | **Done** | 7-rule router, `classify_query` MCP tool, structural backend, Axum HTTP server, Vite/React UI |
| 3 — CLI Polish + Optimizations | Planned | Batch API, prompt caching, result ranking, Tauri desktop packaging |
| 4 — Semantic Search | Planned | LanceDB + fastembed (nomic-embed-text-v1.5), ANN index |

## Supported file types

| Extension | Handling |
|-----------|----------|
| `.pdf` | pdfium text extraction per page; PNG fallback at 300 DPI if text < 50 chars |
| `.jpg` / `.jpeg` / `.png` | Sent directly to the configured LLM as base64 image |

## Configuration reference

`config/pdf-lab/config.json` (relative to your working directory):
```json
{
  "model": "claude-haiku-4-5-20251001",
  "api_key": "sk-ant-...",
  "base_url": null,
  "source_dir": "/path/to/source-files",
  "outputs_dir": "/path/to/outputs",
  "backend": "claude",
  "ollama_url": "http://localhost:11434",
  "ollama_model": "qwen3.5:9b",
  "gemini_api_key": "AIza...",
  "gemini_model": "gemini-2.0-flash"
}
```

### Environment variables

Create a `.env` file in the directory where you run `pdf-lab`, or export in your shell. Env vars take precedence over the config file.

```bash
ANTHROPIC_API_KEY=sk-ant-...
PDF_LAB_MODEL=claude-haiku-4-5-20251001
ANTHROPIC_BASE_URL=https://api.anthropic.com
PDF_LAB_SOURCE_DIR=/path/to/source-files
PDF_LAB_OUTPUTS_DIR=/path/to/outputs
PDF_LAB_BACKEND=claude           # claude | gemini | ollama
PDF_LAB_OLLAMA_URL=http://localhost:11434
PDF_LAB_OLLAMA_MODEL=qwen3.5:9b
GEMINI_API_KEY=AIza...
PDF_LAB_GEMINI_MODEL=gemini-2.0-flash
```

**Resolution order** (highest → lowest): env var / `.env` → config file → built-in default.
