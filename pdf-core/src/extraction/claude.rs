//! Claude API backend for PDF extraction.
//!
//! This module implements a two-pass extraction pipeline using Anthropic's Claude API:
//!
//! **Pass 1 — Type detection** (`classify_doc_type`):
//!   A cheap, single-turn call that classifies the document type (e.g. "aadhaar", "pan",
//!   "bank_statement"). Only the first page's text is sent; the image is never uploaded.
//!   Uses Haiku-class tokens (≈50–300 input tokens) to keep costs low.
//!
//! **Pass 2 — Full extraction** (`call_claude`):
//!   An agentic multi-turn loop. Claude receives every page (embedded text or image),
//!   then orchestrates its own OCR strategy by calling the `ocr_scan` tool for each
//!   scanned image page. When done, it calls `submit_extraction` once with the full
//!   structured result. The loop continues until `submit_extraction` is received or the
//!   iteration cap is hit.
//!
//! The `submit_extraction` tool schema is dynamically generated from the `SchemaRegistry`
//! for the detected document type, so Claude only needs to populate fields relevant to
//! that document class.

use std::collections::HashMap;

use anyhow::Context;
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use serde_json::{Value, json};

use crate::config::ClaudeConfig;
use crate::schema::SchemaRegistry;

use super::{ExtractionResult, PageContent, PageText, ocr};

/// Maximum tokens Claude may produce in a single response turn.
/// 4096 is enough for structured JSON output across typical multi-page documents.
const MAX_TOKENS: u32 = 4096;

// ---------------------------------------------------------------------------
// Debug helpers — gated behind the PDF_LAB_DEBUG env var
// ---------------------------------------------------------------------------

/// Returns true when PDF_LAB_DEBUG is set to a non-empty, non-"0" value.
/// This avoids any performance cost when debugging is off.
fn debug_enabled() -> bool {
    std::env::var("PDF_LAB_DEBUG").map(|v| !v.is_empty() && v != "0").unwrap_or(false)
}

/// Logs the OCR result for a single page, including the routing path chosen
/// (tesseract-only, tesseract+cleanup, or llm-vision) and the raw OCR text.
fn debug_ocr(page_num: u32, result: &ocr::OcrResult) {
    if !debug_enabled() { return; }
    // Translate the numeric confidence into the human-readable routing label.
    let path = ocr::OcrPath::from_confidence(result.mean_confidence).as_str();
    eprintln!("\n[debug:ocr] page {page_num} | conf {:.0} → {path}", result.mean_confidence);
    eprintln!("---");
    eprintln!("{}", if result.text.is_empty() { "(empty)" } else { &result.text });
    eprintln!("---");
}

/// Logs the final extraction result after the full Claude agentic loop completes.
/// Shows per-page text and the OCR routing path taken for each page.
fn debug_llm(result: &ExtractionResult, pages: &[PageContent], confidence_map: &HashMap<u32, f32>) {
    if !debug_enabled() { return; }
    eprintln!("\n[debug:llm] doc_type={} ocr_method={}", result.doc_type, result.ocr_method);
    for page_text in &result.pages {
        // Reconstruct the routing label from the original page type + confidence recorded
        // during the agentic loop so we can show it alongside the extracted text.
        let path_str = pages.iter()
            .find(|p| p.page_num() == page_text.page_num)
            .map(|p| match p {
                PageContent::Text { .. } => "text-embedded",
                PageContent::Image { page_num, .. } => {
                    match confidence_map.get(page_num) {
                        Some(&conf) => ocr::OcrPath::from_confidence(conf).as_str(),
                        // Claude used its vision directly without calling ocr_scan.
                        None => "llm-vision",
                    }
                }
            })
            .unwrap_or("unknown");
        eprintln!("[debug:llm] page {} | {path_str}", page_text.page_num);
        eprintln!("---");
        eprintln!("{}", if page_text.text.is_empty() { "(empty)" } else { &page_text.text });
        eprintln!("---");
    }
}

// ---------------------------------------------------------------------------
// System prompt — describes the OCR-strategy workflow to Claude
// ---------------------------------------------------------------------------

/// The system prompt tells Claude exactly how to handle each page type.
///
/// Key design decisions baked in:
/// - Scanned pages: Claude must call `ocr_scan` first, then choose a strategy
///   based on the returned confidence score (HIGH/MEDIUM/LOW thresholds).
/// - Embedded text pages: no OCR call needed — use the text as-is.
/// - Claude calls `submit_extraction` exactly once when all pages are processed.
///
/// This prompt is marked `cache_control: ephemeral` so Anthropic's prompt
/// caching reuses it across turns in the same session, reducing input token costs.
const SYSTEM_PROMPT: &str = "\
You are a document extraction assistant with access to a local OCR tool.

