use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::Args;
use indicatif::{ProgressBar, ProgressStyle};
use serde_json::json;

use pdf_core::{
    config::{ClaudeConfig, LlmBackend},
    extraction::{PageContent, claude, enrich, gemini, ollama, pdfium, suggest},
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

#[derive(Args)]
pub struct ExtractArgs {
    #[arg(
        help = "PDF or image files to extract. Omit to scan the source directory from config."
    )]
    pub paths: Vec<PathBuf>,

    #[arg(long, help = "Override source directory (scan for PDFs/images when no paths given)")]
    pub source_dir: Option<PathBuf>,

    #[arg(long, help = "Override output directory for .md files")]
    pub outputs_dir: Option<PathBuf>,

    #[arg(long, help = "Emit JSON lines to stdout instead of human-readable stderr")]
    pub json: bool,

    #[arg(long, help = "Auto-suggest and persist schema fields for unknown document types")]
    pub auto_schema: bool,
}

pub async fn run(args: ExtractArgs) -> anyhow::Result<()> {
    let config = ClaudeConfig::load()?;

    let outputs_dir = config.resolve_outputs_dir(args.outputs_dir);
    let source_dir = config.resolve_source_dir(args.source_dir.clone());

    // Load schema (user's schema.toml or built-in default)
    let mut schema = match &config.schema_path {
        Some(p) => SchemaRegistry::load_or_default(std::path::Path::new(p)),
        None => SchemaRegistry::load_default(),
    };

    // Resolve paths: explicit args > scan source_dir > error
    // scan_root is Some only when scanning a directory; used to mirror subdirectory structure in outputs.
    let (paths, scan_root): (Vec<PathBuf>, Option<PathBuf>) = if !args.paths.is_empty() {
        let mut expanded: Vec<PathBuf> = Vec::new();
        let mut dir_root: Option<PathBuf> = None;

        for p in args.paths {
            let resolved = if p.exists() {
                p
            } else if let Some(ref src) = source_dir {
                let candidate = src.join(&p);
                if candidate.exists() { candidate } else { p }
            } else {
                p
            };

            if resolved.is_dir() {
                collect_files(&resolved, &mut expanded)
                    .with_context(|| format!("scanning directory: {}", resolved.display()))?;
                if dir_root.is_none() {
                    dir_root = Some(resolved);
                }
            } else {
                expanded.push(resolved);
            }
        }

        // Use dir_root as scan_root only when a single directory was the sole argument
        // (multiple dirs or mixed file+dir args produce flat output).
        (expanded, dir_root)
    } else {
        let src = source_dir
            .context("No files specified and no source_dir set in config. Pass file paths or run: pdf-lab config set --source-dir DIR")?;
        let files = scan_source_dir(&src)?;
        (files, Some(src))
    };

    if paths.is_empty() {
        anyhow::bail!("No supported files found to extract.");
    }

    // When scanning from a source root, place outputs under "{source-folder}-{model}" so
    // extractions from different models never overwrite each other.
    let outputs_dir = if let Some(ref root) = scan_root {
        let source_folder = root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("source");
        let model_slug = match config.backend {
            LlmBackend::Gemini => config.resolved_gemini_model().replace(':', "-"),
            LlmBackend::Claude => config.model.replace(':', "-"),
            LlmBackend::Ollama => config.resolved_ollama_model().replace(':', "-"),
        };
        outputs_dir.join(format!("{source_folder}-{model_slug}"))
    } else {
        outputs_dir
    };

    std::fs::create_dir_all(&outputs_dir)
        .with_context(|| format!("creating outputs dir: {}", outputs_dir.display()))?;

    // Collect as owned strings before entering the &mut schema loop.
    let person_field_names: Vec<String> = schema.searchable_person_field_names().into_iter().map(str::to_string).collect();
    let date_field_names: Vec<String> = schema.searchable_date_field_names().into_iter().map(str::to_string).collect();
    let person_refs: Vec<&str> = person_field_names.iter().map(String::as_str).collect();
    let date_refs: Vec<&str> = date_field_names.iter().map(String::as_str).collect();
    let index = MetadataIndex::build(&outputs_dir, &person_refs, &date_refs).unwrap_or_else(|_| MetadataIndex {
        entries: Default::default(),
        known_persons: vec![],
    });

    let name_width = paths.iter()
        .filter_map(|p| p.file_name()?.to_str().map(|s| s.len()))
        .max()
        .unwrap_or(8)
        .max(8);

    let bar: Option<ProgressBar> = if !args.json {
        let pb = ProgressBar::new(paths.len() as u64);
        pb.set_style(
            ProgressStyle::with_template(
                "{spinner:.cyan} [{pos}/{len}] {prefix:.bold.cyan}  {msg}"
            )
            .unwrap()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
        );
        pb.enable_steady_tick(Duration::from_millis(80));
        Some(pb)
    } else {
        None
    };

    let mut completed: Vec<FileReport> = Vec::new();
    let mut error_count: usize = 0;
    let total_start = Instant::now();

    for path in &paths {
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");

        if args.json {
            println!("{}", json!({"event": "started", "file": file_name}));
        } else if let Some(ref pb) = bar {
            pb.set_prefix(file_name.to_string());
        }

        let file_start = Instant::now();
        match extract_one(path, &config, &mut schema, &outputs_dir, scan_root.as_deref(), &index.known_persons, args.json, args.auto_schema, bar.as_ref()).await {
            Ok((output_path, ocr_method)) => {
                let file_elapsed = file_start.elapsed();
                let char_count = std::fs::read_to_string(&output_path)
                    .map(|s| s.len())
                    .unwrap_or(0);
                if args.json {
                    println!("{}", json!({"event":"complete","file":file_name,"output":output_path.display().to_string(),"chars_extracted":char_count,"extraction_type":extraction_type_label(&ocr_method),"ocr_method":ocr_method,"elapsed_ms":file_elapsed.as_millis()}));
                } else if let Some(ref pb) = bar {
                    let label = extraction_type_label(&ocr_method);
                    pb.println(format!("  ✓ {:<name_width$}  {:<10}  ({char_count} chars)  {:.1}s",
                        file_name, label, file_elapsed.as_secs_f32(), name_width = name_width));
                    pb.inc(1);
                }
                completed.push(FileReport {
                    file_name: file_name.to_string(),
                    output_path,
                    ocr_method,
                    char_count,
                });
            }
            Err(e) => {
                let file_elapsed = file_start.elapsed();
                if args.json {
                    println!("{}", json!({"event":"error","file":file_name,"elapsed_ms":file_elapsed.as_millis(),"message":e.to_string()}));
                } else if let Some(ref pb) = bar {
                    pb.println(format!("  ✗ {file_name}  error: {e:#}  ({:.1}s)", file_elapsed.as_secs_f32()));
                    pb.inc(1);
                }
                error_count += 1;
            }
        }
    }

    if let Some(pb) = bar {
        pb.finish_and_clear();
    }

    print_report(&completed, error_count, args.json, total_start.elapsed());

    Ok(())
}

