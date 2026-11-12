//! Cross-platform document preview and thumbnail generation.
//!
//! Provides a two-stage pipeline: `generate_thumbnail` for compact previews and
//! `generate_preview` for fuller document inspection. It checks embedded thumbnails
//! (`docProps/thumbnail.jpeg`, `QuickLook/Thumbnail.jpg`, and
//! `Thumbnails/thumbnail.png`) before falling back to structural rendering.

use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{render, PixmapSettings, RenderCache, RenderSettings};
use quick_xml::events::Event;
use quick_xml::Reader;
use std::io::{Cursor, Read};
use std::sync::Arc;
use zip::ZipArchive;

/// Maximum input size accepted by either preview function (64 MiB).
pub const MAX_INPUT_SIZE_BYTES: usize = 64 * 1024 * 1024;

/// Compact data returned for a file thumbnail.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PreviewThumbnail {
    /// Detected file category.
    pub kind: PreviewKind,
    /// Optional thumbnail image and its media type.
    pub image: Option<PreviewImage>,
    /// Human-readable summary in English.
    pub summary: String,
    /// Optional text snippet or excerpt (e.g., opening paragraph of DOCX, slide 1 text, code lines).
    pub text_snippet: Option<String>,
    /// Mini-grid preview rows for spreadsheets/CSV (up to 5 rows x 4 columns).
    pub table_preview: Vec<Vec<String>>,
    /// Total pages, sheets, slides, or archive items.
    pub item_count: Option<u32>,
}

/// A single spreadsheet tab for full preview.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PreviewSheet {
    /// Worksheet name.
    pub name: String,
    /// First row used as column headings.
    pub headers: Vec<String>,
    /// Preview rows after the header.
    pub rows: Vec<Vec<String>>,
    /// Total number of data rows when available.
    pub total_rows: u32,
}

/// A single archive entry for ZIP/package inspection.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArchiveEntry {
    /// Path inside the archive.
    pub path: String,
    /// Uncompressed entry size.
    pub size_bytes: u64,
    /// Whether this entry is a directory.
    pub is_dir: bool,
}

/// A logical document section or presentation slide.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PreviewSection {
    /// Section or slide title.
    pub title: String,
    /// Extracted section or slide text.
    pub body: String,
}

/// Encoded image bytes with the media type required to decode them.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PreviewImage {
    /// Encoded image data.
    pub bytes: Vec<u8>,
    /// Image MIME type, such as `image/png` or `image/jpeg`.
    pub mime_type: String,
}

impl PreviewImage {
    fn from_encoded_bytes(bytes: Vec<u8>, fallback_mime_type: Option<&str>) -> Self {
        let mime_type = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            "image/png"
        } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            "image/jpeg"
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            "image/gif"
        } else if bytes.starts_with(b"BM") {
            "image/bmp"
        } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
            "image/webp"
        } else {
            fallback_mime_type
                .filter(|value| value.starts_with("image/"))
                .unwrap_or("application/octet-stream")
        };

        Self {
            bytes,
            mime_type: mime_type.to_string(),
        }
    }

    fn from_png(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            mime_type: "image/png".to_string(),
        }
    }
}

/// Full preview data for a file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DocumentPreview {
    /// Detected file category.
    pub kind: PreviewKind,
    /// Original file name supplied by the caller.
    pub filename: String,
    /// Human-readable summary in English.
    pub summary: String,
    /// Page or slide images with their media types.
    pub page_images: Vec<PreviewImage>,
    /// Number of pages, slides, or preview units returned.
    pub total_pages: u32,
    /// Spreadsheet tabs (XLSX, CSV, TSV).
    pub sheets: Vec<PreviewSheet>,
    /// Document sections or presentation slides (DOCX, ODT, PPTX, Markdown).
    pub sections: Vec<PreviewSection>,
    /// Raw text or source code content (TXT, MD, JSON, XML, YAML, Code).
    pub raw_text: Option<String>,
    /// Archive directory listing (ZIP, JAR, APK, EPUB).
    pub archive_entries: Vec<ArchiveEntry>,
}

/// Broad category detected for an input file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PreviewKind {
    /// PDF file.
    Pdf,
    /// Spreadsheet or delimited table.
    Spreadsheet,
    /// Word processing document.
    Document,
    /// Slide presentation.
    Presentation,
    /// Compressed archive or package.
    Archive,
    /// Source code.
    Code,
    /// Plain text.
    Text,
    /// Raster image.
    Image,
    /// Unsupported, invalid, or oversized input.
    Unknown,
}

