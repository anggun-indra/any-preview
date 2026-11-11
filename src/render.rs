//! High-Fidelity OpenXML Layout Mappers (`WMDocumentMapper`, `PMPresentationMapper`,
//! `EMWorkbookMapper`) that compile `.docx`, `.pptx`, and `.xlsx`/`.csv` documents
//! into precision SVG layouts and rasterize them to PNG via `resvg`.

use crate::engine::{PreviewSection, PreviewSheet};
use base64::Engine;
use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use resvg::tiny_skia::Pixmap;
use resvg::usvg::{fontdb, Options, Transform, Tree};
use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::sync::{Arc, OnceLock};
use zip::ZipArchive;

fn shared_fontdb() -> Arc<fontdb::Database> {
    static DB: OnceLock<Arc<fontdb::Database>> = OnceLock::new();
    Arc::clone(DB.get_or_init(|| {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        db.set_serif_family("Times New Roman");
        db.set_sans_serif_family("Arial");
        db.set_monospace_family("Courier New");
        Arc::new(db)
    }))
}

/// Pre-warms the system font database on a background thread so the first document
/// preview does not pay the one-time ~200-400ms font scanning cost.
pub fn prewarm_font_database() {
    let _ = shared_fontdb();
}

pub fn render_svg_to_png(svg: &str, scale: f32) -> Option<Vec<u8>> {
    let mut opt = Options::default();
    opt.fontdb = shared_fontdb();
    opt.font_family = "Arial".to_string();
    let tree = Tree::from_str(svg, &opt).ok()?;
    let size = tree.size();
    let w = ((size.width() * scale).round() as u32).clamp(1, 2400);
    let h = ((size.height() * scale).round() as u32).clamp(1, 2400);
    let mut pixmap = Pixmap::new(w, h)?;
    resvg::render(
        &tree,
        Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    pixmap.encode_png().ok()
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c if c.is_control() && c != '\n' && c != '\t' => {}
            c => out.push(c),
        }
    }
    out
}

fn map_font_family(raw: &str) -> &'static str {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("times")
        || lower.contains("georgia")
        || lower.contains("cambria")
        || lower.contains("garamond")
        || lower.contains("palatino")
        || lower.contains("serif") && !lower.contains("sans")
    {
        "Times New Roman, Georgia, serif"
    } else if lower.contains("consolas")
        || lower.contains("courier")
        || lower.contains("menlo")
        || lower.contains("mono")
    {
        "Menlo, Courier New, monospace"
    } else {
        "Arial, Helvetica Neue, Helvetica, sans-serif"
    }
}

fn estimate_char_width_factor(ch: char, is_serif: bool, bold: bool) -> f32 {
    let base = match ch {
        ' ' => 0.27,
        'i' | 'l' | 'I' | '!' | '|' | '.' | ',' | ':' | ';' | '\'' => 0.26,
        'j' | 't' | 'f' | 'r' | '(' | ')' | '[' | ']' | '-' => 0.33,
        'm' | 'w' | 'M' | 'W' | '@' | '%' => 0.80,
        'A'..='Z' => {
            if is_serif {
                0.64
            } else {
                0.61
            }
        }
        '0'..='9' => 0.52,
        _ => {
            if is_serif {
                0.48
            } else {
                0.49
            }
        }
    };
    if bold {
        base * 1.04
    } else {
        base
    }
}

fn estimate_text_width(text: &str, font_size: f32, is_serif: bool, bold: bool) -> f32 {
    text.chars()
        .map(|c| estimate_char_width_factor(c, is_serif, bold) * font_size)
        .sum()
}

fn read_zip_string(archive: &mut ZipArchive<Cursor<&[u8]>>, name: &str) -> Option<String> {
    let mut file = archive.by_name(name).ok()?;
    if file.size() > 16_000_000 {
        return None;
    }
    let mut s = String::new();
    file.read_to_string(&mut s).ok()?;
    Some(s)
}

fn read_zip_bytes(archive: &mut ZipArchive<Cursor<&[u8]>>, name: &str) -> Option<Vec<u8>> {
    let mut file = archive.by_name(name).ok()?;
    if file.size() == 0 || file.size() > 12_000_000 {
        return None;
    }
    let mut buf = Vec::with_capacity(file.size() as usize);
    file.read_to_end(&mut buf).ok()?;
    Some(buf)
}