fn print_report(completed: &[FileReport], error_count: usize, json_mode: bool, elapsed: Duration) {
    if json_mode {
        let entries: Vec<_> = completed.iter().map(|r| json!({
            "file": r.file_name,
            "output": r.output_path.display().to_string(),
            "extraction_type": extraction_type_label(&r.ocr_method),
            "ocr_method": r.ocr_method,
            "chars_extracted": r.char_count,
        })).collect();
        println!("{}", json!({"event":"report","files":entries,"errors":error_count,"elapsed_ms":elapsed.as_millis()}));
        return;
    }

    let total = completed.len() + error_count;
    if total == 0 {
        return;
    }

    let name_width = completed.iter().map(|r| r.file_name.len()).max().unwrap_or(8).max(8);
    let bar = "─".repeat(name_width + 30);

    eprintln!();
    eprintln!("{bar}");
    eprintln!("  Extract report  {total} file{}", if total == 1 { "" } else { "s" });
    eprintln!("{bar}");

    for r in completed {
        let label = extraction_type_label(&r.ocr_method);
        eprintln!("  {:<name_width$}  {:<10}  ({} chars)",
            r.file_name, label, r.char_count, name_width = name_width);
    }

    if error_count > 0 {
        eprintln!("  {} error{}", error_count, if error_count == 1 { "" } else { "s" });
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

async fn extract_one(
    path: &PathBuf,
    config: &ClaudeConfig,
    schema: &mut SchemaRegistry,
    outputs_dir: &PathBuf,
    source_root: Option<&Path>,
    known_persons: &[String],
    json_mode: bool,
    auto_schema: bool,
    bar: Option<&ProgressBar>,
) -> anyhow::Result<(PathBuf, String)> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");

    let pages: Vec<PageContent> = match ext.as_str() {
        "pdf" => {
            if let Some(pb) = bar { pb.set_message("rendering pages..."); }
            let p = pdfium::render_pdf(path)
                .with_context(|| format!("rendering {}", path.display()))?;
            let text_count = p.iter().filter(|pg| !pg.is_image()).count();
            let img_count = p.iter().filter(|pg| pg.is_image()).count();
            if let Some(pb) = bar {
                pb.println(format!("    {} pages  (text: {}  image: {})", p.len(), text_count, img_count));
            } else if !json_mode {
                eprintln!("  → {} pages (text: {}, image: {})", p.len(), text_count, img_count);
            }
            p
        }
        "jpg" | "jpeg" | "png" => {
            if let Some(pb) = bar { pb.set_message("loading image..."); }
            else if !json_mode { eprintln!("  → loading image"); }
            pdfium::load_image(path)
                .with_context(|| format!("loading image {}", path.display()))?
        }
        other => anyhow::bail!("Unsupported file type: .{other}"),
    };

    let backend = &config.backend;

    let log: Box<dyn Fn(&str)> = if let Some(pb) = bar {
        let pb = pb.clone();
        Box::new(move |s: &str| pb.println(s))
    } else if !json_mode {
        Box::new(|s: &str| eprintln!("{s}"))
    } else {
        Box::new(|_: &str| {})
    };

    // Pass 0: filename heuristic (free — no LLM call)
    let doc_type = if let Some(dt) = schema.infer_doc_type_from_stem(stem) {
        if let Some(pb) = bar {
            pb.println(format!("    doc_type: {dt}  (from filename)"));
        } else if !json_mode {
            eprintln!("  → doc_type={dt} (from filename)");
        }
        dt
    } else {
        // Pass 1: classification
        if let Some(pb) = bar { pb.set_message("classifying..."); }
        let dt = match backend {
            LlmBackend::Gemini => {
                gemini::classify_doc_type(&pages, config, schema)
                    .await
                    .with_context(|| format!("classifying {file_name} via Gemini"))?
            }
            LlmBackend::Claude => {
                claude::classify_doc_type(&pages, config, schema)
                    .await
                    .with_context(|| format!("classifying {file_name} via Claude"))?
            }
            LlmBackend::Ollama => {
                match ollama::classify_doc_type(&pages, config, schema).await {
                    Ok(dt) => dt,
                    Err(_) => schema.doc_type_default.clone(),
                }
            }
        };
        if let Some(pb) = bar {
            pb.println(format!("    doc_type: {dt}"));
        } else if !json_mode {
            eprintln!("  → doc_type={dt}");
        }
        dt
    };

    // Pass 1.5: auto-schema — suggest and persist per-type fields for unknown doc types
    if auto_schema
        && doc_type != schema.doc_type_default
        && !schema.type_fields.contains_key(&doc_type)
        && !config.api_key.is_empty()
    {
        if let Some(pb) = bar { pb.set_message("suggesting schema fields..."); }
        else if !json_mode { eprintln!("  → suggesting fields for new type: {doc_type}"); }

        let first_page_text: String = pages.iter()
            .find_map(|p| if let PageContent::Text { text, .. } = p {
                Some(text.chars().take(2000).collect())
            } else {
                None
            })
            .unwrap_or_default();

        let global_names: Vec<&str> = schema.global_fields.iter()
            .map(|f| f.name.as_str())
            .collect();

        match suggest::call_claude_suggest_fields(
            &first_page_text, &doc_type, &global_names, config,
        ).await {
            Ok(suggested) if !suggested.is_empty() => {
                let field_defs: Vec<FieldDef> = suggested.into_iter().map(|s| FieldDef {
                    name: s.name,
                    field_type: s.field_type,
                    required: s.required,
                    searchable: s.searchable,
                }).collect();

                let default_path = SchemaRegistry::default_config_path();
                let schema_path = config.schema_path.as_deref()
                    .map(std::path::Path::new)
                    .unwrap_or(&default_path);

                if let Err(e) = schema.append_type_to_file(schema_path, &doc_type, &field_defs) {
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

    // Pass 2: extraction
    let (result, enrich_backend) = match backend {
        LlmBackend::Gemini => {
            if let Some(pb) = bar { pb.set_message(format!("extracting fields (Gemini {})...", config.resolved_gemini_model())); }
            else if !json_mode { eprintln!("  → Gemini ({}) extracting...", config.resolved_gemini_model()); }
            let r = gemini::call_gemini(&pages, config, schema, &doc_type)
                .await
                .with_context(|| format!("Gemini extraction for {file_name}"))?;
            (r, Some(LlmBackend::Gemini))
        }
        LlmBackend::Claude => {
            if let Some(pb) = bar { pb.set_message("extracting fields (Claude)..."); }
            else if !json_mode { eprintln!("  → Claude extracting fields..."); }
            let r = claude::call_claude(&pages, config, schema, &doc_type)
                .await
                .with_context(|| format!("Claude extraction for {file_name}"))?;
            (r, Some(LlmBackend::Claude))
        }
        LlmBackend::Ollama => {
            if let Some(pb) = bar { pb.set_message(format!("Ollama ({})...", config.resolved_ollama_model())); }
            else if !json_mode { eprintln!("  → Ollama ({}) extracting...", config.resolved_ollama_model()); }
            let r = ollama::call_ollama(&pages, config, schema, &doc_type, log.as_ref())
                .await
                .with_context(|| format!("Ollama extraction failed for {file_name}"))?;
            (r, None)
        }
    };

    let ocr_method = result.ocr_method.clone();

    // Pass 3: enrich with entities and key_info
    let enrichment = match enrich_backend {
        Some(LlmBackend::Claude) => {
            if let Some(pb) = bar { pb.set_message("enriching..."); }
            else if !json_mode { eprintln!("  → enriching document..."); }
            match enrich::call_claude_enrich(&result.pages, &result.doc_type, config).await {
                Ok(e) => Some(e),
                Err(err) => {
                    let msg = format!("    ⚠ enrichment skipped: {err:#}");
                    if let Some(pb) = bar { pb.println(&msg); }
                    else if !json_mode { eprintln!("{msg}"); }
                    None
                }
            }
        }
        Some(LlmBackend::Gemini) => {
            if let Some(pb) = bar { pb.set_message("enriching..."); }
            else if !json_mode { eprintln!("  → enriching document..."); }
            match enrich::call_gemini_enrich(&result.pages, &result.doc_type, config).await {
                Ok(e) => Some(e),
                Err(err) => {
                    let msg = format!("    ⚠ enrichment skipped: {err:#}");
                    if let Some(pb) = bar { pb.println(&msg); }
                    else if !json_mode { eprintln!("{msg}"); }
                    None
                }
            }
        }
        Some(LlmBackend::Ollama) | None => None,
    };

    let md_content = frontmatter::generate_md(&result, path, &pages, schema, known_persons, enrichment.as_ref());

    // Mirror subdirectory structure when scanning from a source root.
    let output_path = if let Some(root) = source_root {
        if let Ok(rel) = path.strip_prefix(root) {
            let sub = rel.parent().unwrap_or(Path::new(""));
            let out_dir = outputs_dir.join(sub);
            std::fs::create_dir_all(&out_dir)
                .with_context(|| format!("creating output subdir: {}", out_dir.display()))?;
            out_dir.join(format!("{stem}.md"))
        } else {
            outputs_dir.join(format!("{stem}.md"))
        }
    } else {
        outputs_dir.join(format!("{stem}.md"))
    };

    std::fs::write(&output_path, &md_content)
        .with_context(|| format!("writing {}", output_path.display()))?;

    Ok((output_path, ocr_method))
}
