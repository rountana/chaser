use std::time::Duration;

use anyhow::Context;
use reqwest::Client;
use serde_json::{Value, json};

use crate::config::ClaudeConfig;
use crate::schema::FieldType;

pub struct SuggestedField {
    pub name: String,
    pub field_type: FieldType,
    pub required: bool,
    pub searchable: bool,
}

fn build_system_prompt(global_field_names: &[&str]) -> String {
    let excluded = global_field_names.join(", ");
    format!(
        "You are a document schema designer. Your job is to identify the structured metadata fields \
that are worth extracting from a specific type of document for filing and search purposes.\n\n\
The following global fields are already extracted from EVERY document — do NOT include them:\n  \
{excluded}\n\n\
Only suggest fields SPECIFIC to this document type. Use snake_case names.\n\
Choose the most appropriate type for each field:\n  \
freetext   — free-form text (names, identifiers, descriptions)\n  \
person     — a person's full name\n  \
date       — a single date (YYYY-MM-DD)\n  \
date_range — a date range e.g. billing period, coverage period\n  \
currency   — a monetary amount (no symbol)\n\n\
Mark required=true only if the field is virtually always present in this document type.\n\
Mark searchable=true for fields users would filter or search by (e.g. vendor name, policy number).\n\
Call submit_field_suggestions once with your suggestions."
    )
}

/// Single-turn Claude call that suggests per-type schema fields for an unknown document type.
/// `first_page_text` may be empty — Claude falls back to domain knowledge of the doc type.
/// `global_field_names` lists fields already extracted globally (e.g. ["date", "person", "institution"]).
pub async fn call_claude_suggest_fields(
    first_page_text: &str,
    doc_type: &str,
    global_field_names: &[&str],
    config: &ClaudeConfig,
) -> anyhow::Result<Vec<SuggestedField>> {
    let client = reqwest::Client::new();
    let url = format!("{}/v1/messages", config.api_base());

    let user_content = format!(
        "Document type: {doc_type}\n\nFirst page text:\n{first_page_text}"
    );

    let system_prompt = build_system_prompt(global_field_names);
    let fields_desc = format!(
        "Per-type fields to extract. Do not include global fields ({}).",
        global_field_names.join(", ")
    );

    let tools = json!([{
        "name": "submit_field_suggestions",
        "description": "Submit the suggested per-type schema fields for this document type.",
        "input_schema": {
            "type": "object",
            "required": ["fields"],
            "properties": {
                "fields": {
                    "type": "array",
                    "description": fields_desc,
                    "items": {
                        "type": "object",
                        "required": ["name", "type"],
                        "properties": {
                            "name":       { "type": "string",  "description": "snake_case field identifier" },
                            "type":       { "type": "string",  "enum": ["date","date_range","person","currency","freetext"] },
                            "required":   { "type": "boolean", "description": "true only if always present in this doc type" },
                            "searchable": { "type": "boolean", "description": "true for names, vendors, identifiers worth searching" }
                        }
                    }
                }
            }
        }
    }]);

    let body = json!({
        "model": config.model,
        "max_tokens": 512,
        "system": system_prompt,
        "tools": tools,
        "tool_choice": {"type": "any"},
        "messages": [{"role": "user", "content": user_content}]
    });

    let response = client
        .post(&url)
        .header("x-api-key", &config.api_key)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await
        .context("sending field suggestion request to Claude API")?;

    let status = response.status();
    let response_text = response.text().await.context("reading field suggestion response")?;
    if !status.is_success() {
        anyhow::bail!("Claude API error {status}: {response_text}");
    }

    let response_json: Value = serde_json::from_str(&response_text)
        .context("parsing field suggestion response")?;

    let content = response_json["content"]
        .as_array()
        .context("field suggestion response missing content array")?;

    for block in content {
        if block["type"] == "tool_use" && block["name"] == "submit_field_suggestions" {
            let raw_fields = block["input"]["fields"].as_array().cloned().unwrap_or_default();
            return Ok(validate_suggestions(raw_fields, global_field_names));
        }
    }

    // Claude responded without calling the tool — return empty
    Ok(vec![])
}

