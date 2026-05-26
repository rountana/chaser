//! Ollama (local LLM) backend for PDF extraction.
//!
//! This module implements the same two-pass extraction pipeline as `claude.rs` but
//! routes all inference to a locally running Ollama instance instead of the Claude API.
//!
//! **Pass 1 — Type detection** (`classify_doc_type`):
//!   Identical strategy to the Claude backend: OCR the first page cheaply with Tesseract,
//!   then send the text to Ollama's `/api/chat` endpoint for classification.
//!
//! **Pass 2 — Full extraction** (`call_ollama`):
//!   Agentic tool-calling loop: all pages (text labels + base64 images) are sent in one
//!   user message alongside two tool definitions (`ocr_scan`, `submit_extraction`).
//!   Qwen3.5:9B orchestrates its own OCR strategy — calling `ocr_scan` for each image
//!   page and routing based on confidence — then submits the structured result via
//!   `submit_extraction`. The Rust loop runs until `submit_extraction` is received or
//!   the iteration cap is hit.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Context;
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use reqwest::Client;
use serde_json::{Value, json};

use crate::config::ClaudeConfig;
use crate::schema::SchemaRegistry;

use super::{ExtractionResult, PageContent, PageText, ocr};

fn debug_enabled() -> bool {
    std::env::var("PDF_LAB_DEBUG").map(|v| !v.is_empty() && v != "0").unwrap_or(false)
}

/// Convenience macro for debug-gated eprintln! calls.
/// Using a macro avoids evaluating format arguments when debugging is off.
macro_rules! debug {
    ($($arg:tt)*) => { if debug_enabled() { eprintln!($($arg)*); } }
}

#[allow(dead_code)]
const OLLAMA_SYSTEM_PROMPT: &str = "\
You are a document extraction assistant with access to a local OCR tool.

## Document pages
Pages arrive as either embedded text (already extracted from the PDF) or as scanned images. Each page is labeled so you know which type it is.

## Workflow for scanned image pages
Each scanned image page is labeled:
  [Page N — scanned image; call ocr_scan(page_num=N) first]

For each such page:
1. Call ocr_scan(page_num=N) to get local Tesseract OCR text and a confidence score (0-100).
2. Choose ONE strategy based on mean_confidence:
   - HIGH (>= 85): The OCR is reliable — use the OCR text verbatim.
   - MEDIUM (60 <= conf < 85): The OCR has errors — correct obvious mistakes using your language model knowledge.
   - LOW (< 60): The OCR is too unreliable — ignore it entirely. Read the image directly with your vision.

## Workflow for embedded text pages
Text pages are labeled:
  [Page N — embedded text]
Do NOT call ocr_scan for these. Use the provided text as-is.

## Final step
After processing ALL pages, call submit_extraction exactly once with the complete structured result.

Confidence thresholds: HIGH = 85.0, MEDIUM = 60.0";

