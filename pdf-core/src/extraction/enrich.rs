use anyhow::Context;
use serde_json::{Value, json};

use crate::config::ClaudeConfig;
use super::PageText;
use super::gemini::build_generation_config;

pub struct EnrichmentResult {
    pub entities: Vec<Value>,
    pub key_info: serde_json::Map<String, Value>,
}

/// Pass 3: single-turn Claude call on already-extracted page text.
/// Returns entities (list of {name, role}) and key_info (doc-type-specific facts).
/// Uses no images — Pass 2 already produced clean text.
pub async fn call_claude_enrich(
    page_texts: &[PageText],
    doc_type: &str,
    config: &ClaudeConfig,
) -> anyhow::Result<EnrichmentResult> {
    let body_text: String = page_texts
        .iter()
        .map(|p| format!("[Page {}]\n{}", p.page_num, p.text))
        .collect::<Vec<_>>()
        .join("\n\n");

    if body_text.trim().is_empty() {
        return Ok(EnrichmentResult {
            entities: vec![],
            key_info: serde_json::Map::new(),
        });
    }

    let client = reqwest::Client::new();
    let url = format!("{}/v1/messages", config.api_base());

    let system = format!(
        "Analyse the provided {doc_type} document. Extract the key entities (people, organisations, \
        authorities) and the most important facts specific to this document type. \
        Call submit_enrichment once with your findings."
    );

    let user_content = format!("Document type: {doc_type}\n\nDocument content:\n{body_text}");

    let tools = json!([{
        "name": "submit_enrichment",
        "description": "Submit extracted entities and key information from the document.",
        "input_schema": {
            "type": "object",
            "required": ["entities", "key_info"],
            "properties": {
                "entities": {
                    "type": "array",
                    "description": "Key people, organisations, and authorities in the document with their role.",
                    "items": {
                        "type": "object",
                        "required": ["name", "role"],
                        "properties": {
                            "name": {"type": "string", "description": "Full name of the entity"},
                            "role": {"type": "string", "description": "Role in the document, e.g. owner, issuing_authority, signatory, buyer, seller, card_holder"}
                        }
                    }
                },
                "key_info": {
                    "type": "object",
                    "description": "Most important facts for this document type (IDs, numbers, amounts, dates, addresses). Values must be strings.",
                    "additionalProperties": {"type": "string"}
                }
            }
        }
    }]);

    let body = json!({
        "model": config.model,
        "max_tokens": 1024,
        "system": system,
        "tools": tools,
        "tool_choice": {"type": "auto"},
        "messages": [{"role": "user", "content": user_content}]
    });

    let response = client
        .post(&url)
        .header("x-api-key", &config.api_key)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await
        .context("sending enrichment request to Claude API")?;

    let status = response.status();
    let response_text = response.text().await.context("reading enrichment response")?;
    if !status.is_success() {
        anyhow::bail!("Claude API error {status}: {response_text}");
    }

    let response_json: Value = serde_json::from_str(&response_text)
        .context("parsing enrichment response")?;

    let content = response_json["content"]
        .as_array()
        .context("enrichment response missing content array")?;

    for block in content {
        if block["type"] == "tool_use" && block["name"] == "submit_enrichment" {
            let input = &block["input"];
            let entities = input["entities"].as_array().cloned().unwrap_or_default();
            let key_info = input["key_info"].as_object().cloned().unwrap_or_default();
            return Ok(EnrichmentResult { entities, key_info });
        }
    }

    // Claude responded without calling the tool — return empty result
    Ok(EnrichmentResult {
        entities: vec![],
        key_info: serde_json::Map::new(),
    })
}

