use std::path::{Path, PathBuf};

use clap::Args;

use pdf_core::{
    config::ClaudeConfig,
    schema::SchemaRegistry,
    search::{Backend, SearchMode, SearchResult, execute, intent::IntentIndex},
};

#[derive(Args)]
pub struct SearchArgs {
    #[arg(help = "Search query")]
    pub query: String,

    #[arg(long, default_value_t = 5, help = "Number of results to return")]
    pub top: usize,

    #[arg(long, help = "Override output directory to search")]
    pub index_dir: Option<PathBuf>,

    #[arg(long, help = "Emit JSON array to stdout")]
    pub json: bool,

    #[arg(long, default_value = "text", help = "Search pool: text (all backends, outputs/text/) or images (metadata only, outputs/images/)")]
    pub mode: String,
}

pub async fn run(args: SearchArgs) -> anyhow::Result<()> {
    let config = ClaudeConfig::load()?;
    let index_base = config.resolve_index_dir(args.index_dir);
    let mode: SearchMode = args.mode.parse()?;
    let schema = SchemaRegistry::load_auto(config.schema_path.as_ref().map(Path::new))?;
    let intent_index = IntentIndex::new(&schema.doc_type_values)?;

    let combined = execute(&args.query, &index_base, &mode, args.top, &intent_index, &config, &schema).await;

    if args.json {
        let json_results: Vec<serde_json::Value> = combined.iter().map(|r| r.to_json()).collect();
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
        Backend::Keyword => {
            let doc_type = result.meta.doc_type.as_deref().unwrap_or("—");
            println!("  Type: {doc_type}");
        }
    }

    if !result.snippet.is_empty() {
        let preview: String = result.snippet.chars().take(120).collect();
        println!("  \"{preview}\"");
    }
    println!();
}
