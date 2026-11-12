# any-preview

A Rust library for generating document thumbnails and previews in applications.
It is UI-framework independent and can be used from a Rust application or exposed
through an application-specific bridge.

## Supported formats

The current implementation has preview paths for:

- PDF
- DOCX, DOCM, ODT text
- XLSX, XLSM, CSV, TSV
- PPTX, PPTM
- ZIP, JAR, APK, IPA, EPUB, WHL
- PNG, JPEG, GIF, WebP, BMP
- Common text and source-code formats

ODF spreadsheets and presentations, and Keynote files, are recognized but do not
yet have complete structured preview support. Office preview fidelity varies by
format; unsupported layout details may not appear. Unknown or invalid inputs may
produce an empty or `unknown` preview result.

Inputs larger than 64 MiB return an `unknown` result without parsing. Embedded ZIP
parts are also subject to per-entry limits.

## Usage

```rust
use any_preview::{generate_preview, generate_thumbnail};

fn inspect_file(filename: &str, mime_type: &str, bytes: &[u8]) {
    let thumbnail = generate_thumbnail(filename, mime_type, bytes);
    println!("{} ({})", thumbnail.summary, thumbnail.kind.as_str());

    let preview = generate_preview(filename, mime_type, bytes);
    println!("{}: {} pages", preview.filename, preview.total_pages);
}
```

When the crate is published, add it to `Cargo.toml`:

```toml
[dependencies]
any-preview = "0.1"
```

The crate name uses a hyphen in `Cargo.toml`; Rust code imports it as `any_preview`.
See [`examples/preview.rs`](examples/preview.rs) for a complete command-line example.
Run it with `cargo run --example preview -- path/to/file.pdf`.

The first render may load system fonts. Applications can call
`prewarm_font_database()` during startup to move that cost out of the first
preview request.

Thumbnail and page image values include both encoded bytes and a MIME type, so
callers can pass them directly to their image decoder.

## License

This project is licensed under the Zero-Clause BSD license (0BSD). See [LICENSE](LICENSE).
