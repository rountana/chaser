use std::path::{Path, PathBuf};

use serde_json::{Value as JsonValue, json};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SearchMode {
    #[default]
    Text,
    Images,
}

impl std::str::FromStr for SearchMode {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Self> {
        match s {
            "text"   => Ok(Self::Text),
            "images" => Ok(Self::Images),
            other    => anyhow::bail!("unknown search mode {:?}; expected \"text\" or \"images\"", other),
        }
    }
}

impl std::fmt::Display for SearchMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text   => write!(f, "text"),
            Self::Images => write!(f, "images"),
        }
    }
}

/// Returns the subfolder to search for the given mode.
/// `Text` → `base/text/`    `Images` → `base/images/`
pub fn search_subdir(base: &Path, mode: &SearchMode) -> PathBuf {
    match mode {
        SearchMode::Text   => base.join("text"),
        SearchMode::Images => base.join("images"),
    }
}

/// Holds the offline and online subdirectories for a given search mode.
pub struct MergedDirs {
    pub offline: PathBuf,
    pub online: PathBuf,
}

/// Returns the offline + online directories for a search mode.
/// Callers use `MetadataIndex::build_merged_with_fields` to get a merged index from both.
pub fn merged_dirs(base: &Path, mode: &SearchMode) -> MergedDirs {
    let sub = match mode {
        SearchMode::Text   => "text",
        SearchMode::Images => "images",
    };
    MergedDirs {
        offline: base.join("offline").join(sub),
        online:  base.join("online").join(sub),
    }
}

pub mod classify;
pub mod index;
pub mod intent;
pub mod keyword;
pub mod merge;
pub mod metadata;
pub mod router;
pub mod semantic;
pub mod structural;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Backend {
    Metadata,
    Structural,
    Semantic,
    Keyword,
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Backend::Metadata => write!(f, "metadata"),
            Backend::Structural => write!(f, "structural"),
            Backend::Semantic => write!(f, "semantic"),
            Backend::Keyword => write!(f, "keyword"),
        }
    }
}

