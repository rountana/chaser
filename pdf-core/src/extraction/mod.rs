pub mod claude;
pub mod enrich;
pub mod gemini;
pub mod ocr;
pub mod ollama;
pub mod pdfium;
pub mod suggest;
pub mod table;

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub enum PageContent {
    Text { page_num: u32, text: String },
    Image { page_num: u32, data: Vec<u8>, media_type: String },
}

impl PageContent {
    pub fn page_num(&self) -> u32 {
        match self {
            PageContent::Text { page_num, .. } => *page_num,
            PageContent::Image { page_num, .. } => *page_num,
        }
    }

    pub fn is_image(&self) -> bool {
        matches!(self, PageContent::Image { .. })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageText {
    pub page_num: u32,
    pub text: String,
}

/// Result of a full document extraction.
///
/// `doc_type` is the type discriminator (from Pass 1).
/// `doc_category` is "text" or "image" — determined by whether the source had
/// extractable text layers (text) or required rendering/vision (image).
/// `fields` contains all document metadata fields, keyed by field name.
/// `ocr_method` is set by the extraction backend after the loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionResult {
    pub pages: Vec<PageText>,
    pub doc_type: String,
    pub doc_category: String,
    pub fields: HashMap<String, String>,
    pub ocr_method: String,
}
