use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::{Args, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use serde_json::json;

use pdf_core::{
    config::{ClaudeConfig, LlmBackend},
    extraction::{PageContent, claude, enrich, gemini, ollama, pdfium, suggest, table},
    frontmatter,
    schema::{FieldDef, SchemaRegistry},
    search::index::MetadataIndex,
};

const SUPPORTED_EXTENSIONS: &[&str] = &["pdf", "jpg", "jpeg", "png"];

struct FileReport {
    file_name: String,
    output_path: PathBuf,
    ocr_method: String,
    char_count: usize,
}

struct FailedReport {
    file_name: String,
    reason: String,
}

fn extraction_type_label(ocr_method: &str) -> &'static str {
    if ocr_method.starts_with("mixed:") {
        return "Mixed";
    }
    match ocr_method {
        "text-embedded" => "Text",
        "tesseract-only" => "OCR",
        "tesseract-llm-cleanup" | "tesseract-ollama-cleanup" => "OCR+Vision",
        "llm-vision" | "ollama-vision" => "Vision",
        _ => "Unknown",
    }
}

// ---------------------------------------------------------------------------
// Top-level command types
// ---------------------------------------------------------------------------

#[derive(Args)]
pub struct ExtractCommand {
    #[command(subcommand)]
    pub subcommand: ExtractSubcommand,
}

#[derive(Subcommand)]
pub enum ExtractSubcommand {
    /// Extract documents locally — no API key required (pdfium + Tesseract + heuristics)
    Offline(ExtractOfflineArgs),
    /// Enrich offline-extracted documents with LLM (reads from outputs/offline/)
    Online(ExtractOnlineArgs),
}

#[derive(Args)]
pub struct ExtractOfflineArgs {
    #[arg(help = "PDF or image files to extract. Omit to scan the source directory from config.")]
    pub paths: Vec<PathBuf>,
    #[arg(long, help = "Override source directory")]
    pub source_dir: Option<PathBuf>,
    #[arg(long, help = "Override output directory (offline/ subdirectory is always appended)")]
    pub outputs_dir: Option<PathBuf>,
    #[arg(long, help = "Emit JSON lines to stdout instead of human-readable stderr")]
    pub json: bool,
}

#[derive(Args)]
pub struct ExtractOnlineArgs {
    #[arg(help = "Source file paths to enrich (must have matching offline extraction). Omit when using --all.")]
    pub paths: Vec<PathBuf>,
    #[arg(long, help = "Enrich all files in outputs/offline/ that lack a corresponding outputs/online/ file")]
    pub all: bool,
    #[arg(long, help = "Override output directory")]
    pub outputs_dir: Option<PathBuf>,
    #[arg(long, help = "Emit JSON lines to stdout")]
    pub json: bool,
    #[arg(long, help = "Auto-suggest schema fields for unknown document types")]
    pub auto_schema: bool,
}