/// Pass 3 enrichment using the Gemini API.
///
/// Mirrors `call_claude_enrich` but uses Gemini's function calling protocol:
/// UPPERCASE schema types, `function_declarations` wrapper, `systemInstruction`,
/// and `functionCall`/`functionResponse` parts. Safety settings are set to BLOCK_NONE
/// to prevent PII in document content from triggering filters.
pub async fn call_gemini_enrich(
    page_texts: &[PageText],
    doc_type: &str,
    config: &ClaudeConfig,
) -> anyhow::Result<EnrichmentResult> {
    let body_text: String = page_texts
        .iter()
        .map(|p| format!("[Page {}]\n{}", p.page_num, p.text))
        .collect::<Vec<_>>()
        .join("\n\n");

    if body_text.trim().is_empty() {
        return Ok(EnrichmentResult {
            entities: vec![],
            key_info: serde_json::Map::new(),
        });
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;

    let model = config.resolved_gemini_model();
    let key = config.gemini_api_key.as_deref().unwrap_or("");
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
        model, key
    );

    let system_text = format!(
        "Analyse the provided {doc_type} document. Extract the key entities (people, organisations, \
        authorities) and the most important facts specific to this document type. \
        Call submit_enrichment once with your findings."
    );

    let user_content = format!("Document type: {doc_type}\n\nDocument content:\n{body_text}");

    let tools = json!([{
        "function_declarations": [{
            "name": "submit_enrichment",
            "description": "Submit extracted entities and key information from the document.",
            "parameters": {
                "type": "OBJECT",
                "required": ["entities", "key_info"],
                "properties": {
                    "entities": {
                        "type": "ARRAY",
                        "description": "Key people, organisations, and authorities in the document with their role.",
                        "items": {
                            "type": "OBJECT",
                            "required": ["name", "role"],
                            "properties": {
                                "name": {"type": "STRING", "description": "Full name of the entity"},
                                "role": {"type": "STRING", "description": "Role in the document, e.g. owner, issuing_authority, signatory, buyer, seller, card_holder"}
                            }
                        }
                    },
                    "key_info": {
                        "type": "ARRAY",
                        "description": "Most important facts for this document type (IDs, numbers, amounts, dates, addresses).",
                        "items": {
                            "type": "OBJECT",
                            "required": ["key", "value"],
                            "properties": {
                                "key": {"type": "STRING", "description": "Fact name"},
                                "value": {"type": "STRING", "description": "Fact value"}
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
        "contents": [{"role": "user", "parts": [{"text": user_content}]}],
        "tools": tools,
        "toolConfig": {"functionCallingConfig": {"mode": "AUTO"}},
        "safetySettings": safety_settings,
        "generationConfig": build_generation_config(&model, 1024)
    });

    let response = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .context("sending enrichment request to Gemini API")?;

    let status = response.status();
    let response_text = response.text().await.context("reading enrichment response")?;
    if !status.is_success() {
        anyhow::bail!("Gemini API error {status}: {response_text}");
    }

    let response_json: Value = serde_json::from_str(&response_text)
        .context("parsing Gemini enrichment response")?;

    let candidates = response_json["candidates"].as_array();
    if let Some(candidates) = candidates {
        if let Some(candidate) = candidates.first() {
            if let Some(parts) = candidate["content"]["parts"].as_array() {
                for part in parts {
                    if let Some(fc) = part.get("functionCall") {
                        if fc["name"].as_str() == Some("submit_enrichment") {
                            let args = &fc["args"];
                            let entities = args["entities"].as_array().cloned().unwrap_or_default();
                            let key_info = args["key_info"].as_array()
                                .map(|arr| {
                                    arr.iter().filter_map(|item| {
                                        let k = item["key"].as_str()?.to_string();
                                        let v = Value::String(item["value"].as_str().unwrap_or("").to_string());
                                        Some((k, v))
                                    }).collect::<serde_json::Map<_, _>>()
                                })
                                .unwrap_or_default();
                            return Ok(EnrichmentResult { entities, key_info });
                        }
                    }
                }
            }
        }
    }

    Ok(EnrichmentResult {
        entities: vec![],
        key_info: serde_json::Map::new(),
    })
}
