use anyhow::Context;
use serde_json::{Value, json};

use crate::config::ClaudeConfig;
use super::{PageContent, PageText};
use super::gemini::build_generation_config;

/// Pass 2 (text pipeline): single-turn LLM call that reformats tables and
/// columnar content in page text as markdown tables, leaving prose unchanged.
/// Returns the same pages with updated text; image pages are passed through.
pub async fn call_claude_table_reformat(
    pages: &[PageContent],
    config: &ClaudeConfig,
) -> anyhow::Result<Vec<PageContent>> {
    let text_pages: Vec<&PageContent> = pages.iter()
        .filter(|p| !p.is_image())
        .collect();

    if text_pages.is_empty() {
        return Ok(pages.to_vec());
    }

    let combined: String = text_pages.iter()
        .map(|p| {
            if let PageContent::Text { page_num, text } = p {
                format!("[Page {page_num}]\n{text}")
            } else {
                String::new()
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    let client = reqwest::Client::new();
    let url = format!("{}/v1/messages", config.api_base());

    let system = "You reformat extracted PDF text. For each page:\n\
        - Convert tab/space-aligned tables and columnar data into markdown tables (| col | col |)\n\
        - Convert form fields to 'Label: Value' pairs\n\
        - Leave prose paragraphs, headings, and lists unchanged\n\
        Call submit_pages once with all pages.";

    let page_schema = {
        let mut props = serde_json::Map::new();
        props.insert("page_num".to_string(), json!({"type": "integer"}));
        props.insert("text".to_string(), json!({"type": "string", "description": "Reformatted page text"}));
        props
    };

    let tools = json!([{
        "name": "submit_pages",
        "description": "Submit all reformatted pages.",
        "input_schema": {
            "type": "object",
            "required": ["pages"],
            "properties": {
                "pages": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["page_num", "text"],
                        "properties": page_schema
                    }
                }
            }
        }
    }]);

    let body = json!({
        "model": config.model,
        "max_tokens": 4096,
        "system": system,
        "tools": tools,
        "tool_choice": {"type": "any"},
        "messages": [{"role": "user", "content": combined}]
    });

    let response = client
        .post(&url)
        .header("x-api-key", &config.api_key)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await
        .context("sending table-reformat request to Claude API")?;

    let status = response.status();
    let response_text = response.text().await.context("reading table-reformat response")?;
    if !status.is_success() {
        anyhow::bail!("Claude API error {status}: {response_text}");
    }

    let response_json: Value = serde_json::from_str(&response_text)
        .context("parsing table-reformat response")?;

    let content = response_json["content"].as_array()
        .context("table-reformat response missing content array")?;

    for block in content {
        if block["type"] == "tool_use" && block["name"] == "submit_pages" {
            if let Some(reformatted) = block["input"]["pages"].as_array() {
                return Ok(merge_reformatted(pages, reformatted));
            }
        }
    }

    // LLM didn't call the tool — pass pages through unchanged
    Ok(pages.to_vec())
}

/// Pass 2 (text pipeline) using the Gemini API.
pub async fn call_gemini_table_reformat(
    pages: &[PageContent],
    config: &ClaudeConfig,
) -> anyhow::Result<Vec<PageContent>> {
    let text_pages: Vec<&PageContent> = pages.iter()
        .filter(|p| !p.is_image())
        .collect();

    if text_pages.is_empty() {
        return Ok(pages.to_vec());
    }

    let combined: String = text_pages.iter()
        .map(|p| {
            if let PageContent::Text { page_num, text } = p {
                format!("[Page {page_num}]\n{text}")
            } else {
                String::new()
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;

    let model = config.resolved_gemini_model();
    let key = config.gemini_api_key.as_deref().unwrap_or("");
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
        model, key
    );

    let system_text = "You reformat extracted PDF text. For each page:\n\
        - Convert tab/space-aligned tables and columnar data into markdown tables (| col | col |)\n\
        - Convert form fields to 'Label: Value' pairs\n\
        - Leave prose paragraphs, headings, and lists unchanged\n\
        Call submit_pages once with all pages.";

    let tools = json!([{
        "function_declarations": [{
            "name": "submit_pages",
            "description": "Submit all reformatted pages.",
            "parameters": {
                "type": "OBJECT",
                "required": ["pages"],
                "properties": {
                    "pages": {
                        "type": "ARRAY",
                        "items": {
                            "type": "OBJECT",
                            "required": ["page_num", "text"],
                            "properties": {
                                "page_num": {"type": "INTEGER"},
                                "text": {"type": "STRING", "description": "Reformatted page text"}
                            }
                        }
                    }
                }
            }
        }]
    }]);

    let safety_settings = json!([
        {"category": "HARM_CATEGORY_HARASSMENT", "threshold": "BLOCK_NONE"},
        {"category": "HARM_CATEGORY_HATE_SPEECH", "threshold": "BLOCK_NONE"},
        {"category": "HARM_CATEGORY_SEXUALLY_EXPLICIT", "threshold": "BLOCK_NONE"},
        {"category": "HARM_CATEGORY_DANGEROUS_CONTENT", "threshold": "BLOCK_NONE"}
    ]);

    let body = json!({
        "systemInstruction": {"parts": [{"text": system_text}]},
        "contents": [{"role": "user", "parts": [{"text": combined}]}],
        "tools": tools,
        "toolConfig": {"functionCallingConfig": {"mode": "ANY"}},
        "safetySettings": safety_settings,
        "generationConfig": build_generation_config(&model, 4096)
    });

    let response = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .context("sending table-reformat request to Gemini API")?;

    let status = response.status();
    let response_text = response.text().await.context("reading Gemini table-reformat response")?;
    if !status.is_success() {
        anyhow::bail!("Gemini API error {status}: {response_text}");
    }

    let response_json: Value = serde_json::from_str(&response_text)
        .context("parsing Gemini table-reformat response")?;

    if let Some(candidates) = response_json["candidates"].as_array() {
        if let Some(candidate) = candidates.first() {
            if let Some(parts) = candidate["content"]["parts"].as_array() {
                for part in parts {
                    if let Some(fc) = part.get("functionCall") {
                        if fc["name"].as_str() == Some("submit_pages") {
                            if let Some(reformatted) = fc["args"]["pages"].as_array() {
                                return Ok(merge_reformatted(pages, reformatted));
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(pages.to_vec())
}

/// Merge LLM-reformatted page texts back into the original page slice.
/// Image pages keep their original content; text pages get updated text.
fn merge_reformatted(original: &[PageContent], reformatted: &[Value]) -> Vec<PageContent> {
    // Build a lookup: page_num → reformatted text
    let mut reformatted_map: std::collections::HashMap<u32, String> =
        std::collections::HashMap::new();
    for item in reformatted {
        if let (Some(num), Some(text)) = (
            item["page_num"].as_u64(),
            item["text"].as_str(),
        ) {
            reformatted_map.insert(num as u32, text.to_string());
        }
    }

    original.iter().map(|p| match p {
        PageContent::Text { page_num, text } => {
            let updated = reformatted_map
                .get(page_num)
                .cloned()
                .unwrap_or_else(|| text.clone());
            PageContent::Text { page_num: *page_num, text: updated }
        }
        PageContent::Image { .. } => p.clone(),
    }).collect()
}

/// Convert page contents to `PageText` structs (for consumers that only need text).
pub fn pages_to_page_texts(pages: &[PageContent]) -> Vec<PageText> {
    pages.iter().filter_map(|p| {
        if let PageContent::Text { page_num, text } = p {
            Some(PageText { page_num: *page_num, text: text.clone() })
        } else {
            None
        }
    }).collect()
}
