use any_preview::{generate_preview, PreviewKind};

#[test]
fn markdown_keeps_source_for_rendered_host_preview() {
    let source = "# Example\n\nA **formatted** paragraph.\n";
    let preview = generate_preview("README.md", "text/markdown", source.as_bytes());

    assert_eq!(preview.kind, PreviewKind::Markdown);
    assert_eq!(preview.raw_text.as_deref(), Some(source));
    assert!(preview.formatted_text.is_none());
}

#[test]
fn json_preserves_source_and_returns_pretty_preview() {
    let source = r#"{"name":"any-preview","enabled":true}"#;
    let preview = generate_preview("config.json", "application/json", source.as_bytes());
    let formatted = preview.formatted_text.as_deref().unwrap();

    assert_eq!(preview.kind, PreviewKind::Code);
    assert_eq!(preview.raw_text.as_deref(), Some(source));
    assert!(formatted.contains("\n"));
    assert!(formatted.contains("\"name\": \"any-preview\""));
}

#[test]
fn xml_indents_elements_and_preserves_mixed_content() {
    let source = "<root><items><item>first</item><item>second</item></items><p>Hello <b>world</b> !</p></root>";
    let preview = generate_preview("document.xml", "application/xml", source.as_bytes());
    let formatted = preview.formatted_text.as_deref().unwrap();

    assert_eq!(preview.raw_text.as_deref(), Some(source));
    assert!(formatted.contains("\n  <items>"));
    assert!(formatted.contains("Hello <b>world</b> !"));
}

#[test]
fn yaml_keeps_source_and_returns_yaml_preview() {
    let source = "name: chat\nports: [8080, 8081]\nactive: true\n";
    let preview = generate_preview("config.yaml", "application/yaml", source.as_bytes());
    let formatted = preview.formatted_text.as_deref().unwrap();

    assert_eq!(preview.raw_text.as_deref(), Some(source));
    assert!(formatted.contains("name: chat"));
    assert!(formatted.contains("8080"));
    assert!(formatted.contains("active: true"));
}

#[test]
fn invalid_json_remains_available_as_raw_text() {
    let source = "{\"name\":";
    let preview = generate_preview("broken.json", "application/json", source.as_bytes());

    assert_eq!(preview.raw_text.as_deref(), Some(source));
    assert!(preview.formatted_text.is_none());
}