/// Build the two tool definitions for the agentic extraction loop in Ollama's
/// OpenAI-compatible format: `{type:"function", function:{name, description, parameters}}`.
///
/// Key difference from `claude.rs::build_tools`: Ollama uses `parameters` (not
/// `input_schema`) and wraps each tool in `{type:"function", function:{...}}`.
///
/// `submit_extraction`'s schema is generated dynamically from the `SchemaRegistry`
/// for the detected `doc_type`, so the model only sees fields relevant to that class.
#[allow(dead_code)]
fn build_ollama_tools(schema: &SchemaRegistry, doc_type: &str) -> Value {
    let effective_fields = schema.effective_fields(doc_type);

    let mut properties = serde_json::Map::new();
    properties.insert("pages".to_string(), json!({
        "type": "array",
        "items": {
            "type": "object",
            "required": ["page_num", "text"],
            "properties": {
                "page_num": {"type": "integer", "description": "1-indexed page number"},
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
    for field in &effective_fields {
        properties.insert(field.name.clone(), schema.field_json_schema_property(field));
        if field.required {
            required_fields.push(field.name.clone());
        }
    }

    json!([
        {
            "type": "function",
            "function": {
                "name": "ocr_scan",
                "description": "Run local Tesseract OCR on a scanned image page. \
                                Returns text and a confidence score (0-100). \
                                Call this for each scanned image page before submit_extraction.",
                "parameters": {
                    "type": "object",
                    "required": ["page_num"],
                    "properties": {
                        "page_num": {
                            "type": "integer",
                            "description": "1-indexed page number of the scanned image to OCR."
                        }
                    }
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "submit_extraction",
                "description": "Submit the structured extraction result. \
                                Call exactly once after processing all pages.",
                "parameters": {
                    "type": "object",
                    "required": required_fields,
                    "properties": properties
                }
            }
        }
    ])
}

/// Build content blocks for the agentic extraction loop in Ollama's content format.
///
/// Converts a list of pages (text + images) into a format suitable for sending to
/// Ollama's chat API alongside tool definitions. Each page becomes one or more blocks:
/// - Text pages: single `{type:"text", text:"[Page N — embedded text]\n{text}"}` block.
/// - Image pages: text label block + `{type:"image_url", image_url:{url:"data:{media_type};base64,{b64}"}}` block.
///
/// This mirrors `claude.rs::build_content_blocks()` but uses Ollama's image format.
#[allow(dead_code)]
fn build_ollama_content_blocks(pages: &[PageContent]) -> Vec<Value> {
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
                blocks.push(json!({
                    "type": "text",
                    "text": format!(
                        "[Page {page_num} — scanned image; call ocr_scan(page_num={page_num}) first]"
                    )
                }));
                let b64 = BASE64.encode(data);
                blocks.push(json!({
                    "type": "image_url",
                    "image_url": {
                        "url": format!("data:{media_type};base64,{b64}")
                    }
                }));
            }
        }
    }
    blocks
}

// ---------------------------------------------------------------------------
// Pass 1 — Type detection
// ---------------------------------------------------------------------------

/// Lightweight classification call: returns the doc_type string using text from
/// the first page only. Uses Tesseract for image pages to avoid sending images
/// to Ollama for this cheap, text-only operation.
pub async fn classify_doc_type(
    pages: &[PageContent],
    config: &ClaudeConfig,
    schema: &SchemaRegistry,
) -> anyhow::Result<String> {
    let first_text = match pages.first() {
        Some(PageContent::Text { text, .. }) => text.chars().take(2000).collect::<String>(),
        Some(PageContent::Image { data, .. }) => {
            ocr::scan_page(data.clone())
                .await
                .map(|r| r.text.chars().take(2000).collect::<String>())
                .unwrap_or_default()
        }
        // Empty document — return the schema default immediately without an API call.
        None => return Ok(schema.doc_type_default.clone()),
    };

    if first_text.is_empty() {
        return Ok(schema.doc_type_default.clone());
    }

    // 30-second timeout is generous for a local model on a typical machine.
    let client = Client::builder().timeout(Duration::from_secs(30)).build()?;
    let prompt = format!("{}\n\nDocument text:\n{}", schema.build_type_detection_prompt(), first_text);
    let raw = text_call(&client, config.ollama_base(), config.resolved_ollama_model(), &prompt).await?;
    // normalise_doc_type handles casing, whitespace, and partial matches.
    Ok(schema.normalise_doc_type(raw.trim()))
}

// ---------------------------------------------------------------------------
// Internal HTTP helpers
// ---------------------------------------------------------------------------

/// Send a plain text prompt to Ollama's `/api/chat` endpoint (no images).
/// Returns the model's response text, trimmed of leading/trailing whitespace.
async fn text_call(client: &Client, base_url: &str, model: &str, prompt: &str) -> anyhow::Result<String> {
    let url = format!("{base_url}/api/chat");
    let body = json!({
        "model": model,
        // stream: false returns the full response in one JSON object instead of
        // a stream of newline-delimited chunks, making parsing straightforward.
        "stream": false,
        "messages": [{"role": "user", "content": prompt}]
    });
    let resp = client.post(&url).json(&body).send().await.context("calling Ollama /api/chat")?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("Ollama error {status}: {text}");
    }
    let val: Value = resp.json().await.context("parsing Ollama response")?;
    val["message"]["content"]
        .as_str()
        .map(|s| s.trim().to_string())
        .ok_or_else(|| anyhow::anyhow!("unexpected Ollama response shape: {val}"))
}

// ---------------------------------------------------------------------------
// Pass 2 — Full extraction
// ---------------------------------------------------------------------------

/// Run the sequential extraction pipeline against a local Ollama instance.
///
/// Unlike the Claude backend, this function does NOT use an agentic tool-call loop.
/// Instead it orchestrates OCR and vision calls itself:
///
/// 1. **Per-page OCR routing**: For each image page, Tesseract runs first.
///    Based on the confidence score the page takes one of three paths:
///    - HIGH (≥85): Use Tesseract text verbatim — no Ollama call needed.
///    - MEDIUM (60–85): Ask Ollama to correct OCR errors using the image.
///    - LOW (<60): Discard Tesseract text; ask Ollama to read the image from scratch.
///
/// 2. **Metadata extraction**: After all pages are processed, the first image is
///    sent to Ollama with a JSON-format prompt to extract structured fields.
///    This is a separate call, not integrated into the page-text loop.
///
/// The `log` callback receives human-readable progress messages suitable for
/// displaying in a terminal UI (page-level timing, pass/fail status).
pub async fn call_ollama(
    pages: &[PageContent],
    config: &ClaudeConfig,
    schema: &SchemaRegistry,
    doc_type: &str,
    log: &dyn Fn(&str),
) -> anyhow::Result<ExtractionResult> {
    // Connectivity is only required when there are image pages that need vision.
    // Text-only documents can be handled without Ollama running at all.
    let has_images = pages.iter().any(|p| p.is_image());
    if has_images {
        test_connection(config).await.context("Ollama is not available")?;
    }

    // Use a per-call timeout; Ollama can be slow on large images or cold starts.
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let base_url = config.ollama_base();
    let model = config.resolved_ollama_model();

    debug!("[ollama] base_url={base_url} model={model} pages={}", pages.len());

    let mut page_texts: Vec<PageText> = Vec::new();
    // Track the routing label for each page so we can compute the aggregate method label.
    let mut path_labels: Vec<&str> = Vec::new();
    // We only keep the first image for the metadata extraction step because sending
    // every image would be expensive and the first page usually has the key fields.
    let mut first_image_b64: Option<String> = None;

    for page in pages {
        match page {
            // Embedded text pages need no OCR — pass the text through directly.
            PageContent::Text { page_num, text } => {
                debug!("[ollama] page={page_num} path=text-embedded chars={}", text.len());
                page_texts.push(PageText { page_num: *page_num, text: text.clone() });
                path_labels.push("text-embedded");
            }
            PageContent::Image { page_num, data, .. } => {
                debug!("[ollama] page={page_num} image bytes={}", data.len());
                let b64 = BASE64.encode(data);
                // Capture the first image for the later metadata extraction call.
                if first_image_b64.is_none() {
                    first_image_b64 = Some(b64.clone());
                }

                // Step 1: run local Tesseract OCR and record the result.
                log(&format!("    page {page_num}: OCR..."));
                let t = Instant::now();
                let ocr_result = ocr::scan_page(data.clone()).await?;
                let conf = ocr_result.mean_confidence;
                log(&format!("    page {page_num}: OCR done ({:.1}s, conf={:.0})", t.elapsed().as_secs_f32(), conf));
                debug!("[ollama] page={page_num} tesseract confidence={conf:.1} text_len={}", ocr_result.text.len());

                // Step 2: choose a strategy based on the Tesseract confidence score.
                let (text, label) = if conf >= ocr::HIGH_CONFIDENCE {
                    // Tesseract is reliable — use it directly, no Ollama call needed.
                    debug!("[ollama] page={page_num} path=tesseract-only");
                    (ocr_result.text, "tesseract-only")
                } else if conf >= ocr::MEDIUM_CONFIDENCE {
                    // Tesseract has recognisable errors — ask Ollama to correct them
                    // using the original image as a reference.
                    debug!("[ollama] page={page_num} path=tesseract-ollama-cleanup");
                    let prompt = format!(
                        "Correct OCR errors in the text below using the document image as reference. \
                        Return only the corrected text, no commentary.\n\nOCR text:\n{}",
                        ocr_result.text
                    );
                    log(&format!("    page {page_num}: Ollama cleanup..."));
                    let t = Instant::now();
                    match vision_call(&client, base_url, model, &prompt, &b64).await {
                        Ok(corrected) => {
                            log(&format!("    page {page_num}: Ollama cleanup done ({:.1}s)", t.elapsed().as_secs_f32()));
                            debug!("[ollama] page={page_num} cleanup response len={}", corrected.len());
                            (corrected, "tesseract-ollama-cleanup")
                        }
                        Err(e) => {
                            // On vision failure, fall back to raw Tesseract text rather than
                            // returning an error — a partial result is better than nothing.
                            log(&format!("    page {page_num}: Ollama cleanup FAILED ({:.1}s): {e:#}", t.elapsed().as_secs_f32()));
                            debug!("[ollama] page={page_num} cleanup vision_call FAILED: {e:#}");
                            (ocr_result.text, "tesseract-ollama-cleanup")
                        }
                    }
                } else {
                    // Tesseract confidence is too low to be useful — let Ollama read
                    // the image directly with its vision capability.
                    debug!("[ollama] page={page_num} path=ollama-vision (tesseract too low)");
                    let prompt = "Extract all visible text from this document image exactly as it appears. \
                                  Return only the raw text, no commentary.";
                    log(&format!("    page {page_num}: Ollama vision..."));
                    let t = Instant::now();
                    match vision_call(&client, base_url, model, prompt, &b64).await {
                        Ok(extracted) => {
                            log(&format!("    page {page_num}: Ollama vision done ({:.1}s)", t.elapsed().as_secs_f32()));
                            let preview: String = extracted.chars().take(120).collect();
                            debug!("[ollama] page={page_num} vision response len={} preview={:?}",
                                extracted.len(), preview);
                            (extracted, "ollama-vision")
                        }
                        Err(e) => {
                            // Return an empty string rather than propagating the error so
                            // the rest of the document still produces output.
                            log(&format!("    page {page_num}: Ollama vision FAILED ({:.1}s): {e:#}", t.elapsed().as_secs_f32()));
                            debug!("[ollama] page={page_num} vision vision_call FAILED: {e:#}");
                            (String::new(), "ollama-vision")
                        }
                    }
                };

                page_texts.push(PageText { page_num: *page_num, text });
                path_labels.push(label);
            }
        }
    }

    let effective_fields = schema.effective_fields(doc_type);

    // Metadata extraction: send the first image to Ollama with a structured JSON prompt.
    // This is a separate call from the page-text loop — it focuses Claude purely on
    // extracting named fields rather than transcribing raw text.
    // Documents without any image pages skip this step entirely.
    let fields = match &first_image_b64 {
        Some(b64) => {
            debug!("[ollama] calling vision for metadata extraction");
            // Build one prompt line per field using the schema's human-readable descriptions.
            let field_descriptions = effective_fields.iter()
                .map(|f| schema.field_prompt_line(f))
                .collect::<Vec<_>>()
                .join("\n");
            // Ask Ollama to respond with raw JSON (no markdown fences) so we can parse
            // it directly without stripping code block delimiters.
            let prompt = format!(
                "Extract these fields from this document image.\n\
                 Field descriptions:\n{field_descriptions}\n\n\
                 Respond with valid JSON only, no markdown fences, no explanation:\n{{\n{}\n}}",
                effective_fields.iter()
                    .map(|f| format!("  \"{}\": \"...\"", f.name))
                    .collect::<Vec<_>>()
                    .join(",\n")
            );
            log("    metadata: Ollama vision...");
            let t = Instant::now();
            match vision_call(&client, base_url, model, &prompt, b64).await {
                Ok(raw) => {
                    log(&format!("    metadata: Ollama vision done ({:.1}s)", t.elapsed().as_secs_f32()));
                    debug!("[ollama] metadata raw response: {raw:?}");
                    parse_fields(&raw, &effective_fields, schema)
                }
                Err(e) => {
                    // Metadata extraction failure is non-fatal — return an empty fields
                    // map and let page texts stand as the extraction output.
                    log(&format!("    metadata: Ollama vision FAILED ({:.1}s): {e:#}", t.elapsed().as_secs_f32()));
                    debug!("[ollama] metadata vision_call FAILED: {e:#}");
                    HashMap::new()
                }
            }
        }
        // Text-only document — no image to extract metadata from.
        None => HashMap::new(),
    };

    Ok(ExtractionResult {
        pages: page_texts,
        doc_type: doc_type.to_string(),
        fields,
        ocr_method: aggregate_method(&path_labels),
    })
}

// ---------------------------------------------------------------------------
// Connectivity / model-presence check
// ---------------------------------------------------------------------------

/// Check that Ollama is running and that the configured model is available locally.
///
/// Queries `/api/tags` (the Ollama model list endpoint) rather than sending a
/// full inference request, keeping the check cheap and fast.
///
/// Returns the round-trip duration on success so callers can surface latency.
pub async fn test_connection(config: &ClaudeConfig) -> anyhow::Result<std::time::Duration> {
    let client = Client::new();
    let start = std::time::Instant::now();

    let resp = client
        .get(format!("{}/api/tags", config.ollama_base()))
        .send()
        .await
        .context("connecting to Ollama — is it running?")?;

    if !resp.status().is_success() {
        anyhow::bail!("Ollama returned HTTP {}", resp.status());
    }

    let val: Value = resp.json().await.context("parsing Ollama /api/tags response")?;
    let model_name = config.resolved_ollama_model();

    // Ollama may store the model as "llama3" or "llama3:latest"; check both forms.
    let found = val["models"]
        .as_array()
        .map(|arr| {
            arr.iter().any(|m| {
                m["name"].as_str().map(|n| n == model_name || n.starts_with(&format!("{model_name}:"))).unwrap_or(false)
            })
        })
        .unwrap_or(false);

    if !found {
        anyhow::bail!("Model '{model_name}' not found in Ollama. Pull it with: ollama pull {model_name}");
    }

    Ok(start.elapsed())
}

// ---------------------------------------------------------------------------
// Vision HTTP helper
// ---------------------------------------------------------------------------

/// Send a prompt and a single base64-encoded image to Ollama's `/api/chat` endpoint.
///
/// Ollama's multimodal chat format attaches images as a list of base64 strings
/// on the message object alongside the text content field.
///
/// Returns the model's response text, trimmed of surrounding whitespace.
async fn vision_call(client: &Client, base_url: &str, model: &str, prompt: &str, image_b64: &str) -> anyhow::Result<String> {
    let url = format!("{base_url}/api/chat");
    debug!("[vision_call] POST {url} model={model} image_b64_len={}", image_b64.len());

    let body = json!({
        "model": model,
        "stream": false,
        "messages": [{
            "role": "user",
            "content": prompt,
            // Ollama's vision API accepts a list of base64 image strings here.
            "images": [image_b64]
        }]
    });

    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .context("calling Ollama /api/chat")?;

    let status = resp.status();
    debug!("[vision_call] HTTP status={status}");

    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        debug!("[vision_call] error body: {text}");
        anyhow::bail!("Ollama error {status}: {text}");
    }

    let val: Value = resp.json().await.context("parsing Ollama response")?;
    debug!("[vision_call] response keys: {}", val.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>().join(", ")).unwrap_or_default());

    val["message"]["content"]
        .as_str()
        .map(|s| s.trim().to_string())
        .ok_or_else(|| anyhow::anyhow!("unexpected Ollama response shape: {val}"))
}

