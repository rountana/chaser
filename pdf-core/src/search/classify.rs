use anyhow::Context;
use serde_json::{Value, json};

use crate::config::{ClaudeConfig, LlmBackend};
use super::Backend;

/// Call the configured LLM backend with classify_backends tool to resolve ambiguous queries (router R6).
/// Falls back to metadata search if the backend is Ollama or if the call fails.
pub async fn classify_backends(
    query: &str,
    known_persons: &[String],
    doc_type_values: &[String],
    config: &ClaudeConfig,
) -> anyhow::Result<Vec<Backend>> {
    if config.backend == LlmBackend::Ollama {
        anyhow::bail!("classify_backends not supported for ollama backend; falling back to metadata");
    }

    if config.backend == LlmBackend::Gemini {
        // Gemini does not support forced tool_choice; fall back to metadata routing.
        anyhow::bail!("classify_backends not supported for gemini backend; falling back to metadata");
    }

    let client = reqwest::Client::new();
    let url = format!("{}/v1/messages", config.api_base());

    let classify_tool = json!({
        "name": "classify_backends",
        "description": "Classify which search backends should handle this query.",
        "input_schema": {
            "type": "object",
            "required": ["backends", "reasoning"],
            "properties": {
                "backends": {
                    "type": "array",
                    "items": {
                        "type": "string",
                        "enum": ["metadata", "structural", "semantic"]
                    },
                    "minItems": 1
                },
                "reasoning": {
                    "type": "string",
                    "description": "One sentence explanation"
                }
            }
        }
    });

    let known_types: Vec<&str> = doc_type_values
        .iter()
        .filter(|s| s.as_str() != "unknown")
        .map(|s| s.as_str())
        .collect();

    let prompt = format!(
        "Query: \"{query}\"\nKnown persons in the document library: {}\nKnown document types: {}\n\
         Classify which search backends should handle this query.",
        if known_persons.is_empty() { "none".to_string() } else { known_persons.join(", ") },
        known_types.join(", ")
    );

    let body = json!({
        "model": config.model,
        "max_tokens": 64,
        "messages": [{"role": "user", "content": prompt}],
        "tools": [classify_tool],
        "tool_choice": {"type": "tool", "name": "classify_backends"}
    });

    let response = client
        .post(&url)
        .header("x-api-key", &config.api_key)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await
        .context("sending classify_backends request to Claude API")?;

    let status = response.status();
    let text = response.text().await.context("reading classify_backends response")?;

    if !status.is_success() {
        anyhow::bail!("Claude API error {status}: {text}");
    }

    let resp: Value = serde_json::from_str(&text).context("parsing classify_backends response")?;

    let tool_input = resp["content"]
        .as_array()
        .and_then(|arr| arr.iter().find(|b| b["type"].as_str() == Some("tool_use")))
        .and_then(|b| b.get("input"))
        .ok_or_else(|| anyhow::anyhow!("No tool_use block in classify_backends response"))?;

    let backends: Vec<Backend> = tool_input["backends"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|v| match v.as_str() {
            Some("metadata") => Some(Backend::Metadata),
            Some("structural") => Some(Backend::Structural),
            Some("semantic") => Some(Backend::Semantic),
            _ => None,
        })
        .collect();

    anyhow::ensure!(!backends.is_empty(), "classify_backends returned no valid backends");
    Ok(backends)
}
