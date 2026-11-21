# Changelog

## 0.3.1

- Upgrade `zip` dependency from `2.x` to `9.x` (`>= 9.0.5`) to address upstream security advisories.

## 0.3.0

- Detect HTML and XHTML files as a distinct `html` preview kind while keeping
  their original source available for a host application's browser view.

## 0.2.0

- Add a distinct Markdown preview kind for `.md` and `.markdown` files.
- Preserve raw source while providing pretty-printed JSON, XML, and YAML previews.
- Parse and emit YAML 1.2 for safe formatting; malformed structured text remains
  available unchanged in the raw preview.

## 0.1.0

- Initial standalone Rust crate for document thumbnails and previews.
- Add typed preview categories and image media types.
- Limit preview inputs to 64 MiB.
- Support PDF, OpenXML documents and spreadsheets, CSV/TSV, presentations,
  common archives, raster images, and text/code previews.
