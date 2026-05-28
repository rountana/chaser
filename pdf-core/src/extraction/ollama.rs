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
use std::time::Duration;

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

/// Replace control characters (except newlines) with spaces.
///
/// Tabs, carriage returns, and other C0/C1 controls embedded in PDF text or OCR output
/// end up in the model's context. When the model reproduces them inside a JSON string
/// literal in its tool-call output, Ollama's parser rejects the whole request with a 500.
fn sanitize_text(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() && c != '\n' { ' ' } else { c })
        .collect()
}

/// Convenience macro for debug-gated eprintln! calls.
/// Using a macro avoids evaluating format arguments when debugging is off.
macro_rules! debug {
    ($($arg:tt)*) => { if debug_enabled() { eprintln!($($arg)*); } }
}

const OLLAMA_SYSTEM_PROMPT: &str = "\
You are a document extraction assistant.

## Document pages

Pages arrive in several labeled forms:

  [Page N — embedded text]
    Text extracted directly from the PDF. Use as-is.

  [Page N — OCR text (conf=NN)]
    Tesseract OCR has already run. conf >= 85 is reliable; 60-84 may have minor errors
    you should correct using context. Do NOT call ocr_scan for these pages.

  [Page N — low OCR confidence (NN); use your vision]
    OCR ran but was unreliable. Read the attached image directly with your vision.
    The low-confidence OCR text is provided as a hint only.

  [Page N — scanned image; call ocr_scan(page_num=N) first]
    OCR has not run yet. Call ocr_scan(page_num=N), then apply the confidence strategy:
    HIGH (>= 85) use verbatim, MEDIUM (60-84) correct errors, LOW (< 60) use vision.

## Final step
After processing ALL pages, call submit_extraction exactly once with the complete structured result.

Confidence thresholds: HIGH = 85.0, MEDIUM = 60.0";