// ---------------------------------------------------------------------------
// Response parsing
// ---------------------------------------------------------------------------

/// Parse the raw JSON string returned by Ollama's metadata extraction call into a
/// normalised field map.
///
/// The model sometimes wraps the JSON in markdown fences or adds leading text —
/// the `start`/`end` slice extraction handles those cases by finding the outermost
/// `{` … `}` regardless of surrounding content.
fn parse_fields(
    response: &str,
    fields: &[&crate::schema::FieldDef],
    schema: &SchemaRegistry,
) -> HashMap<String, String> {
    // Find the JSON object boundaries in case the model added prose around it.
    let start = response.find('{').unwrap_or(0);
    let end = response.rfind('}').map(|i| i + 1).unwrap_or(response.len());
    let slice = &response[start..end];

    let mut result = HashMap::new();
    if let Ok(val) = serde_json::from_str::<Value>(slice) {
        for field in fields {
            let raw = val[&field.name].as_str().unwrap_or("");
            let normalised = schema.normalise(field, raw);
            // Same inclusion rule as the Claude backend: required fields are always
            // present in the map, optional fields are omitted when empty.
            if !normalised.is_empty() || field.required {
                result.insert(field.name.clone(), normalised);
            }
        }
    }
    result
}

// ---------------------------------------------------------------------------
// OCR method aggregation
// ---------------------------------------------------------------------------

