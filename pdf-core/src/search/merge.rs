use std::collections::HashMap;

use super::SearchResult;

/// Merge results from multiple backends into a single ranked list.
///
/// Steps:
///   1. Dedup by file_stem — keep the result from the highest-priority backend.
///   2. Sort: semantic results by score DESC, then non-semantic by backend priority.
///      Within each backend group: Metadata sorted by date DESC, others by score DESC.
///   3. Truncate to `top_n`.
pub fn merge(mut all_results: Vec<SearchResult>, top_n: usize) -> Vec<SearchResult> {
    // Step 1 — Dedup by file_stem, keeping highest-priority backend's result
    let mut best: HashMap<String, SearchResult> = HashMap::new();

    for result in all_results.drain(..) {
        let stem = result
            .file_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();

        let entry = best.entry(stem);
        entry
            .and_modify(|existing| {
                if result.backend.dedup_priority() < existing.backend.dedup_priority() {
                    *existing = result.clone();
                }
            })
            .or_insert(result);
    }

    // Step 2 — Sort
    let mut deduped: Vec<SearchResult> = best.into_values().collect();

    deduped.sort_by(|a, b| {
        use super::Backend;

        let a_semantic = a.backend == Backend::Semantic;
        let b_semantic = b.backend == Backend::Semantic;

        match (a_semantic, b_semantic) {
            // Both semantic: sort by score DESC
            (true, true) => b
                .score
                .unwrap_or(0.0)
                .partial_cmp(&a.score.unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal),

            // Semantic always before non-semantic
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,

            // Both non-semantic: sort by backend priority, then by date/score within group
            (false, false) => {
                let priority_cmp = a.backend.dedup_priority().cmp(&b.backend.dedup_priority());
                if priority_cmp != std::cmp::Ordering::Equal {
                    return priority_cmp;
                }

                // Within the same backend: Metadata → date DESC; others → score DESC
                match a.backend {
                    Backend::Metadata => {
                        let a_date = a.meta.date.as_deref().unwrap_or("");
                        let b_date = b.meta.date.as_deref().unwrap_or("");
                        b_date.cmp(a_date)
                    }
                    _ => b
                        .score
                        .unwrap_or(0.0)
                        .partial_cmp(&a.score.unwrap_or(0.0))
                        .unwrap_or(std::cmp::Ordering::Equal),
                }
            }
        }
    });

    // Step 3 — Truncate
    deduped.truncate(top_n);
    deduped
}
