pub mod config;
pub mod extract;
pub mod index;
pub mod mcp;
pub mod search;
pub mod serve;

pub(crate) fn images_snippet(meta: &pdf_core::search::ResultMeta) -> String {
    format!(
        "person: {}\ndoc_type: {}\ndate: {}\ninstitution: {}",
        meta.person.as_deref().unwrap_or(""),
        meta.doc_type.as_deref().unwrap_or(""),
        meta.date.as_deref().unwrap_or(""),
        meta.institution.as_deref().unwrap_or(""),
    )
}