pub async fn run(cmd: ExtractCommand) -> anyhow::Result<()> {
    match cmd.subcommand {
        ExtractSubcommand::Offline(args) => run_offline(args).await,
        ExtractSubcommand::Online(args) => run_online(args).await,
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn resolve_paths(explicit: Vec<PathBuf>, source_dir: Option<PathBuf>) -> anyhow::Result<Vec<PathBuf>> {
    if !explicit.is_empty() {
        let mut expanded = Vec::new();
        for p in explicit {
            if p.is_dir() {
                collect_files(&p, &mut expanded)?;
            } else {
                expanded.push(p);
            }
        }
        return Ok(expanded);
    }
    let src = source_dir.context(
        "No files specified and no source_dir in config. Pass file paths or run: pdf-lab config set --source-dir DIR",
    )?;
    scan_source_dir(&src)
}

fn file_name_str(path: &Path) -> String {
    path.file_name().and_then(|n| n.to_str()).unwrap_or("unknown").to_string()
}

fn name_column_width(paths: &[PathBuf]) -> usize {
    paths
        .iter()
        .filter_map(|p| p.file_name()?.to_str().map(|s| s.len()))
        .max()
        .unwrap_or(8)
        .max(8)
}

fn make_progress_bar(json_mode: bool, total: u64) -> Option<ProgressBar> {
    if json_mode {
        return None;
    }
    let pb = ProgressBar::new(total);
    pb.set_style(
        ProgressStyle::with_template("{spinner:.cyan} [{pos}/{len}] {prefix:.bold.cyan}  {msg}")
            .unwrap()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    pb.enable_steady_tick(Duration::from_millis(80));
    Some(pb)
}

fn finish_bar(bar: Option<ProgressBar>) {
    if let Some(pb) = bar {
        pb.finish_and_clear();
    }
}

fn emit_started(json: bool, file_name: &str, bar: Option<&ProgressBar>) {
    if json {
        println!("{}", json!({"event": "started", "file": file_name}));
    } else if let Some(pb) = bar {
        pb.set_prefix(file_name.to_string());
    }
}

fn emit_error(json: bool, file_name: &str, reason: &str, elapsed_ms: u64, bar: Option<&ProgressBar>) {
    if json {
        println!("{}", json!({"event":"error","file":file_name,"elapsed_ms":elapsed_ms,"message":reason}));
    } else if let Some(pb) = bar {
        pb.println(format!(
            "  ✗ {file_name}  error: {reason}  ({:.1}s)",
            elapsed_ms as f32 / 1000.0
        ));
        pb.inc(1);
    }
}

fn emit_complete(
    json: bool,
    file_name: &str,
    output_path: &Path,
    char_count: usize,
    ocr_method: &str,
    elapsed_ms: u64,
    extraction_mode: &str,
    bar: Option<&ProgressBar>,
    name_width: usize,
) {
    if json {
        println!(
            "{}",
            json!({
                "event": "complete",
                "file": file_name,
                "output": output_path.display().to_string(),
                "chars_extracted": char_count,
                "extraction_mode": extraction_mode,
                "ocr_method": ocr_method,
                "elapsed_ms": elapsed_ms,
            })
        );
    } else if let Some(pb) = bar {
        let label = extraction_type_label(ocr_method);
        pb.println(format!(
            "  ✓ {:<name_width$}  {:<10}  ({char_count} chars)  {:.1}s",
            file_name,
            label,
            elapsed_ms as f32 / 1000.0,
            name_width = name_width
        ));
        pb.inc(1);
    }
}

fn print_report(
    completed: &[FileReport],
    failed: &[FailedReport],
    json_mode: bool,
    elapsed: Duration,
    model_name: &str,
) {
    if json_mode {
        let entries: Vec<_> = completed
            .iter()
            .map(|r| {
                json!({
                    "file": r.file_name,
                    "output": r.output_path.display().to_string(),
                    "extraction_type": extraction_type_label(&r.ocr_method),
                    "ocr_method": r.ocr_method,
                    "chars_extracted": r.char_count,
                })
            })
            .collect();
        let failed_entries: Vec<_> = failed
            .iter()
            .map(|r| json!({"file": r.file_name, "reason": r.reason}))
            .collect();
        println!(
            "{}",
            json!({"event":"report","model":model_name,"files":entries,"failed":failed_entries,"errors":failed.len(),"elapsed_ms":elapsed.as_millis()})
        );
        return;
    }

    let total = completed.len() + failed.len();
    if total == 0 {
        return;
    }

    let name_width = completed
        .iter()
        .map(|r| r.file_name.len())
        .chain(failed.iter().map(|r| r.file_name.len()))
        .max()
        .unwrap_or(8)
        .max(8);
    let bar = "─".repeat(name_width + 30);

    eprintln!();
    eprintln!("{bar}");
    eprintln!(
        "  Extract report  {total} file{}  [{}]",
        if total == 1 { "" } else { "s" },
        model_name
    );
    eprintln!("{bar}");

    for r in completed {
        let label = extraction_type_label(&r.ocr_method);
        eprintln!(
            "  {:<name_width$}  {:<10}  ({} chars)",
            r.file_name,
            label,
            r.char_count,
            name_width = name_width
        );
    }

    for r in failed {
        let reason = if r.reason.len() > 60 {
            format!("{}…", &r.reason[..60])
        } else {
            r.reason.clone()
        };
        eprintln!(
            "  {:<name_width$}  FAILED      {reason}",
            r.file_name,
            name_width = name_width
        );
    }

    eprintln!("{bar}");

    let mut counts: HashMap<&str, usize> = HashMap::new();
    for r in completed {
        *counts.entry(extraction_type_label(&r.ocr_method)).or_insert(0) += 1;
    }
    let summary: Vec<String> = ["Text", "OCR", "OCR+Vision", "Vision", "Mixed", "Unknown"]
        .iter()
        .filter_map(|&l| counts.get(l).map(|c| format!("{c}× {l}")))
        .collect();
    if !summary.is_empty() {
        eprintln!("  Methods: {}", summary.join("   "));
    }
    if !failed.is_empty() {
        eprintln!("  {} failure{}", failed.len(), if failed.len() == 1 { "" } else { "s" });
    }
    eprintln!("  Total time: {:.1}s", elapsed.as_secs_f32());
    eprintln!("{bar}");
}

fn scan_source_dir(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_files(dir, &mut files)
        .with_context(|| format!("scanning source dir: {}", dir.display()))?;
    files.sort();
    Ok(files)
}

fn collect_files(dir: &Path, files: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("reading dir: {}", dir.display()))?
        .flatten()
    {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            collect_files(&path, files)?;
        } else if path.is_file() {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
            if SUPPORTED_EXTENSIONS.contains(&ext.as_str()) {
                files.push(path);
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Offline subcommand
// ---------------------------------------------------------------------------

async fn run_offline(args: ExtractOfflineArgs) -> anyhow::Result<()> {
    use pdf_core::extraction::offline;

    let config = ClaudeConfig::load()?;
    let outputs_base = config.resolve_outputs_dir(args.outputs_dir);
    let offline_base = outputs_base.join("offline");
    let source_dir = config.resolve_source_dir(args.source_dir.clone());

    let schema = match &config.schema_path {
        Some(p) => SchemaRegistry::load(std::path::Path::new(p))?,
        None => SchemaRegistry::load_default()?,
    };

    let paths = resolve_paths(args.paths, source_dir)?;
    if paths.is_empty() {
        anyhow::bail!("No supported files found to extract.");
    }

    let person_field_names: Vec<String> = schema
        .searchable_person_field_names()
        .into_iter()
        .map(str::to_string)
        .collect();
    let date_field_names: Vec<String> = schema
        .searchable_date_field_names()
        .into_iter()
        .map(str::to_string)
        .collect();
    let person_refs: Vec<&str> = person_field_names.iter().map(String::as_str).collect();
    let date_refs: Vec<&str> = date_field_names.iter().map(String::as_str).collect();
    let existing_index =
        MetadataIndex::build(&offline_base.join("text"), &person_refs, &date_refs)
            .unwrap_or_else(|_| MetadataIndex { entries: Default::default(), known_persons: vec![] });
    let known_persons = existing_index.known_persons;

    let name_width = name_column_width(&paths);
    let bar = make_progress_bar(args.json, paths.len() as u64);
    let mut completed: Vec<FileReport> = Vec::new();
    let mut failed: Vec<FailedReport> = Vec::new();
    let total_start = Instant::now();

    for path in &paths {
        let file_name = file_name_str(path);
        emit_started(args.json, &file_name, bar.as_ref());

        let file_start = Instant::now();
        match offline::extract_offline(path, &schema, &known_persons).await {
            Ok((mut result, pages)) => {
                let elapsed_ms = file_start.elapsed().as_millis() as u64;
                result.extraction_mode = "offline".to_string();

                let doc_category = result.doc_category.clone();
                let ocr_method = result.ocr_method.clone();

                let category_dir = offline_base
                    .join(if doc_category == "image" { "images" } else { "text" });
                std::fs::create_dir_all(&category_dir)
                    .with_context(|| format!("creating {}", category_dir.display()))?;

                let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown");
                let output_path = category_dir.join(format!("{stem}.md"));

                let md = frontmatter::generate_md(
                    &result,
                    path,
                    &pages,
                    &schema,
                    &known_persons,
                    None,
                    elapsed_ms,
                );
                std::fs::write(&output_path, &md)
                    .with_context(|| format!("writing {}", output_path.display()))?;

                let char_count = md.len();
                emit_complete(
                    args.json,
                    &file_name,
                    &output_path,
                    char_count,
                    &ocr_method,
                    elapsed_ms,
                    "offline",
                    bar.as_ref(),
                    name_width,
                );
                completed.push(FileReport { file_name, output_path, ocr_method, char_count });
            }
            Err(e) => {
                let elapsed_ms = file_start.elapsed().as_millis() as u64;
                let reason = format!("{e:#}");
                emit_error(args.json, &file_name, &reason, elapsed_ms, bar.as_ref());
                failed.push(FailedReport { file_name, reason });
            }
        }
    }

    finish_bar(bar);
    print_report(&completed, &failed, args.json, total_start.elapsed(), "offline");
    Ok(())
}

// ---------------------------------------------------------------------------
// Online subcommand
// ---------------------------------------------------------------------------

async fn run_online(args: ExtractOnlineArgs) -> anyhow::Result<()> {
    let config = ClaudeConfig::load()?;
    let outputs_base = config.resolve_outputs_dir(args.outputs_dir.clone());
    let offline_base = outputs_base.join("offline");
    let online_base = outputs_base.join("online");

    let mut schema = match &config.schema_path {
        Some(p) => SchemaRegistry::load(std::path::Path::new(p))?,
        None => SchemaRegistry::load_default()?,
    };

    let md_files: Vec<PathBuf> = if args.all {
        collect_offline_md_files(&offline_base, &online_base)
    } else {
        if args.paths.is_empty() {
            anyhow::bail!(
                "Specify paths to enrich, or use --all to enrich all offline documents."
            );
        }
        args.paths
            .iter()
            .map(|p| find_offline_md(&offline_base, p))
            .collect::<anyhow::Result<Vec<_>>>()?
    };

    if md_files.is_empty() {
        eprintln!("No offline documents found to enrich.");
        return Ok(());
    }

    let person_field_names: Vec<String> = schema
        .searchable_person_field_names()
        .into_iter()
        .map(str::to_string)
        .collect();
    let date_field_names: Vec<String> = schema
        .searchable_date_field_names()
        .into_iter()
        .map(str::to_string)
        .collect();
    let person_refs: Vec<&str> = person_field_names.iter().map(String::as_str).collect();
    let date_refs: Vec<&str> = date_field_names.iter().map(String::as_str).collect();
    let index = MetadataIndex::build(&offline_base.join("text"), &person_refs, &date_refs)
        .unwrap_or_else(|_| MetadataIndex { entries: Default::default(), known_persons: vec![] });
    let known_persons = index.known_persons;

    let name_width = name_column_width(&md_files);
    let bar = make_progress_bar(args.json, md_files.len() as u64);
    let mut completed: Vec<FileReport> = Vec::new();
    let mut failed: Vec<FailedReport> = Vec::new();
    let total_start = Instant::now();

    for md_path in &md_files {
        let file_name = file_name_str(md_path);
        emit_started(args.json, &file_name, bar.as_ref());

        let file_start = Instant::now();
        match enrich_online_file(
            md_path,
            &config,
            &mut schema,
            &online_base,
            &known_persons,
            args.json,
            args.auto_schema,
            bar.as_ref(),
        )
        .await
        {
            Ok((output_path, ocr_method)) => {
                let elapsed_ms = file_start.elapsed().as_millis() as u64;
                let char_count = std::fs::read_to_string(&output_path).map(|s| s.len()).unwrap_or(0);
                emit_complete(
                    args.json,
                    &file_name,
                    &output_path,
                    char_count,
                    &ocr_method,
                    elapsed_ms,
                    "online",
                    bar.as_ref(),
                    name_width,
                );
                completed.push(FileReport { file_name, output_path, ocr_method, char_count });
            }
            Err(e) => {
                let elapsed_ms = file_start.elapsed().as_millis() as u64;
                let reason = format!("{e:#}");
                emit_error(args.json, &file_name, &reason, elapsed_ms, bar.as_ref());
                failed.push(FailedReport { file_name, reason });
            }
        }
    }

    finish_bar(bar);
    let model_name = match config.backend {
        LlmBackend::Gemini => config.resolved_gemini_model().to_string(),
        LlmBackend::Claude => config.model.clone(),
        LlmBackend::Ollama => config.resolved_ollama_model().to_string(),
    };
    print_report(&completed, &failed, args.json, total_start.elapsed(), &model_name);
    Ok(())
}

// ---------------------------------------------------------------------------
// Online helpers
// ---------------------------------------------------------------------------

fn find_offline_md(offline_base: &Path, source_path: &Path) -> anyhow::Result<PathBuf> {
    let stem = source_path
        .file_stem()
        .and_then(|s| s.to_str())
        .context("invalid source path")?;
    let ext = source_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let is_image = matches!(ext.as_str(), "jpg" | "jpeg" | "png");
    if is_image {
        let p = offline_base.join("images").join(format!("{stem}.md"));
        if p.exists() {
            return Ok(p);
        }
    } else {
        let text_p = offline_base.join("text").join(format!("{stem}.md"));
        if text_p.exists() {
            return Ok(text_p);
        }
        let img_p = offline_base.join("images").join(format!("{stem}.md"));
        if img_p.exists() {
            return Ok(img_p);
        }
    }
    anyhow::bail!(
        "No offline extraction found for '{}' — run 'pdf-lab extract offline {}' first.",
        source_path.display(),
        source_path.display()
    )
}

fn collect_offline_md_files(offline_base: &Path, online_base: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    for sub in &["text", "images"] {
        let offline_dir = offline_base.join(sub);
        let online_dir  = online_base.join(sub);
        if !offline_dir.exists() {
            continue;
        }
        if let Ok(entries) = std::fs::read_dir(&offline_dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.extension().and_then(|e| e.to_str()) != Some("md") {
                    continue;
                }
                // Skip if online version already exists
                if let Some(name) = p.file_name() {
                    if online_dir.join(name).exists() {
                        continue;
                    }
                }
                result.push(p);
            }
        }
    }
    result.sort();
    result
}

fn parse_page_body(body: &str) -> Vec<PageContent> {
    let mut pages: Vec<PageContent> = Vec::new();
    let mut current_num: u32 = 0;
    let mut current_text = String::new();

    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("[Page ") {
            if let Some(n_str) = rest.strip_suffix(']') {
                if let Ok(n) = n_str.parse::<u32>() {
                    if current_num > 0 {
                        pages.push(PageContent::Text {
                            page_num: current_num,
                            text: current_text.trim().to_string(),
                        });
                        current_text = String::new();
                    }
                    current_num = n;
                    continue;
                }
            }
        }
        current_text.push_str(line);
        current_text.push('\n');
    }
    if current_num > 0 {
        pages.push(PageContent::Text {
            page_num: current_num,
            text: current_text.trim().to_string(),
        });
    }
    if pages.is_empty() && !body.trim().is_empty() {
        pages.push(PageContent::Text { page_num: 1, text: body.trim().to_string() });
    }
    pages
}

async fn enrich_online_file(
    md_path: &Path,
    config: &ClaudeConfig,
    schema: &mut SchemaRegistry,
    online_base: &Path,
    known_persons: &[String],
    json_mode: bool,
    auto_schema: bool,
    bar: Option<&ProgressBar>,
) -> anyhow::Result<(PathBuf, String)> {
    let content = std::fs::read_to_string(md_path)
        .with_context(|| format!("reading {}", md_path.display()))?;
    let fm = frontmatter::parse_frontmatter(&content)
        .context("no frontmatter in offline file")?;

    let get_fm = |key: &str| -> String {
        fm.get(key)
            .and_then(|v| match v {
                serde_yaml::Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_default()
    };

    let doc_category = get_fm("doc_category");
    let source_file = get_fm("source_file");
    let stem = md_path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown");

    let pages: Vec<PageContent> = if doc_category == "image" {
        if source_file.is_empty() {
            anyhow::bail!("image offline doc missing source_file in frontmatter");
        }
        let src = Path::new(&source_file);
        let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
        match ext.as_str() {
            "pdf" => pdfium::render_pdf(src)?,
            "jpg" | "jpeg" | "png" => pdfium::load_image(src)?,
            _ => anyhow::bail!("unsupported source extension for online enrichment"),
        }
    } else {
        let body = frontmatter::strip_frontmatter(&content);
        parse_page_body(body)
    };

    if doc_category != "image" && source_file.is_empty() {
        anyhow::bail!(
            "text offline doc '{}' is missing source_file in frontmatter — cannot enrich",
            md_path.display()
        );
    }

    let ctx = OnlineFileContext {
        stem: stem.to_string(),
        doc_category: doc_category.clone(),
    };
    let (mut result, enrichment) = ctx
        .run_llm_pipeline(&pages, config, schema, json_mode, auto_schema, bar)
        .await?;
    result.extraction_mode = "online".to_string();
    result.doc_category = doc_category.clone();

    let category_dir = online_base.join(if doc_category == "image" { "images" } else { "text" });
    std::fs::create_dir_all(&category_dir)?;
    let output_path = category_dir.join(format!("{stem}.md"));

    let ocr_method = result.ocr_method.clone();
    let source_path = if source_file.is_empty() {
        md_path.to_path_buf()
    } else {
        PathBuf::from(&source_file)
    };
    let md = frontmatter::generate_md(
        &result,
        &source_path,
        &pages,
        schema,
        known_persons,
        enrichment.as_ref(),
        0,
    );
    std::fs::write(&output_path, &md)?;

    Ok((output_path, ocr_method))
}

// ---------------------------------------------------------------------------
// OnlineFileContext — LLM pipeline (same logic as old FileContext::run)
// ---------------------------------------------------------------------------

struct OnlineFileContext {
    stem: String,
    doc_category: String,
}

impl OnlineFileContext {
    async fn run_llm_pipeline(
        &self,
        pages: &[PageContent],
        config: &ClaudeConfig,
        schema: &mut SchemaRegistry,
        json_mode: bool,
        auto_schema: bool,
        bar: Option<&ProgressBar>,
    ) -> anyhow::Result<(pdf_core::extraction::ExtractionResult, Option<enrich::EnrichmentResult>)>
    {
        let file_name = format!("{}.md", self.stem);
        let log: Box<dyn Fn(&str)> = if let Some(pb) = bar {
            let pb = pb.clone();
            Box::new(move |s: &str| pb.println(s))
        } else if !json_mode {
            Box::new(|s: &str| eprintln!("{s}"))
        } else {
            Box::new(|_: &str| {})
        };

        // Pass 0: doc_type from filename heuristic, then LLM classify
        let doc_type = if let Some(dt) = schema.infer_doc_type_from_stem(&self.stem) {
            dt
        } else {
            if let Some(pb) = bar {
                pb.set_message("classifying...");
            }
            match &config.backend {
                LlmBackend::Claude => claude::classify_doc_type(pages, config, schema)
                    .await
                    .with_context(|| format!("classifying {file_name}"))?,
                LlmBackend::Gemini => gemini::classify_doc_type(pages, config, schema)
                    .await
                    .with_context(|| format!("classifying {file_name}"))?,
                LlmBackend::Ollama => ollama::classify_doc_type(pages, config, schema)
                    .await
                    .unwrap_or_else(|_| schema.doc_type_default.clone()),
            }
        };

        // Pass 1.5: auto-schema — suggest and persist per-type fields for unknown doc types
        if auto_schema
            && doc_type != schema.doc_type_default
            && !schema.type_fields.contains_key(&doc_type)
        {
            if let Some(pb) = bar {
                pb.set_message("suggesting schema fields...");
            } else if !json_mode {
                eprintln!("  → suggesting fields for new type: {doc_type}");
            }

            let first_page_text: String = pages
                .iter()
                .find_map(|p| {
                    if let PageContent::Text { text, .. } = p {
                        Some(text.chars().take(2000).collect())
                    } else {
                        None
                    }
                })
                .unwrap_or_default();

            let global_names: Vec<&str> =
                schema.global_fields.iter().map(|f| f.name.as_str()).collect();

            let suggest_result = match &config.backend {
                LlmBackend::Ollama => {
                    suggest::call_ollama_suggest_fields(
                        &first_page_text,
                        &doc_type,
                        &global_names,
                        config,
                    )
                    .await
                }
                LlmBackend::Claude | LlmBackend::Gemini => {
                    suggest::call_claude_suggest_fields(
                        &first_page_text,
                        &doc_type,
                        &global_names,
                        config,
                    )
                    .await
                }
            };

            match suggest_result {
                Ok(suggested) if !suggested.is_empty() => {
                    let field_defs: Vec<FieldDef> = suggested
                        .into_iter()
                        .map(|s| FieldDef {
                            name: s.name,
                            field_type: s.field_type,
                            required: s.required,
                            searchable: s.searchable,
                        })
                        .collect();
                    let default_path = SchemaRegistry::default_config_path();
                    let schema_path = config
                        .schema_path
                        .as_deref()
                        .map(std::path::Path::new)
                        .unwrap_or(&default_path);
                    if let Err(e) = schema.append_type_to_file(schema_path, &doc_type, &field_defs)
                    {
                        log(&format!("    ⚠ auto-schema write failed: {e:#}"));
                    }
                    let n = field_defs.len();
                    schema.add_type_fields(doc_type.clone(), field_defs);
                    log(&format!("    ✓ auto-schema: added {n} field(s) for {doc_type}"));
                }
                Ok(_) => log(&format!("    ⚠ auto-schema: no fields suggested for {doc_type}")),
                Err(e) => log(&format!("    ⚠ auto-schema suggest failed: {e:#}")),
            }
        }

        if self.doc_category == "text" {
            // ── Text pipeline ────────────────────────────────────────────────────
            // Pass 2: table reformatting
            let extraction_pages = match &config.backend {
                LlmBackend::Claude => {
                    if let Some(pb) = bar {
                        pb.set_message("reformatting tables...");
                    }
                    match table::call_claude_table_reformat(pages, config).await {
                        Ok(p) => p,
                        Err(e) => {
                            log(&format!("    ⚠ table reformat skipped: {e:#}"));
                            pages.to_vec()
                        }
                    }
                }
                LlmBackend::Gemini => {
                    if let Some(pb) = bar {
                        pb.set_message("reformatting tables (Gemini)...");
                    }
                    match table::call_gemini_table_reformat(pages, config).await {
                        Ok(p) => p,
                        Err(e) => {
                            log(&format!("    ⚠ table reformat skipped: {e:#}"));
                            pages.to_vec()
                        }
                    }
                }
                LlmBackend::Ollama => pages.to_vec(),
            };

            // Pass 3: schema extraction
            let r = match &config.backend {
                LlmBackend::Claude => {
                    if let Some(pb) = bar {
                        pb.set_message("extracting fields (Claude)...");
                    }
                    claude::call_claude(&extraction_pages, config, schema, &doc_type)
                        .await
                        .with_context(|| format!("Claude extraction for {file_name}"))?
                }
                LlmBackend::Gemini => {
                    if let Some(pb) = bar {
                        pb.set_message(format!(
                            "extracting fields (Gemini {})...",
                            config.resolved_gemini_model()
                        ));
                    }
                    gemini::call_gemini(&extraction_pages, config, schema, &doc_type)
                        .await
                        .with_context(|| format!("Gemini extraction for {file_name}"))?
                }
                LlmBackend::Ollama => {
                    if let Some(pb) = bar {
                        pb.set_message(format!(
                            "Ollama ({})...",
                            config.resolved_ollama_model()
                        ));
                    }
                    ollama::call_ollama(
                        &extraction_pages,
                        config,
                        schema,
                        &doc_type,
                        log.as_ref(),
                    )
                    .await
                    .with_context(|| format!("Ollama extraction for {file_name}"))?
                }
            };
            Ok((r, None))
        } else {
            // ── Image pipeline ───────────────────────────────────────────────────
            // Pass 2: vision extraction
            let r = match &config.backend {
                LlmBackend::Claude => {
                    if let Some(pb) = bar {
                        pb.set_message("extracting fields (Claude)...");
                    }
                    claude::call_claude(pages, config, schema, &doc_type)
                        .await
                        .with_context(|| format!("Claude extraction for {file_name}"))?
                }
                LlmBackend::Gemini => {
                    if let Some(pb) = bar {
                        pb.set_message(format!(
                            "extracting fields (Gemini {})...",
                            config.resolved_gemini_model()
                        ));
                    }
                    gemini::call_gemini(pages, config, schema, &doc_type)
                        .await
                        .with_context(|| format!("Gemini extraction for {file_name}"))?
                }
                LlmBackend::Ollama => {
                    if let Some(pb) = bar {
                        pb.set_message(format!(
                            "Ollama ({})...",
                            config.resolved_ollama_model()
                        ));
                    }
                    ollama::call_ollama(pages, config, schema, &doc_type, log.as_ref())
                        .await
                        .with_context(|| format!("Ollama extraction for {file_name}"))?
                }
            };

            // Pass 3: mandatory frontmatter review (enrichment). Ollama skipped.
            let enrichment = match &config.backend {
                LlmBackend::Claude => {
                    if let Some(pb) = bar {
                        pb.set_message("reviewing frontmatter...");
                    }
                    match enrich::call_claude_enrich(&r.pages, &r.doc_type, config).await {
                        Ok(e) => Some(e),
                        Err(e) => {
                            log(&format!("    ⚠ frontmatter review skipped: {e:#}"));
                            None
                        }
                    }
                }
                LlmBackend::Gemini => {
                    if let Some(pb) = bar {
                        pb.set_message("reviewing frontmatter (Gemini)...");
                    }
                    match enrich::call_gemini_enrich(&r.pages, &r.doc_type, config).await {
                        Ok(e) => Some(e),
                        Err(e) => {
                            log(&format!("    ⚠ frontmatter review skipped: {e:#}"));
                            None
                        }
                    }
                }
                LlmBackend::Ollama => None,
            };
            Ok((r, enrichment))
        }
    }
}
