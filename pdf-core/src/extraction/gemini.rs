//! Google Gemini API backend for PDF extraction.
//!
//! Mirrors the two-pass agentic pipeline from `claude.rs` using the Gemini API:
//!
//! **Pass 1 — Type detection** (`classify_doc_type`):
//!   Single-turn call classifying the document type from first-page text. Uses 32
//!   output tokens to keep costs low. Falls back to schema default on empty input.
//!
//! **Pass 2 — Full extraction** (`call_gemini`):
//!   Agentic multi-turn loop. Gemini receives all pages (text + images), then calls
//!   `ocr_scan` for each scanned page and `submit_extraction` once when done.
//!   Loop continues until `submit_extraction` is received or the iteration cap is hit.
//!
//! **Key protocol differences from Claude:**
//! - Auth: `?key=` query param (never log the URL — it contains the API key)
//! - Message format: `contents[].parts[]` with `inline_data` for images
//! - Tool calls in response: `{"functionCall": {"name": "...", "args": {...}}}`
//! - Tool results: `{"functionResponse": {"name": "...", "response": {...}}}`
//! - No tool IDs — responses matched by name; preserve order when a function is
//!   called multiple times in one turn
//! - Loop driver: presence of `functionCall` parts (not `finishReason`)
//! - JSON Schema types: UPPERCASE ("OBJECT", "STRING", etc.)
//! - Safety settings: `BLOCK_NONE` required for document extraction (PII content)

use std::collections::HashMap;

use anyhow::Context;
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use serde_json::{Value, json};

use crate::config::ClaudeConfig;
use crate::schema::SchemaRegistry;

use super::{ExtractionResult, PageContent, PageText, ocr};

const MAX_TOKENS: u32 = 8192;

/// Build the generationConfig JSON value.
///
/// Gemini 2.5+ models have thinking enabled by default. For extraction tasks thinking
/// adds latency without benefit and — critically — can cause the model to emit a text
/// summary instead of calling submit_extraction. Setting thinkingBudget=0 disables it.
pub(crate) fn build_generation_config(model: &str, max_tokens: u32) -> Value {
    if model.contains("2.5") || model.contains("-exp") {
        json!({"maxOutputTokens": max_tokens, "thinkingConfig": {"thinkingBudget": 0}})
    } else {
        json!({"maxOutputTokens": max_tokens})
    }
}

// ---------------------------------------------------------------------------
// Debug helpers — same behaviour as claude.rs, gated behind PDF_LAB_DEBUG
// ---------------------------------------------------------------------------

fn debug_enabled() -> bool {
    std::env::var("PDF_LAB_DEBUG").map(|v| !v.is_empty() && v != "0").unwrap_or(false)
}

fn debug_ocr(page_num: u32, result: &ocr::OcrResult) {
    if !debug_enabled() { return; }
    let path = ocr::OcrPath::from_confidence(result.mean_confidence).as_str();
    eprintln!("\n[debug:ocr:gemini] page {page_num} | conf {:.0} → {path}", result.mean_confidence);
    eprintln!("---");
    eprintln!("{}", if result.text.is_empty() { "(empty)" } else { &result.text });
    eprintln!("---");
}

fn debug_llm(result: &ExtractionResult, pages: &[PageContent], confidence_map: &HashMap<u32, f32>) {
    if !debug_enabled() { return; }
    eprintln!("\n[debug:llm:gemini] doc_type={} ocr_method={}", result.doc_type, result.ocr_method);
    for page_text in &result.pages {
        let path_str = pages.iter()
            .find(|p| p.page_num() == page_text.page_num)
            .map(|p| match p {
                PageContent::Text { .. } => "text-embedded",
                PageContent::Image { page_num, .. } => {
                    match confidence_map.get(page_num) {
                        Some(&conf) => ocr::OcrPath::from_confidence(conf).as_str(),
                        None => "llm-vision",
                    }
                }
            })
            .unwrap_or("unknown");
        eprintln!("[debug:llm:gemini] page {} | {path_str}", page_text.page_num);
        eprintln!("---");
        eprintln!("{}", if page_text.text.is_empty() { "(empty)" } else { &page_text.text });
        eprintln!("---");
    }
}

