pub mod filename;

use std::collections::HashMap;
use std::path::Path;

use crate::extraction::{ExtractionResult, PageContent, enrich::EnrichmentResult};
use crate::schema::{FieldType, SchemaRegistry};

/// Generate a `.md` file string from an extraction result.
///
/// Field ordering: title → doc_type → global schema fields → per-type fields →
/// enrichment (entities, key_info) → protected tail.
/// Required fields are always written. Optional fields are omitted if empty after all fallbacks.
pub fn generate_md(
    result: &ExtractionResult,
    source_path: &Path,
    page_contents: &[PageContent],
    schema: &SchemaRegistry,
    known_persons: &[String],
    enrichment: Option<&EnrichmentResult>,
    elapsed_ms: u64,
) -> String {
    let stem = source_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");

    let page_count = page_contents.len();
    let doc_type = &result.doc_type;

    // Resolve each schema field: LLM value → filename fallback → empty
    let mut resolved: HashMap<String, String> = HashMap::new();
    for field in schema.effective_fields(doc_type) {
        let llm_val = result.fields.get(&field.name).map(|s| s.as_str()).unwrap_or("");
        let val = if llm_val.is_empty() {
            apply_filename_fallback(field, stem, known_persons, &schema.doc_type_values)
        } else {
            llm_val.to_string()
        };
        resolved.insert(field.name.clone(), val);
    }

    let title = derive_title(stem, doc_type, &resolved, schema);
    let source_abs = source_path
        .canonicalize()
        .unwrap_or_else(|_| source_path.to_path_buf());
    let extracted_at = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let mut out = String::new();

    // YAML frontmatter
    out.push_str("---\n");
    out.push_str(&format!("title: {}\n", yaml_value(&title)));
    out.push_str(&format!("doc_type: {doc_type}\n"));
    out.push_str(&format!("doc_category: {}\n", result.doc_category));

    for field in schema.effective_fields(doc_type) {
        let val = resolved.get(&field.name).map(|s| s.as_str()).unwrap_or("");
        if field.required || !val.is_empty() {
            out.push_str(&format!("{}: {}\n", field.name, yaml_value(val)));
        }
    }

    // Enrichment — entities and key_info, inserted before protected tail
    if let Some(e) = enrichment {
        if !e.entities.is_empty() {
            out.push_str("entities:\n");
            for entity in &e.entities {
                let name = entity["name"].as_str().unwrap_or("");
                let role = entity["role"].as_str().unwrap_or("");
                out.push_str(&format!("  - name: {}\n    role: {}\n",
                    yaml_value(name), yaml_value(role)));
            }
        }
        if !e.key_info.is_empty() {
            out.push_str("key_info:\n");
            for (k, v) in &e.key_info {
                let owned;
                let val = if let Some(s) = v.as_str() {
                    s
                } else {
                    owned = v.to_string();
                    &owned
                };
                out.push_str(&format!("  {k}: {}\n", yaml_value(val)));
            }
        }
    }

    // Protected tail — always present, never in schema
    out.push_str(&format!("source_file: {}\n", source_abs.display()));
    out.push_str(&format!("pages: {page_count}\n"));
    out.push_str(&format!("ocr_method: {}\n", result.ocr_method));
    out.push_str(&format!("extraction_mode: {}\n", result.extraction_mode));
    out.push_str(&format!("elapsed_ms: {elapsed_ms}\n"));
    out.push_str(&format!("extracted_at: {extracted_at}\n"));
    out.push_str("---\n");

    // Page body
    for page_text in &result.pages {
        out.push_str(&format!("[Page {}]\n", page_text.page_num));
        out.push_str(&page_text.text);
        out.push_str("\n\n");
    }

    out
}

/// Apply filename-based fallback for fields where LLM returned nothing.
/// Only Person and Date types have meaningful filename patterns.
fn apply_filename_fallback(
    field: &crate::schema::FieldDef,
    stem: &str,
    known_persons: &[String],
    doc_type_tokens: &[String],
) -> String {
    match &field.field_type {
        FieldType::Person => {
            filename::extract_person(stem, known_persons, doc_type_tokens).unwrap_or_default()
        }
        FieldType::Date => filename::extract_date(stem).unwrap_or_default(),
        _ => String::new(),
    }
}