impl PreviewKind {
    /// Returns the stable lowercase identifier for this category.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pdf => "pdf",
            Self::Spreadsheet => "spreadsheet",
            Self::Document => "document",
            Self::Presentation => "presentation",
            Self::Archive => "archive",
            Self::Code => "code",
            Self::Text => "text",
            Self::Image => "image",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DocFormat {
    Pdf,
    Word,
    Spreadsheet,
    Csv,
    Presentation,
    Archive,
    CodeOrText,
    Image,
    Unknown,
}

fn detect_format(filename: &str, mime_type: &str, data: &[u8]) -> DocFormat {
    let lower = filename.to_ascii_lowercase();
    let ext = lower.rsplit('.').next().unwrap_or("");

    if ext == "pdf" || mime_type == "application/pdf" || data.starts_with(b"%PDF-") {
        return DocFormat::Pdf;
    }
    if matches!(ext, "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp")
        || mime_type.starts_with("image/")
    {
        return DocFormat::Image;
    }
    if matches!(ext, "docx" | "docm" | "odt")
        || mime_type.contains("wordprocessingml")
        || mime_type.contains("opendocument.text")
    {
        return DocFormat::Word;
    }
    if matches!(ext, "xlsx" | "xlsm" | "ods")
        || mime_type.contains("spreadsheetml")
        || mime_type.contains("opendocument.spreadsheet")
    {
        return DocFormat::Spreadsheet;
    }
    if matches!(ext, "csv" | "tsv")
        || mime_type == "text/csv"
        || mime_type == "text/tab-separated-values"
    {
        return DocFormat::Csv;
    }
    if matches!(ext, "pptx" | "pptm" | "odp" | "key")
        || mime_type.contains("presentationml")
        || mime_type.contains("opendocument.presentation")
    {
        return DocFormat::Presentation;
    }
    if matches!(ext, "zip" | "jar" | "apk" | "ipa" | "epub" | "whl")
        || mime_type == "application/zip"
    {
        return DocFormat::Archive;
    }
    if matches!(
        ext,
        "txt"
            | "md"
            | "markdown"
            | "json"
            | "xml"
            | "yaml"
            | "yml"
            | "toml"
            | "ini"
            | "cfg"
            | "conf"
            | "log"
            | "rs"
            | "dart"
            | "py"
            | "js"
            | "ts"
            | "jsx"
            | "tsx"
            | "html"
            | "htm"
            | "css"
            | "sql"
            | "sh"
            | "bash"
            | "zsh"
            | "c"
            | "cpp"
            | "h"
            | "hpp"
            | "go"
            | "java"
            | "kt"
            | "swift"
            | "rb"
            | "php"
    ) || mime_type.starts_with("text/")
        || mime_type == "application/json"
        || mime_type == "application/xml"
    {
        return DocFormat::CodeOrText;
    }

    // Magic bytes fallback: ZIP header PK\x03\x04
    if data.starts_with(&[0x50, 0x4B, 0x03, 0x04]) {
        return inspect_zip_flavor(data);
    }

    // Check if data looks like valid UTF-8 text
    let sample = &data[..data.len().min(2048)];
    if !sample.is_empty() && std::str::from_utf8(sample).is_ok() {
        return DocFormat::CodeOrText;
    }

    DocFormat::Unknown
}

fn inspect_zip_flavor(data: &[u8]) -> DocFormat {
    if let Ok(mut archive) = ZipArchive::new(Cursor::new(data)) {
        for i in 0..archive.len().min(30) {
            if let Ok(file) = archive.by_index(i) {
                let name = file.name();
                if name.starts_with("word/") {
                    return DocFormat::Word;
                }
                if name.starts_with("xl/") {
                    return DocFormat::Spreadsheet;
                }
                if name.starts_with("ppt/") {
                    return DocFormat::Presentation;
                }
            }
        }
    }
    DocFormat::Archive
}

/// Generate a compact thumbnail for an attachment or confirmation dialog.
///
/// Unsupported, invalid, or oversized inputs return a thumbnail with
/// [`PreviewKind::Unknown`].
pub fn generate_thumbnail(filename: &str, mime_type: &str, data: &[u8]) -> PreviewThumbnail {
    if data.len() > MAX_INPUT_SIZE_BYTES {
        return PreviewThumbnail {
            kind: PreviewKind::Unknown,
            image: None,
            summary: "File exceeds the 64 MiB preview limit".to_string(),
            text_snippet: None,
            table_preview: Vec::new(),
            item_count: None,
        };
    }

    match detect_format(filename, mime_type, data) {
        DocFormat::Pdf => generate_pdf_thumbnail(data),
        DocFormat::Word => generate_word_thumbnail(data),
        DocFormat::Spreadsheet => generate_xlsx_thumbnail(data),
        DocFormat::Csv => generate_csv_thumbnail(filename, data),
        DocFormat::Presentation => generate_pptx_thumbnail(data),
        DocFormat::Archive => generate_archive_thumbnail(data),
        DocFormat::CodeOrText => generate_text_thumbnail(filename, data),
        DocFormat::Image => PreviewThumbnail {
            kind: PreviewKind::Image,
            image: Some(PreviewImage::from_encoded_bytes(
                data.to_vec(),
                Some(mime_type),
            )),
            summary: "Image".to_string(),
            text_snippet: None,
            table_preview: Vec::new(),
            item_count: Some(1),
        },
        DocFormat::Unknown => PreviewThumbnail {
            kind: PreviewKind::Unknown,
            image: None,
            summary: format_extension_summary(filename, data.len()),
            text_snippet: None,
            table_preview: Vec::new(),
            item_count: None,
        },
    }
}

/// Generate a fuller preview when a document is opened.
///
/// Unsupported, invalid, or oversized inputs return a preview with
/// [`PreviewKind::Unknown`].
pub fn generate_preview(filename: &str, mime_type: &str, data: &[u8]) -> DocumentPreview {
    if data.len() > MAX_INPUT_SIZE_BYTES {
        return DocumentPreview {
            kind: PreviewKind::Unknown,
            filename: filename.to_string(),
            summary: "File exceeds the 64 MiB preview limit".to_string(),
            page_images: Vec::new(),
            total_pages: 0,
            sheets: Vec::new(),
            sections: Vec::new(),
            raw_text: None,
            archive_entries: Vec::new(),
        };
    }

    match detect_format(filename, mime_type, data) {
        DocFormat::Pdf => generate_pdf_preview(filename, data),
        DocFormat::Word => generate_word_preview(filename, data),
        DocFormat::Spreadsheet => generate_xlsx_preview(filename, data),
        DocFormat::Csv => generate_csv_preview(filename, data),
        DocFormat::Presentation => generate_pptx_preview(filename, data),
        DocFormat::Archive => generate_archive_preview(filename, data),
        DocFormat::CodeOrText => generate_text_preview(filename, data),
        DocFormat::Image => DocumentPreview {
            kind: PreviewKind::Image,
            filename: filename.to_string(),
            summary: "Image".to_string(),
            page_images: vec![PreviewImage::from_encoded_bytes(
                data.to_vec(),
                Some(mime_type),
            )],
            total_pages: 1,
            sheets: Vec::new(),
            sections: Vec::new(),
            raw_text: None,
            archive_entries: Vec::new(),
        },
        DocFormat::Unknown => DocumentPreview {
            kind: PreviewKind::Unknown,
            filename: filename.to_string(),
            summary: format_extension_summary(filename, data.len()),
            page_images: Vec::new(),
            total_pages: 0,
            sheets: Vec::new(),
            sections: Vec::new(),
            raw_text: None,
            archive_entries: Vec::new(),
        },
    }
}

fn format_extension_summary(filename: &str, len: usize) -> String {
    let ext = filename
        .rsplit('.')
        .next()
        .unwrap_or("FILE")
        .to_ascii_uppercase();
    format!("{} • {} KB", ext, (len / 1024).max(1))
}

// ============================================================================
// 1. PDF Engine (Pure-Rust `hayro` CPU Rasterizer)
// ============================================================================

fn render_pdf_page_png(pdf: &Pdf, page_index: usize, target_width: f32) -> Option<Vec<u8>> {
    let pages = pdf.pages();
    let page = pages.get(page_index)?;
    let (w, h) = page.render_dimensions();
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let scale = (target_width / w).clamp(0.4, 2.5);
    let cache = RenderCache::new();
    let interpreter_settings = InterpreterSettings::default();
    let render_settings = RenderSettings::default();
    let pixmap_settings = PixmapSettings {
        x_scale: scale,
        y_scale: scale,
        bg_color: WHITE,
    };
    let pixmap = render(
        page,
        &cache,
        &interpreter_settings,
        &render_settings,
        &pixmap_settings,
    );
    pixmap.into_png().ok()
}

fn generate_pdf_thumbnail(data: &[u8]) -> PreviewThumbnail {
    let shared = Arc::new(data.to_vec());
    if let Ok(pdf) = Pdf::new(shared) {
        let count = pdf.pages().len() as u32;
        let page1_png = render_pdf_page_png(&pdf, 0, 460.0);
        return PreviewThumbnail {
            kind: PreviewKind::Pdf,
            image: page1_png.map(PreviewImage::from_png),
            summary: format!("{count} pages • PDF"),
            text_snippet: None,
            table_preview: Vec::new(),
            item_count: Some(count),
        };
    }

    PreviewThumbnail {
        kind: PreviewKind::Pdf,
        image: None,
        summary: "PDF document".to_string(),
        text_snippet: None,
        table_preview: Vec::new(),
        item_count: None,
    }
}

fn generate_pdf_preview(filename: &str, data: &[u8]) -> DocumentPreview {
    let shared = Arc::new(data.to_vec());
    if let Ok(pdf) = Pdf::new(shared) {
        let total = pdf.pages().len();
        let max_render = total.min(12);
        let mut page_images = Vec::with_capacity(max_render);
        for idx in 0..max_render {
            if let Some(png) = render_pdf_page_png(&pdf, idx, 900.0) {
                page_images.push(PreviewImage::from_png(png));
            }
        }
        return DocumentPreview {
            kind: PreviewKind::Pdf,
            filename: filename.to_string(),
            summary: format!("{total} pages • PDF"),
            page_images,
            total_pages: total as u32,
            sheets: Vec::new(),
            sections: Vec::new(),
            raw_text: None,
            archive_entries: Vec::new(),
        };
    }

    DocumentPreview {
        kind: PreviewKind::Pdf,
        filename: filename.to_string(),
        summary: "PDF document".to_string(),
        page_images: Vec::new(),
        total_pages: 0,
        sheets: Vec::new(),
        sections: Vec::new(),
        raw_text: None,
        archive_entries: Vec::new(),
    }
}

// ============================================================================
// 2. Embedded-thumbnail fast path for ZIP/OpenXML documents
// ============================================================================

fn extract_embedded_zip_thumbnail(archive: &mut ZipArchive<Cursor<&[u8]>>) -> Option<Vec<u8>> {
    const CANDIDATES: &[&str] = &[
        "docProps/thumbnail.jpeg",
        "docProps/thumbnail.jpg",
        "docProps/thumbnail.png",
        "QuickLook/Thumbnail.jpg",
        "QuickLook/Thumbnail.png",
        "Thumbnails/thumbnail.png",
    ];
    for path in CANDIDATES {
        if let Ok(mut file) = archive.by_name(path) {
            if file.size() > 0 && file.size() <= 4_000_000 {
                let mut buf = Vec::with_capacity(file.size() as usize);
                if file.read_to_end(&mut buf).is_ok() && !buf.is_empty() {
                    return Some(buf);
                }
            }
        }
    }
    None
}

fn read_zip_entry_string(archive: &mut ZipArchive<Cursor<&[u8]>>, name: &str) -> Option<String> {
    let mut file = archive.by_name(name).ok()?;
    if file.size() > 16_000_000 {
        return None;
    }
    let mut content = String::new();
    file.read_to_string(&mut content).ok()?;
    Some(content)
}

// ============================================================================
// 3. Word / OpenDocument Text (.docx, .odt)
// ============================================================================

fn generate_word_thumbnail(data: &[u8]) -> PreviewThumbnail {
    let mut archive = match ZipArchive::new(Cursor::new(data)) {
        Ok(a) => a,
        Err(_) => {
            return PreviewThumbnail {
                kind: PreviewKind::Document,
                image: None,
                summary: "Word document".to_string(),
                text_snippet: None,
                table_preview: Vec::new(),
                item_count: None,
            }
        }
    };

    let embedded_thumb = extract_embedded_zip_thumbnail(&mut archive);
    let (sections, _page_pngs, rendered_thumb, para_count) =
        crate::render::render_docx_document(&mut archive, 1);
    let page_count = sections.len().max(1) as u32;
    let snippet = sections.first().map(|s| s.body.clone()).unwrap_or_default();
    let snippet_trimmed = if snippet.chars().count() > 320 {
        let s: String = snippet.chars().take(320).collect();
        format!("{s}…")
    } else {
        snippet
    };

    PreviewThumbnail {
        kind: PreviewKind::Document,
        image: embedded_thumb
            .or(rendered_thumb)
            .map(|bytes| PreviewImage::from_encoded_bytes(bytes, None)),
        summary: if para_count > 0 {
            format!("{page_count} pages • DOCX")
        } else {
            "Word document".to_string()
        },
        text_snippet: if snippet_trimmed.is_empty() {
            None
        } else {
            Some(snippet_trimmed)
        },
        table_preview: Vec::new(),
        item_count: Some(page_count),
    }
}

fn generate_word_preview(filename: &str, data: &[u8]) -> DocumentPreview {
    let mut archive = match ZipArchive::new(Cursor::new(data)) {
        Ok(a) => a,
        Err(_) => {
            return DocumentPreview {
                kind: PreviewKind::Document,
                filename: filename.to_string(),
                summary: "Word document".to_string(),
                page_images: Vec::new(),
                total_pages: 0,
                sheets: Vec::new(),
                sections: Vec::new(),
                raw_text: None,
                archive_entries: Vec::new(),
            }
        }
    };

    let (sections, page_pngs, _thumb, para_count) =
        crate::render::render_docx_document(&mut archive, 20);
    let full_text = sections
        .iter()
        .map(|s| s.body.as_str())
        .filter(|b| !b.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");

    let page_count = sections.len().max(1) as u32;
    DocumentPreview {
        kind: PreviewKind::Document,
        filename: filename.to_string(),
        summary: format!("{page_count} pages • {para_count} paragraphs"),
        page_images: page_pngs.into_iter().map(PreviewImage::from_png).collect(),
        total_pages: page_count,
        sheets: Vec::new(),
        sections,
        raw_text: if full_text.is_empty() {
            None
        } else {
            Some(full_text)
        },
        archive_entries: Vec::new(),
    }
}

// ============================================================================
// 4. Spreadsheets: Excel (.xlsx) & CSV / TSV
// ============================================================================

fn parse_xlsx_shared_strings(xml: &str) -> Vec<String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut strings = Vec::new();
    let mut current_si = String::new();
    let mut in_si = false;
    let mut in_t = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match e.local_name().as_ref() {
                b"si" => {
                    in_si = true;
                    current_si.clear();
                }
                b"t" if in_si => in_t = true,
                _ => {}
            },
            Ok(Event::Text(e)) if in_si && in_t => {
                if let Ok(decoded) = e.unescape() {
                    current_si.push_str(&decoded);
                }
            }
            Ok(Event::End(e)) => match e.local_name().as_ref() {
                b"t" => in_t = false,
                b"si" => {
                    in_si = false;
                    strings.push(current_si.clone());
                }
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    strings
}

fn parse_xlsx_sheet_names(xml: &str) -> Vec<String> {
    let mut reader = Reader::from_str(xml);
    let mut names = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e) | Event::Empty(e)) if e.local_name().as_ref() == b"sheet" => {
                for attr in e.attributes().flatten() {
                    if attr.key.local_name().as_ref() == b"name" {
                        if let Ok(val) = attr.unescape_value() {
                            names.push(val.to_string());
                        }
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    names
}

fn col_ref_to_index(cell_ref: &str) -> Option<usize> {
    let letters: String = cell_ref
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect();
    if letters.is_empty() {
        return None;
    }
    let mut col = 0usize;
    for ch in letters.chars() {
        col = col * 26 + (ch.to_ascii_uppercase() as usize - b'A' as usize + 1);
    }
    col.checked_sub(1)
}

fn parse_xlsx_worksheet(
    xml: &str,
    shared_strings: &[String],
    max_rows: usize,
    max_cols: usize,
) -> (Vec<Vec<String>>, u32) {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);

    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut total_rows = 0u32;
    let mut current_row: Vec<String> = Vec::new();
    let mut cell_type = String::new();
    let mut cell_col: Option<usize> = None;
    let mut cell_val = String::new();
    let mut in_v_or_t = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match e.local_name().as_ref() {
                b"row" => {
                    total_rows += 1;
                    current_row.clear();
                }
                b"c" => {
                    cell_type.clear();
                    cell_col = None;
                    cell_val.clear();
                    for attr in e.attributes().flatten() {
                        match attr.key.local_name().as_ref() {
                            b"t" => {
                                if let Ok(v) = attr.unescape_value() {
                                    cell_type = v.to_string();
                                }
                            }
                            b"r" => {
                                if let Ok(v) = attr.unescape_value() {
                                    cell_col = col_ref_to_index(&v);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                b"v" | b"t" => {
                    in_v_or_t = true;
                }
                _ => {}
            },
            Ok(Event::Text(e)) if in_v_or_t => {
                if let Ok(decoded) = e.unescape() {
                    cell_val.push_str(&decoded);
                }
            }
            Ok(Event::End(e)) => match e.local_name().as_ref() {
                b"v" | b"t" => {
                    in_v_or_t = false;
                }
                b"c" => {
                    let resolved = if cell_type == "s" {
                        cell_val
                            .trim()
                            .parse::<usize>()
                            .ok()
                            .and_then(|idx| shared_strings.get(idx).cloned())
                            .unwrap_or_default()
                    } else {
                        cell_val.trim().to_string()
                    };
                    let target_col = cell_col.unwrap_or(current_row.len());
                    if target_col < max_cols {
                        while current_row.len() <= target_col {
                            current_row.push(String::new());
                        }
                        current_row[target_col] = resolved;
                    }
                }
                b"row" => {
                    if rows.len() < max_rows && current_row.iter().any(|c| !c.is_empty()) {
                        rows.push(current_row.clone());
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }

    // Normalize column count across rows
    let col_count = rows
        .iter()
        .map(|r| r.len())
        .max()
        .unwrap_or(0)
        .min(max_cols);
    for r in &mut rows {
        r.resize(col_count, String::new());
    }

    (rows, total_rows)
}

fn generate_xlsx_thumbnail(data: &[u8]) -> PreviewThumbnail {
    let mut archive = match ZipArchive::new(Cursor::new(data)) {
        Ok(a) => a,
        Err(_) => {
            return PreviewThumbnail {
                kind: PreviewKind::Spreadsheet,
                image: None,
                summary: "Excel spreadsheet".to_string(),
                text_snippet: None,
                table_preview: Vec::new(),
                item_count: None,
            }
        }
    };

    let embedded_thumb = extract_embedded_zip_thumbnail(&mut archive);
    let shared_strings = read_zip_entry_string(&mut archive, "xl/sharedStrings.xml")
        .map(|s| parse_xlsx_shared_strings(&s))
        .unwrap_or_default();
    let sheet_names = read_zip_entry_string(&mut archive, "xl/workbook.xml")
        .map(|s| parse_xlsx_sheet_names(&s))
        .unwrap_or_default();
    let sheet1_xml =
        read_zip_entry_string(&mut archive, "xl/worksheets/sheet1.xml").unwrap_or_default();
    let (table_preview, total_rows) = parse_xlsx_worksheet(&sheet1_xml, &shared_strings, 5, 4);
    let (mut full_rows, _) = parse_xlsx_worksheet(&sheet1_xml, &shared_strings, 18, 8);
    let headers = if !full_rows.is_empty() {
        full_rows.remove(0)
    } else {
        Vec::new()
    };
    let sheet_obj = PreviewSheet {
        name: sheet_names
            .first()
            .cloned()
            .unwrap_or_else(|| "Sheet 1".to_string()),
        headers,
        rows: full_rows,
        total_rows,
    };
    let rendered_thumb = crate::render::render_sheet_grid_png(&sheet_obj, 1.1);
    let sheet_count = sheet_names.len().max(1) as u32;

    PreviewThumbnail {
        kind: PreviewKind::Spreadsheet,
        image: embedded_thumb
            .or(rendered_thumb)
            .map(|bytes| PreviewImage::from_encoded_bytes(bytes, None)),
        summary: format!("{sheet_count} sheets • {total_rows} rows"),
        text_snippet: sheet_names.first().cloned(),
        table_preview,
        item_count: Some(sheet_count),
    }
}

fn generate_xlsx_preview(filename: &str, data: &[u8]) -> DocumentPreview {
    let mut archive = match ZipArchive::new(Cursor::new(data)) {
        Ok(a) => a,
        Err(_) => {
            return DocumentPreview {
                kind: PreviewKind::Spreadsheet,
                filename: filename.to_string(),
                summary: "Excel spreadsheet".to_string(),
                page_images: Vec::new(),
                total_pages: 0,
                sheets: Vec::new(),
                sections: Vec::new(),
                raw_text: None,
                archive_entries: Vec::new(),
            }
        }
    };

    let shared_strings = read_zip_entry_string(&mut archive, "xl/sharedStrings.xml")
        .map(|s| parse_xlsx_shared_strings(&s))
        .unwrap_or_default();
    let sheet_names = read_zip_entry_string(&mut archive, "xl/workbook.xml")
        .map(|s| parse_xlsx_sheet_names(&s))
        .unwrap_or_default();

    let mut sheets = Vec::new();
    let mut page_images = Vec::new();
    let max_sheets = sheet_names.len().clamp(1, 10);
    for idx in 0..max_sheets {
        let path = format!("xl/worksheets/sheet{}.xml", idx + 1);
        if let Some(xml) = read_zip_entry_string(&mut archive, &path) {
            let (mut rows, total_rows) = parse_xlsx_worksheet(&xml, &shared_strings, 200, 16);
            let headers = if !rows.is_empty() {
                rows.remove(0)
            } else {
                Vec::new()
            };
            let name = sheet_names
                .get(idx)
                .cloned()
                .unwrap_or_else(|| format!("Sheet {}", idx + 1));
            let sheet = PreviewSheet {
                name,
                headers,
                rows,
                total_rows,
            };
            if let Some(png) = crate::render::render_sheet_grid_png(&sheet, 1.45) {
                page_images.push(PreviewImage::from_png(png));
            }
            sheets.push(sheet);
        }
    }

    DocumentPreview {
        kind: PreviewKind::Spreadsheet,
        filename: filename.to_string(),
        summary: format!("{} sheets • Spreadsheet", sheets.len()),
        page_images,
        total_pages: sheets.len() as u32,
        sheets,
        sections: Vec::new(),
        raw_text: None,
        archive_entries: Vec::new(),
    }
}

fn detect_csv_delimiter(filename: &str, data: &[u8]) -> u8 {
    if filename.to_ascii_lowercase().ends_with(".tsv") {
        return b'\t';
    }
    let first_line = data.split(|&b| b == b'\n').next().unwrap_or(data);
    let tabs = first_line.iter().filter(|&&b| b == b'\t').count();
    let semis = first_line.iter().filter(|&&b| b == b';').count();
    let commas = first_line.iter().filter(|&&b| b == b',').count();
    if tabs > commas && tabs > semis {
        b'\t'
    } else if semis > commas {
        b';'
    } else {
        b','
    }
}

fn parse_csv_rows(
    filename: &str,
    data: &[u8],
    max_rows: usize,
    max_cols: usize,
) -> (Vec<Vec<String>>, u32) {
    let delim = detect_csv_delimiter(filename, data);
    let mut rdr = csv::ReaderBuilder::new()
        .delimiter(delim)
        .has_headers(false)
        .flexible(true)
        .from_reader(Cursor::new(data));

    let mut rows = Vec::new();
    let mut total_rows = 0u32;
    for record in rdr.records().flatten() {
        total_rows += 1;
        if rows.len() < max_rows {
            let row: Vec<String> = record
                .iter()
                .take(max_cols)
                .map(|s| s.trim().to_string())
                .collect();
            if row.iter().any(|c| !c.is_empty()) {
                rows.push(row);
            }
        }
    }
    let col_count = rows
        .iter()
        .map(|r| r.len())
        .max()
        .unwrap_or(0)
        .min(max_cols);
    for r in &mut rows {
        r.resize(col_count, String::new());
    }
    (rows, total_rows)
}

fn generate_csv_thumbnail(filename: &str, data: &[u8]) -> PreviewThumbnail {
    let (table_preview, total_rows) = parse_csv_rows(filename, data, 5, 4);
    let (mut full_rows, _) = parse_csv_rows(filename, data, 16, 8);
    let headers = if !full_rows.is_empty() {
        full_rows.remove(0)
    } else {
        Vec::new()
    };
    let sheet = PreviewSheet {
        name: filename.to_string(),
        headers,
        rows: full_rows,
        total_rows,
    };
    let rendered_thumb = crate::render::render_sheet_grid_png(&sheet, 1.1);
    PreviewThumbnail {
        kind: PreviewKind::Spreadsheet,
        image: rendered_thumb.map(PreviewImage::from_png),
        summary: format!("{total_rows} rows • CSV"),
        text_snippet: None,
        table_preview,
        item_count: Some(total_rows),
    }
}

fn generate_csv_preview(filename: &str, data: &[u8]) -> DocumentPreview {
    let (mut rows, total_rows) = parse_csv_rows(filename, data, 250, 20);
    let headers = if !rows.is_empty() {
        rows.remove(0)
    } else {
        Vec::new()
    };
    let sheet = PreviewSheet {
        name: filename.to_string(),
        headers,
        rows,
        total_rows,
    };
    let page_images = crate::render::render_sheet_grid_png(&sheet, 1.45)
        .map(|png| vec![PreviewImage::from_png(png)])
        .unwrap_or_default();
    DocumentPreview {
        kind: PreviewKind::Spreadsheet,
        filename: filename.to_string(),
        summary: format!("{total_rows} rows • CSV table"),
        page_images,
        total_pages: 1,
        sheets: vec![sheet],
        sections: Vec::new(),
        raw_text: None,
        archive_entries: Vec::new(),
    }
}

// ============================================================================
// 5. Presentations: PowerPoint (.pptx) & Keynote / ODP
// ============================================================================

fn generate_pptx_thumbnail(data: &[u8]) -> PreviewThumbnail {
    let mut archive = match ZipArchive::new(Cursor::new(data)) {
        Ok(a) => a,
        Err(_) => {
            return PreviewThumbnail {
                kind: PreviewKind::Presentation,
                image: None,
                summary: "Presentation".to_string(),
                text_snippet: None,
                table_preview: Vec::new(),
                item_count: None,
            }
        }
    };

    let embedded_thumb = extract_embedded_zip_thumbnail(&mut archive);
    let (slides, _slide_pngs, rendered_thumb) =
        crate::render::render_pptx_document(&mut archive, 1);
    let count = slides.len() as u32;
    let first_slide = slides
        .first()
        .map(|s| {
            let clean_title = s.title.split_once(": ").map(|(_, t)| t).unwrap_or(&s.title);
            if s.body.is_empty() {
                clean_title.to_string()
            } else {
                format!("{clean_title}\n{}", s.body)
            }
        })
        .unwrap_or_default();

    PreviewThumbnail {
        kind: PreviewKind::Presentation,
        image: rendered_thumb
            .or(embedded_thumb)
            .map(|bytes| PreviewImage::from_encoded_bytes(bytes, None)),
        summary: if count > 0 {
            format!("{count} slides • PPTX")
        } else {
            "Presentation".to_string()
        },
        text_snippet: if first_slide.is_empty() {
            None
        } else {
            Some(first_slide)
        },
        table_preview: Vec::new(),
        item_count: Some(count),
    }
}

fn generate_pptx_preview(filename: &str, data: &[u8]) -> DocumentPreview {
    let mut archive = match ZipArchive::new(Cursor::new(data)) {
        Ok(a) => a,
        Err(_) => {
            return DocumentPreview {
                kind: PreviewKind::Presentation,
                filename: filename.to_string(),
                summary: "Presentation".to_string(),
                page_images: Vec::new(),
                total_pages: 0,
                sheets: Vec::new(),
                sections: Vec::new(),
                raw_text: None,
                archive_entries: Vec::new(),
            }
        }
    };

    let (sections, slide_images, _thumb) = crate::render::render_pptx_document(&mut archive, 24);
    let full_text = sections
        .iter()
        .map(|s| {
            if s.body.is_empty() {
                s.title.clone()
            } else {
                format!("{}\n{}", s.title, s.body)
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    DocumentPreview {
        kind: PreviewKind::Presentation,
        filename: filename.to_string(),
        summary: format!("{} slides • Presentation", sections.len()),
        page_images: slide_images
            .into_iter()
            .map(PreviewImage::from_png)
            .collect(),
        total_pages: sections.len() as u32,
        sheets: Vec::new(),
        sections,
        raw_text: if full_text.is_empty() {
            None
        } else {
            Some(full_text)
        },
        archive_entries: Vec::new(),
    }
}

// ============================================================================
// 6. Archives (.zip, .jar, .apk, .epub)
// ============================================================================

fn generate_archive_thumbnail(data: &[u8]) -> PreviewThumbnail {
    let mut archive = match ZipArchive::new(Cursor::new(data)) {
        Ok(a) => a,
        Err(_) => {
            return PreviewThumbnail {
                kind: PreviewKind::Archive,
                image: None,
                summary: "Compressed archive".to_string(),
                text_snippet: None,
                table_preview: Vec::new(),
                item_count: None,
            }
        }
    };

    let mut file_count = 0u32;
    let mut top_files = Vec::new();
    for i in 0..archive.len() {
        if let Ok(file) = archive.by_index(i) {
            if !file.is_dir() {
                file_count += 1;
                if top_files.len() < 4 {
                    top_files.push(format!("📦 {}", file.name()));
                }
            }
        }
    }

    PreviewThumbnail {
        kind: PreviewKind::Archive,
        image: None,
        summary: format!("{file_count} files • ZIP archive"),
        text_snippet: if top_files.is_empty() {
            None
        } else {
            Some(top_files.join("\n"))
        },
        table_preview: Vec::new(),
        item_count: Some(file_count),
    }
}

fn generate_archive_preview(filename: &str, data: &[u8]) -> DocumentPreview {
    let mut archive = match ZipArchive::new(Cursor::new(data)) {
        Ok(a) => a,
        Err(_) => {
            return DocumentPreview {
                kind: PreviewKind::Archive,
                filename: filename.to_string(),
                summary: "ZIP archive".to_string(),
                page_images: Vec::new(),
                total_pages: 0,
                sheets: Vec::new(),
                sections: Vec::new(),
                raw_text: None,
                archive_entries: Vec::new(),
            }
        }
    };

    let mut entries = Vec::with_capacity(archive.len().min(500));
    let mut total_uncompressed = 0u64;
    for i in 0..archive.len().min(500) {
        if let Ok(file) = archive.by_index(i) {
            total_uncompressed += file.size();
            entries.push(ArchiveEntry {
                path: file.name().to_string(),
                size_bytes: file.size(),
                is_dir: file.is_dir(),
            });
        }
    }

    DocumentPreview {
        kind: PreviewKind::Archive,
        filename: filename.to_string(),
        summary: format!(
            "{} Item • Total {} KB",
            entries.len(),
            (total_uncompressed / 1024).max(1)
        ),
        page_images: Vec::new(),
        total_pages: 1,
        sheets: Vec::new(),
        sections: Vec::new(),
        raw_text: None,
        archive_entries: entries,
    }
}

// ============================================================================
// 7. Code, Markdown, JSON, XML, YAML, Config, Logs, Text
// ============================================================================

fn is_code_extension(ext: &str) -> bool {
    matches!(
        ext,
        "json"
            | "xml"
            | "yaml"
            | "yml"
            | "toml"
            | "ini"
            | "rs"
            | "dart"
            | "py"
            | "js"
            | "ts"
            | "jsx"
            | "tsx"
            | "html"
            | "htm"
            | "css"
            | "sql"
            | "sh"
            | "bash"
            | "zsh"
            | "c"
            | "cpp"
            | "h"
            | "hpp"
            | "go"
            | "java"
            | "kt"
            | "swift"
            | "rb"
            | "php"
    )
}

fn generate_text_thumbnail(filename: &str, data: &[u8]) -> PreviewThumbnail {
    let ext = filename
        .rsplit('.')
        .next()
        .unwrap_or("txt")
        .to_ascii_lowercase();
    let kind = if is_code_extension(&ext) {
        PreviewKind::Code
    } else {
        PreviewKind::Text
    };

    let raw = String::from_utf8_lossy(&data[..data.len().min(64_000)]).to_string();
    let formatted = if ext == "json" {
        serde_json::from_str::<serde_json::Value>(&raw)
            .ok()
            .and_then(|v| serde_json::to_string_pretty(&v).ok())
            .unwrap_or(raw)
    } else {
        raw
    };

    let total_lines = formatted.lines().count() as u32;
    let snippet: String = formatted
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(6)
        .map(|l| {
            if l.chars().count() > 64 {
                let s: String = l.chars().take(64).collect();
                format!("{s}…")
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    PreviewThumbnail {
        kind,
        image: None,
        summary: format!("{total_lines} lines • {}", ext.to_ascii_uppercase()),
        text_snippet: if snippet.is_empty() {
            None
        } else {
            Some(snippet)
        },
        table_preview: Vec::new(),
        item_count: Some(total_lines),
    }
}

fn generate_text_preview(filename: &str, data: &[u8]) -> DocumentPreview {
    let ext = filename
        .rsplit('.')
        .next()
        .unwrap_or("txt")
        .to_ascii_lowercase();
    let kind = if is_code_extension(&ext) {
        PreviewKind::Code
    } else {
        PreviewKind::Text
    };

    let raw = String::from_utf8_lossy(&data[..data.len().min(512_000)]).to_string();
    let formatted = if ext == "json" {
        serde_json::from_str::<serde_json::Value>(&raw)
            .ok()
            .and_then(|v| serde_json::to_string_pretty(&v).ok())
            .unwrap_or(raw)
    } else {
        raw
    };

    let total_lines = formatted.lines().count() as u32;

    DocumentPreview {
        kind,
        filename: filename.to_string(),
        summary: format!("{total_lines} lines • {}", ext.to_ascii_uppercase()),
        page_images: Vec::new(),
        total_pages: 1,
        sheets: Vec::new(),
        sections: Vec::new(),
        raw_text: Some(formatted),
        archive_entries: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    #[test]
    fn unsupported_input_returns_unknown_preview_kind() {
        let bytes = [0, 159, 255, 1, 2, 3];
        let thumbnail = generate_thumbnail("sample.bin", "application/octet-stream", &bytes);
        let preview = generate_preview("sample.bin", "application/octet-stream", &bytes);

        assert_eq!(thumbnail.kind, PreviewKind::Unknown);
        assert_eq!(preview.kind, PreviewKind::Unknown);
    }

    #[test]
    fn preview_csv_parses_table_and_preview() {
        let csv_data = b"Name,Role,Department,Location\nAlice,Engineer,Research,North\nBob,Designer,Product,West\n";
        let thumb = generate_thumbnail("sample.csv", "text/csv", csv_data);
        assert_eq!(thumb.kind, PreviewKind::Spreadsheet);
        assert_eq!(thumb.table_preview.len(), 3);
        assert_eq!(thumb.table_preview[0][0], "Name");
        assert_eq!(thumb.table_preview[1][0], "Alice");

        let preview = generate_preview("sample.csv", "text/csv", csv_data);
        assert_eq!(preview.sheets.len(), 1);
        assert_eq!(
            preview.sheets[0].headers,
            vec!["Name", "Role", "Department", "Location"]
        );
        assert_eq!(preview.sheets[0].rows.len(), 2);
    }

    #[test]
    fn preview_docx_extracts_paragraphs_and_embedded_thumbnail() {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            let opts = SimpleFileOptions::default();
            zip.start_file("docProps/thumbnail.jpeg", opts).unwrap();
            zip.write_all(&[0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3]).unwrap();
            zip.start_file("word/document.xml", opts).unwrap();
            zip.write_all(
                br#"<?xml version="1.0" encoding="UTF-8"?>
                <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
                  <w:body>
                    <w:p><w:r><w:t>Sample document title</w:t></w:r></w:p>
                    <w:p><w:r><w:t>Preview text extraction works.</w:t></w:r></w:p>
                  </w:body>
                </w:document>"#,
            )
            .unwrap();
            zip.finish().unwrap();
        }
        let bytes = buf.into_inner();

        let thumb = generate_thumbnail("sample.docx", "application/octet-stream", &bytes);
        assert_eq!(thumb.kind, PreviewKind::Document);
        assert!(
            thumb.image.is_some(),
            "Should extract docProps/thumbnail.jpeg fast-path"
        );
        assert_eq!(thumb.image.as_ref().unwrap().mime_type, "image/jpeg");
        assert!(thumb
            .text_snippet
            .as_deref()
            .unwrap_or("")
            .contains("Sample document title"));

        let preview = generate_preview("sample.docx", "application/octet-stream", &bytes);
        assert_eq!(preview.kind, PreviewKind::Document);
        assert_eq!(preview.sections.len(), 1);
        assert!(preview.sections[0]
            .body
            .contains("Preview text extraction works."));
    }

    #[test]
    fn preview_xlsx_parses_shared_strings_and_cells() {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            let opts = SimpleFileOptions::default();
            zip.start_file("xl/workbook.xml", opts).unwrap();
            zip.write_all(
                br#"<workbook><sheets><sheet name="Summary" sheetId="1"/></sheets></workbook>"#,
            )
            .unwrap();
            zip.start_file("xl/sharedStrings.xml", opts).unwrap();
            zip.write_all(
                br#"<sst><si><t>Month</t></si><si><t>Revenue</t></si><si><t>January</t></si></sst>"#,
            )
            .unwrap();
            zip.start_file("xl/worksheets/sheet1.xml", opts).unwrap();
            zip.write_all(
                br#"<worksheet><sheetData>
                    <row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c></row>
                    <row r="2"><c r="A2" t="s"><v>2</v></c><c r="B2"><v>15000000</v></c></row>
                </sheetData></worksheet>"#,
            )
            .unwrap();
            zip.finish().unwrap();
        }
        let bytes = buf.into_inner();

        let thumb = generate_thumbnail("sample.xlsx", "application/octet-stream", &bytes);
        assert_eq!(thumb.kind, PreviewKind::Spreadsheet);
        assert_eq!(thumb.table_preview.len(), 2);
        assert_eq!(thumb.table_preview[0], vec!["Month", "Revenue"]);
        assert_eq!(thumb.table_preview[1], vec!["January", "15000000"]);

        let preview = generate_preview("sample.xlsx", "application/octet-stream", &bytes);
        assert_eq!(preview.sheets.len(), 1);
        assert_eq!(preview.sheets[0].name, "Summary");
        assert_eq!(preview.sheets[0].headers, vec!["Month", "Revenue"]);
        assert_eq!(preview.sheets[0].rows[0], vec!["January", "15000000"]);
    }

    #[test]
    fn preview_zip_lists_archive_contents() {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            let opts = SimpleFileOptions::default();
            zip.start_file("README.md", opts).unwrap();
            zip.write_all(b"# Package contents").unwrap();
            zip.start_file("src/main.rs", opts).unwrap();
            zip.write_all(b"fn main() {}").unwrap();
            zip.finish().unwrap();
        }
        let bytes = buf.into_inner();

        let thumb = generate_thumbnail("bundle.zip", "application/zip", &bytes);
        assert_eq!(thumb.kind, PreviewKind::Archive);
        assert_eq!(thumb.item_count, Some(2));

        let preview = generate_preview("bundle.zip", "application/zip", &bytes);
        assert_eq!(preview.archive_entries.len(), 2);
        assert_eq!(preview.archive_entries[0].path, "README.md");
    }

    #[test]
    fn preview_pdf_renders_page_thumbnail_and_preview() {
        let minimal_pdf = b"%PDF-1.4\n\
1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n\
2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n\
3 0 obj << /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] /Contents 4 0 R >> endobj\n\
4 0 obj << /Length 35 >> stream\n\
0.1 0.5 0.3 rg 10 10 180 80 re f\n\
endstream endobj\n\
xref\n\
0 5\n\
0000000000 65535 f \n\
0000000009 00000 n \n\
0000000058 00000 n \n\
0000000115 00000 n \n\
0000000203 00000 n \n\
trailer << /Size 5 /Root 1 0 R >>\n\
startxref\n\
288\n\
%%EOF\n";

        let thumb = generate_thumbnail("sample.pdf", "application/pdf", minimal_pdf);
        assert_eq!(thumb.kind, PreviewKind::Pdf);
        assert_eq!(thumb.item_count, Some(1));
        assert!(thumb.image.is_some(), "PDF page 1 should rasterize to PNG");
        assert!(thumb.image.as_ref().unwrap().bytes.starts_with(b"\x89PNG"));
        assert_eq!(thumb.image.as_ref().unwrap().mime_type, "image/png");

        let preview = generate_preview("sample.pdf", "application/pdf", minimal_pdf);
        assert_eq!(preview.kind, PreviewKind::Pdf);
        assert_eq!(preview.total_pages, 1);
        assert_eq!(preview.page_images.len(), 1);
    }

    #[test]
    fn openxml_layout_mappers_render_png_pages() {
        // 1. Synthetic PPTX without embedded thumbnail -> renders 1:1 slide PNG via PMPresentationMapper
        let mut pptx_buf = Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut pptx_buf);
            let opts = SimpleFileOptions::default();
            zip.start_file("ppt/presentation.xml", opts).unwrap();
            zip.write_all(br#"<p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:sldSz cx="12192000" cy="6858000"/></p:presentation>"#)
                .unwrap();
            zip.start_file("ppt/slides/slide1.xml", opts).unwrap();
            zip.write_all(
                br#"<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
                  <p:cSld>
                    <p:bg><p:bgPr><a:solidFill><a:srgbClr val="F8FAFC"/></a:solidFill></p:bgPr></p:bg>
                    <p:spTree>
                      <p:sp>
                        <p:spPr>
                          <a:xfrm><a:off x="685800" y="685800"/><a:ext cx="10820400" cy="1800000"/></a:xfrm>
                          <a:prstGeom prst="roundRect"/>
                          <a:solidFill><a:srgbClr val="2563EB"/></a:solidFill>
                        </p:spPr>
                        <p:txBody>
                          <a:bodyPr anchor="ctr"/>
                          <a:p><a:pPr algn="ctr"/><a:r><a:rPr sz="3200" b="1"><a:solidFill><a:srgbClr val="FFFFFF"/></a:solidFill></a:rPr><a:t>Sample Presentation</a:t></a:r></a:p>
                        </p:txBody>
                      </p:sp>
                    </p:spTree>
                  </p:cSld>
                </p:sld>"#,
            )
            .unwrap();
            zip.finish().unwrap();
        }
        let pptx_bytes = pptx_buf.into_inner();
        let pptx_thumb = generate_thumbnail("sample.pptx", "application/octet-stream", &pptx_bytes);
        assert_eq!(pptx_thumb.kind, PreviewKind::Presentation);
        assert!(pptx_thumb
            .image
            .as_ref()
            .is_some_and(|image| image.bytes.starts_with(b"\x89PNG")));
        let pptx_prev = generate_preview("sample.pptx", "application/octet-stream", &pptx_bytes);
        assert_eq!(pptx_prev.page_images.len(), 1);
        assert!(pptx_prev.page_images[0].bytes.starts_with(b"\x89PNG"));
    }
}
