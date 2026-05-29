use std::path::Path;

use once_cell::sync::Lazy;
use regex::Regex;

use super::{Backend, ResultMeta, SearchResult, intent::{IntentSignals, StructField, StructOp}};

static PAGE_MARKER: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\[Page\s+\d+\]").unwrap());

/// Search by structural properties (page or word count thresholds).
///
/// Returns all matching files — no internal limit (the query defines the threshold explicitly).
pub fn search(signals: &IntentSignals, outputs_dir: &Path) -> Vec<SearchResult> {
    let struct_sig = match &signals.structural {
        Some(s) => s,
        None => return vec![],
    };

    if !outputs_dir.exists() {
        return vec![];
    }

    let mut results = Vec::new();

    let walker = ignore::WalkBuilder::new(outputs_dir)
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

        let source_path = crate::frontmatter::parse_frontmatter(&content)
            .and_then(|fm| fm.get("source_file")?.as_str().map(std::path::PathBuf::from));

        let body = crate::frontmatter::strip_frontmatter(&content);

        let measured: u32 = match struct_sig.field {
            StructField::Pages => PAGE_MARKER.find_iter(body).count() as u32,
            StructField::Words => body.split_whitespace().count() as u32,
        };

        let matches = match struct_sig.op {
            StructOp::Gt => measured > struct_sig.value,
            StructOp::Gte => measured >= struct_sig.value,
            StructOp::Lt => measured < struct_sig.value,
            StructOp::Lte => measured <= struct_sig.value,
        };

        if !matches {
            continue;
        }

        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        let snippet: String = body.chars().take(200).collect();

        let (pages, words) = match struct_sig.field {
            StructField::Pages => (Some(measured), None),
            StructField::Words => (None, Some(measured)),
        };

        results.push(SearchResult {
            file_path: path,
            file_name,
            snippet,
            page_num: None,
            backend: Backend::Structural,
            score: Some(1.0),
            meta: ResultMeta {
                person: None,
                doc_type: None,
                date: None,
                institution: None,
                pages,
                words,
                keyword: None,
            },
            source_path,
        });
    }

    results
}