fn derive_title(
    stem: &str,
    doc_type: &str,
    resolved: &HashMap<String, String>,
    schema: &SchemaRegistry,
) -> String {
    // Find the first non-empty value from any Person-typed field
    let person_val = schema
        .effective_fields(doc_type)
        .into_iter()
        .filter(|f| matches!(f.field_type, FieldType::Person))
        .find_map(|f| {
            let v = resolved.get(&f.name)?;
            if v.is_empty() { None } else { Some(v.as_str()) }
        });

    if let Some(person) = person_val {
        if !doc_type.is_empty() && doc_type != "unknown" {
            return format!("{} - {}", doc_type.to_uppercase(), person);
        }
    }

    // Fall back to humanised stem
    stem.replace('_', " ").replace('-', " ")
}

/// Produce a YAML-safe scalar value. Quotes strings that contain special characters.
fn yaml_value(s: &str) -> String {
    if s.is_empty() {
        return "\"\"".to_string();
    }
    let needs_quotes = s.starts_with(|c: char| {
        matches!(c, '"' | '\'' | '{' | '[' | '|' | '>' | '&' | '*' | '!' | '%' | '@' | '`')
    }) || s.contains(": ")
        || s.contains(" #")
        || s.contains('\n')
        || s.contains('\\');

    if needs_quotes {
        format!("\"{}\"", s.replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

// ---------------------------------------------------------------------------
// Frontmatter parsing (unchanged — used by search/index.rs)
// ---------------------------------------------------------------------------

pub fn parse_frontmatter(content: &str) -> Option<serde_yaml::Value> {
    if !content.starts_with("---\n") {
        return None;
    }
    let rest = &content[4..];
    let end = rest.find("\n---\n")?;
    let yaml_str = &rest[..end];
    serde_yaml::from_str(yaml_str).ok()
}

pub fn strip_frontmatter(content: &str) -> &str {
    if !content.starts_with("---\n") {
        return content;
    }
    let rest = &content[4..];
    if let Some(end) = rest.find("\n---\n") {
        &rest[end + 5..]
    } else {
        content
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extraction::{ExtractionResult, PageContent, PageText};
    use crate::schema::SchemaRegistry;
    use std::collections::HashMap;
    use std::path::Path;

    fn minimal_schema() -> SchemaRegistry {
        SchemaRegistry::from_toml_str(r#"
[doc_type]
values = ["unknown"]
default = "unknown"
"#).unwrap()
    }

    fn dummy_result(extraction_mode: &str) -> ExtractionResult {
        ExtractionResult {
            pages: vec![PageText { page_num: 1, text: "hello".to_string() }],
            doc_type: "unknown".to_string(),
            doc_category: "text".to_string(),
            fields: HashMap::new(),
            ocr_method: "text-embedded".to_string(),
            extraction_mode: extraction_mode.to_string(),
        }
    }

    #[test]
    fn generate_md_includes_extraction_mode_and_elapsed() {
        let schema = minimal_schema();
        let result = dummy_result("offline");
        let pages = vec![PageContent::Text { page_num: 1, text: "hello".to_string() }];
        let md = generate_md(&result, Path::new("test.pdf"), &pages, &schema, &[], None, 312);
        assert!(md.contains("extraction_mode: offline"), "missing extraction_mode");
        assert!(md.contains("elapsed_ms: 312"), "missing elapsed_ms");
    }

    #[test]
    fn generate_md_online_mode() {
        let schema = minimal_schema();
        let result = dummy_result("online");
        let pages = vec![PageContent::Text { page_num: 1, text: "hello".to_string() }];
        let md = generate_md(&result, Path::new("test.pdf"), &pages, &schema, &[], None, 3104);
        assert!(md.contains("extraction_mode: online"));
        assert!(md.contains("elapsed_ms: 3104"));
    }
}
