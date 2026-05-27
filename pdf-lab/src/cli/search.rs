use std::collections::HashSet;
use std::path::PathBuf;

use clap::Args;
use serde_json::json;

use pdf_core::{
    config::ClaudeConfig,
    schema::SchemaRegistry,
    search::{
        Backend, SearchResult,
        index::MetadataIndex,
        intent,
        keyword, merge, metadata, router, semantic, structural,
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
}

pub async fn run(args: SearchArgs) -> anyhow::Result<()> {
    let config = ClaudeConfig::load()?;
    let outputs_dir = config.resolve_outputs_dir(args.outputs_dir);

    let schema = match &config.schema_path {
        Some(p) => SchemaRegistry::load_or_default(std::path::Path::new(p)),
        None => SchemaRegistry::load_default(),
    };
    let person_field_names = schema.searchable_person_field_names();
    let date_field_names = schema.searchable_date_field_names();
    let index = MetadataIndex::build(&outputs_dir, &person_field_names, &date_field_names)?;

    // Parse query intent
    let signals = intent::parse(
        &args.query,
        &index.known_persons,
        &schema.doc_type_values,
    );

    // Route to backends
    let backends = router::route(
        &signals,
        &args.query,
        &config,
        &index.known_persons,
        &schema.doc_type_values,
    )
    .await;

    // Dispatch — collect N×2 candidates per backend before merge
    let candidate_limit = args.top * 2;
    let mut all_results: Vec<SearchResult> = Vec::new();

    for backend in &backends {
        let mut results = match backend {
            Backend::Metadata => {
                let mut r = metadata::search(&signals, &index);
                r.truncate(candidate_limit);
                r
            }

            Backend::Keyword => {
                // 2-pass: if Metadata was also selected, scope keyword to metadata stems
                let scope: Option<HashSet<String>> = if backends.contains(&Backend::Metadata) {
                    let stems = metadata::matching_stems(&signals, &index);
                    if stems.is_empty() {
                        None // metadata matched nothing → fall back to full scan
                    } else {
                        Some(stems.into_iter().collect())
                    }
                } else {
                    None
                };

                let mut r = keyword::search(&signals, &outputs_dir, scope.as_ref());
                r.truncate(candidate_limit);
                r
            }

            Backend::Structural => {
                structural::search(&signals, &outputs_dir)
                // Structural returns all matching files (no limit — threshold is explicit)
            }

            Backend::Semantic => {
                let mut r = semantic::search(&args.query);
                r.truncate(candidate_limit);
                r
            }
        };

        all_results.append(&mut results);
    }

    // If all backends returned empty and semantic was requested, hint the user
    if all_results.is_empty() && backends.contains(&Backend::Semantic) {
        eprintln!("hint: run pdf-lab index to enable full-text and semantic search");
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
                        "pages": r.meta.pages,
                        "words": r.meta.words,
                        "keyword": r.meta.keyword
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
        Backend::Keyword => {
            if let Some(kw) = &result.meta.keyword {
                println!("  Keyword: {kw}");
            }
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
