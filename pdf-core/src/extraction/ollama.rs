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

/// Convenience macro for debug-gated eprintln! calls.
/// Using a macro avoids evaluating format arguments when debugging is off.
macro_rules! debug {
    ($($arg:tt)*) => { if debug_enabled() { eprintln!($($arg)*); } }
}

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
fn build_ollama_content_blocks(pages: &[PageContent]) -> Vec<Value> {
    // Output is for the OpenAI-compat /api/chat endpoint (Ollama format).
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
    let pages: Vec<PageText> = input["pages"]
        .as_array()
        .context("missing pages array in submit_extraction")?
        .iter()
        .map(|p| Ok(PageText {
            page_num: p["page_num"].as_u64().unwrap_or(0) as u32,
            text: p["text"].as_str().unwrap_or("").to_string(),
        }))
        .collect::<anyhow::Result<Vec<_>>>()?;

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

    // 120s per turn: vision + thinking can be slow on local hardware.
    let client = Client::builder()
        .timeout(Duration::from_secs(120))
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

    let image_page_count = image_map.len();
    let max_iters = std::cmp::max(5, image_page_count * 2 + 2);

    let content_blocks = build_ollama_content_blocks(pages);
    let tools = build_ollama_tools(schema, doc_type);

    let mut messages: Vec<Value> = vec![
        json!({"role": "system", "content": OLLAMA_SYSTEM_PROMPT}),
        json!({"role": "user", "content": content_blocks}),
    ];

    let mut confidence_map: HashMap<u32, f32> = HashMap::new();
    let mut extraction_result: Option<ExtractionResult> = None;

    for iter in 0..max_iters {
        let body = json!({
            "model": model,
            "stream": false,
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
                    "Ollama stopped without calling submit_extraction after {} iteration(s)",
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
                    log(&format!("    page {page_num}: OCR..."));
                    match image_map.get(&page_num) {
                        Some(bytes) => {
                            match ocr::scan_page(bytes.clone()).await {
                                Ok(ocr_result) => {
                                    confidence_map.insert(page_num, ocr_result.mean_confidence);
                                    debug!("[ollama] page={page_num} conf={:.1}", ocr_result.mean_confidence);
                                    serde_json::to_string(&ocr_result).unwrap_or_default()
                                }
                                Err(e) => json!({"error": e.to_string()}).to_string(),
                            }
                        }
                        None => json!({
                            "error": format!("page {} is not a scanned image page", page_num)
                        }).to_string(),
                    }
                }
                "submit_extraction" => {
                    match parse_extraction_input(&tool_call.arguments, doc_type, schema) {
                        Ok(result) => extraction_result = Some(result),
                        Err(e) => return Err(e),
                    }
                    found_submit = true;
                    "{\"status\":\"accepted\"}".to_string()
                }
                other => {
                    format!("{{\"error\":\"unknown tool: {}\"}}", other)
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
    use super::{build_ollama_tools, build_ollama_content_blocks, parse_ollama_tool_calls, parse_extraction_input};
    use crate::schema::SchemaRegistry;
    use super::PageContent;
    use serde_json::json;

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
    fn parse_extraction_input_fails_on_missing_pages() {
        let schema = SchemaRegistry::default_schema();
        let input = json!({"some_field": "value"});
        let err = parse_extraction_input(&input, &schema.doc_type_default, &schema);
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("missing pages array"));
    }
}
