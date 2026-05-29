use std::collections::HashMap;
use std::path::Path;

use anyhow::Context;

use crate::schema::SchemaRegistry;
use crate::frontmatter::filename;

use super::{ExtractionResult, PageContent, PageText, heuristic, ocr, pdfium};

/// Run the fully offline extraction pipeline on a single PDF or image file.
///
/// No LLM calls are made. Frontmatter fields are populated from filename regex
/// first, then supplemented by body-text heuristics.
pub async fn extract_offline(
    path: &Path,
    schema: &SchemaRegistry,
    known_persons: &[String],
) -> anyhow::Result<(ExtractionResult, Vec<PageContent>)> {
    let ext = path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let stem = path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");

    // Render the file into pages.
    let pages: Vec<PageContent> = match ext.as_str() {
        "pdf"              => pdfium::render_pdf(path)
            .with_context(|| format!("rendering PDF: {}", path.display()))?,
        "jpg" | "jpeg" | "png" => pdfium::load_image(path)
            .with_context(|| format!("loading image: {}", path.display()))?,
        other => anyhow::bail!("unsupported file type: .{other}"),
    };

    let is_image_source = matches!(ext.as_str(), "jpg" | "jpeg" | "png");
    let has_image_pages = pages.iter().any(|p| p.is_image());
    let doc_category = if is_image_source || has_image_pages { "image" } else { "text" };

    // Extract body text: pdfium text pages as-is; image pages via Tesseract.
    let mut page_texts: Vec<PageText> = Vec::new();
    for page in &pages {
        match page {
            PageContent::Text { page_num, text } => {
                page_texts.push(PageText { page_num: *page_num, text: text.clone() });
            }
            PageContent::Image { page_num, data, .. } => {
                let ocr_result = ocr::scan_page(data.clone()).await
                    .unwrap_or_else(|_| ocr::OcrResult {
                        text: String::new(),
                        mean_confidence: 0.0,
                        words: vec![],
                    });
                page_texts.push(PageText { page_num: *page_num, text: ocr_result.text });
            }
        }
    }

    let full_body: String = page_texts.iter()
        .map(|p| p.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    // Infer doc_type: filename heuristic first, body-text second.
    let doc_type = schema.infer_doc_type_from_stem(stem)
        .or_else(|| heuristic::infer_doc_type_from_text(&full_body))
        .unwrap_or_else(|| schema.doc_type_default.clone());

    // Build fields map. Person and date use filename regex, fall back to body-text.
    let mut fields: HashMap<String, String> = HashMap::new();

    let person = filename::extract_person(stem, known_persons, &schema.doc_type_values)
        .or_else(|| heuristic::infer_person_from_text(&full_body))
        .unwrap_or_default();
    fields.insert("person".to_string(), person);

    let date = filename::extract_date(stem)
        .or_else(|| heuristic::infer_date_from_text(&full_body))
        .unwrap_or_default();
    fields.insert("date".to_string(), date);

    let institution = heuristic::infer_institution_from_text(&full_body).unwrap_or_default();
    fields.insert("institution".to_string(), institution);

    let ocr_method = if doc_category == "image" {
        "tesseract-only".to_string()
    } else {
        "text-embedded".to_string()
    };

    let result = ExtractionResult {
        pages: page_texts,
        doc_type,
        doc_category: doc_category.to_string(),
        fields,
        ocr_method,
        extraction_mode: "offline".to_string(),
    };

    Ok((result, pages))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::SchemaRegistry;

    #[tokio::test]
    #[ignore = "requires pdfium dylib and a test PDF in source-files/"]
    async fn extract_offline_text_pdf() {
        let schema = SchemaRegistry::load_default().unwrap();
        let path = std::path::Path::new("../source-files/agreement to sell.pdf");
        let (result, pages) = extract_offline(path, &schema, &[]).await.unwrap();
        assert_eq!(result.doc_category, "text");
        assert_eq!(result.extraction_mode, "offline");
        assert_eq!(result.ocr_method, "text-embedded");
        assert!(!result.pages.is_empty());
        assert!(!pages.is_empty());
    }

    #[tokio::test]
    #[ignore = "requires pdfium dylib, Tesseract, and test image in source-files/"]
    async fn extract_offline_image() {
        let schema = SchemaRegistry::load_default().unwrap();
        let path = std::path::Path::new("../source-files/Hema_PAN.jpg");
        let (result, _) = extract_offline(path, &schema, &["Hema".to_string()]).await.unwrap();
        assert_eq!(result.doc_category, "image");
        assert_eq!(result.extraction_mode, "offline");
        assert_eq!(result.ocr_method, "tesseract-only");
    }
}
