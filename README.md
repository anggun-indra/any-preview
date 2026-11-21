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
- HTML (`.html`, `.htm`, `.xhtml`) as browser-renderable source
- Markdown (`.md`, `.markdown`), JSON, XML, and YAML with raw and formatted text
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
any-preview = "0.3"
```

The crate name uses a hyphen in `Cargo.toml`; Rust code imports it as `any_preview`.
See [`examples/preview.rs`](examples/preview.rs) for a complete command-line example.
Run it with `cargo run --example preview -- path/to/file.pdf`.

The first render may load system fonts. Applications can call
`prewarm_font_database()` during startup to move that cost out of the first
preview request.

Thumbnail and page image values include both encoded bytes and a MIME type, so
callers can pass them directly to their image decoder.

Text previews preserve the source in `DocumentPreview::raw_text`. Valid JSON,
XML, and YAML files also include a pretty-printed `formatted_text`; invalid
structured text remains available as raw text. Markdown is identified with the
`markdown` preview kind so UI applications can render it while retaining the
source for a raw view. Other source-code files are returned unchanged so a
preview never silently rewrites an attachment.

HTML is identified with the `html` preview kind and returned as source text for
the host application to render. The crate does not provide UI labels or a
browser widget; applications own those controls and their localization. Summary
text returned by the crate is in English.

## License

This project is licensed under the Zero-Clause BSD license (0BSD). See [LICENSE](LICENSE).
