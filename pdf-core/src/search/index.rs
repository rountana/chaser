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
    pub extraction_mode: String,
}

#[derive(Debug, Clone)]
pub struct MetadataIndex {
    pub entries: HashMap<String, FileMeta>,
    pub known_persons: Vec<String>,
}

impl MetadataIndex {
    /// Build the index from all `.md` files in `index_dir`.
    ///
    /// `person_field_names`: YAML keys for searchable person-name fields (from
    /// `SchemaRegistry::searchable_person_field_names()`). Pass `&["person"]` when
    /// running without a schema.
    ///
    /// `date_field_names`: YAML keys for searchable date fields (from
    /// `SchemaRegistry::searchable_date_field_names()`). Pass `&["date"]` when
    /// running without a schema.
    pub fn build(index_dir: &Path, person_field_names: &[&str], date_field_names: &[&str]) -> anyhow::Result<Self> {
        let mut entries = HashMap::new();
        let mut persons_set = std::collections::HashSet::new();

        if !index_dir.exists() {
            return Ok(Self { entries, known_persons: vec![] });
        }

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

            let extraction_mode = get("extraction_mode");

            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();

            entries.insert(stem, FileMeta { person, doc_type, date, institution, title, pages, file_path: path, body_preview, source_file, extraction_mode });
        }

        let mut known_persons: Vec<String> = persons_set.into_iter().collect();
        known_persons.sort();

        Ok(Self { entries, known_persons })
    }

    /// Build index from merged offline + online trees.
    /// For each file stem, the online version wins over offline.
    pub fn build_merged(
        offline_dir: &Path,
        online_dir: &Path,
    ) -> anyhow::Result<Self> {
        let mut merged = Self::build(offline_dir, &["person"], &["date"])
            .unwrap_or_else(|_| Self { entries: Default::default(), known_persons: vec![] });

        let online_index = Self::build(online_dir, &["person"], &["date"])
            .unwrap_or_else(|_| Self { entries: Default::default(), known_persons: vec![] });

        for (stem, meta) in online_index.entries {
            merged.entries.insert(stem, meta);
        }

        let persons: std::collections::HashSet<String> = merged.entries.values()
            .filter(|m| !m.person.is_empty())
            .map(|m| m.person.clone())
            .collect();
        merged.known_persons = persons.into_iter().collect();
        merged.known_persons.sort();

        Ok(merged)
    }

    /// Convenience: build index from `dir` using schema field names and return only the known persons.
    pub fn known_persons_for(dir: &Path, schema: &crate::schema::SchemaRegistry) -> Vec<String> {
        let person_field_names = schema.searchable_person_field_names();
        let date_field_names = schema.searchable_date_field_names();
        Self::build(dir, &person_field_names, &date_field_names)
            .map(|idx| idx.known_persons)
            .unwrap_or_default()
    }

    /// Build index from merged offline + online trees, using schema-specific field names.
    pub fn build_merged_with_fields(
        offline_dir: &Path,
        online_dir: &Path,
        person_field_names: &[&str],
        date_field_names: &[&str],
    ) -> anyhow::Result<Self> {
        let mut merged = Self::build(offline_dir, person_field_names, date_field_names)
            .unwrap_or_else(|_| Self { entries: Default::default(), known_persons: vec![] });

        let online_index = Self::build(online_dir, person_field_names, date_field_names)
            .unwrap_or_else(|_| Self { entries: Default::default(), known_persons: vec![] });

        for (stem, meta) in online_index.entries {
            merged.entries.insert(stem, meta);
        }

        let persons: std::collections::HashSet<String> = merged.entries.values()
            .filter(|m| !m.person.is_empty())
            .map(|m| m.person.clone())
            .collect();
        merged.known_persons = persons.into_iter().collect();
        merged.known_persons.sort();

        Ok(merged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_md(dir: &std::path::Path, name: &str, person: &str, doc_type: &str) {
        fs::create_dir_all(dir).unwrap();
        let content = format!(
            "---\ntitle: test\ndoc_type: {doc_type}\ndoc_category: text\nperson: {person}\ndate: \"\"\ninstitution: \"\"\nsource_file: /tmp/test.pdf\npages: 1\nocr_method: text-embedded\nextraction_mode: offline\nelapsed_ms: 100\nextracted_at: 2026-01-01T00:00:00Z\n---\n[Page 1]\nhello world\n"
        );
        fs::write(dir.join(name), content).unwrap();
    }

    #[test]
    fn build_merged_prefers_online() {
        let tmp = TempDir::new().unwrap();
        let offline = tmp.path().join("offline/text");
        let online  = tmp.path().join("online/text");

        write_md(&offline, "doc_a.md", "Alice", "invoice");
        write_md(&offline, "doc_b.md", "Bob",   "receipt");
        write_md(&online,  "doc_a.md", "Alice Online", "invoice");

        let index = MetadataIndex::build_merged(&offline, &online).unwrap();
        // doc_a: online version should win
        assert_eq!(index.entries["doc_a"].person, "Alice Online");
        // doc_b: only offline, should still appear
        assert_eq!(index.entries["doc_b"].person, "Bob");
        assert_eq!(index.entries.len(), 2);
    }

    #[test]
    fn build_merged_offline_only() {
        let tmp = TempDir::new().unwrap();
        let offline = tmp.path().join("offline/text");
        let online  = tmp.path().join("online/text"); // does not exist

        write_md(&offline, "doc_x.md", "Xavier", "agreement");

        let index = MetadataIndex::build_merged(&offline, &online).unwrap();
        assert_eq!(index.entries["doc_x"].person, "Xavier");
    }
}
