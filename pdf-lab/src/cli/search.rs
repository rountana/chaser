use std::path::PathBuf;

use clap::Args;
use serde_json::json;

use pdf_core::{
    config::ClaudeConfig,
    schema::SchemaRegistry,
    search::{
        Backend, SearchMode, SearchResult,
        index::MetadataIndex,
        intent::IntentIndex,
        merge, merged_dirs, metadata, router, semantic, structural,
    },
};

#[derive(Args)]
pub struct SearchArgs {
    #[arg(help = "Search query")]
    pub query: String,

    #[arg(long, default_value_t = 5, help = "Number of results to return")]
    pub top: usize,

    #[arg(long, help = "Override output directory to search")]
    pub outputs_dir: Option<PathBuf>,

    #[arg(long, help = "Emit JSON array to stdout")]
    pub json: bool,

    #[arg(long, default_value = "text", help = "Search pool: text (all backends, outputs/text/) or images (metadata only, outputs/images/)")]
    pub mode: String,
}

pub async fn run(args: SearchArgs) -> anyhow::Result<()> {
    let config = ClaudeConfig::load()?;
    let outputs_base = config.resolve_outputs_dir(args.outputs_dir);

    let mode: SearchMode = args.mode.parse()?;
    let dirs = merged_dirs(&outputs_base, &mode);

    let schema = match &config.schema_path {
        Some(p) => SchemaRegistry::load(std::path::Path::new(p))?,
        None => SchemaRegistry::load_default()?,
    };
    let intent_index = IntentIndex::new(&schema.doc_type_values)?;
    let person_field_names = schema.searchable_person_field_names();
    let date_field_names = schema.searchable_date_field_names();
    let index = MetadataIndex::build_merged_with_fields(
        &dirs.offline, &dirs.online, &person_field_names, &date_field_names,
    )?;
    let search_dir = if dirs.online.exists() { dirs.online.clone() } else { dirs.offline.clone() };

    let signals = intent_index.parse(&args.query, &index.known_persons);

    let candidate_limit = args.top * 2;
    let mut all_results: Vec<SearchResult> = Vec::new();

    match mode {
        SearchMode::Images => {
            let mut r = metadata::search(&signals, &index);
            r.truncate(candidate_limit);
            for result in &mut r {
                result.snippet = super::images_snippet(&result.meta);
            }
            all_results.append(&mut r);
        }
        SearchMode::Text => {
            let backends = router::route(
                &signals,
                &args.query,
                &config,
                &index.known_persons,
                &schema.doc_type_values,
            )
            .await;

            for backend in &backends {
                let mut results = match backend {
                    Backend::Metadata => {
                        let mut r = metadata::search(&signals, &index);
                        r.truncate(candidate_limit);
                        r
                    }
                    Backend::Structural => structural::search(&signals, &search_dir),
                    Backend::Semantic => {
                        let mut r = semantic::search(&args.query);
                        r.truncate(candidate_limit);
                        r
                    }
                };
                all_results.append(&mut results);
            }
        }
    }

    let combined = merge::merge(all_results, args.top);

    if args.json {
        let json_results: Vec<serde_json::Value> = combined
            .iter()
            .map(|r| {
                json!({
                    "filePath": r.file_path.display().to_string(),
                    "fileName": r.file_name,
                    "snippet": r.snippet,
                    "pageNum": r.page_num,
                    "backend": r.backend.to_string(),
                    "score": r.score,
                    "meta": {
                        "person": r.meta.person,
                        "docType": r.meta.doc_type,
                        "date": r.meta.date,
                        "institution": r.meta.institution,
                        "pages": r.meta.pages,
                        "words": r.meta.words,
                    }
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json_results)?);
    } else {
        if combined.is_empty() {
            println!("No results found for: {}", args.query);
            return Ok(());
        }
        for result in &combined {
            print_result(result);
        }
    }

    Ok(())
}

fn print_result(result: &SearchResult) {
    let page_info = result.page_num.map(|p| format!(" — Page {p}")).unwrap_or_default();
    let backend = &result.backend;
    let score_info = result.score.map(|s| format!(", score: {s:.2}")).unwrap_or_default();

    println!("[{}]{page_info}  ({backend}{score_info})", result.file_name);

    match backend {
        Backend::Metadata => {
            let person = result.meta.person.as_deref().unwrap_or("—");
            let doc_type = result.meta.doc_type.as_deref().unwrap_or("—");
            let date = result.meta.date.as_deref().unwrap_or("—");
            println!("  Person: {person} | Type: {doc_type} | Date: {date}");
        }
        Backend::Structural => {
            let pages = result.meta.pages.map(|p| format!("{p} pages")).unwrap_or_default();
            let words = result.meta.words.map(|w| format!("{w} words")).unwrap_or_default();
            let info = [pages.as_str(), words.as_str()]
                .iter()
                .filter(|s| !s.is_empty())
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            if !info.is_empty() {
                println!("  {info}");
            }
        }
        Backend::Semantic => {
            if let Some(score) = result.score {
                println!("  Similarity: {score:.3}");
            }
        }
    }

    if !result.snippet.is_empty() {
        let preview: String = result.snippet.chars().take(120).collect();
        println!("  \"{preview}\"");
    }
    println!();
}
