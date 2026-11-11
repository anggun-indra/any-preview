//! Cross-platform document previews and thumbnails for Rust applications.
//!
//! The crate provides a compact thumbnail API and a fuller preview API for common
//! document, spreadsheet, presentation, archive, image, and text formats.

mod engine;
mod render;

pub use engine::{
    generate_preview, generate_thumbnail, ArchiveEntry, DocumentPreview, PreviewSection,
    PreviewSheet, PreviewThumbnail,
};
pub use render::prewarm_font_database;
