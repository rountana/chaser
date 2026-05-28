use anyhow::Context;
use serde::{Deserialize, Serialize};

/// Redirect stderr to /dev/null for the duration of `f`, then restore it.
/// Suppresses Leptonica's noisy "Image too small to scale" / "Line cannot be recognized" messages.
#[cfg(unix)]
fn with_stderr_suppressed<F, T>(f: F) -> T
where
    F: FnOnce() -> T,
{
    unsafe {
        let saved = libc::dup(2);
        let devnull = libc::open(b"/dev/null\0".as_ptr() as *const libc::c_char, libc::O_WRONLY);
        libc::dup2(devnull, 2);
        libc::close(devnull);
        let result = f();
        libc::dup2(saved, 2);
        libc::close(saved);
        result
    }
}

#[cfg(not(unix))]
fn with_stderr_suppressed<F, T>(f: F) -> T
where
    F: FnOnce() -> T,
{
    f()
}

pub const HIGH_CONFIDENCE: f32 = 85.0;
pub const MEDIUM_CONFIDENCE: f32 = 60.0;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OcrWord {
    pub word: String,
    pub confidence: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OcrResult {
    pub text: String,
    pub mean_confidence: f32,
    pub words: Vec<OcrWord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OcrPath {
    TesseractOnly,
    TesseractLlmCleanup,
    LlmVision,
    SkippedTextPage,
}

impl OcrPath {
    pub fn from_confidence(confidence: f32) -> Self {
        if confidence >= HIGH_CONFIDENCE {
            OcrPath::TesseractOnly
        } else if confidence >= MEDIUM_CONFIDENCE {
            OcrPath::TesseractLlmCleanup
        } else {
            OcrPath::LlmVision
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            OcrPath::TesseractOnly => "tesseract-only",
            OcrPath::TesseractLlmCleanup => "tesseract-llm-cleanup",
            OcrPath::LlmVision => "llm-vision",
            OcrPath::SkippedTextPage => "text-embedded",
        }
    }

    fn cost_rank(&self) -> u8 {
        match self {
            OcrPath::SkippedTextPage => 0,
            OcrPath::TesseractOnly => 1,
            OcrPath::TesseractLlmCleanup => 2,
            OcrPath::LlmVision => 3,
        }
    }
}

/// Synchronous Tesseract OCR — call only from a blocking thread.
pub fn scan_image_sync(image_bytes: &[u8]) -> anyhow::Result<OcrResult> {
    with_stderr_suppressed(|| {
        let mut tess = tesseract::Tesseract::new(None, Some("eng"))
            .map_err(|e| anyhow::anyhow!("Tesseract init: {e} (install tesseract-ocr + eng language data)"))?
            .set_image_from_mem(image_bytes)
            .map_err(|e| anyhow::anyhow!("Tesseract set_image: {e}"))?
            .set_source_resolution(300)
            .recognize()
            .map_err(|e| anyhow::anyhow!("Tesseract recognize: {e}"))?;

        let text = tess.get_text()
            .map_err(|e| anyhow::anyhow!("Tesseract get_text: {e}"))?;
        let mean_confidence = tess.mean_text_conf() as f32;
        let tsv = tess.get_tsv_text(0).unwrap_or_default();
        let words = parse_tsv_words(&tsv);

        Ok(OcrResult {
            text: text.trim().to_string(),
            mean_confidence,
            words,
        })
    })
}

/// Parse Tesseract TSV output for per-word confidence.
/// TSV columns: level page_num block_num par_num line_num word_num left top width height conf text
/// Level 5 rows are individual words.
fn parse_tsv_words(tsv: &str) -> Vec<OcrWord> {
    tsv.lines()
        .skip(1) // header
        .filter_map(|line| {
            let cols: Vec<&str> = line.split('\t').collect();
            if cols.len() < 12 { return None; }
            let level: u32 = cols[0].parse().ok()?;
            if level != 5 { return None; }
            let conf: f32 = cols[10].parse().ok()?;
            if conf < 0.0 { return None; } // -1 = no text on this row
            let word = cols[11].trim().to_string();
            if word.is_empty() { return None; }
            Some(OcrWord { word, confidence: conf })
        })
        .collect()
}

/// Async wrapper — Tesseract C bindings are sync; run on a blocking thread.
pub async fn scan_page(image_bytes: Vec<u8>) -> anyhow::Result<OcrResult> {
    tokio::task::spawn_blocking(move || scan_image_sync(&image_bytes))
        .await
        .context("Tesseract worker thread panicked")?
}

/// Compute the frontmatter `ocr_method` string from the per-page paths taken.
pub fn aggregate_ocr_method(paths: &[OcrPath]) -> String {
    let image_paths: Vec<&OcrPath> = paths.iter()
        .filter(|p| **p != OcrPath::SkippedTextPage)
        .collect();

    if image_paths.is_empty() {
        return "text-embedded".to_string();
    }

    let dominant = image_paths.iter().max_by_key(|p| p.cost_rank()).unwrap();
    let all_same = image_paths.iter().all(|p| p.cost_rank() == dominant.cost_rank());

    if all_same {
        dominant.as_str().to_string()
    } else {
        format!("mixed:{}", dominant.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_confidence_boundaries() {
        assert_eq!(OcrPath::from_confidence(90.0), OcrPath::TesseractOnly);
        assert_eq!(OcrPath::from_confidence(85.0), OcrPath::TesseractOnly);
        assert_eq!(OcrPath::from_confidence(84.9), OcrPath::TesseractLlmCleanup);
        assert_eq!(OcrPath::from_confidence(72.0), OcrPath::TesseractLlmCleanup);
        assert_eq!(OcrPath::from_confidence(60.0), OcrPath::TesseractLlmCleanup);
        assert_eq!(OcrPath::from_confidence(59.9), OcrPath::LlmVision);
        assert_eq!(OcrPath::from_confidence(0.0), OcrPath::LlmVision);
    }

    #[test]
    fn test_aggregate_all_same() {
        let paths = vec![OcrPath::TesseractOnly, OcrPath::TesseractOnly];
        assert_eq!(aggregate_ocr_method(&paths), "tesseract-only");
    }

    #[test]
    fn test_aggregate_mixed() {
        let paths = vec![OcrPath::TesseractOnly, OcrPath::LlmVision];
        assert_eq!(aggregate_ocr_method(&paths), "mixed:llm-vision");
    }

    #[test]
    fn test_aggregate_text_only() {
        let paths = vec![OcrPath::SkippedTextPage, OcrPath::SkippedTextPage];
        assert_eq!(aggregate_ocr_method(&paths), "text-embedded");
    }

    #[test]
    fn test_aggregate_empty() {
        assert_eq!(aggregate_ocr_method(&[]), "text-embedded");
    }

    #[test]
    fn test_aggregate_mixed_with_text() {
        let paths = vec![OcrPath::SkippedTextPage, OcrPath::TesseractLlmCleanup, OcrPath::LlmVision];
        assert_eq!(aggregate_ocr_method(&paths), "mixed:llm-vision");
    }

    #[tokio::test]
    #[ignore = "requires system Tesseract installation and eng language data"]
    async fn test_scan_page_live() {
        let bytes = std::fs::read("../source-files/Shyam_PAN.jpg").unwrap();
        let result = scan_page(bytes).await.unwrap();
        assert!(result.mean_confidence >= 0.0);
        assert!(!result.text.is_empty());
        println!("text: {}", &result.text[..result.text.len().min(200)]);
        println!("confidence: {}", result.mean_confidence);
    }
}
