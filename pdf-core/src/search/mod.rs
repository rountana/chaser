use std::path::{Path, PathBuf};

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

pub mod classify;
pub mod index;
pub mod intent;
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
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Backend::Metadata => write!(f, "metadata"),
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
            Backend::Structural => 2,
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
