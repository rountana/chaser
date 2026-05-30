use super::SearchResult;

/// Semantic search stub — returns empty until Phase 4 (LanceDB + fastembed).
pub fn search(query: &str) -> Vec<SearchResult> {
    eprintln!("[search:semantic] query={:?} (stub — embedding search not yet implemented)", query);
    vec![]
}