/// Reduce a list of per-page routing labels to a single summary `ocr_method` string.
///
/// Rules:
/// - All pages used embedded text → `"text-embedded"`.
/// - All image pages used the same path → that path name.
/// - Image pages took different paths → `"mixed:{dominant}"` where `dominant` is
///   the highest-ranked path by the severity scale below.
///
/// Severity scale (higher = more heavyweight processing):
///   text-embedded < tesseract-only < tesseract-ollama-cleanup < ollama-vision
fn aggregate_method(labels: &[&str]) -> String {
    let rank = |l: &str| match l {
        "text-embedded" => 0u8,
        "tesseract-only" => 1,
        "tesseract-ollama-cleanup" => 2,
        "ollama-vision" => 3,
        _ => 0,
    };

    // Exclude text-embedded pages from the image-path analysis.
    let image_labels: Vec<&str> = labels.iter().copied().filter(|l| *l != "text-embedded").collect();

    if image_labels.is_empty() {
        return "text-embedded".to_string();
    }

    let dominant = image_labels.iter().copied().max_by_key(|l| rank(l)).unwrap();
    let all_same = image_labels.iter().all(|l| rank(l) == rank(dominant));

    if all_same {
        dominant.to_string()
    } else {
        format!("mixed:{dominant}")
    }
}