## Document pages
Pages arrive as either embedded text (already extracted from the PDF's text layer) \
or as scanned images. Each page is labeled so you know which type it is.

## Workflow for scanned image pages
Each scanned image page is labeled:
  [Page N — scanned image; call ocr_scan(page_num=N) first]

For each such page:
1. Call ocr_scan(page_num=N) to get local Tesseract OCR text and a confidence score (0–100).
2. Choose ONE strategy based on mean_confidence:
   - HIGH (≥ 85): The OCR is reliable — use the OCR text verbatim.
   - MEDIUM (60 ≤ conf < 85): The OCR has errors — use it as a base but correct obvious \
     mistakes using your language model knowledge of the document domain (names, numbers, \
     common Indian document formats).
   - LOW (< 60): The OCR is too unreliable — ignore it entirely. \
     The image is already in this conversation — read it directly with your vision.

## Workflow for embedded text pages
Text pages are labeled:
  [Page N — embedded text]
Do NOT call ocr_scan for these. Use the provided text as-is.

## Final step
After processing ALL pages (OCR + strategy choice), call submit_extraction exactly once \
with the complete structured result covering every page.

Confidence thresholds: HIGH = 85.0, MEDIUM = 60.0";

// ---------------------------------------------------------------------------
// Pass 1 — Type detection
// ---------------------------------------------------------------------------

/// Lightweight single-turn call that classifies the document type.
///
/// Only the first page's text (up to 2000 characters) is sent — never the image —
/// so this call costs roughly 50–300 input tokens on a Haiku-class model.
///
/// The returned string is a normalised doc_type key (e.g. "aadhaar", "pan",
/// "bank_statement") drawn from the schema registry's known types. If the document
/// cannot be identified or the first page is blank, the schema's default type is used.
pub async fn classify_doc_type(
    pages: &[PageContent],
    config: &ClaudeConfig,
    schema: &SchemaRegistry,
) -> anyhow::Result<String> {
    // For image pages, run a quick local OCR to get some text cheaply rather than
    // uploading the image to Claude just for classification.
    let first_page_text = match pages.first() {
        Some(PageContent::Text { text, .. }) => text.chars().take(2000).collect::<String>(),
        Some(PageContent::Image { data, .. }) => {
            ocr::scan_page(data.clone())
                .await
                .map(|r| r.text.chars().take(2000).collect::<String>())
                .unwrap_or_default()
        }
        None => String::new(),
    };

    // Nothing to classify — fall back to the schema's catch-all type.
    if first_page_text.is_empty() {
        return Ok(schema.doc_type_default.clone());
    }

    let client = reqwest::Client::new();
    let url = format!("{}/v1/messages", config.api_base());

    // The type-detection prompt lists the known doc_type values and asks Claude
    // to respond with exactly one of them (no explanation, just the key string).
    let prompt = schema.build_type_detection_prompt();
    let content = format!("{prompt}\n\nDocument text:\n{first_page_text}");

    // 32 tokens is enough for a short doc_type label; keeps this call cheap.
    let body = json!({
        "model": config.model,
        "max_tokens": 32,
        "messages": [{"role": "user", "content": content}]
    });

    let response = client
        .post(&url)
        .header("x-api-key", &config.api_key)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await
        .context("sending type-detection request to Claude API")?;

    let status = response.status();
    let response_text = response.text().await.context("reading type-detection response")?;
    if !status.is_success() {
        anyhow::bail!("Claude API error {status}: {response_text}");
    }

    let response_json: Value = serde_json::from_str(&response_text)
        .context("parsing type-detection response")?;

    // Pull the plain text out of the first content block.
    let raw_type = response_json["content"]
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|block| block["text"].as_str())
        .unwrap_or("")
        .trim()
        .to_string();

    if debug_enabled() {
        eprintln!("[debug:classify] raw={raw_type:?}");
    }

    // Normalise handles whitespace, casing, and partial matches against known types.
    Ok(schema.coerce_doc_type(&raw_type))
}

// ---------------------------------------------------------------------------
// Pass 2 — Full extraction
// ---------------------------------------------------------------------------

