use super::{Backend, ResultMeta, SearchResult, index::MetadataIndex, intent::IntentSignals};

/// Search the in-memory metadata index using pre-parsed intent signals.
///
/// AND-chain across non-empty signals. Multiple persons/doc_types use OR within their group.
pub fn search(signals: &IntentSignals, index: &MetadataIndex) -> Vec<SearchResult> {
    eprintln!("[search:metadata] persons={:?} doc_types={:?} dates={:?} index_size={}",
        signals.persons, signals.doc_types, signals.dates, index.entries.len());

    if signals.persons.is_empty() && signals.doc_types.is_empty() && signals.dates.is_empty() {
        eprintln!("[search:metadata] no signals → early return (0 results)");
        return vec![];
    }

    let results = index
        .entries
        .values()
        .filter(|meta| {
            // Person: OR across all matched persons
            if !signals.persons.is_empty() {
                let any_person = signals.persons.iter().any(|p| {
                    meta.person.to_lowercase().contains(&p.to_lowercase())
                });
                if !any_person {
                    return false;
                }
            }

            // Doc type: OR across all matched doc types
            if !signals.doc_types.is_empty() {
                let any_dt = signals.doc_types.iter().any(|dt| {
                    meta.doc_type.to_lowercase() == dt.to_lowercase()
                });
                if !any_dt {
                    return false;
                }
            }

            // Date: OR across all matched dates; support year-only prefix match
            if !signals.dates.is_empty() {
                let any_date = signals.dates.iter().any(|date| {
                    meta.date.starts_with(date.as_str())
                });
                if !any_date {
                    return false;
                }
            }

            true
        })
        .map(|meta| {
            let file_name = meta
                .file_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();

            SearchResult {
                file_path: meta.file_path.clone(),
                file_name,
                snippet: meta.body_preview.clone(),
                page_num: None,
                backend: Backend::Metadata,
                score: Some(1.0),
                meta: ResultMeta {
                    person: if meta.person.is_empty() { None } else { Some(meta.person.clone()) },
                    doc_type: if meta.doc_type.is_empty() { None } else { Some(meta.doc_type.clone()) },
                    date: if meta.date.is_empty() { None } else { Some(meta.date.clone()) },
                    institution: if meta.institution.is_empty() { None } else { Some(meta.institution.clone()) },
                    pages: if meta.pages > 0 { Some(meta.pages) } else { None },
                    words: None,
                },
                source_path: meta.source_file.clone(),
                extraction_mode: meta.extraction_mode.clone(),
            }
        })
        .collect::<Vec<_>>();
    eprintln!("[search:metadata] matched {} entries", results.len());
    results
}