#[cfg(test)]
mod tests {
    use super::{build_ollama_tools, build_ollama_content_blocks};
    use crate::schema::SchemaRegistry;
    use super::PageContent;

    #[test]
    fn build_tools_has_openai_wrapper() {
        let schema = SchemaRegistry::default_schema();
        let tools = build_ollama_tools(&schema, &schema.doc_type_default);
        let tools_arr = tools.as_array().unwrap();
        assert_eq!(tools_arr.len(), 2);
        // Every tool must be wrapped in {type:"function", function:{...}}
        for tool in tools_arr {
            assert_eq!(tool["type"], "function");
            assert!(tool["function"]["name"].is_string());
            assert!(tool["function"]["parameters"]["type"].is_string());
            let func = tool["function"].as_object().unwrap();
            assert!(func.contains_key("parameters"), "tool must use 'parameters' key (not 'input_schema')");
            assert!(!func.contains_key("input_schema"), "must not use Claude's 'input_schema' key");
        }
        // ocr_scan must require page_num
        let ocr = &tools_arr[0];
        assert_eq!(ocr["function"]["name"], "ocr_scan");
        assert!(ocr["function"]["parameters"]["required"]
            .as_array().unwrap()
            .iter().any(|v| v == "page_num"));
        // submit_extraction must require pages
        let submit = &tools_arr[1];
        assert_eq!(submit["function"]["name"], "submit_extraction");
        assert!(submit["function"]["parameters"]["required"]
            .as_array().unwrap()
            .iter().any(|v| v == "pages"));
    }

    #[test]
    fn content_blocks_text_page() {
        let pages = vec![PageContent::Text {
            page_num: 1,
            text: "hello world".to_string(),
        }];
        let blocks = build_ollama_content_blocks(&pages);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "text");
        assert!(blocks[0]["text"].as_str().unwrap().contains("[Page 1 — embedded text]"));
        assert!(blocks[0]["text"].as_str().unwrap().contains("hello world"));
    }

    #[test]
    fn content_blocks_image_page() {
        let pages = vec![PageContent::Image {
            page_num: 2,
            data: vec![0xFF, 0xD8, 0xFF], // minimal JPEG magic bytes
            media_type: "image/jpeg".to_string(),
        }];
        let blocks = build_ollama_content_blocks(&pages);
        // Text label + image_url block
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "text");
        assert!(blocks[0]["text"].as_str().unwrap().contains("ocr_scan(page_num=2)"));
        assert_eq!(blocks[1]["type"], "image_url");
        let url = blocks[1]["image_url"]["url"].as_str().unwrap();
        assert!(url.starts_with("data:image/jpeg;base64,"));
    }
}
