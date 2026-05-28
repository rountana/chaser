use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::Context;
use image::imageops::FilterType;
use pdfium_render::prelude::*;

use super::PageContent;

const OCR_TEXT_THRESHOLD: usize = 50;
const RENDER_DPI: f32 = 300.0;
const MAX_IMAGE_DIMENSION: u32 = 1568;
const JPEG_QUALITY: u8 = 85;

static PDFIUM: OnceLock<Pdfium> = OnceLock::new();

fn get_pdfium() -> anyhow::Result<&'static Pdfium> {
    if let Some(p) = PDFIUM.get() {
        return Ok(p);
    }

    let bin_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));

    let result = Pdfium::bind_to_system_library()
        .or_else(|_| Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path(&bin_dir)))
        .or_else(|_| Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path(".")));

    let pdfium = result
        .map(Pdfium::new)
        .with_context(|| format!(
            "pdfium library not found (checked system, {}, and .)\n  \
             Download libpdfium for your platform from:\n  \
             https://github.com/bblanchon/pdfium-binaries/releases\n  \
             Then place it at: {}",
            bin_dir.display(),
            bin_dir.join(Pdfium::pdfium_platform_library_name_at_path("")).display()
        ))?;

    Ok(PDFIUM.get_or_init(|| pdfium))
}

pub fn render_pdf(path: &Path) -> anyhow::Result<Vec<PageContent>> {
    let pdfium = get_pdfium()?;

    let doc = pdfium
        .load_pdf_from_file(path, None)
        .with_context(|| format!("loading PDF: {}", path.display()))?;

    let mut pages = Vec::new();
    let page_count = doc.pages().len();

    for i in 0..page_count {
        let page = doc.pages().get(i).with_context(|| format!("getting page {i}"))?;
        let page_num = (i + 1) as u32;

        let text = page
            .text()
            .map(|t| t.all())
            .unwrap_or_default();

        if text.trim().len() >= OCR_TEXT_THRESHOLD {
            pages.push(PageContent::Text { page_num, text });
        } else {
            let jpeg_data = render_page_to_jpeg(&page)?;
            pages.push(PageContent::Image { page_num, data: jpeg_data, media_type: "image/jpeg".to_string() });
        }
    }

    Ok(pages)
}

pub fn load_image(path: &Path) -> anyhow::Result<Vec<PageContent>> {
    let img = image::open(path)
        .with_context(|| format!("reading image: {}", path.display()))?;

    let img = if img.width() > MAX_IMAGE_DIMENSION || img.height() > MAX_IMAGE_DIMENSION {
        img.resize(MAX_IMAGE_DIMENSION, MAX_IMAGE_DIMENSION, FilterType::Lanczos3)
    } else {
        img
    };

    let mut data = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut data, JPEG_QUALITY);
    img.write_with_encoder(encoder)
        .context("encoding image as JPEG")?;

    Ok(vec![PageContent::Image { page_num: 1, data, media_type: "image/jpeg".to_string() }])
}

fn render_page_to_jpeg(page: &PdfPage) -> anyhow::Result<Vec<u8>> {
    let width_px = (page.width().to_inches() * RENDER_DPI) as i32;
    let height_px = (page.height().to_inches() * RENDER_DPI) as i32;

    let config = PdfRenderConfig::new()
        .set_target_width(width_px)
        .set_target_height(height_px);

    let bitmap = page
        .render_with_config(&config)
        .context("rendering page to bitmap")?;

    let img = bitmap.as_image().context("converting bitmap to image")?;

    let img = if img.width() > MAX_IMAGE_DIMENSION || img.height() > MAX_IMAGE_DIMENSION {
        img.resize(MAX_IMAGE_DIMENSION, MAX_IMAGE_DIMENSION, FilterType::Lanczos3)
    } else {
        img
    };

    let mut jpeg_data = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg_data, JPEG_QUALITY);
    img.write_with_encoder(encoder)
        .context("encoding page as JPEG")?;

    Ok(jpeg_data)
}