// ---------------------------------------------------------------------------
// System prompt — identical to claude.rs; delivered via systemInstruction
// ---------------------------------------------------------------------------

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
Do NOT call ocr_scan for these. Their full text is already captured by the system — \
do NOT include embedded text pages in submit_extraction.pages. Only include scanned \
image pages in that array.

## Final step
After processing ALL scanned image pages (OCR + strategy choice), call submit_extraction \
exactly once. The pages array must contain ONLY scanned image page texts. \
Embedded text pages must be omitted from pages.

Confidence thresholds: HIGH = 85.0, MEDIUM = 60.0";

// ---------------------------------------------------------------------------
// URL helpers — keep API key out of error messages
// ---------------------------------------------------------------------------

fn gemini_url(config: &ClaudeConfig) -> String {
    let model = config.resolved_gemini_model();
    let key = config.gemini_api_key.as_deref().unwrap_or("");
    format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
        model, key
    )
}

fn gemini_display(config: &ClaudeConfig) -> String {
    format!(
        "generativelanguage.googleapis.com model={}",
        config.resolved_gemini_model()
    )
}

// ---------------------------------------------------------------------------
// Pass 1 — Type detection
// ---------------------------------------------------------------------------

/// Lightweight single-turn call that classifies the document type.
///
/// Only the first page's text (up to 2000 characters) is sent. For image pages,
/// a quick local Tesseract scan extracts text cheaply rather than uploading the image.
pub async fn classify_doc_type(
    pages: &[PageContent],
    config: &ClaudeConfig,
    schema: &SchemaRegistry,
) -> anyhow::Result<String> {
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

    if first_page_text.is_empty() {
        return Ok(schema.doc_type_default.clone());
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;

    let prompt = schema.build_type_detection_prompt();
    let content = format!("{prompt}\n\nDocument text:\n{first_page_text}");

    let body = json!({
        "contents": [{"role": "user", "parts": [{"text": content}]}],
        "safetySettings": build_safety_settings(),
        "generationConfig": build_generation_config(config.resolved_gemini_model(), 32)
    });

    let response = client
        .post(&gemini_url(config))
        .json(&body)
        .send()
        .await
        .with_context(|| format!("sending type-detection request to {}", gemini_display(config)))?;

    let status = response.status();
    let response_text = response.text().await.context("reading type-detection response")?;
    if !status.is_success() {
        anyhow::bail!("Gemini API error {status} during type detection: {response_text}");
    }

    let response_json: Value = serde_json::from_str(&response_text)
        .context("parsing type-detection response")?;

    if let Some(reason) = response_json["promptFeedback"]["blockReason"].as_str() {
        if debug_enabled() {
            eprintln!("[debug:classify:gemini] blocked: {reason}");
        }
        return Ok(schema.doc_type_default.clone());
    }

    let raw_type = response_json["candidates"][0]["content"]["parts"][0]["text"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string();

    if debug_enabled() {
        eprintln!("[debug:classify:gemini] raw={raw_type:?}");
    }

    Ok(schema.coerce_doc_type(&raw_type))
}

// ---------------------------------------------------------------------------
// Pass 2 — Full extraction
// ---------------------------------------------------------------------------

/// Run the agentic extraction loop against the Gemini API.
///
/// The loop mirrors claude.rs:
/// 1. Build one user message with all pages (text + images).
/// 2. On each turn, Gemini may call:
///    - `ocr_scan(page_num=N)` — run Tesseract locally and return the result.
///    - `submit_extraction(...)` — parse the structured output and break.
/// 3. Loop driver: presence of `functionCall` parts (not `finishReason`).
/// 4. Max iterations: `max(5, num_image_pages * 2 + 2)`.
pub async fn call_gemini(
    pages: &[PageContent],
    config: &ClaudeConfig,
    schema: &SchemaRegistry,
    doc_type: &str,
) -> anyhow::Result<ExtractionResult> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?;

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
    let max_iters = std::cmp::max(5, image_page_count * 2 + 2);

    let initial_parts = build_content_parts(pages);
    let tools = build_tools(schema, doc_type);
    let system_instruction = json!({"parts": [{"text": SYSTEM_PROMPT}]});

    let mut contents: Vec<Value> = vec![
        json!({"role": "user", "parts": initial_parts})
    ];

    let mut confidence_map: HashMap<u32, f32> = HashMap::new();
    let mut extraction_result: Option<ExtractionResult> = None;

    for iter in 0..max_iters {
        let body = json!({
            "systemInstruction": system_instruction,
            "contents": contents,
            "tools": tools,
            "toolConfig": {"functionCallingConfig": {"mode": "ANY"}},
            "safetySettings": build_safety_settings(),
            "generationConfig": build_generation_config(config.resolved_gemini_model(), MAX_TOKENS)
        });

        let response = client
            .post(&gemini_url(config))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("sending request to {}", gemini_display(config)))?;

        let status = response.status();
        let response_text = response.text().await.context("reading response body")?;
        if !status.is_success() {
            anyhow::bail!("Gemini API error {status} during extraction: {response_text}");
        }

        let response_json: Value = serde_json::from_str(&response_text)
            .context("parsing Gemini API response JSON")?;

        if let Some(reason) = response_json["promptFeedback"]["blockReason"].as_str() {
            anyhow::bail!("Gemini blocked the request: {reason}");
        }

        let candidates = response_json["candidates"]
            .as_array()
            .context("Gemini response missing candidates array")?;

        if candidates.is_empty() {
            anyhow::bail!("Gemini returned no candidates");
        }

        let candidate_content = &candidates[0]["content"];
        let parts = candidate_content["parts"]
            .as_array()
            .cloned()
            .unwrap_or_default();

        // Append model turn to conversation history (even when parts is empty).
        contents.push(json!({"role": "model", "parts": parts}));

        // Identify which parts are function calls.
        let function_call_parts: Vec<&Value> = parts.iter()
            .filter(|p| p.get("functionCall").is_some())
            .collect();

        if function_call_parts.is_empty() {
            // No tool calls — if we have a submit result we're done, otherwise error.
            if extraction_result.is_some() {
                break;
            }
            let finish_reason = candidates[0]["finishReason"].as_str().unwrap_or("unknown");
            anyhow::bail!(
                "Gemini stopped without calling submit_extraction (finishReason={finish_reason}) after {} iteration(s)",
                iter + 1
            );
        }

        // Process each function call in order (ordering matters for Gemini 2.x matching).
        let mut function_response_parts: Vec<Value> = Vec::new();
        let mut found_submit = false;

        for part in &function_call_parts {
            let fc = &part["functionCall"];
            let tool_name = fc["name"].as_str().unwrap_or("");
            let args = &fc["args"];

            match tool_name {
                "ocr_scan" => {
                    let page_num = args["page_num"].as_u64().unwrap_or(0) as u32;
                    let response_obj = match image_map.get(&page_num) {
                        Some(bytes) => {
                            match ocr::scan_page(bytes.clone()).await {
                                Ok(ocr_result) => {
                                    confidence_map.insert(page_num, ocr_result.mean_confidence);
                                    debug_ocr(page_num, &ocr_result);
                                    serde_json::to_value(&ocr_result).unwrap_or(json!({}))
                                }
                                Err(e) => json!({"error": e.to_string()}),
                            }
                        }
                        None => json!({
                            "error": format!("page {} is not a scanned image page", page_num)
                        }),
                    };
                    function_response_parts.push(json!({
                        "functionResponse": {
                            "name": "ocr_scan",
                            "response": response_obj
                        }
                    }));
                }
                "submit_extraction" => {
                    match parse_extraction_input(args, doc_type, schema) {
                        Ok(result) => extraction_result = Some(result),
                        Err(e) => return Err(e),
                    }
                    function_response_parts.push(json!({
                        "functionResponse": {
                            "name": "submit_extraction",
                            "response": {"status": "accepted"}
                        }
                    }));
                    found_submit = true;
                }
                other => {
                    function_response_parts.push(json!({
                        "functionResponse": {
                            "name": other,
                            "response": {"error": format!("unknown tool: {}", other)}
                        }
                    }));
                }
            }
        }

        // Send function responses back as the next user turn.
        contents.push(json!({"role": "user", "parts": function_response_parts}));

        if found_submit {
            break;
        }
    }

    let mut result = extraction_result
        .context("max iterations reached without a submit_extraction call")?;

    // Inject embedded text pages that Gemini intentionally omitted from submit_extraction.pages.
    // Gemini only returns scanned image page texts; we fill in the rest from pdfium's extraction.
    for page in pages {
        if let PageContent::Text { page_num, text } = page {
            if !result.pages.iter().any(|p| p.page_num == *page_num) {
                result.pages.push(PageText { page_num: *page_num, text: text.clone() });
            }
        }
    }
    result.pages.sort_by_key(|p| p.page_num);

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

/// Safety settings: BLOCK_NONE for all categories.
/// Required because document extraction involves PII (names, IDs, addresses)
/// which Gemini's default safety filters would otherwise block.
fn build_safety_settings() -> Value {
    json!([
        {"category": "HARM_CATEGORY_HARASSMENT", "threshold": "BLOCK_NONE"},
        {"category": "HARM_CATEGORY_HATE_SPEECH", "threshold": "BLOCK_NONE"},
        {"category": "HARM_CATEGORY_SEXUALLY_EXPLICIT", "threshold": "BLOCK_NONE"},
        {"category": "HARM_CATEGORY_DANGEROUS_CONTENT", "threshold": "BLOCK_NONE"}
    ])
}

/// Convert pages into Gemini `parts` array for the initial user message.
///
/// Text pages → single text part with embedded content.
/// Image pages → text label part + inline_data image part.
fn build_content_parts(pages: &[PageContent]) -> Vec<Value> {
    let mut parts = Vec::new();
    for page in pages {
        match page {
            PageContent::Text { page_num, text } => {
                parts.push(json!({
                    "text": format!("[Page {page_num} — embedded text]\n{text}")
                }));
            }
            PageContent::Image { page_num, data, media_type } => {
                parts.push(json!({
                    "text": format!(
                        "[Page {page_num} — scanned image; call ocr_scan(page_num={page_num}) first]"
                    )
                }));
                let b64 = BASE64.encode(data);
                parts.push(json!({
                    "inline_data": {
                        "mime_type": media_type,
                        "data": b64
                    }
                }));
            }
        }
    }
    parts
}

/// Build Gemini function declarations for `ocr_scan` and `submit_extraction`.
///
/// Gemini uses UPPERCASE type names ("OBJECT", "STRING", "INTEGER", "ARRAY") and
/// wraps all declarations in a single `function_declarations` array inside `tools`.
fn build_tools(schema: &SchemaRegistry, doc_type: &str) -> Value {
    let effective_fields = schema.effective_fields(doc_type);

    let mut properties = serde_json::Map::new();

    // pages is optional: only scanned image pages need to be returned here.
    // Embedded text pages are injected from the original pdfium extraction after the call.
    properties.insert("pages".to_string(), json!({
        "type": "ARRAY",
        "description": "Transcribed text for SCANNED IMAGE pages only. Omit embedded text pages.",
        "items": {
            "type": "OBJECT",
            "required": ["page_num", "text"],
            "properties": {
                "page_num": {
                    "type": "INTEGER",
                    "description": "1-indexed page number of the scanned image page"
                },
                "text": {
                    "type": "STRING",
                    "description": "All visible text. Tables as tab-separated values. \
                                    Form fields as 'Label: Value'. \
                                    Preserve numbers, names, dates, addresses exactly."
                }
            }
        }
    }));

    // pages is intentionally NOT required — text-embedded pages are filled in locally.
    let mut required_fields: Vec<String> = Vec::new();

    for field in &effective_fields {
        properties.insert(field.name.clone(), gemini_field_schema_property(schema, field));
        if field.required {
            required_fields.push(field.name.clone());
        }
    }

    json!([{
        "function_declarations": [
            {
                "name": "ocr_scan",
                "description": "Run local Tesseract OCR on a scanned image page. Returns text and \
                                a confidence score (0–100). Call this for each scanned image page \
                                before calling submit_extraction.",
                "parameters": {
                    "type": "OBJECT",
                    "required": ["page_num"],
                    "properties": {
                        "page_num": {
                            "type": "INTEGER",
                            "description": "1-indexed page number of the scanned image to OCR."
                        }
                    }
                }
            },
            {
                "name": "submit_extraction",
                "description": "Submit the structured extraction result. Call exactly once after \
                                processing all scanned image pages. Only include scanned image \
                                page texts in the pages array — omit embedded text pages entirely.",
                "parameters": {
                    "type": "OBJECT",
                    "required": required_fields,
                    "properties": properties
                }
            }
        ]
    }])
}

/// Convert a FieldDef into a Gemini-compatible JSON Schema property (UPPERCASE types).
fn gemini_field_schema_property(schema: &SchemaRegistry, field: &crate::schema::FieldDef) -> Value {
    // Reuse the Claude schema property builder then uppercase all "type" fields.
    let claude_prop = schema.field_json_schema_property(field);
    uppercase_types(claude_prop)
}

/// Recursively replace lowercase JSON Schema type names with Gemini's UPPERCASE variants.
fn uppercase_types(v: Value) -> Value {
    match v {
        Value::Object(mut map) => {
            if let Some(Value::String(t)) = map.get("type") {
                let uppercased = t.to_uppercase();
                map.insert("type".to_string(), Value::String(uppercased));
            }
            Value::Object(
                map.into_iter()
                    .map(|(k, v)| (k, uppercase_types(v)))
                    .collect()
            )
        }
        Value::Array(arr) => Value::Array(arr.into_iter().map(uppercase_types).collect()),
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Response parsing — identical logic to claude.rs
// ---------------------------------------------------------------------------

fn parse_extraction_input(
    input: &Value,
    doc_type: &str,
    schema: &SchemaRegistry,
) -> anyhow::Result<ExtractionResult> {
    // pages is optional: Gemini only returns scanned image pages; embedded text pages
    // are injected from the original pdfium extraction in the caller.
    let pages: Vec<PageText> = input["pages"]
        .as_array()
        .map(|arr| {
            arr.iter().map(|p| PageText {
                page_num: p["page_num"].as_u64().unwrap_or(0) as u32,
                text: p["text"].as_str().unwrap_or("").to_string(),
            }).collect()
        })
        .unwrap_or_default();

    let effective_fields = schema.effective_fields(doc_type);

    let mut fields: HashMap<String, String> = HashMap::new();
    for field in &effective_fields {
        let raw = input[&field.name].as_str().unwrap_or("").to_string();
        let normalised = schema.normalise(field, &raw);
        if !normalised.is_empty() || field.required {
            fields.insert(field.name.clone(), normalised);
        }
    }

    Ok(ExtractionResult {
        pages,
        doc_type: doc_type.to_string(),
        doc_category: String::new(), // set by extract_one after pipeline routing
        fields,
        ocr_method: String::new(),
        extraction_mode: String::new(),
    })
}

// ---------------------------------------------------------------------------
// Connectivity test
// ---------------------------------------------------------------------------

/// Send a minimal request to verify the API key and network path are working.
pub async fn test_connection(config: &ClaudeConfig) -> anyhow::Result<std::time::Duration> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let body = json!({
        "contents": [{"role": "user", "parts": [{"text": "Hi"}]}],
        "generationConfig": {"maxOutputTokens": 10}
    });

    let start = std::time::Instant::now();
    let response = client
        .post(&gemini_url(config))
        .json(&body)
        .send()
        .await
        .with_context(|| format!("connecting to {}", gemini_display(config)))?;

    let elapsed = start.elapsed();
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("Gemini API error {status}: {body}");
    }

    Ok(elapsed)
}
