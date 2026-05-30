use clap::Args;
use rmcp::{ServiceExt, tool, tool_router, handler::server::wrapper::Parameters};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use pdf_core::{
    config::ClaudeConfig,
    extraction::{PageContent, claude, ocr, pdfium},
    frontmatter,
    schema::SchemaRegistry,
    search::{classify, index::MetadataIndex},
};

#[derive(Args)]
pub struct McpArgs {}

pub async fn run(_args: McpArgs) -> anyhow::Result<()> {
    let (stdin, stdout) = rmcp::transport::io::stdio();
    let server = PdfLabServer;
    server.serve((stdin, stdout)).await?.waiting().await?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct PdfLabServer;

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct ExtractDocumentInput {
    #[schemars(description = "Absolute or relative path to the PDF or image file")]
    pub file_path: String,
    #[schemars(description = "File type: 'pdf' or 'image'")]
    pub file_type: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct OcrScanInput {
    #[schemars(description = "Absolute path to a JPEG or PNG image file to OCR")]
    pub file_path: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct ClassifyQueryInput {
    #[schemars(description = "The search query to classify")]
    pub query: String,
    #[schemars(description = "Known person names in the document library")]
    pub known_persons: Vec<String>,
    #[schemars(description = "Known document type values in the schema")]
    pub known_doc_types: Vec<String>,
}

#[tool_router(server_handler)]
impl PdfLabServer {
    #[tool(
        name = "extract_document",
        description = "Extract text and metadata from a PDF or image file using Claude AI with local Tesseract OCR."
    )]
    pub async fn extract_document(
        &self,
        Parameters(input): Parameters<ExtractDocumentInput>,
    ) -> String {
        match do_extract(input).await {
            Ok(output) => output,
            Err(e) => json!({"error": e.to_string()}).to_string(),
        }
    }

    #[tool(
        name = "ocr_scan",
        description = "Run local Tesseract OCR on an image file. Returns extracted text and per-word confidence scores."
    )]
    pub async fn ocr_scan(
        &self,
        Parameters(input): Parameters<OcrScanInput>,
    ) -> String {
        match do_ocr_scan(input).await {
            Ok(result) => serde_json::to_string(&result).unwrap_or_default(),
            Err(e) => json!({"error": e.to_string()}).to_string(),
        }
    }

    #[tool(
        name = "classify_query",
        description = "Classify which search backends should handle a query. Returns backends (metadata, structural, semantic) and reasoning. Use this for hybrid or ambiguous queries before dispatching to search."
    )]
    pub async fn classify_query(
        &self,
        Parameters(input): Parameters<ClassifyQueryInput>,
    ) -> String {
        match do_classify_query(input).await {
            Ok(result) => result,
            Err(e) => json!({"error": e.to_string()}).to_string(),
        }
    }
}

async fn do_extract(input: ExtractDocumentInput) -> anyhow::Result<String> {
    let config = ClaudeConfig::load()?;
    let schema = SchemaRegistry::load_auto(config.schema_path.as_ref().map(std::path::Path::new))?;

    let path = std::path::Path::new(&input.file_path);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");

    let pages: Vec<PageContent> = match input.file_type.to_lowercase().as_str() {
        "pdf" => pdfium::render_pdf(path)?,
        "image" => pdfium::load_image(path)?,
        other => anyhow::bail!("Unsupported file_type: {other}. Use 'pdf' or 'image'."),
    };

    // Pass 0: filename heuristic
    let doc_type = if let Some(dt) = schema.infer_doc_type_from_stem(stem) {
        dt
    } else {
        // Pass 1: LLM classification
        claude::classify_doc_type(&pages, &config, &schema).await?
    };

    // Pass 2: full extraction
    let result = claude::call_claude(&pages, &config, &schema, &doc_type).await?;

    let index_base = config.resolve_index_dir(None);
    let known_persons = MetadataIndex::known_persons_for(&index_base, &schema);

    let md = frontmatter::generate_md(&result, path, &pages, &schema, &known_persons, None, 0);

    if !stem.is_empty() {
        // MCP extraction is online (LLM) extraction, so it lands in the online tier,
        // split into text/ or images/ by the caller-declared file type.
        let sub = if input.file_type.eq_ignore_ascii_case("image") { "images" } else { "text" };
        let category_dir = index_base.join("online").join(sub);
        let _ = std::fs::create_dir_all(&category_dir);
        let out_path = category_dir.join(format!("{stem}.md"));
        let _ = std::fs::write(&out_path, &md);
    }

    // Build frontmatter summary from all extracted fields
    let mut frontmatter_obj = serde_json::Map::new();
    frontmatter_obj.insert("doc_type".to_string(), json!(result.doc_type));
    for (k, v) in &result.fields {
        frontmatter_obj.insert(k.clone(), json!(v));
    }

    let output = json!({
        "text": result.pages.iter().map(|p| p.text.as_str()).collect::<Vec<_>>().join("\n\n"),
        "ocr_method": result.ocr_method,
        "frontmatter": frontmatter_obj
    });

    Ok(output.to_string())
}

async fn do_ocr_scan(input: OcrScanInput) -> anyhow::Result<ocr::OcrResult> {
    let bytes = tokio::fs::read(&input.file_path).await
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", input.file_path))?;
    ocr::scan_page(bytes).await
}

async fn do_classify_query(input: ClassifyQueryInput) -> anyhow::Result<String> {
    let config = ClaudeConfig::load()?;
    let backends = classify::classify_backends(
        &input.query,
        &input.known_persons,
        &input.known_doc_types,
        &config,
    )
    .await?;

    let backend_names: Vec<&str> = backends
        .iter()
        .map(|b| match b {
            pdf_core::search::Backend::Metadata => "metadata",
            pdf_core::search::Backend::Structural => "structural",
            pdf_core::search::Backend::Semantic => "semantic",
            pdf_core::search::Backend::Keyword => "keyword",
        })
        .collect();

    Ok(json!({ "backends": backend_names }).to_string())
}