/// Build tool definitions for the agentic extraction loop in Ollama's OpenAI-compatible
/// format: `{type:"function", function:{name, description, parameters}}`.
///
/// `include_ocr_scan` controls whether the `ocr_scan` tool is included. It should be
/// false when all image pages have been pre-OCR'd (most common case), since the model
/// won't need to call it and a smaller tool list improves JSON generation reliability.
///
/// `submit_extraction`'s schema is generated dynamically from the `SchemaRegistry`
/// for the detected `doc_type`, so the model only sees fields relevant to that class.
fn build_ollama_tools(schema: &SchemaRegistry, doc_type: &str, include_ocr_scan: bool) -> Value {
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

    let submit_tool = json!({
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
    });

    if include_ocr_scan {
        json!([
            {
                "type": "function",
                "function": {
                    "name": "ocr_scan",
                    "description": "Run local Tesseract OCR on a scanned image page. \
                                    Returns text and a confidence score (0-100). \
                                    Call this only for pages labeled 'scanned image; call ocr_scan first'.",
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
            submit_tool
        ])
    } else {
        json!([submit_tool])
    }
}

/// Build the initial user message for the agentic extraction loop in Ollama's native format.
///
/// `pre_ocr` contains results from the upfront Tesseract pass run before calling the LLM.
/// HIGH/MEDIUM confidence pages are sent as text labels (no image bytes). LOW confidence
/// pages include the image so the model can use vision directly. Pages absent from `pre_ocr`
/// (OCR failed or unavailable) fall back to the old "call ocr_scan first" path.
///
/// Ollama's `/api/chat` endpoint requires `content` to be a plain string. Images are sent
/// in a separate `"images"` array of raw base64 strings (no data-URI prefix).
fn build_ollama_user_message(pages: &[PageContent], pre_ocr: &HashMap<u32, ocr::OcrResult>) -> Value {
    let mut text_parts: Vec<String> = Vec::new();
    let mut images: Vec<String> = Vec::new();

    for page in pages {
        match page {
            PageContent::Text { page_num, text } => {
                text_parts.push(format!("[Page {page_num} — embedded text]\n{}", sanitize_text(text)));
            }
            PageContent::Image { page_num, data, .. } => {
                if let Some(result) = pre_ocr.get(page_num) {
                    match ocr::OcrPath::from_confidence(result.mean_confidence) {
                        ocr::OcrPath::TesseractOnly | ocr::OcrPath::TesseractLlmCleanup => {
                            // Reliable enough — send as text, no image bytes needed.
                            text_parts.push(format!(
                                "[Page {page_num} — OCR text (conf={:.0})]\n{}",
                                result.mean_confidence,
                                sanitize_text(&result.text)
                            ));
                        }
                        ocr::OcrPath::LlmVision => {
                            // Low confidence — include the image; hint with OCR text.
                            text_parts.push(format!(
                                "[Page {page_num} — low OCR confidence ({:.0}); use your vision]\n{}",
                                result.mean_confidence,
                                sanitize_text(&result.text)
                            ));
                            images.push(BASE64.encode(data));
                        }
                        ocr::OcrPath::SkippedTextPage => unreachable!(),
                    }
                } else {
                    // Pre-OCR unavailable — send image and ask model to call ocr_scan.
                    text_parts.push(format!(
                        "[Page {page_num} — scanned image; call ocr_scan(page_num={page_num}) first]"
                    ));
                    images.push(BASE64.encode(data));
                }
            }
        }
    }

    let content = text_parts.join("\n\n");
    if images.is_empty() {
        json!({"role": "user", "content": content})
    } else {
        json!({"role": "user", "content": content, "images": images})
    }
}

// ---------------------------------------------------------------------------
// Tool call parsing
// ---------------------------------------------------------------------------

struct OllamaToolCall {
    id: String,
    name: String,
    arguments: Value,
}

fn parse_ollama_tool_calls(response: &Value) -> Vec<OllamaToolCall> {
    let tool_calls = match response["message"]["tool_calls"].as_array() {
        Some(arr) => arr,
        None => return vec![],
    };
    tool_calls.iter().filter_map(|call| {
        let id = call["id"].as_str().unwrap_or("").to_string();
        let name = call["function"]["name"].as_str().unwrap_or("").to_string();
        // Ollama may return arguments as a JSON string (OpenAI-compat) or a JSON object
        // (native /api/chat). Handle both to avoid silently losing tool arguments.
        let arguments = match &call["function"]["arguments"] {
            Value::String(s) => {
                serde_json::from_str(s).unwrap_or_else(|_| {
                    debug!("[ollama] tool call '{}': failed to parse arguments JSON string", name);
                    json!({})
                })
            }
            obj @ Value::Object(_) => obj.clone(),
            _ => json!({}),
        };
        if name.is_empty() { None } else { Some(OllamaToolCall { id, name, arguments }) }
    }).collect()
}

fn parse_extraction_input(
    input: &Value,
    doc_type: &str,
    schema: &SchemaRegistry,
) -> anyhow::Result<ExtractionResult> {
    // `pages` is treated as optional here: smaller local models sometimes omit it.
    // The caller patches in fallback page texts if the result comes back empty.
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
        ocr_method: String::new(), // set by call_ollama after the loop
    })
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
    let prompt = format!("{}\n\nDocument text:\n{}", schema.build_type_detection_prompt(), sanitize_text(&first_text));
    let raw = text_call(&client, config.ollama_base(), config.resolved_ollama_model(), &prompt).await?;
    // coerce_doc_type handles known types (fuzzy match) and free-text snake_case
    // labels that the model returns when no known type fits the document.
    Ok(schema.coerce_doc_type(raw.trim()))
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
        // Disable Qwen3 thinking mode — this is a simple classification call and
        // thinking tokens add latency with no accuracy benefit here.
        "think": false,
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

/// Run the agentic tool-calling extraction loop against a local Ollama instance.
///
/// All pages (text labels + base64 images) are sent in a single user message
/// alongside two tool definitions (`ocr_scan`, `submit_extraction`). The model
/// orchestrates its own OCR strategy — calling `ocr_scan` for each image page
/// and routing based on confidence — then submits the structured result via
/// `submit_extraction`. The Rust loop runs until `submit_extraction` is received
/// or the iteration cap is hit.
///
/// The `log` callback receives human-readable progress messages suitable for
/// displaying in a terminal UI.
pub async fn call_ollama(
    pages: &[PageContent],
    config: &ClaudeConfig,
    schema: &SchemaRegistry,
    doc_type: &str,
    log: &dyn Fn(&str),
) -> anyhow::Result<ExtractionResult> {
    let has_images = pages.iter().any(|p| p.is_image());
    if has_images {
        test_connection(config).await.context("Ollama is not available")?;
    }

    // Scale timeout with document size: base 180s + 30s per page, capped at 600s.
    // Large documents (many pages of text or images) need more inference time on local hardware.
    let timeout_secs = (180u64 + pages.len() as u64 * 30).min(600);
    let client = Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .build()?;
    let base_url = config.ollama_base();
    let model = config.resolved_ollama_model();

    debug!("[ollama] base_url={base_url} model={model} pages={}", pages.len());

    let image_map: HashMap<u32, Vec<u8>> = pages.iter()
        .filter_map(|p| {
            if let PageContent::Image { page_num, data, .. } = p {
                Some((*page_num, data.clone()))
            } else {
                None
            }
        })
        .collect();

    // Pre-OCR all image pages concurrently. HIGH/MEDIUM confidence pages become text
    // in the user message (no image bytes sent to Ollama). LOW confidence pages retain
    // their images so the model can apply vision directly.
    let mut pre_ocr: HashMap<u32, ocr::OcrResult> = HashMap::new();
    if !image_map.is_empty() {
        let mut join_set = tokio::task::JoinSet::new();
        for (&page_num, bytes) in &image_map {
            let bytes = bytes.clone();
            join_set.spawn(async move {
                let start = std::time::Instant::now();
                let result = ocr::scan_page(bytes).await;
                (page_num, result, start.elapsed())
            });
        }
        while let Some(join_result) = join_set.join_next().await {
            if let Ok((page_num, Ok(ocr_result), elapsed)) = join_result {
                log(&format!("    page {page_num}: OCR done ({:.1}s, conf={:.0})",
                    elapsed.as_secs_f32(), ocr_result.mean_confidence));
                pre_ocr.insert(page_num, ocr_result);
            }
        }
    }

    // Unprocessed pages (OCR failed) still need the ocr_scan tool as a fallback.
    let has_unprocessed = image_map.keys().any(|p| !pre_ocr.contains_key(p));
    // Each unprocessed page may need up to 2 iterations (ocr_scan + submit); pre-OCR'd
    // pages need at most 1. Cap at 3 as the floor so there's always room for submit.
    let unprocessed_count = image_map.keys().filter(|p| !pre_ocr.contains_key(p)).count();
    let max_iters = std::cmp::max(3, unprocessed_count * 2 + 2);

    let user_message = build_ollama_user_message(pages, &pre_ocr);
    let tools = build_ollama_tools(schema, doc_type, has_unprocessed);

    let mut messages: Vec<Value> = vec![
        json!({"role": "system", "content": OLLAMA_SYSTEM_PROMPT}),
        user_message,
    ];

    let mut confidence_map: HashMap<u32, f32> = HashMap::new();
    let mut extraction_result: Option<ExtractionResult> = None;

    // Fallback page texts: pre-seeded from embedded text pages and pre-OCR results,
    // then augmented as any live ocr_scan results come back.
    let mut page_text_fallback: HashMap<u32, String> = pages.iter().filter_map(|p| {
        if let PageContent::Text { page_num, text } = p {
            Some((*page_num, text.clone()))
        } else {
            None
        }
    }).collect();

    // Seed confidence_map and page_text_fallback from pre-OCR results upfront so
    // aggregate_ocr_method is correct even when the model skips ocr_scan entirely.
    for (&page_num, ocr_result) in &pre_ocr {
        confidence_map.insert(page_num, ocr_result.mean_confidence);
        page_text_fallback.insert(page_num, sanitize_text(&ocr_result.text));
    }

    for iter in 0..max_iters {
        let body = json!({
            "model": model,
            "stream": false,
            // Disable Qwen3 thinking mode — the agentic loop calls this endpoint
            // multiple times per document; thinking tokens per iteration multiply
            // into significant latency with no benefit for structured extraction.
            "think": false,
            "messages": messages,
            "tools": tools
        });

        let response = client
            .post(format!("{base_url}/api/chat"))
            .json(&body)
            .send()
            .await
            .context("sending request to Ollama")?;

        let status = response.status();
        let response_text = response.text().await.context("reading Ollama response")?;
        if !status.is_success() {
            anyhow::bail!("Ollama error {status}: {response_text}");
        }

        let response_json: Value = serde_json::from_str(&response_text)
            .context("parsing Ollama response JSON")?;

        // Append the assistant's turn (including any tool_calls) to maintain context.
        let assistant_message = response_json["message"].clone();
        messages.push(assistant_message);

        let tool_calls = parse_ollama_tool_calls(&response_json);

        if tool_calls.is_empty() {
            // Model stopped without calling any tool.
            if extraction_result.is_none() {
                anyhow::bail!(
                    "Ollama stopped after {} iteration(s) — submit_extraction was never called (did the model exhaust its context?)",
                    iter + 1
                );
            }
            break;
        }

        let mut found_submit = false;

        for tool_call in &tool_calls {
            let result_content = match tool_call.name.as_str() {
                "ocr_scan" => {
                    let page_num = tool_call.arguments["page_num"].as_u64().unwrap_or(0) as u32;
                    // Serve pre-computed result if available — avoids re-running Tesseract.
                    if let Some(cached) = pre_ocr.get(&page_num) {
                        let clean_text = sanitize_text(&cached.text);
                        confidence_map.insert(page_num, cached.mean_confidence);
                        page_text_fallback.insert(page_num, clean_text.clone());
                        log(&format!("    page {page_num}: OCR done (cached, conf={:.0})", cached.mean_confidence));
                        json!({"text": clean_text, "mean_confidence": cached.mean_confidence}).to_string()
                    } else {
                        match image_map.get(&page_num) {
                            Some(bytes) => {
                                match ocr::scan_page(bytes.clone()).await {
                                    Ok(ocr_result) => {
                                        let clean_text = sanitize_text(&ocr_result.text);
                                        confidence_map.insert(page_num, ocr_result.mean_confidence);
                                        page_text_fallback.insert(page_num, clean_text.clone());
                                        log(&format!("    page {page_num}: OCR done (conf={:.0})", ocr_result.mean_confidence));
                                        debug!("[ollama] page={page_num} conf={:.1}", ocr_result.mean_confidence);
                                        json!({"text": clean_text, "mean_confidence": ocr_result.mean_confidence}).to_string()
                                    }
                                    Err(e) => json!({"error": e.to_string()}).to_string(),
                                }
                            }
                            None => json!({
                                "error": format!("page {} is not a scanned image page", page_num)
                            }).to_string(),
                        }
                    }
                }
                "submit_extraction" => {
                    match parse_extraction_input(&tool_call.arguments, doc_type, schema) {
                        Ok(result) => extraction_result = Some(result),
                        Err(e) => return Err(e),
                    }
                    log("    submit_extraction received — extraction complete");
                    found_submit = true;
                    "{\"status\":\"accepted\"}".to_string()
                }
                other => {
                    json!({"error": format!("unknown tool: {}", other)}).to_string()
                }
            };

            // Each tool result is a separate message in Ollama's OpenAI-compat format.
            messages.push(json!({
                "role": "tool",
                "tool_call_id": tool_call.id,
                "content": result_content
            }));
        }

        if found_submit {
            break;
        }
    }

    let mut result = extraction_result
        .context("max iterations reached without a submit_extraction call")?;

    // If the model omitted the pages array, synthesize it from collected OCR/text.
    if result.pages.is_empty() && !page_text_fallback.is_empty() {
        let mut page_nums: Vec<u32> = page_text_fallback.keys().cloned().collect();
        page_nums.sort_unstable();
        result.pages = page_nums.into_iter().map(|n| PageText {
            page_num: n,
            text: page_text_fallback[&n].clone(),
        }).collect();
    }

    let paths: Vec<ocr::OcrPath> = pages.iter().map(|p| {
        match p {
            PageContent::Text { .. } => ocr::OcrPath::SkippedTextPage,
            PageContent::Image { page_num, .. } => {
                match confidence_map.get(page_num) {
                    Some(&conf) => ocr::OcrPath::from_confidence(conf),
                    None => ocr::OcrPath::LlmVision, // model used vision without calling ocr_scan
                }
            }
        }
    }).collect();

    result.ocr_method = ocr::aggregate_ocr_method(&paths);
    Ok(result)
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


#[cfg(test)]
mod tests {
    use super::{build_ollama_tools, build_ollama_user_message, parse_ollama_tool_calls, parse_extraction_input, call_ollama};
    use crate::schema::SchemaRegistry;
    use super::{PageContent, ocr};
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn build_tools_has_openai_wrapper() {
        let schema = SchemaRegistry::default_schema();
        // include_ocr_scan=true: both tools present
        let tools = build_ollama_tools(&schema, &schema.doc_type_default, true);
        let tools_arr = tools.as_array().unwrap();
        assert_eq!(tools_arr.len(), 2);
        for tool in tools_arr {
            assert_eq!(tool["type"], "function");
            assert!(tool["function"]["name"].is_string());
            assert!(tool["function"]["parameters"]["type"].is_string());
            let func = tool["function"].as_object().unwrap();
            assert!(func.contains_key("parameters"), "tool must use 'parameters' key (not 'input_schema')");
            assert!(!func.contains_key("input_schema"), "must not use Claude's 'input_schema' key");
        }
        let ocr_tool = &tools_arr[0];
        assert_eq!(ocr_tool["function"]["name"], "ocr_scan");
        assert!(ocr_tool["function"]["parameters"]["required"]
            .as_array().unwrap()
            .iter().any(|v| v == "page_num"));
        let submit = &tools_arr[1];
        assert_eq!(submit["function"]["name"], "submit_extraction");
        assert!(submit["function"]["parameters"]["required"]
            .as_array().unwrap()
            .iter().any(|v| v == "pages"));
    }

    #[test]
    fn build_tools_omits_ocr_scan_when_all_preocrd() {
        let schema = SchemaRegistry::default_schema();
        // include_ocr_scan=false: only submit_extraction
        let tools = build_ollama_tools(&schema, &schema.doc_type_default, false);
        let tools_arr = tools.as_array().unwrap();
        assert_eq!(tools_arr.len(), 1);
        assert_eq!(tools_arr[0]["function"]["name"], "submit_extraction");
    }

    #[test]
    fn user_message_text_page() {
        let pages = vec![PageContent::Text {
            page_num: 1,
            text: "hello world".to_string(),
        }];
        let msg = build_ollama_user_message(&pages, &HashMap::new());
        assert_eq!(msg["role"], "user");
        let content = msg["content"].as_str().unwrap();
        assert!(content.contains("[Page 1 — embedded text]"));
        assert!(content.contains("hello world"));
        assert!(msg.get("images").is_none() || msg["images"].as_array().map(|a| a.is_empty()).unwrap_or(true));
    }

    #[test]
    fn user_message_image_page_no_preocr_sends_image_with_ocr_scan_label() {
        // When pre_ocr is empty, falls back to "call ocr_scan first" path.
        let pages = vec![PageContent::Image {
            page_num: 2,
            data: vec![0xFF, 0xD8, 0xFF],
            media_type: "image/jpeg".to_string(),
        }];
        let msg = build_ollama_user_message(&pages, &HashMap::new());
        assert_eq!(msg["role"], "user");
        let content = msg["content"].as_str().unwrap();
        assert!(content.contains("ocr_scan(page_num=2)"), "fallback path should reference ocr_scan");
        let images = msg["images"].as_array().unwrap();
        assert_eq!(images.len(), 1);
        let b64 = images[0].as_str().unwrap();
        assert!(!b64.starts_with("data:"), "Ollama images must be raw base64, not a data URI");
    }

    #[test]
    fn user_message_image_page_high_confidence_sends_no_image() {
        // HIGH confidence pre-OCR: model receives text only, no image bytes.
        let pages = vec![PageContent::Image {
            page_num: 1,
            data: vec![0xFF, 0xD8, 0xFF],
            media_type: "image/jpeg".to_string(),
        }];
        let mut pre_ocr = HashMap::new();
        pre_ocr.insert(1u32, ocr::OcrResult {
            text: "Invoice total: $100".to_string(),
            mean_confidence: 90.0,
            words: vec![],
        });
        let msg = build_ollama_user_message(&pages, &pre_ocr);
        let content = msg["content"].as_str().unwrap();
        assert!(content.contains("OCR text (conf=90)"), "should use OCR text label");
        assert!(content.contains("Invoice total: $100"));
        assert!(msg.get("images").is_none() || msg["images"].as_array().map(|a| a.is_empty()).unwrap_or(true),
            "no image should be sent for high-confidence pages");
    }

    #[test]
    fn user_message_image_page_low_confidence_sends_image() {
        // LOW confidence pre-OCR: model still receives the image for direct vision.
        let pages = vec![PageContent::Image {
            page_num: 1,
            data: vec![0xFF, 0xD8, 0xFF],
            media_type: "image/jpeg".to_string(),
        }];
        let mut pre_ocr = HashMap::new();
        pre_ocr.insert(1u32, ocr::OcrResult {
            text: "garbled text".to_string(),
            mean_confidence: 35.0,
            words: vec![],
        });
        let msg = build_ollama_user_message(&pages, &pre_ocr);
        let content = msg["content"].as_str().unwrap();
        assert!(content.contains("low OCR confidence"), "should flag low confidence");
        let images = msg["images"].as_array().unwrap();
        assert_eq!(images.len(), 1, "image must be sent for low-confidence pages");
    }

    #[test]
    fn parse_tool_calls_returns_empty_when_none() {
        let response = json!({"message": {"role": "assistant", "content": "hi", "tool_calls": null}});
        let calls = parse_ollama_tool_calls(&response);
        assert!(calls.is_empty());
    }

    #[test]
    fn parse_tool_calls_parses_string_arguments() {
        let response = json!({
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "ocr_scan",
                        "arguments": "{\"page_num\": 3}"
                    }
                }]
            }
        });
        let calls = parse_ollama_tool_calls(&response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "ocr_scan");
        assert_eq!(calls[0].arguments["page_num"], 3);
    }

    #[test]
    fn parse_tool_calls_parses_object_arguments() {
        let response = json!({
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call_2",
                    "type": "function",
                    "function": {
                        "name": "ocr_scan",
                        "arguments": {"page_num": 5}
                    }
                }]
            }
        });
        let calls = parse_ollama_tool_calls(&response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "ocr_scan");
        assert_eq!(calls[0].arguments["page_num"], 5);
    }

    #[test]
    fn parse_tool_calls_skips_empty_name() {
        let response = json!({
            "message": {
                "tool_calls": [
                    {"id": "x", "function": {"name": "", "arguments": "{}"}},
                    {"id": "y", "function": {"name": "ocr_scan", "arguments": "{\"page_num\": 1}"}}
                ]
            }
        });
        let calls = parse_ollama_tool_calls(&response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "ocr_scan");
    }

    #[test]
    fn parse_tool_calls_returns_empty_for_empty_array() {
        let response = json!({"message": {"role": "assistant", "content": "hi", "tool_calls": []}});
        let calls = parse_ollama_tool_calls(&response);
        assert!(calls.is_empty());
    }

    #[test]
    fn parse_extraction_input_maps_pages_and_fields() {
        let schema = SchemaRegistry::default_schema();
        let doc_type = schema.doc_type_default.clone();
        let input = json!({
            "pages": [{"page_num": 1, "text": "Sample text"}]
        });
        let result = parse_extraction_input(&input, &doc_type, &schema).unwrap();
        assert_eq!(result.pages.len(), 1);
        assert_eq!(result.pages[0].page_num, 1);
        assert_eq!(result.pages[0].text, "Sample text");
        assert_eq!(result.doc_type, doc_type);
        assert!(result.ocr_method.is_empty()); // filled in by caller
    }

    #[test]
    fn parse_extraction_input_returns_empty_pages_when_missing() {
        // Smaller local models sometimes omit the pages array; we treat it as empty
        // rather than an error so the caller can patch in fallback OCR text.
        let schema = SchemaRegistry::default_schema();
        let input = json!({"some_field": "value"});
        let result = parse_extraction_input(&input, &schema.doc_type_default, &schema).unwrap();
        assert!(result.pages.is_empty());
    }

    #[tokio::test]
    #[ignore = "requires local Ollama with qwen2.5vl:7b pulled — run: PDF_LAB_DEBUG=1 cargo test -p pdf-core -- live_agentic_loop --include-ignored --nocapture"]
    async fn live_agentic_loop_text_only_doc() {
        // Uses a text-only page — no Tesseract required, no image upload.
        // Verifies the loop terminates and submit_extraction is called.
        let config = crate::config::ClaudeConfig::default();
        let schema = SchemaRegistry::default_schema();
        let doc_type = schema.doc_type_default.clone();

        let pages = vec![PageContent::Text {
            page_num: 1,
            text: "Name: Arjun Sharma\nDate: 12/05/2023\nAmount: INR 5,000".to_string(),
        }];

        let log = |msg: &str| eprintln!("{msg}");
        let result = call_ollama(&pages, &config, &schema, &doc_type, &log).await;
        assert!(result.is_ok(), "call_ollama failed: {:?}", result.err());

        let extraction = result.unwrap();
        assert_eq!(extraction.pages.len(), 1);
        assert_eq!(extraction.pages[0].page_num, 1, "model should return page 1");
        assert!(!extraction.pages[0].text.is_empty(), "model should extract page text");
        assert_eq!(extraction.ocr_method, "text-embedded", "text-only docs should use text-embedded path");
        assert!(!extraction.fields.is_empty(), "model should have extracted at least one structured field");
    }
}