/// Single-turn Ollama call that suggests per-type schema fields for an unknown document type.
/// Uses the OpenAI-compatible tool format (`parameters` + `{type:"function"}` wrapper).
pub async fn call_ollama_suggest_fields(
    first_page_text: &str,
    doc_type: &str,
    global_field_names: &[&str],
    config: &ClaudeConfig,
) -> anyhow::Result<Vec<SuggestedField>> {
    let client = Client::builder().timeout(Duration::from_secs(60)).build()?;
    let url = format!("{}/api/chat", config.ollama_base());
    let model = config.resolved_ollama_model();

    let user_content = format!(
        "Document type: {doc_type}\n\nFirst page text:\n{first_page_text}"
    );

    let system_prompt = build_system_prompt(global_field_names);
    let fields_desc = format!(
        "Per-type fields to extract. Do not include global fields ({}).",
        global_field_names.join(", ")
    );

    let tools = json!([{
        "type": "function",
        "function": {
            "name": "submit_field_suggestions",
            "description": "Submit the suggested per-type schema fields for this document type.",
            "parameters": {
                "type": "object",
                "required": ["fields"],
                "properties": {
                    "fields": {
                        "type": "array",
                        "description": fields_desc,
                        "items": {
                            "type": "object",
                            "required": ["name", "type"],
                            "properties": {
                                "name":       { "type": "string",  "description": "snake_case field identifier" },
                                "type":       { "type": "string",  "enum": ["date","date_range","person","currency","freetext"] },
                                "required":   { "type": "boolean", "description": "true only if always present in this doc type" },
                                "searchable": { "type": "boolean", "description": "true for names, vendors, identifiers worth searching" }
                            }
                        }
                    }
                }
            }
        }
    }]);

    let body = json!({
        "model": model,
        "stream": false,
        "messages": [
            {"role": "system", "content": system_prompt},
            {"role": "user",   "content": user_content}
        ],
        "tools": tools
    });

    let response = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .context("sending field suggestion request to Ollama")?;

    let status = response.status();
    let response_text = response.text().await.context("reading Ollama field suggestion response")?;
    if !status.is_success() {
        anyhow::bail!("Ollama error {status}: {response_text}");
    }

    let response_json: Value = serde_json::from_str(&response_text)
        .context("parsing Ollama field suggestion response")?;

    let tool_calls = response_json["message"]["tool_calls"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    for call in &tool_calls {
        let name = call["function"]["name"].as_str().unwrap_or("");
        if name != "submit_field_suggestions" {
            continue;
        }
        let raw_fields = match &call["function"]["arguments"] {
            Value::String(s) => serde_json::from_str::<Value>(s)
                .unwrap_or_default()["fields"]
                .as_array()
                .cloned()
                .unwrap_or_default(),
            obj @ Value::Object(_) => obj["fields"].as_array().cloned().unwrap_or_default(),
            _ => vec![],
        };
        return Ok(validate_suggestions(raw_fields, global_field_names));
    }

    Ok(vec![])
}

fn validate_suggestions(raw: Vec<Value>, blocked_names: &[&str]) -> Vec<SuggestedField> {
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();

    for item in raw {
        let raw_name = item["name"].as_str().unwrap_or("").to_string();
        let name = sanitize_field_name(&raw_name);
        if name.is_empty() {
            continue;
        }
        if blocked_names.contains(&name.as_str()) {
            continue;
        }
        if !seen.insert(name.clone()) {
            continue; // deduplicate
        }

        let type_str = item["type"].as_str().unwrap_or("freetext");
        let field_type = parse_suggested_type(type_str);
        let required = item["required"].as_bool().unwrap_or(false);
        let searchable = item["searchable"].as_bool().unwrap_or(false);

        result.push(SuggestedField { name, field_type, required, searchable });
    }

    result
}

fn sanitize_field_name(raw: &str) -> String {
    let lower = raw.to_lowercase();
    let cleaned: String = lower
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    let parts: Vec<&str> = cleaned.split('_').filter(|p| !p.is_empty()).collect();
    parts.join("_")
}

fn parse_suggested_type(s: &str) -> FieldType {
    match s {
        "date" => FieldType::Date,
        "date_range" => FieldType::DateRange,
        "person" => FieldType::Person,
        "currency" => FieldType::Currency,
        _ => FieldType::FreeText,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_field_name_basic() {
        assert_eq!(sanitize_field_name("account_number"), "account_number");
        assert_eq!(sanitize_field_name("Account Number"), "account_number");
        assert_eq!(sanitize_field_name("  due-date  "), "due_date");
        assert_eq!(sanitize_field_name("___foo___"), "foo");
        assert_eq!(sanitize_field_name("!!!"), "");
    }

    #[test]
    fn validate_suggestions_blocks_globals() {
        let raw = vec![
            json!({"name": "date", "type": "date"}),
            json!({"name": "provider", "type": "freetext", "searchable": true}),
            json!({"name": "person", "type": "person"}),
        ];
        let result = validate_suggestions(raw, &["date", "person", "institution"]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "provider");
    }

    #[test]
    fn validate_suggestions_deduplicates() {
        let raw = vec![
            json!({"name": "provider", "type": "freetext"}),
            json!({"name": "provider", "type": "freetext"}),
        ];
        let result = validate_suggestions(raw, &[]);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn validate_suggestions_coerces_unknown_type() {
        let raw = vec![json!({"name": "foo", "type": "blob"})];
        let result = validate_suggestions(raw, &[]);
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0].field_type, FieldType::FreeText));
    }
}