impl Backend {
    /// Dedup priority: lower number = higher priority (kept when same file appears in multiple backends).
    pub fn dedup_priority(&self) -> u8 {
        match self {
            Backend::Semantic => 0,
            Backend::Metadata => 1,
            Backend::Structural => 2,
            Backend::Keyword => 3,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResultMeta {
    pub person: Option<String>,
    pub doc_type: Option<String>,
    pub date: Option<String>,
    pub institution: Option<String>,
    pub pages: Option<u32>,
    pub words: Option<u32>,
}

impl ResultMeta {
    pub fn as_snippet(&self) -> String {
        format!(
            "person: {}\ndoc_type: {}\ndate: {}\ninstitution: {}",
            self.person.as_deref().unwrap_or(""),
            self.doc_type.as_deref().unwrap_or(""),
            self.date.as_deref().unwrap_or(""),
            self.institution.as_deref().unwrap_or(""),
        )
    }
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub file_path: PathBuf,
    pub file_name: String,
    pub snippet: String,
    pub page_num: Option<u32>,
    pub backend: Backend,
    pub score: Option<f32>,
    pub meta: ResultMeta,
    /// Absolute path to the original source document (PDF, image, etc.) referenced by the .md index file.
    pub source_path: Option<PathBuf>,
    /// "offline" | "online" — which extraction tier produced this document.
    pub extraction_mode: String,
}

impl SearchResult {
    /// Canonical production JSON shape used by the HTTP API and CLI --json output.
    pub fn to_json(&self) -> JsonValue {
        let source_path_str = self.source_path.as_ref()
            .filter(|p| p.exists())
            .map(|p| p.display().to_string());
        let file_name = self.source_path
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.file_name.clone());
        json!({
            "filePath": self.file_path.display().to_string(),
            "fileName": file_name,
            "sourcePath": source_path_str,
            "snippet": self.snippet,
            "pageNum": self.page_num,
            "backend": self.backend.to_string(),
            "score": self.score,
            "extractionMode": if self.extraction_mode.is_empty() { "offline" } else { self.extraction_mode.as_str() },
            "meta": {
                "person": self.meta.person,
                "docType": self.meta.doc_type,
                "date": self.meta.date,
                "institution": self.meta.institution,
                "pages": self.meta.pages,
                "words": self.meta.words,
            }
        })
    }
}

/// Run the full search pipeline: index → intent parse → backend dispatch → merge.
///
/// The `intent_index` is passed in because it holds a ~133 MB embedding model; the serve
/// path caches it in `AppState` while the CLI builds it fresh per call.
pub async fn execute(
    query: &str,
    index_base: &Path,
    mode: &SearchMode,
    top: usize,
    intent_index: &intent::IntentIndex,
    config: &crate::config::ClaudeConfig,
    schema: &crate::schema::SchemaRegistry,
) -> Vec<SearchResult> {
    eprintln!("[search:execute] query={:?} mode={} index={}", query, mode, index_base.display());

    let dirs = merged_dirs(index_base, mode);

    let person_field_names = schema.searchable_person_field_names();
    let date_field_names = schema.searchable_date_field_names();

    // Metadata index is scoped to the active mode's subdir (text/ or images/).
    let idx = index::MetadataIndex::build_merged_with_fields(
        &dirs.offline, &dirs.online, &person_field_names, &date_field_names,
    ).unwrap_or_else(|_| index::MetadataIndex { entries: Default::default(), known_persons: vec![] });

    eprintln!("[search:execute] index built: {} entries, {} known_persons", idx.entries.len(), idx.known_persons.len());

    // Structural and semantic search target the same mode-specific dir; prefer online if it exists.
    let search_dir = if dirs.online.exists() { dirs.online } else { dirs.offline };

    let signals = intent_index.parse(query, &idx.known_persons);
    eprintln!("[search:execute] intent signals: persons={:?} doc_types={:?} dates={:?} structural={:?}",
        signals.persons, signals.doc_types, signals.dates, signals.structural);

    let candidate_limit = top * 2;
    let mut all_results: Vec<SearchResult> = Vec::new();

    match mode {
        SearchMode::Images => {
            eprintln!("[search:execute] mode=images → metadata only");
            let mut r = metadata::search(&signals, &idx);
            eprintln!("[search:execute] metadata returned {} (pre-truncate)", r.len());
            r.truncate(candidate_limit);
            for result in &mut r {
                result.snippet = result.meta.as_snippet();
            }
            all_results.append(&mut r);
        }
        SearchMode::Text => {
            let backends = router::route(&signals, query, config, &idx.known_persons, &schema.doc_type_values).await;
            eprintln!("[search:execute] router selected backends: {:?}", backends);
            for backend in &backends {
                let mut results = match backend {
                    Backend::Metadata => {
                        let mut r = metadata::search(&signals, &idx);
                        r.truncate(candidate_limit);
                        r
                    }
                    Backend::Structural => structural::search(&signals, &search_dir),
                    Backend::Semantic => {
                        let mut r = semantic::search(query);
                        r.truncate(candidate_limit);
                        r
                    }
                    Backend::Keyword => {
                        let mut r = keyword::search(query, &search_dir);
                        r.truncate(candidate_limit);
                        r
                    }
                };
                eprintln!("[search:execute] backend={} returned {} results", backend, results.len());
                all_results.append(&mut results);
            }

            // Keyword fallback: when all intent signals are empty and no results yet,
            // do a fulltext scan so bare keyword queries ("invoices") still return hits.
            if all_results.is_empty()
                && signals.persons.is_empty()
                && signals.doc_types.is_empty()
                && signals.dates.is_empty()
                && signals.structural.is_none()
            {
                let mut r = keyword::search(query, &search_dir);
                r.truncate(candidate_limit);
                eprintln!("[search:execute] keyword fallback returned {} results", r.len());
                all_results.append(&mut r);
            }
        }
    }

    eprintln!("[search:execute] pre-merge total={} top={}", all_results.len(), top);
    let merged = merge::merge(all_results, top);
    eprintln!("[search:execute] final results={}", merged.len());
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn search_mode_from_str_text() {
        assert_eq!("text".parse::<SearchMode>().unwrap(), SearchMode::Text);
    }

    #[test]
    fn search_mode_from_str_images() {
        assert_eq!("images".parse::<SearchMode>().unwrap(), SearchMode::Images);
    }

    #[test]
    fn search_mode_from_str_invalid() {
        assert!("pdf".parse::<SearchMode>().is_err());
    }

    #[test]
    fn search_subdir_text() {
        let base = Path::new("/data/outputs");
        assert_eq!(search_subdir(base, &SearchMode::Text), base.join("text"));
    }

    #[test]
    fn search_subdir_images() {
        let base = Path::new("/data/outputs");
        assert_eq!(search_subdir(base, &SearchMode::Images), base.join("images"));
    }
}