/// Run the agentic extraction loop against the Claude API.
///
/// The loop works as follows:
/// 1. Build one user message containing all pages (text blocks + images).
/// 2. On each turn, Claude may call:
///    - `ocr_scan(page_num=N)` — we run Tesseract locally and return the result.
///    - `submit_extraction(...)` — we parse the structured output and break.
/// 3. Iteration cap: `max(5, num_image_pages * 2 + 2)` turns to prevent infinite loops
///    while still allowing one OCR call per image page plus overhead.
/// 4. After the loop, `ocr_method` is derived from which routing paths were actually taken.
pub async fn call_claude(
    pages: &[PageContent],
    config: &ClaudeConfig,
    schema: &SchemaRegistry,
    doc_type: &str,
) -> anyhow::Result<ExtractionResult> {
    let client = reqwest::Client::new();
    let url = format!("{}/v1/messages", config.api_base());

    // Pre-index image bytes by page number so we can serve ocr_scan requests
    // without scanning the pages slice on every tool call.
    let image_map: HashMap<u32, Vec<u8>> = pages.iter()
        .filter_map(|p| {
            if let PageContent::Image { page_num, data, .. } = p {
                Some((*page_num, data.clone()))
            } else {
                None
            }
        })
        .collect();

    let image_page_count = image_map.len();
    // At minimum allow 5 turns; for image-heavy docs allow 2 turns per image page
    // (one for ocr_scan, one for any follow-up) plus 2 for setup/submit overhead.
    let max_iters = std::cmp::max(5, image_page_count * 2 + 2);

    // Convert pages into the API content block format (text labels + base64 images).
    let content_blocks = build_content_blocks(pages);
    // Build tool definitions; submit_extraction schema is tailored to doc_type.
    let tools = build_tools(schema, doc_type);
    // Wrap the system prompt in a cacheable block.
    let system = build_system();

    // Conversation history accumulates across turns so Claude maintains context.
    let mut messages: Vec<Value> = vec![
        json!({"role": "user", "content": content_blocks})
    ];

    // Track OCR confidence per page so we can compute the final ocr_method label.
    let mut confidence_map: HashMap<u32, f32> = HashMap::new();
    let mut extraction_result: Option<ExtractionResult> = None;

    for iter in 0..max_iters {
        let body = json!({
            "model": config.model,
            "max_tokens": MAX_TOKENS,
            "system": system,
            "tools": tools,
            // "auto" lets Claude decide when to call tools vs. stop naturally.
            "tool_choice": {"type": "auto"},
            "messages": messages
        });

        let response = client
            .post(&url)
            .header("x-api-key", &config.api_key)
            .header("anthropic-version", "2023-06-01")
            // Enable prompt caching to reuse the system prompt and tool definitions
            // across turns, significantly reducing input token costs on long documents.
            .header("anthropic-beta", "prompt-caching-2024-07-31")
            .json(&body)
            .send()
            .await
            .context("sending request to Claude API")?;

        let status = response.status();
        let response_text = response.text().await.context("reading response body")?;
        if !status.is_success() {
            anyhow::bail!("Claude API error {status}: {response_text}");
        }

        let response_json: Value = serde_json::from_str(&response_text)
            .context("parsing Claude API response JSON")?;

        let stop_reason = response_json["stop_reason"].as_str().unwrap_or("");
        let content = response_json["content"]
            .as_array()
            .context("response missing content array")?
            .clone();

        // Append Claude's response to the conversation so subsequent turns have context.
        messages.push(json!({"role": "assistant", "content": content}));

        let mut tool_results: Vec<Value> = Vec::new();
        let mut found_submit = false;

        // Process every tool_use block in this response.
        // Claude may call multiple tools in one turn (e.g. ocr_scan several pages).
        for block in &content {
            if block["type"] != "tool_use" { continue; }

            let tool_name = block["name"].as_str().unwrap_or("");
            let tool_use_id = block["id"].as_str().unwrap_or("").to_string();

            match tool_name {
                // Claude requests local Tesseract OCR for a specific page.
                "ocr_scan" => {
                    let page_num = block["input"]["page_num"].as_u64().unwrap_or(0) as u32;
                    let result_content = match image_map.get(&page_num) {
                        Some(bytes) => {
                            match ocr::scan_page(bytes.clone()).await {
                                Ok(ocr_result) => {
                                    // Record confidence so we can aggregate ocr_method later.
                                    confidence_map.insert(page_num, ocr_result.mean_confidence);
                                    debug_ocr(page_num, &ocr_result);
                                    // Serialise the full OcrResult struct as JSON for Claude.
                                    serde_json::to_string(&ocr_result).unwrap_or_default()
                                }
                                Err(e) => json!({"error": e.to_string()}).to_string(),
                            }
                        }
                        // Claude called ocr_scan on a page that has embedded text — not valid.
                        None => json!({
                            "error": format!("page {} is not a scanned image page", page_num)
                        }).to_string(),
                    };
                    tool_results.push(json!({
                        "type": "tool_result",
                        "tool_use_id": tool_use_id,
                        "content": result_content
                    }));
                }
                // Claude has finished processing all pages and is submitting the result.
                "submit_extraction" => {
                    match parse_extraction_input(&block["input"], doc_type, schema) {
                        Ok(result) => extraction_result = Some(result),
                        Err(e) => return Err(e),
                    }
                    // Acknowledge the submission so the conversation can close cleanly.
                    tool_results.push(json!({
                        "type": "tool_result",
                        "tool_use_id": tool_use_id,
                        "content": "{\"status\":\"accepted\"}"
                    }));
                    found_submit = true;
                }
                // Safety net: tell Claude about unknown tools rather than silently ignoring.
                other => {
                    tool_results.push(json!({
                        "type": "tool_result",
                        "tool_use_id": tool_use_id,
                        "content": format!("{{\"error\":\"unknown tool: {}\"}}", other)
                    }));
                }
            }
        }

        // Send tool results back as a user turn so Claude can continue.
        if !tool_results.is_empty() {
            messages.push(json!({"role": "user", "content": tool_results}));
        }

        if found_submit {
            break;
        }
        // If Claude stopped naturally without submitting, something went wrong —
        // the system prompt requires submit_extraction to always be called.
        if stop_reason == "end_turn" {
            anyhow::bail!(
                "Claude stopped without calling submit_extraction after {} iteration(s)",
                iter + 1
            );
        }
    }

    let mut result = extraction_result
        .context("max iterations reached without a submit_extraction call")?;

    // Derive the aggregate OCR routing label from the per-page paths taken.
    // Pages that had ocr_scan called use the confidence-based path; image pages
    // that Claude read directly (no ocr_scan call) are labelled "llm-vision".
    let paths: Vec<ocr::OcrPath> = pages.iter().map(|p| {
        match p {
            PageContent::Text { .. } => ocr::OcrPath::SkippedTextPage,
            PageContent::Image { page_num, .. } => {
                match confidence_map.get(page_num) {
                    Some(&conf) => ocr::OcrPath::from_confidence(conf),
                    None => ocr::OcrPath::LlmVision,
                }
            }
        }
    }).collect();

    result.ocr_method = ocr::aggregate_ocr_method(&paths);
    debug_llm(&result, pages, &confidence_map);
    Ok(result)
}

