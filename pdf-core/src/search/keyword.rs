use std::collections::HashSet;
use std::path::Path;

use grep_regex::RegexMatcher;
use grep_searcher::{
    BinaryDetection, Encoding, SearcherBuilder, Sink, SinkContext, SinkContextKind, SinkMatch,
};
use ignore::WalkBuilder;
use once_cell::sync::Lazy;
use regex::Regex;

use super::{Backend, ResultMeta, SearchResult, intent::IntentSignals};

static PAGE_NUM_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\[Page\s+(\d+)\]").unwrap());

/// Search `.md` files using grep (ripgrep family).
///
/// `scope_stems`: if non-empty, only search files whose stem is in this set (2-pass mode from R2/R5).
pub fn search(
    signals: &IntentSignals,
    outputs_dir: &Path,
    scope_stems: Option<&HashSet<String>>,
) -> Vec<SearchResult> {
    let keyword = match signals.primary_keyword() {
        Some(k) => k.to_string(),
        None => return vec![],
    };

    if keyword.len() < 3 {
        return vec![];
    }

    // Secondary keywords that must also appear in the file (AND semantics).
    // Prevents "records" alone from matching a doc that doesn't contain "vet".
    let secondary: Vec<String> = signals.keywords.iter()
        .filter(|k| k.as_str() != keyword.as_str())
        .cloned()
        .collect();

    let pattern = regex::escape(&keyword);
    let matcher = match RegexMatcher::new_line_matcher(&format!("(?i){}", pattern)) {
        Ok(m) => m,
        Err(_) => return vec![],
    };

    let searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(0x00))
        .encoding(Some(Encoding::new("UTF-8").unwrap()))
        .before_context(1)
        .after_context(1)
        .build();

    if !outputs_dir.exists() {
        return vec![];
    }

    let walk = WalkBuilder::new(outputs_dir)
        .hidden(false)
        .git_ignore(false)
        .build();
    let mut all_results: Vec<SearchResult> = Vec::new();

    for entry in walk.flatten() {
        let path = entry.path().to_path_buf();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }

        // 2-pass scope: skip files not in the allowed stem set
        if let Some(stems) = scope_stems {
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            if !stems.contains(stem) {
                continue;
            }
        }

        let file_text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => continue,
        };

        let fm = crate::frontmatter::parse_frontmatter(&file_text);

        // Date pre-filter: skip files whose document date doesn't match any date signal.
        if !signals.dates.is_empty() {
            let doc_date = fm.as_ref()
                .and_then(|f| f.get("date")?.as_str().map(|s| s.to_string()))
                .unwrap_or_default();
            let date_ok = signals.dates.iter().any(|d| doc_date.starts_with(d.as_str()));
            if !date_ok {
                continue;
            }
        }

        let source_path = fm
            .and_then(|f| f.get("source_file")?.as_str().map(std::path::PathBuf::from));

        let mut file_results: Vec<SearchResult> = Vec::new();
        let mut current_context: Vec<String> = Vec::new();
        let mut matched_page: Option<u32> = None;

        {
            let mut sink = KeywordSink {
                keyword: keyword.clone(),
                file_path: path.clone(),
                source_path: source_path.clone(),
                results: &mut file_results,
                context_buf: &mut current_context,
                matched_page: &mut matched_page,
            };
            let mut s = searcher.clone();
            let _ = s.search_path(&matcher, &path, &mut sink);
        }

        if file_results.is_empty() {
            continue;
        }

        // AND-check: every secondary keyword must appear somewhere in the file
        let file_lower = file_text.to_lowercase();
        if !secondary.iter().all(|kw| file_lower.contains(kw.as_str())) {
            continue;
        }

        // TF-based score: fraction of lines matched, clamped to [0.05, 0.90]
        let total_lines = file_text.lines().count().max(1);
        let score = (file_results.len() as f32 / total_lines as f32)
            .min(0.90)
            .max(0.05);
        for r in &mut file_results {
            r.score = Some(score);
        }

        all_results.extend(file_results);
    }

    all_results
}

fn extract_page_from_context(lines: &[String]) -> Option<u32> {
    for line in lines {
        if let Some(caps) = PAGE_NUM_RE.captures(line) {
            return caps[1].parse().ok();
        }
    }
    None
}

struct KeywordSink<'a> {
    keyword: String,
    file_path: std::path::PathBuf,
    source_path: Option<std::path::PathBuf>,
    results: &'a mut Vec<SearchResult>,
    context_buf: &'a mut Vec<String>,
    matched_page: &'a mut Option<u32>,
}

impl<'a> Sink for KeywordSink<'a> {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        mat: &SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        let line = String::from_utf8_lossy(mat.bytes()).to_string();

        if let Some(caps) = PAGE_NUM_RE.captures(&line) {
            *self.matched_page = caps[1].parse().ok();
        }

        let mut parts = self.context_buf.clone();
        parts.push(line.trim_end().to_string());

        if self.matched_page.is_none() {
            *self.matched_page = extract_page_from_context(&parts);
        }

        let snippet: String = parts.join("\n").chars().take(200).collect();
        let file_name = self
            .file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();

        self.results.push(SearchResult {
            file_path: self.file_path.clone(),
            file_name,
            snippet,
            page_num: *self.matched_page,
            backend: Backend::Keyword,
            score: None,
            meta: ResultMeta {
                person: None,
                doc_type: None,
                date: None,
                institution: None,
                pages: None,
                words: None,
                keyword: Some(self.keyword.clone()),
            },
            source_path: self.source_path.clone(),
        });

        self.context_buf.clear();
        Ok(true)
    }

    fn context(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        ctx: &SinkContext<'_>,
    ) -> Result<bool, Self::Error> {
        let line = String::from_utf8_lossy(ctx.bytes()).to_string();
        match ctx.kind() {
            SinkContextKind::Before => {
                self.context_buf.push(line.trim_end().to_string());
            }
            SinkContextKind::After => {
                if let Some(last) = self.results.last_mut() {
                    let current_chars = last.snippet.chars().count();
                    if current_chars < 200 {
                        last.snippet.push('\n');
                        let remaining = 200 - current_chars - 1;
                        last.snippet.push_str(
                            &line.trim_end().chars().take(remaining).collect::<String>(),
                        );
                    }
                }
            }
            _ => {}
        }
        Ok(true)
    }
}