fn attr_val(e: &BytesStart<'_>, key: &[u8]) -> Option<String> {
    for attr in e.attributes().flatten() {
        if attr.key.local_name().as_ref() == key {
            if let Ok(v) = attr.unescape_value() {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn parse_rels_map(xml: &str) -> HashMap<String, String> {
    let mut reader = Reader::from_str(xml);
    let mut map = HashMap::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e) | Event::Empty(e))
                if e.local_name().as_ref() == b"Relationship" =>
            {
                if let (Some(id), Some(target)) = (attr_val(&e, b"Id"), attr_val(&e, b"Target")) {
                    map.insert(id, target);
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    map
}

fn resolve_zip_rel_path(base_dir: &str, target: &str) -> String {
    if let Some(stripped) = target.strip_prefix('/') {
        return stripped.to_string();
    }
    let mut parts: Vec<&str> = base_dir
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    for seg in target.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

fn image_bytes_to_data_uri(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 8 {
        return None;
    }
    let mime = if bytes.starts_with(b"\x89PNG") {
        "image/png"
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else if bytes.starts_with(b"RIFF") {
        "image/webp"
    } else {
        return None;
    };
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    Some(format!("data:{mime};base64,{encoded}"))
}

// ============================================================================
// PART 1: PowerPoint (.pptx) Layout Mapper (`PMPresentationMapper`)
// ============================================================================

#[derive(Clone, Debug)]
struct PptxColor {
    hex: String,
    alpha: f32,
}

impl Default for PptxColor {
    fn default() -> Self {
        Self {
            hex: "000000".to_string(),
            alpha: 1.0,
        }
    }
}

#[derive(Clone, Debug)]
enum PptxFill {
    None,
    Solid(PptxColor),
    LinearGradient {
        angle_deg: f32,
        stops: Vec<(f32, PptxColor)>,
    },
}

#[derive(Clone, Debug)]
struct PptxStroke {
    color: PptxColor,
    width_pt: f32,
}

#[derive(Clone, Debug)]
struct PptxCustomPath {
    w_emu: f32,
    h_emu: f32,
    d_commands: Vec<PathCmd>,
}

#[derive(Clone, Debug)]
enum PathCmd {
    MoveTo(f32, f32),
    LineTo(f32, f32),
    CubicTo(f32, f32, f32, f32, f32, f32),
    QuadTo(f32, f32, f32, f32),
    Close,
}

#[derive(Clone, Debug)]
struct PptxRun {
    text: String,
    font_family: String,
    font_size_pt: f32,
    bold: bool,
    italic: bool,
    color: PptxColor,
}

#[derive(Clone, Debug)]
struct PptxParagraph {
    align: String, // "l", "ctr", "r", "just"
    bullet: Option<String>,
    line_spacing_mult: f32,
    space_before_pt: f32,
    space_after_pt: f32,
    margin_left_pt: f32,
    runs: Vec<PptxRun>,
}

#[derive(Clone, Debug)]
struct PptxShape {
    x_emu: f32,
    y_emu: f32,
    w_emu: f32,
    h_emu: f32,
    prst_geom: String,
    cust_paths: Vec<PptxCustomPath>,
    fill: PptxFill,
    stroke: Option<PptxStroke>,
    has_shadow: bool,
    v_anchor: String, // "t", "ctr", "b"
    ins_l_pt: f32,
    ins_t_pt: f32,
    ins_r_pt: f32,
    ins_b_pt: f32,
    paragraphs: Vec<PptxParagraph>,
    pic_r_id: Option<String>,
    ph_type: Option<String>,
}

fn parse_theme_colors(xml: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    map.insert("dk1".to_string(), "0F172A".to_string());
    map.insert("lt1".to_string(), "FFFFFF".to_string());
    map.insert("dk2".to_string(), "1E293B".to_string());
    map.insert("lt2".to_string(), "F8FAFC".to_string());
    map.insert("accent1".to_string(), "2563EB".to_string());
    map.insert("accent2".to_string(), "EA580C".to_string());
    map.insert("accent3".to_string(), "16A34A".to_string());
    map.insert("accent4".to_string(), "D97706".to_string());
    map.insert("accent5".to_string(), "7C3AED".to_string());
    map.insert("accent6".to_string(), "0891B2".to_string());

    let mut reader = Reader::from_str(xml);
    let mut in_clr_scheme = false;
    let mut current_slot: Option<String> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let local = e.local_name();
                match local.as_ref() {
                    b"clrScheme" => in_clr_scheme = true,
                    b"dk1" | b"lt1" | b"dk2" | b"lt2" | b"accent1" | b"accent2" | b"accent3"
                    | b"accent4" | b"accent5" | b"accent6"
                        if in_clr_scheme =>
                    {
                        current_slot =
                            Some(String::from_utf8_lossy(local.as_ref()).to_string());
                    }
                    b"srgbClr" if current_slot.is_some() => {
                        if let Some(val) = attr_val(&e, b"val") {
                            if let Some(slot) = &current_slot {
                                map.insert(slot.clone(), val);
                            }
                        }
                    }
                    b"sysClr" if current_slot.is_some() => {
                        if let Some(val) =
                            attr_val(&e, b"lastClr").or_else(|| attr_val(&e, b"val"))
                        {
                            if val.len() == 6 {
                                if let Some(slot) = &current_slot {
                                    map.insert(slot.clone(), val);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(e)) => {
                let local = e.local_name();
                match local.as_ref() {
                    b"srgbClr" if current_slot.is_some() => {
                        if let Some(val) = attr_val(&e, b"val") {
                            if let Some(slot) = &current_slot {
                                map.insert(slot.clone(), val);
                            }
                        }
                    }
                    b"sysClr" if current_slot.is_some() => {
                        if let Some(val) =
                            attr_val(&e, b"lastClr").or_else(|| attr_val(&e, b"val"))
                        {
                            if val.len() == 6 {
                                if let Some(slot) = &current_slot {
                                    map.insert(slot.clone(), val);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                let local = e.local_name();
                match local.as_ref() {
                    b"clrScheme" => in_clr_scheme = false,
                    b"dk1" | b"lt1" | b"dk2" | b"lt2" | b"accent1" | b"accent2" | b"accent3"
                    | b"accent4" | b"accent5" | b"accent6" => {
                        current_slot = None;
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }

    if let Some(dk1) = map.get("dk1").cloned() {
        map.insert("tx1".to_string(), dk1);
    }
    if let Some(lt1) = map.get("lt1").cloned() {
        map.insert("bg1".to_string(), lt1);
    }
    if let Some(dk2) = map.get("dk2").cloned() {
        map.insert("tx2".to_string(), dk2);
    }
    if let Some(lt2) = map.get("lt2").cloned() {
        map.insert("bg2".to_string(), lt2);
    }
    map
}

fn parse_slide_size_emu(presentation_xml: &str) -> (f32, f32) {
    let mut reader = Reader::from_str(presentation_xml);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e) | Event::Empty(e)) if e.local_name().as_ref() == b"sldSz" => {
                let cx = attr_val(&e, b"cx")
                    .and_then(|v| v.parse::<f32>().ok())
                    .unwrap_or(12_192_000.0);
                let cy = attr_val(&e, b"cy")
                    .and_then(|v| v.parse::<f32>().ok())
                    .unwrap_or(6_858_000.0);
                return (cx.max(1.0), cy.max(1.0));
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    (12_192_000.0, 6_858_000.0)
}

fn resolve_scheme_color(slot: &str, theme: &HashMap<String, String>) -> String {
    theme
        .get(slot)
        .cloned()
        .unwrap_or_else(|| "0F172A".to_string())
}

fn parse_pptx_slide_xml(
    xml: &str,
    theme: &HashMap<String, String>,
) -> (Option<PptxFill>, Vec<PptxShape>) {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);

    let mut bg_fill: Option<PptxFill> = None;
    let mut shapes: Vec<PptxShape> = Vec::new();

    let mut in_bg = false;
    let mut current_shape: Option<PptxShape> = None;
    let mut in_sp_pr = false;
    let mut in_ln = false;
    let mut in_effect = false;
    let mut in_tx_body = false;
    let mut in_r_pr = false;
    let mut in_t = false;
    let mut in_grad = false;
    let mut grad_stops: Vec<(f32, PptxColor)> = Vec::new();
    let mut grad_angle: f32 = 90.0;
    let mut current_gs_pos: f32 = 0.0;
    let mut active_color: Option<PptxColor> = None;

    let mut current_path: Option<PptxCustomPath> = None;
    let mut path_cmd_tag: Option<String> = None;
    let mut path_pts: Vec<(f32, f32)> = Vec::new();

    let mut current_para: Option<PptxParagraph> = None;
    let mut current_run: Option<PptxRun> = None;
    let mut in_spc_bef = false;
    let mut in_spc_aft = false;
    let mut in_ln_spc = false;

    // Group transform stack: (gx, gy, gcx, gcy, chx, chy, chcx, chcy)
    let mut grp_stack: Vec<(f32, f32, f32, f32, f32, f32, f32, f32)> = Vec::new();
    let mut in_grp_sp_pr = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let local = e.local_name();
                match local.as_ref() {
                    b"bg" => in_bg = true,
                    b"grpSp" => {
                        grp_stack.push((0.0, 0.0, 1.0, 1.0, 0.0, 0.0, 1.0, 1.0));
                    }
                    b"grpSpPr" => in_grp_sp_pr = true,
                    b"sp" | b"pic" | b"cxnSp" => {
                        let is_cxn = local.as_ref() == b"cxnSp";
                        current_shape = Some(PptxShape {
                            x_emu: 0.0,
                            y_emu: 0.0,
                            w_emu: 0.0,
                            h_emu: 0.0,
                            prst_geom: if is_cxn {
                                "line".to_string()
                            } else {
                                "rect".to_string()
                            },
                            cust_paths: Vec::new(),
                            fill: PptxFill::None,
                            stroke: None,
                            has_shadow: false,
                            v_anchor: "t".to_string(),
                            ins_l_pt: 7.2,
                            ins_t_pt: 3.6,
                            ins_r_pt: 7.2,
                            ins_b_pt: 3.6,
                            paragraphs: Vec::new(),
                            pic_r_id: None,
                            ph_type: None,
                        });
                    }
                    b"ph" => {
                        if let Some(sh) = &mut current_shape {
                            sh.ph_type = attr_val(&e, b"type").or_else(|| Some("body".to_string()));
                        }
                    }
                    b"spPr" => in_sp_pr = true,
                    b"effectLst" if in_sp_pr => in_effect = true,
                    b"outerShdw" if in_sp_pr => {
                        in_effect = true;
                        if let Some(sh) = &mut current_shape {
                            sh.has_shadow = true;
                        }
                    }
                    b"ln" if in_sp_pr => {
                        in_ln = true;
                        let w_emu = attr_val(&e, b"w")
                            .and_then(|v| v.parse::<f32>().ok())
                            .unwrap_or(12700.0);
                        if let Some(sh) = &mut current_shape {
                            sh.stroke = Some(PptxStroke {
                                color: PptxColor::default(),
                                width_pt: (w_emu / 12700.0).max(0.75),
                            });
                        }
                    }
                    b"gradFill" if in_bg || (in_sp_pr && !in_ln) => {
                        in_grad = true;
                        grad_stops.clear();
                        grad_angle = 90.0;
                    }
                    b"gs" if in_grad => {
                        current_gs_pos = attr_val(&e, b"pos")
                            .and_then(|v| v.parse::<f32>().ok())
                            .unwrap_or(0.0)
                            / 100_000.0;
                    }
                    b"srgbClr" => {
                        if let Some(val) = attr_val(&e, b"val") {
                            active_color = Some(PptxColor {
                                hex: val,
                                alpha: 1.0,
                            });
                        }
                    }
                    b"schemeClr" => {
                        if let Some(val) = attr_val(&e, b"val") {
                            active_color = Some(PptxColor {
                                hex: resolve_scheme_color(&val, theme),
                                alpha: 1.0,
                            });
                        }
                    }
                    b"path" if in_sp_pr => {
                        let w = attr_val(&e, b"w")
                            .and_then(|v| v.parse::<f32>().ok())
                            .unwrap_or(1.0);
                        let h = attr_val(&e, b"h")
                            .and_then(|v| v.parse::<f32>().ok())
                            .unwrap_or(1.0);
                        current_path = Some(PptxCustomPath {
                            w_emu: w.max(1.0),
                            h_emu: h.max(1.0),
                            d_commands: Vec::new(),
                        });
                    }
                    b"moveTo" | b"lnTo" | b"cubicBezTo" | b"quadBezTo"
                        if current_path.is_some() =>
                    {
                        path_cmd_tag =
                            Some(String::from_utf8_lossy(local.as_ref()).to_string());
                        path_pts.clear();
                    }
                    b"txBody" => in_tx_body = true,
                    b"bodyPr" if in_tx_body => {
                        if let Some(sh) = &mut current_shape {
                            if let Some(anc) = attr_val(&e, b"anchor") {
                                sh.v_anchor = anc;
                            }
                            if let Some(v) =
                                attr_val(&e, b"lIns").and_then(|s| s.parse::<f32>().ok())
                            {
                                sh.ins_l_pt = v / 12700.0;
                            }
                            if let Some(v) =
                                attr_val(&e, b"tIns").and_then(|s| s.parse::<f32>().ok())
                            {
                                sh.ins_t_pt = v / 12700.0;
                            }
                            if let Some(v) =
                                attr_val(&e, b"rIns").and_then(|s| s.parse::<f32>().ok())
                            {
                                sh.ins_r_pt = v / 12700.0;
                            }
                            if let Some(v) =
                                attr_val(&e, b"bIns").and_then(|s| s.parse::<f32>().ok())
                            {
                                sh.ins_b_pt = v / 12700.0;
                            }
                        }
                    }
                    b"p" if in_tx_body => {
                        current_para = Some(PptxParagraph {
                            align: "l".to_string(),
                            bullet: None,
                            line_spacing_mult: 1.18,
                            space_before_pt: 0.0,
                            space_after_pt: 0.0,
                            margin_left_pt: 0.0,
                            runs: Vec::new(),
                        });
                    }
                    b"pPr" if current_para.is_some() => {
                        if let Some(p) = &mut current_para {
                            if let Some(algn) = attr_val(&e, b"algn") {
                                p.align = algn;
                            }
                            if let Some(mar_l) =
                                attr_val(&e, b"marL").and_then(|v| v.parse::<f32>().ok())
                            {
                                p.margin_left_pt = mar_l / 12700.0;
                            }
                        }
                    }
                    b"lnSpc" if current_para.is_some() => in_ln_spc = true,
                    b"spcBef" if current_para.is_some() => in_spc_bef = true,
                    b"spcAft" if current_para.is_some() => in_spc_aft = true,
                    b"r" if current_para.is_some() => {
                        current_run = Some(PptxRun {
                            text: String::new(),
                            font_family: "Arial".to_string(),
                            font_size_pt: 16.0,
                            bold: false,
                            italic: false,
                            color: PptxColor {
                                hex: "0F172A".to_string(),
                                alpha: 1.0,
                            },
                        });
                    }
                    b"rPr" if current_run.is_some() => {
                        in_r_pr = true;
                        if let Some(r) = &mut current_run {
                            if let Some(sz) =
                                attr_val(&e, b"sz").and_then(|v| v.parse::<f32>().ok())
                            {
                                r.font_size_pt = (sz / 100.0).clamp(6.0, 96.0);
                            }
                            if let Some(b) = attr_val(&e, b"b") {
                                r.bold = b == "1" || b.eq_ignore_ascii_case("true");
                            }
                            if let Some(i) = attr_val(&e, b"i") {
                                r.italic = i == "1" || i.eq_ignore_ascii_case("true");
                            }
                        }
                    }
                    b"t" if current_run.is_some() => in_t = true,
                    b"blip" => {
                        if let Some(sh) = &mut current_shape {
                            if let Some(rid) = attr_val(&e, b"embed") {
                                sh.pic_r_id = Some(rid);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(e)) => {
                let local = e.local_name();
                match local.as_ref() {
                    b"off" => {
                        let x = attr_val(&e, b"x")
                            .and_then(|v| v.parse::<f32>().ok())
                            .unwrap_or(0.0);
                        let y = attr_val(&e, b"y")
                            .and_then(|v| v.parse::<f32>().ok())
                            .unwrap_or(0.0);
                        if in_grp_sp_pr {
                            if let Some(last) = grp_stack.last_mut() {
                                last.0 = x;
                                last.1 = y;
                            }
                        } else if let Some(sh) = &mut current_shape {
                            sh.x_emu = x;
                            sh.y_emu = y;
                        }
                    }
                    b"ext" => {
                        let cx = attr_val(&e, b"cx")
                            .and_then(|v| v.parse::<f32>().ok())
                            .unwrap_or(0.0);
                        let cy = attr_val(&e, b"cy")
                            .and_then(|v| v.parse::<f32>().ok())
                            .unwrap_or(0.0);
                        if in_grp_sp_pr {
                            if let Some(last) = grp_stack.last_mut() {
                                last.2 = cx.max(1.0);
                                last.3 = cy.max(1.0);
                            }
                        } else if let Some(sh) = &mut current_shape {
                            sh.w_emu = cx;
                            sh.h_emu = cy;
                        }
                    }
                    b"chOff" if in_grp_sp_pr => {
                        let x = attr_val(&e, b"x")
                            .and_then(|v| v.parse::<f32>().ok())
                            .unwrap_or(0.0);
                        let y = attr_val(&e, b"y")
                            .and_then(|v| v.parse::<f32>().ok())
                            .unwrap_or(0.0);
                        if let Some(last) = grp_stack.last_mut() {
                            last.4 = x;
                            last.5 = y;
                        }
                    }
                    b"chExt" if in_grp_sp_pr => {
                        let cx = attr_val(&e, b"cx")
                            .and_then(|v| v.parse::<f32>().ok())
                            .unwrap_or(1.0);
                        let cy = attr_val(&e, b"cy")
                            .and_then(|v| v.parse::<f32>().ok())
                            .unwrap_or(1.0);
                        if let Some(last) = grp_stack.last_mut() {
                            last.6 = cx.max(1.0);
                            last.7 = cy.max(1.0);
                        }
                    }
                    b"ph" => {
                        if let Some(sh) = &mut current_shape {
                            sh.ph_type = attr_val(&e, b"type").or_else(|| Some("body".to_string()));
                        }
                    }
                    b"prstGeom" if in_sp_pr => {
                        if let Some(prst) = attr_val(&e, b"prst") {
                            if let Some(sh) = &mut current_shape {
                                sh.prst_geom = prst;
                            }
                        }
                    }
                    b"noFill" => {
                        if in_ln {
                            if let Some(sh) = &mut current_shape {
                                sh.stroke = None;
                            }
                        } else if in_sp_pr {
                            if let Some(sh) = &mut current_shape {
                                sh.fill = PptxFill::None;
                            }
                        }
                    }
                    b"srgbClr" => {
                        if !in_effect {
                            if let Some(val) = attr_val(&e, b"val") {
                                let c = PptxColor {
                                    hex: val,
                                    alpha: 1.0,
                                };
                                apply_parsed_color(
                                    c,
                                    in_bg,
                                    in_sp_pr,
                                    in_ln,
                                    in_grad,
                                    current_gs_pos,
                                    in_r_pr,
                                    &mut bg_fill,
                                    &mut current_shape,
                                    &mut grad_stops,
                                    &mut current_run,
                                );
                            }
                        }
                    }
                    b"schemeClr" => {
                        if !in_effect {
                            if let Some(val) = attr_val(&e, b"val") {
                                let c = PptxColor {
                                    hex: resolve_scheme_color(&val, theme),
                                    alpha: 1.0,
                                };
                                apply_parsed_color(
                                    c,
                                    in_bg,
                                    in_sp_pr,
                                    in_ln,
                                    in_grad,
                                    current_gs_pos,
                                    in_r_pr,
                                    &mut bg_fill,
                                    &mut current_shape,
                                    &mut grad_stops,
                                    &mut current_run,
                                );
                            }
                        }
                    }
                    b"alpha" => {
                        if let Some(a) = attr_val(&e, b"val")
                            .and_then(|v| v.parse::<f32>().ok())
                        {
                            if let Some(c) = &mut active_color {
                                c.alpha = (a / 100_000.0).clamp(0.0, 1.0);
                            }
                        }
                    }
                    b"lin" if in_grad => {
                        if let Some(ang) = attr_val(&e, b"ang")
                            .and_then(|v| v.parse::<f32>().ok())
                        {
                            grad_angle = ang / 60_000.0;
                        }
                    }
                    b"outerShdw" if in_sp_pr => {
                        if let Some(sh) = &mut current_shape {
                            sh.has_shadow = true;
                        }
                    }
                    b"pt" if path_cmd_tag.is_some() => {
                        let x = attr_val(&e, b"x")
                            .and_then(|v| v.parse::<f32>().ok())
                            .unwrap_or(0.0);
                        let y = attr_val(&e, b"y")
                            .and_then(|v| v.parse::<f32>().ok())
                            .unwrap_or(0.0);
                        path_pts.push((x, y));
                    }
                    b"close" if current_path.is_some() => {
                        if let Some(p) = &mut current_path {
                            p.d_commands.push(PathCmd::Close);
                        }
                    }
                    b"bodyPr" if in_tx_body => {
                        if let Some(sh) = &mut current_shape {
                            if let Some(anc) = attr_val(&e, b"anchor") {
                                sh.v_anchor = anc;
                            }
                        }
                    }
                    b"pPr" if current_para.is_some() => {
                        if let Some(p) = &mut current_para {
                            if let Some(algn) = attr_val(&e, b"algn") {
                                p.align = algn;
                            }
                            if let Some(mar_l) =
                                attr_val(&e, b"marL").and_then(|v| v.parse::<f32>().ok())
                            {
                                p.margin_left_pt = mar_l / 12700.0;
                            }
                        }
                    }
                    b"buChar" if current_para.is_some() => {
                        if let Some(p) = &mut current_para {
                            p.bullet = Some(attr_val(&e, b"char").unwrap_or_else(|| "•".to_string()));
                        }
                    }
                    b"buNone" if current_para.is_some() => {
                        if let Some(p) = &mut current_para {
                            p.bullet = None;
                        }
                    }
                    b"spcPct" if in_ln_spc => {
                        if let Some(val) = attr_val(&e, b"val")
                            .and_then(|v| v.parse::<f32>().ok())
                        {
                            if let Some(p) = &mut current_para {
                                p.line_spacing_mult = (val / 100_000.0).clamp(0.85, 2.5);
                            }
                        }
                    }
                    b"spcPts" => {
                        if let Some(val) = attr_val(&e, b"val")
                            .and_then(|v| v.parse::<f32>().ok())
                        {
                            let pts = val / 100.0;
                            if let Some(p) = &mut current_para {
                                if in_spc_bef {
                                    p.space_before_pt = pts;
                                } else if in_spc_aft {
                                    p.space_after_pt = pts;
                                }
                            }
                        }
                    }
                    b"rPr" if current_run.is_some() => {
                        if let Some(r) = &mut current_run {
                            if let Some(sz) =
                                attr_val(&e, b"sz").and_then(|v| v.parse::<f32>().ok())
                            {
                                r.font_size_pt = (sz / 100.0).clamp(6.0, 96.0);
                            }
                            if let Some(b) = attr_val(&e, b"b") {
                                r.bold = b == "1" || b.eq_ignore_ascii_case("true");
                            }
                            if let Some(i) = attr_val(&e, b"i") {
                                r.italic = i == "1" || i.eq_ignore_ascii_case("true");
                            }
                        }
                    }
                    b"latin" if in_r_pr => {
                        if let Some(tf) = attr_val(&e, b"typeface") {
                            if let Some(r) = &mut current_run {
                                r.font_family = tf;
                            }
                        }
                    }
                    b"blip" => {
                        if let Some(sh) = &mut current_shape {
                            if let Some(rid) = attr_val(&e, b"embed") {
                                sh.pic_r_id = Some(rid);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(e)) if in_t => {
                if let Ok(decoded) = e.unescape() {
                    if let Some(r) = &mut current_run {
                        r.text.push_str(&decoded);
                    }
                }
            }
            Ok(Event::End(e)) => {
                let local = e.local_name();
                match local.as_ref() {
                    b"bg" => in_bg = false,
                    b"grpSpPr" => in_grp_sp_pr = false,
                    b"grpSp" => {
                        grp_stack.pop();
                    }
                    b"spPr" => {
                        in_sp_pr = false;
                        in_effect = false;
                    }
                    b"effectLst" | b"outerShdw" => in_effect = false,
                    b"ln" => in_ln = false,
                    b"srgbClr" | b"schemeClr" => {
                        if let Some(c) = active_color.take() {
                            if !in_effect {
                                apply_parsed_color(
                                    c,
                                    in_bg,
                                    in_sp_pr,
                                    in_ln,
                                    in_grad,
                                    current_gs_pos,
                                    in_r_pr,
                                    &mut bg_fill,
                                    &mut current_shape,
                                    &mut grad_stops,
                                    &mut current_run,
                                );
                            }
                        }
                    }
                    b"gradFill" => {
                        if in_grad {
                            in_grad = false;
                            if !grad_stops.is_empty() {
                                let fill = PptxFill::LinearGradient {
                                    angle_deg: grad_angle,
                                    stops: grad_stops.clone(),
                                };
                                if in_bg {
                                    bg_fill = Some(fill);
                                } else if in_sp_pr && !in_ln {
                                    if let Some(sh) = &mut current_shape {
                                        sh.fill = fill;
                                    }
                                }
                            }
                        }
                    }
                    b"moveTo" | b"lnTo" | b"cubicBezTo" | b"quadBezTo" => {
                        if let (Some(tag), Some(path)) = (&path_cmd_tag, &mut current_path) {
                            match tag.as_str() {
                                "moveTo" if !path_pts.is_empty() => {
                                    let (x, y) = path_pts[0];
                                    path.d_commands.push(PathCmd::MoveTo(x, y));
                                }
                                "lnTo" if !path_pts.is_empty() => {
                                    let (x, y) = path_pts[0];
                                    path.d_commands.push(PathCmd::LineTo(x, y));
                                }
                                "cubicBezTo" if path_pts.len() >= 3 => {
                                    path.d_commands.push(PathCmd::CubicTo(
                                        path_pts[0].0,
                                        path_pts[0].1,
                                        path_pts[1].0,
                                        path_pts[1].1,
                                        path_pts[2].0,
                                        path_pts[2].1,
                                    ));
                                }
                                "quadBezTo" if path_pts.len() >= 2 => {
                                    path.d_commands.push(PathCmd::QuadTo(
                                        path_pts[0].0,
                                        path_pts[0].1,
                                        path_pts[1].0,
                                        path_pts[1].1,
                                    ));
                                }
                                _ => {}
                            }
                        }
                        path_cmd_tag = None;
                        path_pts.clear();
                    }
                    b"path" => {
                        if let (Some(path), Some(sh)) = (current_path.take(), &mut current_shape) {
                            if !path.d_commands.is_empty() {
                                sh.cust_paths.push(path);
                            }
                        }
                    }
                    b"txBody" => in_tx_body = false,
                    b"lnSpc" => in_ln_spc = false,
                    b"spcBef" => in_spc_bef = false,
                    b"spcAft" => in_spc_aft = false,
                    b"t" => in_t = false,
                    b"rPr" => in_r_pr = false,
                    b"r" => {
                        if let (Some(r), Some(p)) = (current_run.take(), &mut current_para) {
                            if !r.text.is_empty() {
                                p.runs.push(r);
                            }
                        }
                    }
                    b"p" => {
                        if let (Some(p), Some(sh)) = (current_para.take(), &mut current_shape) {
                            if !p.runs.is_empty() {
                                sh.paragraphs.push(p);
                            }
                        }
                    }
                    b"sp" | b"pic" | b"cxnSp" => {
                        if let Some(mut sh) = current_shape.take() {
                            // Apply group transforms if inside nested <p:grpSp>
                            for &(gx, gy, gcx, gcy, chx, chy, chcx, chcy) in grp_stack.iter().rev()
                            {
                                if chcx > 1.0 && chcy > 1.0 && gcx > 1.0 && gcy > 1.0 {
                                    sh.x_emu = gx + (sh.x_emu - chx) * (gcx / chcx);
                                    sh.y_emu = gy + (sh.y_emu - chy) * (gcy / chcy);
                                    sh.w_emu *= gcx / chcx;
                                    sh.h_emu *= gcy / chcy;
                                }
                            }
                            shapes.push(sh);
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }

    (bg_fill, shapes)
}

#[allow(clippy::too_many_arguments)]
fn apply_parsed_color(
    c: PptxColor,
    in_bg: bool,
    in_sp_pr: bool,
    in_ln: bool,
    in_grad: bool,
    current_gs_pos: f32,
    in_r_pr: bool,
    bg_fill: &mut Option<PptxFill>,
    current_shape: &mut Option<PptxShape>,
    grad_stops: &mut Vec<(f32, PptxColor)>,
    current_run: &mut Option<PptxRun>,
) {
    if in_grad {
        grad_stops.push((current_gs_pos, c));
    } else if in_r_pr {
        if let Some(r) = current_run {
            r.color = c;
        }
    } else if in_ln {
        if let Some(sh) = current_shape {
            if let Some(st) = &mut sh.stroke {
                st.color = c;
            }
        }
    } else if in_sp_pr {
        if let Some(sh) = current_shape {
            sh.fill = PptxFill::Solid(c);
        }
    } else if in_bg {
        *bg_fill = Some(PptxFill::Solid(c));
    }
}

fn render_pptx_slide_svg(
    slide_w_emu: f32,
    slide_h_emu: f32,
    bg_fill: &Option<PptxFill>,
    shapes: &[PptxShape],
    rels: &HashMap<String, String>,
    archive: &mut ZipArchive<Cursor<&[u8]>>,
) -> String {
    // 1 pt = 12700 EMU. Standard 16:9 widescreen is 960 x 540 pt.
    let canvas_w = (slide_w_emu / 12700.0).max(400.0);
    let canvas_h = (slide_h_emu / 12700.0).max(225.0);

    let mut defs = String::new();
    let mut body = String::new();
    let mut grad_counter = 0usize;

    let mut emit_fill_attr = |fill: &PptxFill, defs: &mut String| -> String {
        match fill {
            PptxFill::None => r#"fill="none""#.to_string(),
            PptxFill::Solid(c) => {
                if c.alpha < 0.999 {
                    format!(r##"fill="#{}" fill-opacity="{:.3}""##, c.hex, c.alpha)
                } else {
                    format!(r##"fill="#{}""##, c.hex)
                }
            }
            PptxFill::LinearGradient { angle_deg, stops } => {
                grad_counter += 1;
                let id = format!("grad_{grad_counter}");
                let rad = angle_deg.to_radians();
                let x1 = 50.0 - 50.0 * rad.cos();
                let y1 = 50.0 - 50.0 * rad.sin();
                let x2 = 50.0 + 50.0 * rad.cos();
                let y2 = 50.0 + 50.0 * rad.sin();
                defs.push_str(&format!(
                    r#"<linearGradient id="{id}" x1="{x1:.1}%" y1="{y1:.1}%" x2="{x2:.1}%" y2="{y2:.1}%">"#
                ));
                for (pos, col) in stops {
                    let pct = (pos * 100.0).clamp(0.0, 100.0);
                    defs.push_str(&format!(
                        r##"<stop offset="{pct:.1}%" stop-color="#{}" stop-opacity="{:.3}"/>"##,
                        col.hex, col.alpha
                    ));
                }
                defs.push_str("</linearGradient>");
                format!(r#"fill="url(#{id})""#)
            }
        }
    };

    // 1. Slide background
    let bg_attr = match bg_fill {
        Some(f) => emit_fill_attr(f, &mut defs),
        None => r##"fill="#FFFFFF""##.to_string(),
    };
    body.push_str(&format!(
        r#"<rect x="0" y="0" width="{canvas_w:.2}" height="{canvas_h:.2}" {bg_attr}/>"#
    ));

    // 2. Render each shape in exact Z-order
    for sh in shapes {
        let mut x = sh.x_emu / 12700.0;
        let mut y = sh.y_emu / 12700.0;
        let mut w = sh.w_emu / 12700.0;
        let mut h = sh.h_emu / 12700.0;

        // Fallback placeholder coordinates when a slide relies on master/layout placeholder bounds
        if w <= 1.0 && h <= 1.0 {
            if let Some(ph) = &sh.ph_type {
                match ph.as_str() {
                    "ctrTitle" | "title" => {
                        x = canvas_w * 0.07;
                        y = canvas_h * 0.08;
                        w = canvas_w * 0.86;
                        h = canvas_h * 0.18;
                    }
                    "subTitle" => {
                        x = canvas_w * 0.10;
                        y = canvas_h * 0.30;
                        w = canvas_w * 0.80;
                        h = canvas_h * 0.15;
                    }
                    _ => {
                        x = canvas_w * 0.07;
                        y = canvas_h * 0.24;
                        w = canvas_w * 0.86;
                        h = canvas_h * 0.66;
                    }
                }
            }
        }

        // Embedded picture (<p:pic>)
        if let Some(rid) = &sh.pic_r_id {
            if let Some(target) = rels.get(rid) {
                let zip_path = resolve_zip_rel_path("ppt/slides", target);
                if let Some(img_bytes) = read_zip_bytes(archive, &zip_path) {
                    if let Some(data_uri) = image_bytes_to_data_uri(&img_bytes) {
                        body.push_str(&format!(
                            r#"<image x="{x:.2}" y="{y:.2}" width="{w:.2}" height="{h:.2}" preserveAspectRatio="none" href="{data_uri}"/>"#
                        ));
                    }
                }
            }
            continue;
        }

        let fill_attr = emit_fill_attr(&sh.fill, &mut defs);
        let stroke_attr = match &sh.stroke {
            Some(st) if st.color.alpha > 0.01 => format!(
                r##"stroke="#{}" stroke-opacity="{:.3}" stroke-width="{:.2}""##,
                st.color.hex, st.color.alpha, st.width_pt
            ),
            _ => r#"stroke="none""#.to_string(),
        };

        let has_visible_paint =
            !matches!(sh.fill, PptxFill::None) || sh.stroke.is_some();

        if has_visible_paint && w > 0.5 && h > 0.5 {
            if !sh.cust_paths.is_empty() {
                for cp in &sh.cust_paths {
                    let mut d = String::new();
                    let sx = w / cp.w_emu.max(1.0);
                    let sy = h / cp.h_emu.max(1.0);
                    for cmd in &cp.d_commands {
                        match *cmd {
                            PathCmd::MoveTo(px, py) => {
                                d.push_str(&format!("M {:.2} {:.2} ", x + px * sx, y + py * sy));
                            }
                            PathCmd::LineTo(px, py) => {
                                d.push_str(&format!("L {:.2} {:.2} ", x + px * sx, y + py * sy));
                            }
                            PathCmd::CubicTo(x1, y1, x2, y2, x3, y3) => {
                                d.push_str(&format!(
                                    "C {:.2} {:.2}, {:.2} {:.2}, {:.2} {:.2} ",
                                    x + x1 * sx,
                                    y + y1 * sy,
                                    x + x2 * sx,
                                    y + y2 * sy,
                                    x + x3 * sx,
                                    y + y3 * sy
                                ));
                            }
                            PathCmd::QuadTo(x1, y1, x2, y2) => {
                                d.push_str(&format!(
                                    "Q {:.2} {:.2}, {:.2} {:.2} ",
                                    x + x1 * sx,
                                    y + y1 * sy,
                                    x + x2 * sx,
                                    y + y2 * sy
                                ));
                            }
                            PathCmd::Close => d.push_str("Z "),
                        }
                    }
                    if sh.has_shadow && !matches!(sh.fill, PptxFill::None) {
                        let mut sd = String::new();
                        for cmd in &cp.d_commands {
                            match *cmd {
                                PathCmd::MoveTo(px, py) => sd.push_str(&format!(
                                    "M {:.2} {:.2} ",
                                    x + px * sx,
                                    y + py * sy + 2.5
                                )),
                                PathCmd::LineTo(px, py) => sd.push_str(&format!(
                                    "L {:.2} {:.2} ",
                                    x + px * sx,
                                    y + py * sy + 2.5
                                )),
                                PathCmd::CubicTo(x1, y1, x2, y2, x3, y3) => sd.push_str(&format!(
                                    "C {:.2} {:.2}, {:.2} {:.2}, {:.2} {:.2} ",
                                    x + x1 * sx,
                                    y + y1 * sy + 2.5,
                                    x + x2 * sx,
                                    y + y2 * sy + 2.5,
                                    x + x3 * sx,
                                    y + y3 * sy + 2.5
                                )),
                                PathCmd::QuadTo(x1, y1, x2, y2) => sd.push_str(&format!(
                                    "Q {:.2} {:.2}, {:.2} {:.2} ",
                                    x + x1 * sx,
                                    y + y1 * sy + 2.5,
                                    x + x2 * sx,
                                    y + y2 * sy + 2.5
                                )),
                                PathCmd::Close => sd.push_str("Z "),
                            }
                        }
                        body.push_str(&format!(
                            r##"<path d="{}" fill="#000000" fill-opacity="0.06"/>"##,
                            sd.trim()
                        ));
                    }
                    body.push_str(&format!(
                        r#"<path d="{}" {fill_attr} {stroke_attr}/>"#,
                        d.trim()
                    ));
                }
            } else {
                match sh.prst_geom.as_str() {
                    "ellipse" => {
                        let cx = x + w * 0.5;
                        let cy = y + h * 0.5;
                        let rx = w * 0.5;
                        let ry = h * 0.5;
                        body.push_str(&format!(
                            r#"<ellipse cx="{cx:.2}" cy="{cy:.2}" rx="{rx:.2}" ry="{ry:.2}" {fill_attr} {stroke_attr}/>"#
                        ));
                    }
                    "roundRect" => {
                        let r = (w.min(h) * 0.14).clamp(4.0, 24.0);
                        if sh.has_shadow && !matches!(sh.fill, PptxFill::None) {
                            body.push_str(&format!(
                                r##"<rect x="{x:.2}" y="{:.2}" width="{w:.2}" height="{h:.2}" rx="{r:.2}" fill="#000000" fill-opacity="0.06"/>"##,
                                y + 2.5
                            ));
                        }
                        body.push_str(&format!(
                            r#"<rect x="{x:.2}" y="{y:.2}" width="{w:.2}" height="{h:.2}" rx="{r:.2}" {fill_attr} {stroke_attr}/>"#
                        ));
                    }
                    "line" | "straightConnector1" => {
                        body.push_str(&format!(
                            r#"<line x1="{x:.2}" y1="{y:.2}" x2="{:.2}" y2="{:.2}" {stroke_attr}/>"#,
                            x + w,
                            y + h
                        ));
                    }
                    _ => {
                        if sh.has_shadow && !matches!(sh.fill, PptxFill::None) {
                            body.push_str(&format!(
                                r##"<rect x="{x:.2}" y="{:.2}" width="{w:.2}" height="{h:.2}" fill="#000000" fill-opacity="0.06"/>"##,
                                y + 2.5
                            ));
                        }
                        body.push_str(&format!(
                            r#"<rect x="{x:.2}" y="{y:.2}" width="{w:.2}" height="{h:.2}" {fill_attr} {stroke_attr}/>"#
                        ));
                    }
                }
            }
        }

        // 3. Render text body inside the shape
        if !sh.paragraphs.is_empty() && w > 4.0 && h > 4.0 {
            render_pptx_textbox_svg(&mut body, sh, x, y, w, h);
        }
    }

    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {canvas_w:.2} {canvas_h:.2}" width="{canvas_w:.2}" height="{canvas_h:.2}"><defs>{defs}</defs>{body}</svg>"#
    )
}

#[derive(Clone)]
struct WrappedSlideLine {
    align: String,
    font_family: &'static str,
    font_size_pt: f32,
    line_height_pt: f32,
    bold: bool,
    italic: bool,
    color: PptxColor,
    text: String,
    space_before_pt: f32,
    space_after_pt: f32,
    margin_left_pt: f32,
}

fn render_pptx_textbox_svg(
    body: &mut String,
    sh: &PptxShape,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
) {
    let inner_x = x + sh.ins_l_pt;
    let inner_w = (w - sh.ins_l_pt - sh.ins_r_pt).max(12.0);
    let mut lines: Vec<WrappedSlideLine> = Vec::new();

    for para in &sh.paragraphs {
        let first_run = match para.runs.first() {
            Some(r) => r,
            None => continue,
        };
        let font_family = map_font_family(&first_run.font_family);
        let is_serif = font_family.starts_with("Times");
        let font_size = first_run.font_size_pt;
        let bold = para.runs.iter().any(|r| r.bold);
        let italic = para.runs.iter().any(|r| r.italic);
        let color = first_run.color.clone();

        let mut full_text: String = para.runs.iter().map(|r| r.text.as_str()).collect();
        if let Some(bul) = &para.bullet {
            if !full_text.trim().is_empty() {
                full_text = format!("{bul} {}", full_text.trim());
            }
        }
        if full_text.trim().is_empty() {
            continue;
        }

        let avail_w = (inner_w - para.margin_left_pt).max(16.0);
        let wrapped = wrap_plain_text(&full_text, font_size, is_serif, bold, avail_w);
        let line_h = font_size * para.line_spacing_mult;
        let count = wrapped.len();
        for (idx, line_str) in wrapped.into_iter().enumerate() {
            lines.push(WrappedSlideLine {
                align: para.align.clone(),
                font_family,
                font_size_pt: font_size,
                line_height_pt: line_h,
                bold,
                italic,
                color: color.clone(),
                text: line_str,
                space_before_pt: if idx == 0 { para.space_before_pt } else { 0.0 },
                space_after_pt: if idx + 1 == count {
                    para.space_after_pt
                } else {
                    0.0
                },
                margin_left_pt: para.margin_left_pt,
            });
        }
    }

    if lines.is_empty() {
        return;
    }

    let total_text_h: f32 = lines
        .iter()
        .map(|l| l.space_before_pt + l.line_height_pt + l.space_after_pt)
        .sum();

    let inner_h = (h - sh.ins_t_pt - sh.ins_b_pt).max(8.0);
    let mut cursor_y = match sh.v_anchor.as_str() {
        "ctr" => y + sh.ins_t_pt + ((inner_h - total_text_h) * 0.5).max(-4.0),
        "b" => y + h - sh.ins_b_pt - total_text_h,
        _ => y + sh.ins_t_pt,
    };

    for line in lines {
        cursor_y += line.space_before_pt;
        let baseline_y = cursor_y + line.font_size_pt * 0.84;
        let (tx, anchor) = match line.align.as_str() {
            "ctr" => (inner_x + inner_w * 0.5, "middle"),
            "r" => (inner_x + inner_w, "end"),
            _ => (inner_x + line.margin_left_pt, "start"),
        };
        let weight = if line.bold { "bold" } else { "normal" };
        let style = if line.italic { "italic" } else { "normal" };
        let opacity_attr = if line.color.alpha < 0.999 {
            format!(r#" fill-opacity="{:.3}""#, line.color.alpha)
        } else {
            String::new()
        };
        body.push_str(&format!(
            r##"<text x="{tx:.2}" y="{baseline_y:.2}" font-family="{}" font-size="{:.2}" font-weight="{weight}" font-style="{style}" fill="#{}"{opacity_attr} text-anchor="{anchor}">{}</text>"##,
            line.font_family,
            line.font_size_pt,
            line.color.hex,
            xml_escape(&line.text)
        ));
        cursor_y += line.line_height_pt + line.space_after_pt;
    }
}

fn wrap_plain_text(
    text: &str,
    font_size: f32,
    is_serif: bool,
    bold: bool,
    max_width: f32,
) -> Vec<String> {
    let mut out = Vec::new();
    for raw_line in text.split('\n') {
        let words: Vec<&str> = raw_line.split_whitespace().collect();
        if words.is_empty() {
            continue;
        }
        let mut current = String::new();
        let mut current_w = 0.0f32;
        let space_w = estimate_char_width_factor(' ', is_serif, bold) * font_size;

        for word in words {
            let word_w = estimate_text_width(word, font_size, is_serif, bold);
            if !current.is_empty() && current_w + space_w + word_w > max_width * 1.03 {
                out.push(current);
                current = word.to_string();
                current_w = word_w;
            } else {
                if !current.is_empty() {
                    current.push(' ');
                    current_w += space_w;
                }
                current.push_str(word);
                current_w += word_w;
            }
        }
        if !current.is_empty() {
            out.push(current);
        }
    }
    out
}

pub fn render_pptx_document(
    archive: &mut ZipArchive<Cursor<&[u8]>>,
    max_rendered_slides: usize,
) -> (Vec<PreviewSection>, Vec<Vec<u8>>, Option<Vec<u8>>) {
    let pres_xml = read_zip_string(archive, "ppt/presentation.xml").unwrap_or_default();
    let (slide_w_emu, slide_h_emu) = parse_slide_size_emu(&pres_xml);
    let theme_xml = read_zip_string(archive, "ppt/theme/theme1.xml").unwrap_or_default();
    let theme = parse_theme_colors(&theme_xml);

    // Optional master background fallback
    let master_bg = read_zip_string(archive, "ppt/slideMasters/slideMaster1.xml")
        .and_then(|xml| parse_pptx_slide_xml(&xml, &theme).0);

    let mut slide_indices = Vec::new();
    for i in 0..archive.len() {
        if let Ok(file) = archive.by_index(i) {
            let name = file.name().to_string();
            if let Some(rest) = name.strip_prefix("ppt/slides/slide") {
                if let Some(num_str) = rest.strip_suffix(".xml") {
                    if let Ok(num) = num_str.parse::<u32>() {
                        slide_indices.push((num, name));
                    }
                }
            }
        }
    }
    slide_indices.sort_by_key(|(num, _)| *num);

    let mut sections = Vec::new();
    let mut slide_pngs = Vec::new();
    let mut thumb_png: Option<Vec<u8>> = None;

    for (idx, (num, path)) in slide_indices.into_iter().enumerate() {
        let rels_path = format!("ppt/slides/_rels/slide{num}.xml.rels");
        let rels = read_zip_string(archive, &rels_path)
            .map(|x| parse_rels_map(&x))
            .unwrap_or_default();

        if let Some(xml) = read_zip_string(archive, &path) {
            let (slide_bg, shapes) = parse_pptx_slide_xml(&xml, &theme);
            let effective_bg = slide_bg.or_else(|| master_bg.clone());

            // Extract section text for outline & copy-text support
            let mut paras: Vec<String> = Vec::new();
            for sh in &shapes {
                for p in &sh.paragraphs {
                    let t: String = p.runs.iter().map(|r| r.text.as_str()).collect();
                    let trimmed = t.trim();
                    if !trimmed.is_empty() {
                        paras.push(trimmed.to_string());
                    }
                }
            }
            if !paras.is_empty() {
                let (raw_title, body_start) = if paras.len() > 1
                    && (paras[0].ends_with('&')
                        || paras[0].ends_with('-')
                        || paras[0].ends_with(':'))
                {
                    (format!("{} {}", paras[0], paras[1]), 2usize)
                } else {
                    (paras[0].clone(), 1usize)
                };
                let title = format!("Slide {num}: {raw_title}");
                let body = if paras.len() > body_start {
                    paras[body_start..]
                        .iter()
                        .map(|p| format!("• {p}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    String::new()
                };
                sections.push(PreviewSection { title, body });
            } else {
                sections.push(PreviewSection {
                    title: format!("Slide {num}"),
                    body: String::new(),
                });
            }

            if idx < max_rendered_slides {
                let svg = render_pptx_slide_svg(
                    slide_w_emu,
                    slide_h_emu,
                    &effective_bg,
                    &shapes,
                    &rels,
                    archive,
                );
                if idx == 0 {
                    thumb_png = render_svg_to_png(&svg, 0.75);
                }
                if let Some(png) = render_svg_to_png(&svg, 1.35) {
                    slide_pngs.push(png);
                }
            }
        }
    }

    (sections, slide_pngs, thumb_png)
}

// ============================================================================
// PART 2: Word (.docx) Layout Mapper (`WMDocumentMapper`)
// ============================================================================

#[derive(Clone, Debug)]
struct DocxStyle {
    based_on: Option<String>,
    font_family: Option<String>,
    font_size_pt: Option<f32>,
    bold: Option<bool>,
    italic: Option<bool>,
    underline: Option<bool>,
    color_hex: Option<String>,
    align: Option<String>, // "left", "center", "right", "both"
    space_before_pt: Option<f32>,
    space_after_pt: Option<f32>,
    line_mult: Option<f32>,
    indent_left_pt: Option<f32>,
    indent_first_line_pt: Option<f32>,
    outline_lvl: Option<u8>,
}

impl Default for DocxStyle {
    fn default() -> Self {
        Self {
            based_on: None,
            font_family: None,
            font_size_pt: None,
            bold: None,
            italic: None,
            underline: None,
            color_hex: None,
            align: None,
            space_before_pt: None,
            space_after_pt: None,
            line_mult: None,
            indent_left_pt: None,
            indent_first_line_pt: None,
            outline_lvl: None,
        }
    }
}

#[derive(Clone, Debug)]
struct ResolvedParaStyle {
    font_family: String,
    font_size_pt: f32,
    bold: bool,
    italic: bool,
    underline: bool,
    color_hex: String,
    align: String,
    space_before_pt: f32,
    space_after_pt: f32,
    line_mult: f32,
    indent_left_pt: f32,
    indent_first_line_pt: f32,
    outline_lvl: Option<u8>,
}

impl Default for ResolvedParaStyle {
    fn default() -> Self {
        Self {
            font_family: "Times New Roman".to_string(),
            font_size_pt: 12.0,
            bold: false,
            italic: false,
            underline: false,
            color_hex: "111827".to_string(),
            align: "left".to_string(),
            space_before_pt: 0.0,
            space_after_pt: 6.0,
            line_mult: 1.25,
            indent_left_pt: 0.0,
            indent_first_line_pt: 0.0,
            outline_lvl: None,
        }
    }
}

fn parse_docx_styles(xml: &str) -> (ResolvedParaStyle, HashMap<String, DocxStyle>) {
    let mut defaults = ResolvedParaStyle::default();
    let mut styles: HashMap<String, DocxStyle> = HashMap::new();

    let mut reader = Reader::from_str(xml);
    let mut in_doc_defaults = false;
    let mut current_id: Option<String> = None;
    let mut current_style = DocxStyle::default();
    let mut in_p_pr = false;
    let mut in_r_pr = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let local = e.local_name();
                match local.as_ref() {
                    b"docDefaults" => in_doc_defaults = true,
                    b"style" => {
                        current_id = attr_val(&e, b"styleId");
                        current_style = DocxStyle::default();
                    }
                    b"pPr" => in_p_pr = true,
                    b"rPr" => in_r_pr = true,
                    _ => apply_docx_style_tag(
                        &e,
                        in_doc_defaults,
                        in_p_pr,
                        in_r_pr,
                        &mut defaults,
                        &mut current_style,
                    ),
                }
            }
            Ok(Event::Empty(e)) => {
                apply_docx_style_tag(
                    &e,
                    in_doc_defaults,
                    in_p_pr,
                    in_r_pr,
                    &mut defaults,
                    &mut current_style,
                );
            }
            Ok(Event::End(e)) => {
                let local = e.local_name();
                match local.as_ref() {
                    b"docDefaults" => in_doc_defaults = false,
                    b"pPr" => in_p_pr = false,
                    b"rPr" => in_r_pr = false,
                    b"style" => {
                        if let Some(id) = current_id.take() {
                            styles.insert(id, current_style.clone());
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }

    (defaults, styles)
}

fn apply_docx_style_tag(
    e: &BytesStart<'_>,
    in_doc_defaults: bool,
    in_p_pr: bool,
    in_r_pr: bool,
    defaults: &mut ResolvedParaStyle,
    st: &mut DocxStyle,
) {
    match e.local_name().as_ref() {
        b"basedOn" => {
            st.based_on = attr_val(e, b"val");
        }
        b"rFonts" if in_r_pr => {
            if let Some(f) = attr_val(e, b"ascii").or_else(|| attr_val(e, b"hAnsi")) {
                if in_doc_defaults {
                    defaults.font_family = f;
                } else {
                    st.font_family = Some(f);
                }
            }
        }
        b"sz" if in_r_pr => {
            if let Some(v) = attr_val(e, b"val").and_then(|s| s.parse::<f32>().ok()) {
                let pt = (v / 2.0).clamp(6.0, 72.0);
                if in_doc_defaults {
                    defaults.font_size_pt = pt;
                } else {
                    st.font_size_pt = Some(pt);
                }
            }
        }
        b"b" if in_r_pr => {
            let val = attr_val(e, b"val")
                .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
                .unwrap_or(true);
            if in_doc_defaults {
                defaults.bold = val;
            } else {
                st.bold = Some(val);
            }
        }
        b"i" if in_r_pr => {
            let val = attr_val(e, b"val")
                .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
                .unwrap_or(true);
            if in_doc_defaults {
                defaults.italic = val;
            } else {
                st.italic = Some(val);
            }
        }
        b"u" if in_r_pr => {
            let val = attr_val(e, b"val")
                .map(|v| v != "none" && v != "0")
                .unwrap_or(true);
            if !in_doc_defaults {
                st.underline = Some(val);
            }
        }
        b"color" if in_r_pr => {
            if let Some(c) = attr_val(e, b"val") {
                if c != "auto" && c.len() == 6 {
                    if in_doc_defaults {
                        defaults.color_hex = c;
                    } else {
                        st.color_hex = Some(c);
                    }
                }
            }
        }
        b"jc" if in_p_pr => {
            if let Some(jc) = attr_val(e, b"val") {
                if in_doc_defaults {
                    defaults.align = jc;
                } else {
                    st.align = Some(jc);
                }
            }
        }
        b"spacing" if in_p_pr => {
            if let Some(bef) = attr_val(e, b"before").and_then(|s| s.parse::<f32>().ok()) {
                let pt = (bef / 20.0).clamp(0.0, 72.0);
                if in_doc_defaults {
                    defaults.space_before_pt = pt;
                } else {
                    st.space_before_pt = Some(pt);
                }
            }
            if let Some(aft) = attr_val(e, b"after").and_then(|s| s.parse::<f32>().ok()) {
                let pt = (aft / 20.0).clamp(0.0, 72.0);
                if in_doc_defaults {
                    defaults.space_after_pt = pt;
                } else {
                    st.space_after_pt = Some(pt);
                }
            }
            if let Some(line) = attr_val(e, b"line").and_then(|s| s.parse::<f32>().ok()) {
                let mult = (line / 240.0).clamp(1.0, 2.5);
                if in_doc_defaults {
                    defaults.line_mult = mult;
                } else {
                    st.line_mult = Some(mult);
                }
            }
        }
        b"ind" if in_p_pr => {
            if let Some(l) = attr_val(e, b"left").and_then(|s| s.parse::<f32>().ok()) {
                st.indent_left_pt = Some((l / 20.0).clamp(0.0, 200.0));
            }
            if let Some(fl) = attr_val(e, b"firstLine").and_then(|s| s.parse::<f32>().ok()) {
                st.indent_first_line_pt = Some((fl / 20.0).clamp(0.0, 120.0));
            }
        }
        b"outlineLvl" if in_p_pr => {
            if let Some(lvl) = attr_val(e, b"val").and_then(|s| s.parse::<u8>().ok()) {
                st.outline_lvl = Some(lvl);
            }
        }
        _ => {}
    }
}

fn resolve_docx_style(
    style_id: Option<&str>,
    defaults: &ResolvedParaStyle,
    styles: &HashMap<String, DocxStyle>,
) -> ResolvedParaStyle {
    let mut chain = Vec::new();
    let mut curr = style_id.map(|s| s.to_string());
    let mut depth = 0;
    while let Some(id) = curr {
        if depth > 8 {
            break;
        }
        depth += 1;
        if let Some(st) = styles.get(&id) {
            chain.push(st);
            curr = st.based_on.clone();
        } else {
            // Built-in heading/title fallbacks if styleId is not explicitly defined
            let mut fallback = defaults.clone();
            let lower = id.to_ascii_lowercase();
            if lower.contains("title") {
                fallback.font_size_pt = 22.0;
                fallback.bold = true;
                fallback.space_after_pt = 10.0;
            } else if lower.contains("heading1") || lower.contains("judul1") {
                fallback.font_size_pt = 18.0;
                fallback.bold = true;
                fallback.space_before_pt = 12.0;
                fallback.space_after_pt = 6.0;
                fallback.outline_lvl = Some(0);
            } else if lower.contains("heading2") || lower.contains("judul2") {
                fallback.font_size_pt = 14.5;
                fallback.bold = true;
                fallback.space_before_pt = 10.0;
                fallback.space_after_pt = 4.0;
                fallback.outline_lvl = Some(1);
            }
            return fallback;
        }
    }

    let mut res = defaults.clone();
    for st in chain.into_iter().rev() {
        if let Some(v) = &st.font_family {
            res.font_family = v.clone();
        }
        if let Some(v) = st.font_size_pt {
            res.font_size_pt = v;
        }
        if let Some(v) = st.bold {
            res.bold = v;
        }
        if let Some(v) = st.italic {
            res.italic = v;
        }
        if let Some(v) = st.underline {
            res.underline = v;
        }
        if let Some(v) = &st.color_hex {
            res.color_hex = v.clone();
        }
        if let Some(v) = &st.align {
            res.align = v.clone();
        }
        if let Some(v) = st.space_before_pt {
            res.space_before_pt = v;
        }
        if let Some(v) = st.space_after_pt {
            res.space_after_pt = v;
        }
        if let Some(v) = st.line_mult {
            res.line_mult = v;
        }
        if let Some(v) = st.indent_left_pt {
            res.indent_left_pt = v;
        }
        if let Some(v) = st.indent_first_line_pt {
            res.indent_first_line_pt = v;
        }
        if let Some(v) = st.outline_lvl {
            res.outline_lvl = Some(v);
        }
    }
    res
}

#[derive(Clone, Debug)]
struct DocxRun {
    text: String,
    font_family: String,
    font_size_pt: f32,
    bold: bool,
    italic: bool,
    underline: bool,
    color_hex: String,
}

#[derive(Clone, Debug)]
struct DocxInlineImage {
    r_id: String,
    w_pt: f32,
    h_pt: f32,
}

#[derive(Clone, Debug)]
struct DocxParagraphBlock {
    style: ResolvedParaStyle,
    runs: Vec<DocxRun>,
    images: Vec<DocxInlineImage>,
    is_list_item: bool,
    page_break_before: bool,
}

#[derive(Clone, Debug)]
struct DocxTableCell {
    bg_hex: Option<String>,
    paragraphs: Vec<DocxParagraphBlock>,
}

#[derive(Clone, Debug)]
struct DocxTableBlock {
    rows: Vec<Vec<DocxTableCell>>,
}

#[derive(Clone, Debug)]
enum DocxBlock {
    Paragraph(DocxParagraphBlock),
    Table(DocxTableBlock),
}

#[derive(Clone, Copy, Debug)]
struct DocxPageSetup {
    w_pt: f32,
    h_pt: f32,
    margin_top_pt: f32,
    margin_bottom_pt: f32,
    margin_left_pt: f32,
    margin_right_pt: f32,
}

impl Default for DocxPageSetup {
    fn default() -> Self {
        Self {
            w_pt: 595.3,
            h_pt: 841.9,
            margin_top_pt: 56.7,
            margin_bottom_pt: 56.7,
            margin_left_pt: 56.7,
            margin_right_pt: 56.7,
        }
    }
}

fn parse_docx_body(
    xml: &str,
    defaults: &ResolvedParaStyle,
    styles: &HashMap<String, DocxStyle>,
) -> (DocxPageSetup, Vec<DocxBlock>) {
    let mut setup = DocxPageSetup::default();
    let mut blocks: Vec<DocxBlock> = Vec::new();

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);

    let mut in_tbl = false;
    let mut current_tbl: Option<DocxTableBlock> = None;
    let mut current_tr: Vec<DocxTableCell> = Vec::new();
    let mut current_tc: Option<DocxTableCell> = None;
    let mut in_tc_pr = false;

    let mut current_p: Option<DocxParagraphBlock> = None;
    let mut p_inline_overrides = DocxStyle::default();
    let mut p_style_id: Option<String> = None;
    let mut in_p_pr = false;
    let mut in_p_r_pr = false;

    let mut current_r: Option<DocxRun> = None;
    let mut r_overrides = DocxStyle::default();
    let mut in_r_pr = false;
    let mut in_t = false;
    let mut pending_page_break = false;

    let mut in_drawing = false;
    let mut drawing_w_pt = 240.0f32;
    let mut drawing_h_pt = 140.0f32;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let local = e.local_name();
                match local.as_ref() {
                    b"tbl" => {
                        in_tbl = true;
                        current_tbl = Some(DocxTableBlock { rows: Vec::new() });
                    }
                    b"tr" if in_tbl => {
                        current_tr.clear();
                    }
                    b"tc" if in_tbl => {
                        current_tc = Some(DocxTableCell {
                            bg_hex: None,
                            paragraphs: Vec::new(),
                        });
                    }
                    b"tcPr" if in_tbl => in_tc_pr = true,
                    b"shd" if in_tc_pr => {
                        if let Some(fill) = attr_val(&e, b"fill") {
                            if fill != "auto" && fill.len() == 6 {
                                if let Some(tc) = &mut current_tc {
                                    tc.bg_hex = Some(fill);
                                }
                            }
                        }
                    }
                    b"p" => {
                        p_style_id = None;
                        p_inline_overrides = DocxStyle::default();
                        current_p = Some(DocxParagraphBlock {
                            style: defaults.clone(),
                            runs: Vec::new(),
                            images: Vec::new(),
                            is_list_item: false,
                            page_break_before: pending_page_break,
                        });
                        pending_page_break = false;
                    }
                    b"pPr" if current_p.is_some() => in_p_pr = true,
                    b"numPr" if in_p_pr => {
                        if let Some(p) = &mut current_p {
                            p.is_list_item = true;
                        }
                    }
                    b"rPr" if in_p_pr => in_p_r_pr = true,
                    b"r" if current_p.is_some() => {
                        r_overrides = DocxStyle::default();
                        current_r = Some(DocxRun {
                            text: String::new(),
                            font_family: defaults.font_family.clone(),
                            font_size_pt: defaults.font_size_pt,
                            bold: defaults.bold,
                            italic: defaults.italic,
                            underline: defaults.underline,
                            color_hex: defaults.color_hex.clone(),
                        });
                    }
                    b"rPr" if current_r.is_some() && !in_p_pr => in_r_pr = true,
                    b"t" if current_r.is_some() => in_t = true,
                    b"drawing" if current_p.is_some() => {
                        in_drawing = true;
                        drawing_w_pt = 240.0;
                        drawing_h_pt = 140.0;
                    }
                    b"extent" if in_drawing => {
                        if let Some(cx) =
                            attr_val(&e, b"cx").and_then(|v| v.parse::<f32>().ok())
                        {
                            drawing_w_pt = (cx / 12700.0).clamp(24.0, 480.0);
                        }
                        if let Some(cy) =
                            attr_val(&e, b"cy").and_then(|v| v.parse::<f32>().ok())
                        {
                            drawing_h_pt = (cy / 12700.0).clamp(16.0, 600.0);
                        }
                    }
                    b"blip" if in_drawing => {
                        if let Some(rid) = attr_val(&e, b"embed") {
                            if let Some(p) = &mut current_p {
                                p.images.push(DocxInlineImage {
                                    r_id: rid,
                                    w_pt: drawing_w_pt,
                                    h_pt: drawing_h_pt,
                                });
                            }
                        }
                    }
                    _ => {
                        handle_docx_inline_tag(
                            &e,
                            in_p_pr,
                            in_p_r_pr,
                            in_r_pr,
                            &mut p_style_id,
                            &mut p_inline_overrides,
                            &mut r_overrides,
                            &mut setup,
                            &mut current_p,
                            &mut pending_page_break,
                        );
                    }
                }
            }
            Ok(Event::Empty(e)) => {
                let local = e.local_name();
                match local.as_ref() {
                    b"shd" if in_tc_pr => {
                        if let Some(fill) = attr_val(&e, b"fill") {
                            if fill != "auto" && fill.len() == 6 {
                                if let Some(tc) = &mut current_tc {
                                    tc.bg_hex = Some(fill);
                                }
                            }
                        }
                    }
                    b"extent" if in_drawing => {
                        if let Some(cx) =
                            attr_val(&e, b"cx").and_then(|v| v.parse::<f32>().ok())
                        {
                            drawing_w_pt = (cx / 12700.0).clamp(24.0, 480.0);
                        }
                        if let Some(cy) =
                            attr_val(&e, b"cy").and_then(|v| v.parse::<f32>().ok())
                        {
                            drawing_h_pt = (cy / 12700.0).clamp(16.0, 600.0);
                        }
                    }
                    b"blip" if in_drawing => {
                        if let Some(rid) = attr_val(&e, b"embed") {
                            if let Some(p) = &mut current_p {
                                p.images.push(DocxInlineImage {
                                    r_id: rid,
                                    w_pt: drawing_w_pt,
                                    h_pt: drawing_h_pt,
                                });
                            }
                        }
                    }
                    b"tab" if current_r.is_some() => {
                        if let Some(r) = &mut current_r {
                            r.text.push_str("    ");
                        }
                    }
                    _ => {
                        handle_docx_inline_tag(
                            &e,
                            in_p_pr,
                            in_p_r_pr,
                            in_r_pr,
                            &mut p_style_id,
                            &mut p_inline_overrides,
                            &mut r_overrides,
                            &mut setup,
                            &mut current_p,
                            &mut pending_page_break,
                        );
                    }
                }
            }
            Ok(Event::Text(e)) if in_t => {
                if let Ok(decoded) = e.unescape() {
                    if let Some(r) = &mut current_r {
                        r.text.push_str(&decoded);
                    }
                }
            }
            Ok(Event::End(e)) => {
                let local = e.local_name();
                match local.as_ref() {
                    b"tcPr" => in_tc_pr = false,
                    b"pPr" => {
                        in_p_pr = false;
                        in_p_r_pr = false;
                        if let Some(p) = &mut current_p {
                            let mut resolved =
                                resolve_docx_style(p_style_id.as_deref(), defaults, styles);
                            apply_style_override(&mut resolved, &p_inline_overrides);
                            p.style = resolved;
                        }
                    }
                    b"rPr" => {
                        if in_p_r_pr {
                            in_p_r_pr = false;
                        } else {
                            in_r_pr = false;
                        }
                    }
                    b"t" => in_t = false,
                    b"drawing" => in_drawing = false,
                    b"r" => {
                        if let (Some(mut r), Some(p)) = (current_r.take(), &mut current_p) {
                            if !r.text.is_empty() {
                                r.font_family = r_overrides
                                    .font_family
                                    .clone()
                                    .unwrap_or_else(|| p.style.font_family.clone());
                                r.font_size_pt =
                                    r_overrides.font_size_pt.unwrap_or(p.style.font_size_pt);
                                r.bold = r_overrides.bold.unwrap_or(p.style.bold);
                                r.italic = r_overrides.italic.unwrap_or(p.style.italic);
                                r.underline = r_overrides.underline.unwrap_or(p.style.underline);
                                r.color_hex = r_overrides
                                    .color_hex
                                    .clone()
                                    .unwrap_or_else(|| p.style.color_hex.clone());
                                p.runs.push(r);
                            }
                        }
                    }
                    b"p" => {
                        if let Some(mut p) = current_p.take() {
                            // Ensure style is resolved even if <w:pPr> was absent
                            if p_style_id.is_some() {
                                let mut resolved =
                                    resolve_docx_style(p_style_id.as_deref(), defaults, styles);
                                apply_style_override(&mut resolved, &p_inline_overrides);
                                p.style = resolved;
                            }
                            if in_tbl {
                                if let Some(tc) = &mut current_tc {
                                    if !p.runs.is_empty() || !p.images.is_empty() {
                                        tc.paragraphs.push(p);
                                    }
                                }
                            } else if !p.runs.is_empty()
                                || !p.images.is_empty()
                                || p.page_break_before
                            {
                                blocks.push(DocxBlock::Paragraph(p));
                            }
                        }
                    }
                    b"tc" if in_tbl => {
                        if let Some(tc) = current_tc.take() {
                            current_tr.push(tc);
                        }
                    }
                    b"tr" if in_tbl => {
                        if !current_tr.is_empty() {
                            if let Some(tbl) = &mut current_tbl {
                                tbl.rows.push(current_tr.clone());
                            }
                            current_tr.clear();
                        }
                    }
                    b"tbl" => {
                        in_tbl = false;
                        if let Some(tbl) = current_tbl.take() {
                            if !tbl.rows.is_empty() {
                                blocks.push(DocxBlock::Table(tbl));
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }

    (setup, blocks)
}

#[allow(clippy::too_many_arguments)]
fn handle_docx_inline_tag(
    e: &BytesStart<'_>,
    in_p_pr: bool,
    in_p_r_pr: bool,
    in_r_pr: bool,
    p_style_id: &mut Option<String>,
    p_inline_overrides: &mut DocxStyle,
    r_overrides: &mut DocxStyle,
    setup: &mut DocxPageSetup,
    current_p: &mut Option<DocxParagraphBlock>,
    pending_page_break: &mut bool,
) {
    match e.local_name().as_ref() {
        b"pStyle" if in_p_pr => {
            *p_style_id = attr_val(e, b"val");
        }
        b"lastRenderedPageBreak" => {
            if let Some(p) = current_p {
                if p.runs.is_empty() {
                    p.page_break_before = true;
                } else {
                    *pending_page_break = true;
                }
            } else {
                *pending_page_break = true;
            }
        }
        b"br" => {
            if attr_val(e, b"type").as_deref() == Some("page") {
                if let Some(p) = current_p {
                    if p.runs.is_empty() {
                        p.page_break_before = true;
                    } else {
                        *pending_page_break = true;
                    }
                } else {
                    *pending_page_break = true;
                }
            }
        }
        b"pgSz" => {
            if let Some(w) = attr_val(e, b"w").and_then(|v| v.parse::<f32>().ok()) {
                setup.w_pt = (w / 20.0).clamp(320.0, 1200.0);
            }
            if let Some(h) = attr_val(e, b"h").and_then(|v| v.parse::<f32>().ok()) {
                setup.h_pt = (h / 20.0).clamp(400.0, 1600.0);
            }
        }
        b"pgMar" => {
            if let Some(t) = attr_val(e, b"top").and_then(|v| v.parse::<f32>().ok()) {
                setup.margin_top_pt = (t / 20.0).clamp(24.0, 144.0);
            }
            if let Some(b) = attr_val(e, b"bottom").and_then(|v| v.parse::<f32>().ok()) {
                setup.margin_bottom_pt = (b / 20.0).clamp(24.0, 144.0);
            }
            if let Some(l) = attr_val(e, b"left").and_then(|v| v.parse::<f32>().ok()) {
                setup.margin_left_pt = (l / 20.0).clamp(24.0, 144.0);
            }
            if let Some(r) = attr_val(e, b"right").and_then(|v| v.parse::<f32>().ok()) {
                setup.margin_right_pt = (r / 20.0).clamp(24.0, 144.0);
            }
        }
        _ => {
            let mut dummy_defaults = ResolvedParaStyle::default();
            if in_r_pr {
                apply_docx_style_tag(e, false, false, true, &mut dummy_defaults, r_overrides);
            } else if in_p_pr {
                apply_docx_style_tag(
                    e,
                    false,
                    !in_p_r_pr,
                    in_p_r_pr,
                    &mut dummy_defaults,
                    p_inline_overrides,
                );
            }
        }
    }
}

fn apply_style_override(res: &mut ResolvedParaStyle, ov: &DocxStyle) {
    if let Some(v) = &ov.font_family {
        res.font_family = v.clone();
    }
    if let Some(v) = ov.font_size_pt {
        res.font_size_pt = v;
    }
    if let Some(v) = ov.bold {
        res.bold = v;
    }
    if let Some(v) = ov.italic {
        res.italic = v;
    }
    if let Some(v) = ov.underline {
        res.underline = v;
    }
    if let Some(v) = &ov.color_hex {
        res.color_hex = v.clone();
    }
    if let Some(v) = &ov.align {
        res.align = v.clone();
    }
    if let Some(v) = ov.space_before_pt {
        res.space_before_pt = v;
    }
    if let Some(v) = ov.space_after_pt {
        res.space_after_pt = v;
    }
    if let Some(v) = ov.line_mult {
        res.line_mult = v;
    }
    if let Some(v) = ov.indent_left_pt {
        res.indent_left_pt = v;
    }
    if let Some(v) = ov.indent_first_line_pt {
        res.indent_first_line_pt = v;
    }
    if let Some(v) = ov.outline_lvl {
        res.outline_lvl = Some(v);
    }
}

#[derive(Clone)]
struct DocxWordToken {
    text: String,
    font_family: &'static str,
    font_size_pt: f32,
    bold: bool,
    italic: bool,
    color_hex: String,
    width_pt: f32,
    space_width_pt: f32,
}

#[derive(Clone)]
struct DocxWrappedLine {
    x_pt: f32,
    avail_w_pt: f32,
    line_h_pt: f32,
    font_size_pt: f32,
    align: String,
    is_last_in_para: bool,
    tokens: Vec<DocxWordToken>,
}

fn wrap_docx_paragraph(
    p: &DocxParagraphBlock,
    base_x: f32,
    max_w: f32,
) -> Vec<DocxWrappedLine> {
    let mut words: Vec<DocxWordToken> = Vec::new();
    if p.is_list_item {
        let fam = map_font_family(&p.style.font_family);
        let is_serif = fam.starts_with("Times");
        let sz = p.style.font_size_pt;
        words.push(DocxWordToken {
            text: "•".to_string(),
            font_family: fam,
            font_size_pt: sz,
            bold: true,
            italic: false,
            color_hex: p.style.color_hex.clone(),
            width_pt: estimate_text_width("•", sz, is_serif, true),
            space_width_pt: estimate_char_width_factor(' ', is_serif, false) * sz * 1.5,
        });
    }

    for run in &p.runs {
        let fam = map_font_family(&run.font_family);
        let is_serif = fam.starts_with("Times");
        let sz = run.font_size_pt;
        let space_w = estimate_char_width_factor(' ', is_serif, run.bold) * sz;
        for w_str in run.text.split_whitespace() {
            let w_pt = estimate_text_width(w_str, sz, is_serif, run.bold);
            let is_trailing_punct = !words.is_empty()
                && w_str
                    .chars()
                    .all(|c| matches!(c, ',' | '.' | ';' | ':' | '!' | '?' | ')'));
            if is_trailing_punct {
                if let Some(last) = words.last_mut() {
                    last.text.push_str(w_str);
                    last.width_pt += w_pt;
                    continue;
                }
            }
            words.push(DocxWordToken {
                text: w_str.to_string(),
                font_family: fam,
                font_size_pt: sz,
                bold: run.bold,
                italic: run.italic,
                color_hex: run.color_hex.clone(),
                width_pt: w_pt,
                space_width_pt: space_w,
            });
        }
    }

    if words.is_empty() {
        return Vec::new();
    }

    let indent_left = p.style.indent_left_pt + if p.is_list_item { 14.0 } else { 0.0 };
    let first_indent = if p.is_list_item {
        0.0
    } else {
        p.style.indent_first_line_pt
    };

    let mut lines: Vec<DocxWrappedLine> = Vec::new();
    let mut current_tokens: Vec<DocxWordToken> = Vec::new();
    let mut current_w = 0.0f32;
    let mut is_first_line = true;

    for tok in words {
        let line_x = base_x + indent_left + if is_first_line { first_indent } else { 0.0 };
        let line_avail = (max_w - indent_left - if is_first_line { first_indent } else { 0.0 })
            .max(40.0);

        let added_w = if current_tokens.is_empty() {
            tok.width_pt
        } else {
            tok.space_width_pt + tok.width_pt
        };

        if !current_tokens.is_empty() && current_w + added_w > line_avail {
            let max_sz = current_tokens
                .iter()
                .map(|t| t.font_size_pt)
                .fold(p.style.font_size_pt, f32::max);
            lines.push(DocxWrappedLine {
                x_pt: line_x,
                avail_w_pt: line_avail,
                line_h_pt: max_sz * p.style.line_mult,
                font_size_pt: max_sz,
                align: p.style.align.clone(),
                is_last_in_para: false,
                tokens: current_tokens,
            });
            current_tokens = vec![tok.clone()];
            current_w = tok.width_pt;
            is_first_line = false;
        } else {
            current_w += added_w;
            current_tokens.push(tok);
        }
    }

    if !current_tokens.is_empty() {
        let line_x = base_x + indent_left + if is_first_line { first_indent } else { 0.0 };
        let line_avail = (max_w - indent_left - if is_first_line { first_indent } else { 0.0 })
            .max(40.0);
        let max_sz = current_tokens
            .iter()
            .map(|t| t.font_size_pt)
            .fold(p.style.font_size_pt, f32::max);
        lines.push(DocxWrappedLine {
            x_pt: line_x,
            avail_w_pt: line_avail,
            line_h_pt: max_sz * p.style.line_mult,
            font_size_pt: max_sz,
            align: p.style.align.clone(),
            is_last_in_para: true,
            tokens: current_tokens,
        });
    }

    lines
}

fn emit_docx_line_svg(svg_body: &mut String, line: &DocxWrappedLine, baseline_y: f32) {
    if line.tokens.is_empty() {
        return;
    }

    let words_w: f32 = line.tokens.iter().map(|t| t.width_pt).sum();
    let spaces_count = line.tokens.len().saturating_sub(1);
    let natural_spaces_w: f32 = line
        .tokens
        .iter()
        .take(spaces_count)
        .map(|t| t.space_width_pt)
        .sum();
    let natural_total_w = words_w + natural_spaces_w;

    // Determine if all tokens share identical styling
    let first = &line.tokens[0];
    let uniform_style = line.tokens.iter().all(|t| {
        t.font_family == first.font_family
            && (t.font_size_pt - first.font_size_pt).abs() < 0.1
            && t.bold == first.bold
            && t.italic == first.italic
            && t.color_hex == first.color_hex
    });

    if uniform_style {
        let joined: String = line
            .tokens
            .iter()
            .map(|t| t.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let weight = if first.bold { "bold" } else { "normal" };
        let style = if first.italic { "italic" } else { "normal" };
        let (tx, anchor, extra_attr) = match line.align.as_str() {
            "center" => (line.x_pt + line.avail_w_pt * 0.5, "middle", String::new()),
            "right" => (line.x_pt + line.avail_w_pt, "end", String::new()),
            "both" if !line.is_last_in_para && spaces_count >= 2 => {
                let extra_per_space =
                    ((line.avail_w_pt - natural_total_w) / spaces_count as f32).clamp(-0.5, 10.0);
                (
                    line.x_pt,
                    "start",
                    format!(r#" word-spacing="{extra_per_space:.2}""#),
                )
            }
            _ => (line.x_pt, "start", String::new()),
        };
        svg_body.push_str(&format!(
            r##"<text x="{tx:.2}" y="{baseline_y:.2}" font-family="{}" font-size="{:.2}" font-weight="{weight}" font-style="{style}" fill="#{}" text-anchor="{anchor}"{extra_attr}>{}</text>"##,
            first.font_family,
            first.font_size_pt,
            first.color_hex,
            xml_escape(&joined)
        ));
    } else {
        // Mixed inline runs (e.g., regular Times New Roman + italic "cloud" + bold label)
        let extra_space = if line.align == "both" && !line.is_last_in_para && spaces_count >= 2 {
            ((line.avail_w_pt - natural_total_w) / spaces_count as f32).clamp(0.0, 8.0)
        } else {
            0.0
        };
        let mut cursor_x = match line.align.as_str() {
            "center" => line.x_pt + ((line.avail_w_pt - natural_total_w) * 0.5).max(0.0),
            "right" => line.x_pt + (line.avail_w_pt - natural_total_w).max(0.0),
            _ => line.x_pt,
        };
        for (idx, tok) in line.tokens.iter().enumerate() {
            let weight = if tok.bold { "bold" } else { "normal" };
            let style = if tok.italic { "italic" } else { "normal" };
            svg_body.push_str(&format!(
                r##"<text x="{cursor_x:.2}" y="{baseline_y:.2}" font-family="{}" font-size="{:.2}" font-weight="{weight}" font-style="{style}" fill="#{}">{}</text>"##,
                tok.font_family,
                tok.font_size_pt,
                tok.color_hex,
                xml_escape(&tok.text)
            ));
            cursor_x += tok.width_pt;
            if idx < spaces_count {
                cursor_x += tok.space_width_pt + extra_space;
            }
        }
    }
}

pub fn render_docx_document(
    archive: &mut ZipArchive<Cursor<&[u8]>>,
    max_rendered_pages: usize,
) -> (Vec<PreviewSection>, Vec<Vec<u8>>, Option<Vec<u8>>, usize) {
    let styles_xml = read_zip_string(archive, "word/styles.xml").unwrap_or_default();
    let (defaults, styles) = parse_docx_styles(&styles_xml);
    let rels = read_zip_string(archive, "word/_rels/document.xml.rels")
        .map(|x| parse_rels_map(&x))
        .unwrap_or_default();

    let doc_xml = read_zip_string(archive, "word/document.xml")
        .or_else(|| read_zip_string(archive, "content.xml"))
        .unwrap_or_default();
    let (setup, blocks) = parse_docx_body(&doc_xml, &defaults, &styles);

    let page_w = setup.w_pt;
    let page_h = setup.h_pt;
    let content_x = setup.margin_left_pt;
    let content_w = (page_w - setup.margin_left_pt - setup.margin_right_pt).max(160.0);
    let top_y = setup.margin_top_pt;
    let bottom_limit_y = (page_h - setup.margin_bottom_pt).max(top_y + 120.0);

    let mut page_svgs: Vec<String> = Vec::new();
    let mut page_texts: Vec<Vec<String>> = Vec::new();
    let mut current_svg = String::new();
    let mut current_texts: Vec<String> = Vec::new();
    let mut cursor_y = top_y;
    let mut total_paragraphs = 0usize;

    let start_new_page = |svg: &mut String| {
        svg.clear();
        svg.push_str(&format!(
            r##"<rect x="0" y="0" width="{page_w:.2}" height="{page_h:.2}" fill="#FFFFFF"/>"##
        ));
    };
    start_new_page(&mut current_svg);

    let flush_page = |svg: &mut String,
                      texts: &mut Vec<String>,
                      page_svgs: &mut Vec<String>,
                      page_texts: &mut Vec<Vec<String>>| {
        let full_svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {page_w:.2} {page_h:.2}" width="{page_w:.2}" height="{page_h:.2}">{svg}</svg>"#
        );
        page_svgs.push(full_svg);
        page_texts.push(std::mem::take(texts));
    };

    for block in blocks {
        match block {
            DocxBlock::Paragraph(p) => {
                if p.page_break_before && (cursor_y > top_y + 4.0 || !current_texts.is_empty()) {
                    flush_page(
                        &mut current_svg,
                        &mut current_texts,
                        &mut page_svgs,
                        &mut page_texts,
                    );
                    start_new_page(&mut current_svg);
                    cursor_y = top_y;
                }

                let para_text: String = p
                    .runs
                    .iter()
                    .map(|r| r.text.as_str())
                    .collect::<Vec<_>>()
                    .join("");
                let trimmed = para_text.trim();
                if !trimmed.is_empty() {
                    total_paragraphs += 1;
                    current_texts.push(trimmed.to_string());
                }

                // Render inline images (<w:drawing>) if present
                for img in &p.images {
                    let scale = (content_w / img.w_pt).min(1.0);
                    let draw_w = img.w_pt * scale;
                    let draw_h = img.h_pt * scale;
                    if cursor_y + draw_h > bottom_limit_y && cursor_y > top_y + 10.0 {
                        flush_page(
                            &mut current_svg,
                            &mut current_texts,
                            &mut page_svgs,
                            &mut page_texts,
                        );
                        start_new_page(&mut current_svg);
                        cursor_y = top_y;
                    }
                    if let Some(target) = rels.get(&img.r_id) {
                        let zip_path = resolve_zip_rel_path("word", target);
                        if let Some(bytes) = read_zip_bytes(archive, &zip_path) {
                            if let Some(uri) = image_bytes_to_data_uri(&bytes) {
                                let img_x = match p.style.align.as_str() {
                                    "center" => content_x + (content_w - draw_w) * 0.5,
                                    "right" => content_x + content_w - draw_w,
                                    _ => content_x,
                                };
                                current_svg.push_str(&format!(
                                    r#"<image x="{img_x:.2}" y="{cursor_y:.2}" width="{draw_w:.2}" height="{draw_h:.2}" href="{uri}"/>"#
                                ));
                                cursor_y += draw_h + 8.0;
                            }
                        }
                    }
                }

                let wrapped = wrap_docx_paragraph(&p, content_x, content_w);
                if wrapped.is_empty() {
                    continue;
                }

                cursor_y += p.style.space_before_pt;
                for line in wrapped {
                    if cursor_y + line.line_h_pt > bottom_limit_y && cursor_y > top_y + 10.0 {
                        flush_page(
                            &mut current_svg,
                            &mut current_texts,
                            &mut page_svgs,
                            &mut page_texts,
                        );
                        start_new_page(&mut current_svg);
                        cursor_y = top_y;
                    }
                    let baseline_y = cursor_y + line.font_size_pt * 0.82;
                    emit_docx_line_svg(&mut current_svg, &line, baseline_y);
                    cursor_y += line.line_h_pt;
                }
                cursor_y += p.style.space_after_pt;
            }
            DocxBlock::Table(tbl) => {
                let col_count = tbl.rows.iter().map(|r| r.len()).max().unwrap_or(0);
                if col_count == 0 {
                    continue;
                }
                let col_w = content_w / col_count as f32;
                let pad_x = 5.0f32;
                let pad_y = 4.0f32;

                cursor_y += 4.0;
                for row in &tbl.rows {
                    let mut cell_wrapped: Vec<Vec<DocxWrappedLine>> = Vec::with_capacity(col_count);
                    let mut row_h = 18.0f32;

                    for (c_idx, cell) in row.iter().enumerate() {
                        let cell_x = content_x + c_idx as f32 * col_w;
                        let mut c_lines = Vec::new();
                        for cp in &cell.paragraphs {
                            let t: String = cp.runs.iter().map(|r| r.text.as_str()).collect();
                            if !t.trim().is_empty() {
                                current_texts.push(t.trim().to_string());
                            }
                            let mut wl =
                                wrap_docx_paragraph(cp, cell_x + pad_x, (col_w - pad_x * 2.0).max(20.0));
                            c_lines.append(&mut wl);
                        }
                        let ch: f32 =
                            c_lines.iter().map(|l| l.line_h_pt).sum::<f32>() + pad_y * 2.0;
                        if ch > row_h {
                            row_h = ch;
                        }
                        cell_wrapped.push(c_lines);
                    }

                    if cursor_y + row_h > bottom_limit_y && cursor_y > top_y + 10.0 {
                        flush_page(
                            &mut current_svg,
                            &mut current_texts,
                            &mut page_svgs,
                            &mut page_texts,
                        );
                        start_new_page(&mut current_svg);
                        cursor_y = top_y;
                    }

                    for (c_idx, cell) in row.iter().enumerate() {
                        let cell_x = content_x + c_idx as f32 * col_w;
                        let fill_hex = cell.bg_hex.as_deref().unwrap_or("FFFFFF");
                        current_svg.push_str(&format!(
                            r##"<rect x="{cell_x:.2}" y="{cursor_y:.2}" width="{col_w:.2}" height="{row_h:.2}" fill="#{fill_hex}" stroke="#CBD5E1" stroke-width="0.75"/>"##
                        ));
                        let mut ly = cursor_y + pad_y;
                        if let Some(lines) = cell_wrapped.get(c_idx) {
                            for line in lines {
                                let by = ly + line.font_size_pt * 0.82;
                                emit_docx_line_svg(&mut current_svg, line, by);
                                ly += line.line_h_pt;
                            }
                        }
                    }
                    cursor_y += row_h;
                }
                cursor_y += 8.0;
            }
        }
    }

    if cursor_y > top_y + 1.0 || page_svgs.is_empty() {
        flush_page(
            &mut current_svg,
            &mut current_texts,
            &mut page_svgs,
            &mut page_texts,
        );
    }

    let mut sections = Vec::new();
    let mut page_pngs = Vec::new();
    let mut thumb_png: Option<Vec<u8>> = None;

    for (idx, (svg, texts)) in page_svgs.into_iter().zip(page_texts.into_iter()).enumerate() {
        let heading = texts.first().map(|h| {
            if h.chars().count() > 46 {
                let s: String = h.chars().take(46).collect();
                format!("{s}…")
            } else {
                h.clone()
            }
        });
        let title = match heading {
            Some(h) if !h.is_empty() => format!("Halaman {} • {h}", idx + 1),
            _ => format!("Halaman {}", idx + 1),
        };
        sections.push(PreviewSection {
            title,
            body: texts.join("\n\n"),
        });

        if idx < max_rendered_pages {
            let target_scale = (900.0 / page_w).clamp(1.2, 2.2);
            if idx == 0 {
                let thumb_scale = (520.0 / page_w).clamp(0.8, 1.4);
                thumb_png = render_svg_to_png(&svg, thumb_scale);
            }
            if let Some(png) = render_svg_to_png(&svg, target_scale) {
                page_pngs.push(png);
            }
        }
    }

    (sections, page_pngs, thumb_png, total_paragraphs)
}

// ============================================================================
// PART 3: Spreadsheet (.xlsx & .csv) Layout Mapper (`EMWorkbookMapper`)
// ============================================================================

pub fn render_sheet_grid_png(sheet: &PreviewSheet, target_scale: f32) -> Option<Vec<u8>> {
    let mut all_rows: Vec<&Vec<String>> = Vec::new();
    if !sheet.headers.is_empty() {
        all_rows.push(&sheet.headers);
    }
    for r in sheet.rows.iter().take(28) {
        all_rows.push(r);
    }
    if all_rows.is_empty() {
        return None;
    }

    let col_count = all_rows
        .iter()
        .map(|r| r.len())
        .max()
        .unwrap_or(0)
        .clamp(1, 10);

    let row_num_w = 34.0f32;
    let mut col_widths = vec![85.0f32; col_count];
    for c in 0..col_count {
        let max_chars = all_rows
            .iter()
            .filter_map(|r| r.get(c))
            .map(|s| s.chars().count())
            .max()
            .unwrap_or(6)
            .clamp(6, 28);
        col_widths[c] = (max_chars as f32 * 6.8 + 18.0).clamp(68.0, 210.0);
    }

    let total_w: f32 = row_num_w + col_widths.iter().sum::<f32>();
    let header_bar_h = 28.0f32;
    let col_letter_h = 22.0f32;
    let row_h = 22.0f32;
    let total_h = header_bar_h + col_letter_h + (all_rows.len() as f32) * row_h;

    let mut body = String::new();
    body.push_str(&format!(
        r##"<rect x="0" y="0" width="{total_w:.2}" height="{total_h:.2}" fill="#FFFFFF"/>"##
    ));

    // Top sheet title bar
    body.push_str(&format!(
        r##"<rect x="0" y="0" width="{total_w:.2}" height="{header_bar_h:.2}" fill="#107C41"/>"##
    ));
    body.push_str(&format!(
        r##"<text x="12" y="18.5" font-family="Arial, sans-serif" font-size="11.5" font-weight="bold" fill="#FFFFFF">Sheet: {} ({} baris)</text>"##,
        xml_escape(&sheet.name),
        sheet.total_rows
    ));

    // Column letters row (A, B, C...)
    let y_letters = header_bar_h;
    body.push_str(&format!(
        r##"<rect x="0" y="{y_letters:.2}" width="{total_w:.2}" height="{col_letter_h:.2}" fill="#F1F5F9" stroke="#CBD5E1" stroke-width="0.75"/>"##
    ));
    let mut cx = row_num_w;
    for (c_idx, &cw) in col_widths.iter().enumerate() {
        let letter = (b'A' + (c_idx as u8).min(25)) as char;
        body.push_str(&format!(
            r##"<line x1="{cx:.2}" y1="{y_letters:.2}" x2="{cx:.2}" y2="{total_h:.2}" stroke="#E2E8F0" stroke-width="0.75"/>"##
        ));
        body.push_str(&format!(
            r##"<text x="{:.2}" y="{:.2}" font-family="Arial, sans-serif" font-size="10" font-weight="bold" fill="#475569" text-anchor="middle">{letter}</text>"##,
            cx + cw * 0.5,
            y_letters + 15.0
        ));
        cx += cw;
    }

    // Data rows
    for (r_idx, row) in all_rows.iter().enumerate() {
        let ry = header_bar_h + col_letter_h + r_idx as f32 * row_h;
        let is_header = r_idx == 0 && !sheet.headers.is_empty();
        let bg = if is_header {
            "#E8F5E9"
        } else if r_idx % 2 == 1 {
            "#FAFAFA"
        } else {
            "#FFFFFF"
        };
        body.push_str(&format!(
            r##"<rect x="0" y="{ry:.2}" width="{total_w:.2}" height="{row_h:.2}" fill="{bg}"/>"##
        ));
        // Row number cell
        body.push_str(&format!(
            r##"<rect x="0" y="{ry:.2}" width="{row_num_w:.2}" height="{row_h:.2}" fill="#F8FAFC" stroke="#E2E8F0" stroke-width="0.6"/>"##
        ));
        body.push_str(&format!(
            r##"<text x="{:.2}" y="{:.2}" font-family="Arial, sans-serif" font-size="9.5" fill="#64748B" text-anchor="middle">{}</text>"##,
            row_num_w * 0.5,
            ry + 14.5,
            r_idx + 1
        ));

        let mut cell_x = row_num_w;
        for (c_idx, &cw) in col_widths.iter().enumerate() {
            body.push_str(&format!(
                r##"<rect x="{cell_x:.2}" y="{ry:.2}" width="{cw:.2}" height="{row_h:.2}" fill="none" stroke="#E2E8F0" stroke-width="0.6"/>"##
            ));
            if let Some(val) = row.get(c_idx) {
                let trimmed = val.trim();
                if !trimmed.is_empty() {
                    let max_chars = ((cw - 12.0) / 6.2).max(4.0) as usize;
                    let display = if trimmed.chars().count() > max_chars {
                        let s: String = trimmed.chars().take(max_chars.saturating_sub(1)).collect();
                        format!("{s}…")
                    } else {
                        trimmed.to_string()
                    };
                    let is_num = !is_header
                        && trimmed.chars().all(|c| {
                            c.is_ascii_digit() || matches!(c, '.' | ',' | '-' | '%' | 'R' | 'p' | '$')
                        });
                    let (tx, anchor) = if is_num {
                        (cell_x + cw - 6.0, "end")
                    } else {
                        (cell_x + 6.0, "start")
                    };
                    let weight = if is_header { "bold" } else { "normal" };
                    let color = if is_header { "#0F172A" } else { "#1E293B" };
                    body.push_str(&format!(
                        r##"<text x="{tx:.2}" y="{:.2}" font-family="Arial, sans-serif" font-size="10" font-weight="{weight}" fill="{color}" text-anchor="{anchor}">{}</text>"##,
                        ry + 14.8,
                        xml_escape(&display)
                    ));
                }
            }
            cell_x += cw;
        }
    }

    let svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {total_w:.2} {total_h:.2}" width="{total_w:.2}" height="{total_h:.2}">{body}</svg>"#
    );
    render_svg_to_png(&svg, target_scale)
}