// ---------------------------------------------------------------------------
// Tool builders
// ---------------------------------------------------------------------------

/// Wrap the system prompt in an `ephemeral` cache-control block.
///
/// Anthropic's prompt caching reuses this across turns within the same session,
/// avoiding repeated input-token charges for the system prompt on every iteration.
fn build_system() -> Value {
    json!([{
        "type": "text",
        "text": SYSTEM_PROMPT,
        "cache_control": {"type": "ephemeral"}
    }])
}

/// Convert the page list into the multi-modal content block array expected by the API.
///
/// Each page becomes one or two blocks:
/// - Text pages: a single text block with the embedded content.
/// - Image pages: a text label block (instructs Claude to call ocr_scan first)
///   followed by a base64-encoded image block so Claude can also read it visually.
fn build_content_blocks(pages: &[PageContent]) -> Vec<Value> {
    let mut blocks = Vec::new();
    for page in pages {
        match page {
            PageContent::Text { page_num, text } => {
                blocks.push(json!({
                    "type": "text",
                    "text": format!("[Page {page_num} — embedded text]\n{text}")
                }));
            }
            PageContent::Image { page_num, data, media_type } => {
                // The label primes Claude to call ocr_scan before reading the image,
                // matching the workflow described in the system prompt.
                blocks.push(json!({
                    "type": "text",
                    "text": format!(
                        "[Page {page_num} — scanned image; call ocr_scan(page_num={page_num}) first]"
                    )
                }));
                let b64 = BASE64.encode(data);
                blocks.push(json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": media_type,
                        "data": b64
                    }
                }));
            }
        }
    }
    blocks
}

