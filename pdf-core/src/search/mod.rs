pub mod classify;
pub mod index;
pub mod intent;
pub mod keyword;
pub mod merge;
pub mod metadata;
pub mod router;
pub mod semantic;
pub mod structural;

use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Backend {
    Metadata,
    Keyword,
    Structural,
    Semantic,
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Backend::Metadata => write!(f, "metadata"),
            Backend::Keyword => write!(f, "keyword"),
            Backend::Structural => write!(f, "structural"),
            Backend::Semantic => write!(f, "semantic"),
        }
    }
}

impl Backend {
    /// Dedup priority: lower number = higher priority (kept when same file appears in multiple backends).
    pub fn dedup_priority(&self) -> u8 {
        match self {
            Backend::Semantic => 0,
            Backend::Metadata => 1,
            Backend::Keyword => 2,
            Backend::Structural => 3,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResultMeta {
    pub person: Option<String>,
    pub doc_type: Option<String>,
    pub date: Option<String>,
    pub pages: Option<u32>,
    pub words: Option<u32>,
    pub keyword: Option<String>,
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
}
