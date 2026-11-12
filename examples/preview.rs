use std::error::Error;
use std::path::Path;

fn main() -> Result<(), Box<dyn Error>> {
    let path = std::env::args().nth(1).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "usage: preview <file>")
    })?;
    let bytes = std::fs::read(&path)?;
    let filename = Path::new(&path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("input");

    let thumbnail = any_preview::generate_thumbnail(filename, "application/octet-stream", &bytes);
    println!(
        "Thumbnail: {} ({})",
        thumbnail.summary,
        thumbnail.kind.as_str()
    );

    let preview = any_preview::generate_preview(filename, "application/octet-stream", &bytes);
    println!("Preview: {} ({})", preview.summary, preview.kind.as_str());
    println!("Pages or preview units: {}", preview.total_pages);

    Ok(())
}