/// Build the two tool definitions: `ocr_scan` and `submit_extraction`.
///
/// `submit_extraction`'s input schema is generated dynamically from the schema
/// registry for the detected `doc_type`, so Claude only sees fields relevant to
/// that document class (e.g. aadhaar fields for an Aadhaar card).
///
/// The `pages` array field is always present regardless of doc_type and carries
/// the per-page extracted text.
fn build_tools(schema: &SchemaRegistry, doc_type: &str) -> Value {
    let effective_fields = schema.effective_fields(doc_type);

    let mut properties = serde_json::Map::new();

    // The pages array captures verbatim page text and is required for every document.
    properties.insert("pages".to_string(), json!({
        "type": "array",
        "items": {
            "type": "object",
            "required": ["page_num", "text"],
            "properties": {
                "page_num": {
                    "type": "integer",
                    "description": "1-indexed page number"
                },
                "text": {
                    "type": "string",
                    "description": "All visible text. Tables as tab-separated values. \
                                    Form fields as 'Label: Value'. \
                                    Preserve numbers, names, dates, addresses exactly."
                }
            }
        }
    }));

    let mut required_fields = vec!["pages".to_string()];

    // Append doc-type-specific structured fields (e.g. "aadhaar_number", "name", "dob").
    for field in &effective_fields {
        properties.insert(field.name.clone(), schema.field_json_schema_property(field));
        if field.required {
            required_fields.push(field.name.clone());
        }
    }

    json!([
        {
            "name": "ocr_scan",
            "description": "Run local Tesseract OCR on a scanned image page. Returns text and \
                            a confidence score (0–100). Call this for each scanned image page \
                            before calling submit_extraction.",
            "input_schema": {
                "type": "object",
                "required": ["page_num"],
                "properties": {
                    "page_num": {
                        "type": "integer",
                        "description": "1-indexed page number of the scanned image to OCR."
                    }
                }
            }
        },
        {
            "name": "submit_extraction",
            "description": "Submit the structured extraction result. Call exactly once after \
                            processing all pages.",
            "input_schema": {
                "type": "object",
                "required": required_fields,
                "properties": properties
            }
        }
    ])
}

// ---------------------------------------------------------------------------
// Response parsing
// ---------------------------------------------------------------------------

/// Parse the `submit_extraction` tool input into an `ExtractionResult`.
///
/// Each doc-type-specific field is normalised by the schema registry (e.g. date
/// formatting, whitespace stripping, enum coercion) before being stored. Fields
/// that are both empty and optional are omitted from the output map to keep the
/// result lean.
///
/// Note: `ocr_method` is left empty here and filled in by `call_claude` once the
/// full routing picture is available after the loop completes.
fn parse_extraction_input(
    input: &Value,
    doc_type: &str,
    schema: &SchemaRegistry,
) -> anyhow::Result<ExtractionResult> {
    let pages: Vec<PageText> = input["pages"]
        .as_array()
        .context("missing pages array in submit_extraction")?
        .iter()
        .map(|p| {
            Ok(PageText {
                page_num: p["page_num"].as_u64().unwrap_or(0) as u32,
                text: p["text"].as_str().unwrap_or("").to_string(),
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    let effective_fields = schema.effective_fields(doc_type);

    let mut fields: HashMap<String, String> = HashMap::new();
    for field in &effective_fields {
        let raw = input[&field.name].as_str().unwrap_or("").to_string();
        let normalised = schema.normalise(field, &raw);
        // Always include required fields even when empty, so downstream code can
        // distinguish "field missing" from "field present but blank".
        if !normalised.is_empty() || field.required {
            fields.insert(field.name.clone(), normalised);
        }
    }

    Ok(ExtractionResult {
        pages,
        doc_type: doc_type.to_string(),
        doc_category: String::new(), // set by extract_one after pipeline routing
        fields,
        // Populated by call_claude after the agentic loop finishes.
        ocr_method: String::new(),
    })
}

// ---------------------------------------------------------------------------
// Connectivity test (unchanged)
// ---------------------------------------------------------------------------

/// Send a minimal "Hi" message to verify the API key and network path are working.
/// Returns the round-trip duration so callers can surface latency to the user.
pub async fn test_connection(config: &ClaudeConfig) -> anyhow::Result<std::time::Duration> {
    let client = reqwest::Client::new();
    let url = format!("{}/v1/messages", config.api_base());

    let body = json!({
        "model": config.model,
        "max_tokens": 10,
        "messages": [{"role": "user", "content": "Hi"}]
    });

    let start = std::time::Instant::now();
    let response = client
        .post(&url)
        .header("x-api-key", &config.api_key)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await
        .context("connecting to Claude API")?;

    let elapsed = start.elapsed();
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("API error {status}: {body}");
    }

    Ok(elapsed)
}
