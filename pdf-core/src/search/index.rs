use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::frontmatter::parse_frontmatter;

#[derive(Debug, Clone)]
pub struct FileMeta {
    pub person: String,
    pub doc_type: String,
    pub date: String,
    pub institution: String,
    pub title: String,
    pub pages: u32,
    pub file_path: PathBuf,
    pub body_preview: String,
    pub source_file: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct MetadataIndex {
    pub entries: HashMap<String, FileMeta>,
    pub known_persons: Vec<String>,
}

impl MetadataIndex {
    /// Build the index from all `.md` files in `outputs_dir`.
    ///
    /// `person_field_names`: YAML keys for searchable person-name fields (from
    /// `SchemaRegistry::searchable_person_field_names()`). Pass `&["person"]` when
    /// running without a schema.
    pub fn build(outputs_dir: &Path, person_field_names: &[&str], date_field_names: &[&str]) -> anyhow::Result<Self> {
        let mut entries = HashMap::new();
        let mut persons_set = std::collections::HashSet::new();

        if !outputs_dir.exists() {
            return Ok(Self { entries, known_persons: vec![] });
        }

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

            let Some(fm) = parse_frontmatter(&content) else { continue };

            let get = |key: &str| {
                fm.get(key)
                    .and_then(|v| match v {
                        serde_yaml::Value::String(s) => Some(s.clone()),
                        serde_yaml::Value::Number(n) => Some(n.to_string()),
                        _ => None,
                    })
                    .unwrap_or_default()
            };

            // Collect persons from all person-typed fields.
            let person = {
                let mut first = String::new();
                for &field_name in person_field_names {
                    let v = get(field_name);
                    if !v.is_empty() {
                        if first.is_empty() {
                            first = v.clone();
                        }
                        persons_set.insert(v);
                    }
                }
                first
            };

            let doc_type = get("doc_type");
            let date = {
                let fields = if date_field_names.is_empty() { &["date"][..] } else { date_field_names };
                let mut first = String::new();
                for &field_name in fields {
                    let v = get(field_name);
                    if !v.is_empty() { first = v; break; }
                }
                first
            };
            let institution = get("institution");
            let title = get("title");
            let pages: u32 = get("pages").parse().unwrap_or(0);

            let body_preview = crate::frontmatter::strip_frontmatter(&content)
                .chars()
                .take(200)
                .collect();

            let source_file = fm.get("source_file")
                .and_then(|v| v.as_str())
                .map(PathBuf::from);

            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();

            entries.insert(stem, FileMeta { person, doc_type, date, institution, title, pages, file_path: path, body_preview, source_file });
        }

        let mut known_persons: Vec<String> = persons_set.into_iter().collect();
        known_persons.sort();

        Ok(Self { entries, known_persons })
    }
}
