use std::path::Path;

use super::{Backend, ResultMeta, SearchResult};

/// Fulltext keyword search — walks `.md` files and returns those whose lowercased
/// content contains any query token of length ≥ 3.
///
/// Used as a fallback when all intent signals (person, doc_type, date, structural)
/// are empty, so bare keyword queries like "invoices" still return results.
pub fn search(query: &str, index_dir: &Path) -> Vec<SearchResult> {
    if !index_dir.exists() {
        return vec![];
    }

    let tokens: Vec<String> = query
        .split_whitespace()
        .filter(|t| t.len() >= 3)
        .map(|t| t.to_lowercase())
        .collect();
    if tokens.is_empty() {
        return vec![];
    }

    let mut results = Vec::new();

    let walker = ignore::WalkBuilder::new(index_dir)
        .hidden(false)
        .git_ignore(false)
        .build();

    for entry in walker.flatten() {
        let path = entry.path().to_path_buf();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let content_lower = content.to_lowercase();
        if !tokens.iter().any(|t| content_lower.contains(t.as_str())) {
            continue;
        }

        let fm = crate::frontmatter::parse_frontmatter(&content);
        let source_path = fm.as_ref()
            .and_then(|fm| fm.get("source_file")?.as_str().map(std::path::PathBuf::from));
        let extraction_mode = fm.as_ref()
            .and_then(|fm| fm.get("extraction_mode")?.as_str().map(str::to_owned))
            .unwrap_or_default();

        let get_str = |key: &str| -> String {
            fm.as_ref()
                .and_then(|m| m.get(key))
                .and_then(|v| match v {
                    serde_yaml::Value::String(s) => Some(s.clone()),
                    serde_yaml::Value::Number(n) => Some(n.to_string()),
                    _ => None,
                })
                .unwrap_or_default()
        };

        let doc_type = get_str("doc_type");
        let person = get_str("person");
        let date = get_str("date");
        let institution = get_str("institution");
        let pages: u32 = get_str("pages").parse().unwrap_or(0);

        let body = crate::frontmatter::strip_frontmatter(&content);
        let snippet: String = body.chars().take(200).collect();

        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();

        results.push(SearchResult {
            file_path: path,
            file_name,
            snippet,
            page_num: None,
            backend: Backend::Keyword,
            score: Some(0.5),
            meta: ResultMeta {
                person: if person.is_empty() { None } else { Some(person) },
                doc_type: if doc_type.is_empty() { None } else { Some(doc_type) },
                date: if date.is_empty() { None } else { Some(date) },
                institution: if institution.is_empty() { None } else { Some(institution) },
                pages: if pages > 0 { Some(pages) } else { None },
                words: None,
            },
            source_path,
            extraction_mode,
        });
    }

    results
}
