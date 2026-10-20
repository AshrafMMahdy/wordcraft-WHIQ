//! WHIQ: Markdown import that maps the platform's document shapes faithfully, and themes.
//!
//! `file.importMarkdown` builds a new document from Markdown (the CommonMark/GFM importer in
//! `wordcraft_formats::markdown`), adding what the platform's documents use:
//!
//! - front matter (`---` … `---`, `key: value` lines): `title`, `subtitle`, `date`, `author` make
//!   a cover block (Title, Subtitle, date and author, then a page break; the cover page has no
//!   header or footer); `header` and `footer` texts with `{page}`, `{pages}`, `{title}`, `{date}`
//!   (the footer defaults to "Page {page} of {pages}"; `footer: none` leaves it out); `toc: true`;
//!   theme keys (`font`, `heading_font`, `primary_color`, `accent_color`, `logo`);
//! - page breaks: `\pagebreak`, `\newpage`, `<!-- pagebreak -->`, `[pagebreak]`, `---pagebreak---`
//!   or an HTML element with a `page-break-*` / `break-*` style;
//! - a table of contents (a TOC field with its entries): `[TOC]`, `[[TOC]]`, `[[_TOC_]]`, `{{toc}}`,
//!   `<!-- toc -->`;
//! - callouts / admonitions: GitHub alerts (`> [!NOTE]`, TIP, IMPORTANT, WARNING, CAUTION),
//!   MkDocs (`!!! note "Title"` with an indented body) and fenced (`:::warning Title` … `:::`),
//!   as a shaded, left-bordered block with its title;
//! - images by path (`![alt](figures/chart.png)`), read inside the read root and embedded;
//! - table header rows shaded (theme colour), bold, repeated on each page; code blocks shaded;
//! - paragraphs that start with Arabic or Hebrew letters are right to left.
//!
//! Instead of Markdown, `blocks` gives the document as structured blocks whose text is taken
//! literally (user text, verbatim: no Markdown is read, leading spaces and line breaks stay):
//! paragraphs with an optional style or heading level, runs, tables of cells, page breaks. The
//! cover, header, footer, contents and theme apply to them as to Markdown.
//!
//! `styles.applyTheme` applies a theme to any document as one style batch: body and heading
//! fonts, text, heading and accent colours, table header colours, and a logo in the header.

use std::sync::Arc;

use serde_json::{Value, json};
use wordcraft_doc::para::InlineObject;
use wordcraft_doc::props::{Align, Border, BorderStyle, Borders, CharProps, Rgb, TextColor};
use wordcraft_doc::{Block, Blocks, Document, Paragraph, PartKind, Pos, StoryRef, para_block};
use wordcraft_formats::model::{Cell as FCell, FBlock, FTable, Flow, Fmt, Kind as FKind, Para as FPara};

use crate::{CmdError, CmdResult, CommandSpec, Selection, Session, p};

const IMPORT_PARAMS: &str = r#"{"markdown"?: string, "path"?: string (a .md file; images are read relative to it), "blocks"?: [Block] (instead of Markdown: text taken literally), "theme"?: Theme, "toc"?: bool, "cover"?: bool (default: when front matter has a title), "header"?: string, "footer"?: string ("none" for none; {page} {pages} {title} {date}), "title"?, "subtitle"?, "date"?, "author"?} with Theme = {"fonts"?: {"body"?, "heading"?}, "colors"?: {"text"?, "primary"?, "accent"?, "tableHeader"?, "tableHeaderText"?}, "logo"?: path, "size"?: pt}, Block = string (a paragraph) | {"type": "paragraph", "text"?: string, "runs"?: [string | {"text", "bold"?, "italic"?, "underline"?, "strike"?, "color"?, "size"?, "font"?}], "style"?: name, "heading"?: 1-6, "bold"?…} | {"type": "heading", "level"?: 1-6, "text"} | {"type": "table", "rows": [[string | paragraph]], "header"?: bool (default true: the first row)} | {"type": "page_break"}"#;

pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec::new("file.importMarkdown", "Import Markdown", "Agents", import).params(IMPORT_PARAMS),
        CommandSpec::new("styles.applyTheme", "Apply Theme", "Agents", |s, v| {
            let theme = Theme::from_json(v.get("theme").unwrap_or(v));
            let applied = apply_theme(s, &theme)?;
            Ok(json!({"applied": applied}))
        })
        .params(r#"{"theme": Theme (see file.importMarkdown)} → {applied: [what changed]}"#),
    ]
}

/// A marker the preprocessor leaves in the text for the post-processor (never in real text).
const MARK: char = '\u{E0FF}';

fn marker(kind: &str, arg: &str) -> String {
    let hex: String = arg.bytes().map(|b| format!("{b:02x}")).collect();
    format!("{MARK}WHIQ:{kind}:{hex}{MARK}")
}

fn parse_marker(text: &str) -> Option<(String, String)> {
    let t = text.trim().strip_prefix(MARK)?.strip_suffix(MARK)?.strip_prefix("WHIQ:")?;
    let (kind, hex) = t.split_once(':')?;
    let bytes: Option<Vec<u8>> = (0..hex.len() / 2).map(|i| hex.get(i * 2..i * 2 + 2).and_then(|h| u8::from_str_radix(h, 16).ok())).collect();
    Some((kind.to_string(), String::from_utf8(bytes?).ok()?))
}

/// The theme: fonts, colours and a logo.
#[derive(Clone, Debug, Default)]
pub struct Theme {
    pub body_font: Option<String>,
    pub heading_font: Option<String>,
    pub size: Option<f32>,
    pub text: Option<Rgb>,
    pub primary: Option<Rgb>,
    pub accent: Option<Rgb>,
    pub table_header: Option<Rgb>,
    pub table_header_text: Option<Rgb>,
    pub logo: Option<String>,
}

impl Theme {
    /// From `{"fonts": {"body", "heading"}, "colors": {…}, "logo", "size"}`, with flat and
    /// snake/camel spellings accepted.
    pub fn from_json(v: &Value) -> Theme {
        let get = |paths: &[&[&str]]| -> Option<String> {
            paths.iter().find_map(|path| {
                let mut cur = v;
                for k in *path {
                    cur = cur.get(*k)?;
                }
                cur.as_str().map(str::to_string)
            })
        };
        let color = |paths: &[&[&str]]| get(paths).and_then(|c| Rgb::parse(c.trim().trim_start_matches('#')));
        Theme {
            body_font: get(&[&["fonts", "body"], &["font"], &["body_font"], &["bodyFont"]]),
            heading_font: get(&[&["fonts", "heading"], &["fonts", "headings"], &["heading_font"], &["headingFont"]]),
            size: v.get("size").or_else(|| v.get("font_size")).and_then(Value::as_f64).map(|x| x as f32).filter(|x| (4.0..=96.0).contains(x)),
            text: color(&[&["colors", "text"], &["text_color"], &["textColor"]]),
            primary: color(&[&["colors", "primary"], &["primary_color"], &["primaryColor"], &["primary"]]),
            accent: color(&[&["colors", "accent"], &["accent_color"], &["accentColor"], &["accent"]]),
            table_header: color(&[&["colors", "tableHeader"], &["colors", "table_header"], &["table_header"]]),
            table_header_text: color(&[&["colors", "tableHeaderText"], &["colors", "table_header_text"], &["table_header_text"]]),
            logo: get(&[&["logo"], &["logo_path"], &["logoPath"]]),
        }
    }

    fn is_empty(&self) -> bool {
        self.body_font.is_none()
            && self.heading_font.is_none()
            && self.size.is_none()
            && self.text.is_none()
            && self.primary.is_none()
            && self.accent.is_none()
            && self.table_header.is_none()
            && self.table_header_text.is_none()
            && self.logo.is_none()
    }

    fn header_fill(&self) -> Rgb {
        self.table_header.or(self.primary).unwrap_or(Rgb(0xD9, 0xE2, 0xF3))
    }

    fn header_text(&self) -> Option<Rgb> {
        self.table_header_text.or_else(|| {
            let Rgb(r, g, b) = self.header_fill();
            let lum = 0.299 * f32::from(r) + 0.587 * f32::from(g) + 0.114 * f32::from(b);
            (lum < 140.0).then_some(Rgb(0xFF, 0xFF, 0xFF))
        })
    }
}

/// Front matter: `key: value` lines between `---` lines at the very start.
fn front_matter(text: &str) -> (Vec<(String, String)>, &str) {
    let Some(rest) = text.strip_prefix("---\n").or_else(|| text.strip_prefix("---\r\n")) else { return (Vec::new(), text) };
    let mut keys = Vec::new();
    let mut at = 0;
    for line in rest.split_inclusive('\n') {
        at += line.len();
        let l = line.trim_end();
        if l == "---" || l == "..." {
            return (keys, rest.get(at..).unwrap_or(""));
        }
        if let Some((k, v)) = l.split_once(':') {
            let v = v.trim().trim_matches('"').trim_matches('\'').to_string();
            keys.push((k.trim().to_ascii_lowercase().replace('-', "_"), v));
        }
        if keys.len() > 200 {
            break;
        }
    }
    // No closing line: not front matter.
    (Vec::new(), text)
}

fn is_page_break(t: &str) -> bool {
    let l = t.to_ascii_lowercase();
    matches!(
        l.as_str(),
        "\\pagebreak"
            | "\\newpage"
            | "\\clearpage"
            | "<!-- pagebreak -->"
            | "<!--pagebreak-->"
            | "<!-- page break -->"
            | "[pagebreak]"
            | "---pagebreak---"
            | "<pagebreak/>"
            | "<pagebreak />"
    ) || (l.starts_with('<') && (l.contains("page-break-") || l.contains("break-before: page") || l.contains("break-after: page")))
}

fn is_toc(t: &str) -> bool {
    matches!(t.to_ascii_lowercase().as_str(), "[toc]" | "[[toc]]" | "[[_toc_]]" | "{{toc}}" | "<!-- toc -->" | "<!--toc-->" | "${toc}")
}

const CALLOUTS: &[&str] = &[
    "note",
    "tip",
    "important",
    "warning",
    "caution",
    "info",
    "danger",
    "success",
    "example",
    "quote",
    "abstract",
    "hint",
    "attention",
    "error",
    "bug",
    "question",
    "summary",
    "todo",
];

/// A GitHub alert's first line: `> [!KIND] optional title`.
fn gh_alert(t: &str) -> Option<(String, String)> {
    let rest = t.strip_prefix('>')?.trim_start().strip_prefix("[!")?;
    let (kind, title) = rest.split_once(']')?;
    let kind = kind.trim().to_ascii_lowercase();
    CALLOUTS.contains(&kind.as_str()).then(|| (kind, title.trim().to_string()))
}

/// An MkDocs admonition's first line: `!!! kind "Title"` (or `???`, collapsible).
fn mkdocs(t: &str) -> Option<(String, String)> {
    let rest = t.strip_prefix("!!!").or_else(|| t.strip_prefix("???+")).or_else(|| t.strip_prefix("???"))?.trim();
    let (kind, title) = rest.split_once(' ').unwrap_or((rest, ""));
    let kind = kind.trim().to_ascii_lowercase();
    (!kind.is_empty() && kind.chars().all(|c| c.is_ascii_alphabetic())).then(|| (kind, title.trim().trim_matches('"').to_string()))
}

/// A fenced admonition's first line: `:::kind Title`.
fn fenced(t: &str) -> Option<(String, String)> {
    let rest = t.strip_prefix(":::")?.trim();
    if rest.is_empty() {
        return None;
    }
    let (kind, title) = rest.split_once(' ').unwrap_or((rest, ""));
    let kind = kind.trim().trim_start_matches('{').trim_start_matches('.').trim_end_matches('}').to_ascii_lowercase();
    (!kind.is_empty() && kind.chars().all(|c| c.is_ascii_alphabetic())).then(|| (kind, title.trim().to_string()))
}

fn mime(bytes: &[u8], name: &str) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG") {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xFF, 0xD8]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF8") {
        return Some("image/gif");
    }
    if bytes.starts_with(b"BM") {
        return Some("image/bmp");
    }
    if bytes.get(8..12) == Some(b"WEBP") {
        return Some("image/webp");
    }
    match name.rsplit('.').next().map(str::to_ascii_lowercase).as_deref() {
        Some("png") => Some("image/png"),
        Some("jpg" | "jpeg") => Some("image/jpeg"),
        _ => None,
    }
}

/// `![alt](path)` with a file path → a `data:` URI (so the importer embeds the picture). Paths are
/// read inside the read root, relative to `base`.
fn embed_images(line: &str, base: &std::path::Path, notes: &mut Vec<String>, images: &mut usize) -> String {
    const MAX_IMAGE: u64 = 32 * 1024 * 1024;
    let mut out = String::new();
    let mut rest = line;
    while let Some(i) = rest.find("![") {
        let (head, tail) = rest.split_at(i);
        out.push_str(head);
        let Some(close) = tail.find("](") else {
            out.push_str(tail);
            return out;
        };
        let Some(end) = tail.get(close + 2..).and_then(|t| t.find(')')).map(|e| close + 2 + e) else {
            out.push_str(tail);
            return out;
        };
        let alt = tail.get(2..close).unwrap_or("");
        let target = tail.get(close + 2..end).unwrap_or("").trim();
        // `path "title"` or `<path with spaces>`
        let path = target.split_once(" \"").map_or(target, |(p, _)| p).trim().trim_start_matches('<').trim_end_matches('>');
        let lower = path.to_ascii_lowercase();
        if path.is_empty() || lower.starts_with("data:") || lower.starts_with("http://") || lower.starts_with("https://") {
            out.push_str(tail.get(..=end).unwrap_or(""));
        } else {
            let full = if std::path::Path::new(path).is_absolute() { std::path::PathBuf::from(path) } else { base.join(path) };
            let full_s = full.to_string_lossy().into_owned();
            match crate::io::read_file(&full_s)
                .and_then(|b| if b.len() as u64 > MAX_IMAGE { Err(format!("larger than {} MB", MAX_IMAGE >> 20)) } else { Ok(b) })
            {
                Ok(bytes) => match mime(&bytes, path) {
                    Some(m) => {
                        *images += 1;
                        out.push_str(&format!("![{alt}](data:{m};base64,{})", super::insert::base64_encode(&bytes)));
                    }
                    None => {
                        notes.push(format!("picture {path}: not a PNG, JPEG, GIF, BMP or WebP file; its alt text was kept"));
                        out.push_str(alt);
                    }
                },
                Err(e) => {
                    notes.push(format!("picture {path}: {e}; its alt text was kept"));
                    out.push_str(alt);
                }
            }
        }
        rest = tail.get(end + 1..).unwrap_or("");
    }
    out.push_str(rest);
    out
}

/// The Markdown with the platform's extra shapes turned into markers the importer passes
/// through as paragraphs.
fn preprocess(text: &str, base: &std::path::Path, notes: &mut Vec<String>, images: &mut usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len() + 16);
    let mut fence: Option<(char, usize)> = None;
    let mut i = 0;
    let block = |out: &mut Vec<String>, m: String| {
        out.push(String::new());
        out.push(m);
        out.push(String::new());
    };
    while let Some(&l) = lines.get(i) {
        let t = l.trim();
        if let Some((c, n)) = fence {
            out.push(l.to_string());
            if t.starts_with(&c.to_string().repeat(n)) && t.chars().all(|x| x == c) {
                fence = None;
            }
            i += 1;
            continue;
        }
        let ticks = t.chars().take_while(|c| *c == '`').count();
        let tildes = t.chars().take_while(|c| *c == '~').count();
        if ticks >= 3 || tildes >= 3 {
            fence = Some(if ticks >= 3 { ('`', ticks) } else { ('~', tildes) });
            out.push(l.to_string());
            i += 1;
            continue;
        }
        if is_page_break(t) {
            block(&mut out, marker("pagebreak", ""));
            i += 1;
            continue;
        }
        if is_toc(t) {
            block(&mut out, marker("toc", ""));
            i += 1;
            continue;
        }
        if let Some((kind, title)) = gh_alert(t) {
            block(&mut out, marker("callout", &format!("{kind}\t{title}")));
            i += 1;
            while let Some(&c) = lines.get(i) {
                let ct = c.trim_start();
                let Some(body) = ct.strip_prefix('>') else { break };
                out.push(embed_images(body.strip_prefix(' ').unwrap_or(body), base, notes, images));
                i += 1;
            }
            block(&mut out, marker("endcallout", ""));
            continue;
        }
        if let Some((kind, title)) = mkdocs(t) {
            block(&mut out, marker("callout", &format!("{kind}\t{title}")));
            i += 1;
            while let Some(&c) = lines.get(i) {
                if c.trim().is_empty() {
                    // A blank line inside the body if the next line is still indented.
                    if lines.get(i + 1).is_some_and(|n| n.starts_with("    ") || n.starts_with('\t')) {
                        out.push(String::new());
                        i += 1;
                        continue;
                    }
                    break;
                }
                let Some(body) = c.strip_prefix("    ").or_else(|| c.strip_prefix('\t')) else { break };
                out.push(embed_images(body, base, notes, images));
                i += 1;
            }
            block(&mut out, marker("endcallout", ""));
            continue;
        }
        if let Some((kind, title)) = fenced(t) {
            block(&mut out, marker("callout", &format!("{kind}\t{title}")));
            i += 1;
            while let Some(&c) = lines.get(i) {
                i += 1;
                if c.trim() == ":::" {
                    break;
                }
                out.push(embed_images(c, base, notes, images));
            }
            block(&mut out, marker("endcallout", ""));
            continue;
        }
        out.push(embed_images(l, base, notes, images));
        i += 1;
    }
    out.join("\n")
}

/// Callout colours by kind: (accent, fill, title).
fn callout_style(kind: &str) -> (Rgb, Rgb, &'static str) {
    match kind {
        "tip" | "hint" | "success" => (Rgb(0x2E, 0x7D, 0x32), Rgb(0xE8, 0xF5, 0xE9), "Tip"),
        "important" | "abstract" | "summary" => (Rgb(0x6A, 0x1B, 0x9A), Rgb(0xF3, 0xE5, 0xF5), "Important"),
        "warning" | "attention" | "todo" => (Rgb(0xB2, 0x6A, 0x00), Rgb(0xFF, 0xF4, 0xE0), "Warning"),
        "caution" | "danger" | "error" | "bug" => (Rgb(0xC6, 0x28, 0x28), Rgb(0xFD, 0xEC, 0xEA), "Caution"),
        "example" => (Rgb(0x45, 0x5A, 0x64), Rgb(0xEC, 0xEF, 0xF1), "Example"),
        "question" => (Rgb(0x00, 0x83, 0x8F), Rgb(0xE0, 0xF7, 0xFA), "Question"),
        "quote" => (Rgb(0x61, 0x61, 0x61), Rgb(0xF5, 0xF5, 0xF5), "Quote"),
        _ => (Rgb(0x15, 0x65, 0xC0), Rgb(0xE3, 0xF2, 0xFD), "Note"),
    }
}

fn callout_para(p: &mut Paragraph, accent: Rgb, fill: Rgb) {
    p.props.shading = Some(fill);
    // One box: no space between the callout's paragraphs (the last gets its space after back).
    p.props.space_before = Some(0.0);
    p.props.space_after = Some(0.0);
    let side = Border { style: BorderStyle::Single, width: 3.0, color: Some(accent), space: 6.0 };
    p.props.borders = Some(Borders { left: Some(side), ..Default::default() });
    p.props.indent_left = Some(p.props.indent_left.unwrap_or(0.0) + 8.0);
    p.props.indent_right = Some(8.0);
    p.touch();
}

/// Paragraphs whose first letter is right to left and that set no direction become right to left.
fn auto_direction(doc: &mut Document) -> usize {
    let mut n = 0;
    for path in doc.para_paths(StoryRef::Body) {
        let unset = doc.para(StoryRef::Body, &path).is_some_and(|p| {
            p.props.bidi.is_none()
                && doc.styles.para_style_props(p.props.style.as_deref().unwrap_or("Normal")).0.bidi.is_none()
                && p.text.chars().any(char::is_alphabetic)
                && wordcraft_doc::bidi::first_strong_is_rtl(&p.text)
        });
        if unset && let Ok(p) = doc.para_mut(StoryRef::Body, &path) {
            p.props.bidi = Some(true);
            p.touch();
            n += 1;
        }
    }
    n
}

/// A header or footer paragraph from `text` with `{page}`, `{pages}`, `{title}`, `{date}`.
fn hf_paragraph(text: &str, title: &str, date: &str, align: Align) -> Paragraph {
    let mut p = Paragraph::with_text("", CharProps::default());
    p.props.align = Some(align);
    let mut rest = text.to_string();
    rest = rest.replace("{title}", title).replace("{date}", date);
    let props = CharProps::default();
    while !rest.is_empty() {
        let next = ["{page}", "{pages}"].iter().filter_map(|t| rest.find(t).map(|i| (i, *t))).min();
        match next {
            Some((i, tag)) => {
                let before = rest.get(..i).unwrap_or("").to_string();
                let at = p.len();
                let _ = p.insert_text(at, &before, &props);
                let instr = if tag == "{page}" { "PAGE" } else { "NUMPAGES" };
                let at = p.len();
                let _ = p.insert_object(
                    at,
                    InlineObject::Field { instr: instr.into(), result: "1".into(), locked: false, dirty: false, code: false, form: String::new() },
                    &props,
                );
                rest = rest.get(i + tag.len()..).unwrap_or("").to_string();
            }
            None => {
                let at = p.len();
                let _ = p.insert_text(at, &rest, &props);
                rest.clear();
            }
        }
    }
    p
}

/// Text a block gives (a number or boolean as written).
fn literal_text(v: &Value) -> Option<String> {
    match v {
        Value::String(t) => Some(t.clone()),
        Value::Number(_) | Value::Bool(_) => Some(v.to_string()),
        _ => None,
    }
}

/// Character formatting a run (or a paragraph's `text`) asks for.
fn literal_fmt(v: &Value) -> Fmt {
    let on = |k: &str| p::bool(v, k).unwrap_or(false);
    Fmt {
        bold: on("bold"),
        italic: on("italic"),
        underline: on("underline"),
        strike: on("strike"),
        color: p::str(v, "color").and_then(|c| Rgb::parse(c.trim().trim_start_matches('#'))),
        size: p::f32(v, "size"),
        font: p::str(v, "font").map(str::to_string),
        ..Default::default()
    }
}

/// A paragraph from a string or `{text, runs, …}`, every character as given.
fn literal_para(v: &Value, at: &str) -> Result<FPara, CmdError> {
    let mut out = FPara::default();
    let bad = |what: &str| CmdError::Params(format!("{at}: {what}"));
    match v {
        Value::Object(_) => {
            if let Some(t) = v.get("text").filter(|t| !t.is_null()) {
                out.push_text(&literal_text(t).ok_or_else(|| bad("`text` is a string"))?, &literal_fmt(v));
            }
            for (k, r) in v.get("runs").and_then(Value::as_array).into_iter().flatten().enumerate() {
                let (t, f) = match r {
                    Value::Object(_) => (r.get("text").and_then(literal_text), literal_fmt(r)),
                    _ => (literal_text(r), Fmt::default()),
                };
                out.push_text(&t.ok_or_else(|| bad(&format!("runs[{k}] is a string or {{\"text\": …}}")))?, &f);
            }
        }
        Value::Null => {}
        _ => out.push_text(&literal_text(v).ok_or_else(|| bad("a paragraph is a string or {\"text\": …}"))?, &Fmt::default()),
    }
    Ok(out)
}

/// `blocks` as a flow, every string taken literally: no Markdown is read; leading spaces, tabs,
/// line breaks (`\n`) and every other character stay as given (carriage returns and the other
/// control characters a Word file can't hold are left out). Also each top-level block's `style`,
/// and how many page breaks there are.
fn literal_flow(blocks: &[Value]) -> Result<(Flow, Vec<Option<String>>, usize), CmdError> {
    let mut flow = Flow::default();
    let mut styles = Vec::new();
    let mut page_break = false;
    let mut breaks = 0;
    for (i, b) in blocks.iter().enumerate() {
        let at = format!("blocks[{i}]");
        let kind = match p::str(b, "type") {
            Some(t) => t.to_ascii_lowercase().replace(['_', '-', ' '], ""),
            None if b.get("rows").is_some() => "table".into(),
            None if p::bool(b, "page_break") == Some(true) && b.get("text").is_none() && b.get("runs").is_none() => "pagebreak".into(),
            None => "paragraph".into(),
        };
        match kind.as_str() {
            "pagebreak" | "newpage" | "break" => {
                // A second break in a row: a page of its own.
                if std::mem::replace(&mut page_break, true) {
                    flow.blocks.push(FBlock::Para(FPara { page_break: true, ..Default::default() }));
                    styles.push(None);
                }
                breaks += 1;
            }
            "table" => {
                let header = p::bool(b, "header").unwrap_or(true);
                let rows = b.get("rows").and_then(Value::as_array);
                let rows = rows.ok_or_else(|| CmdError::Params(format!("{at}: a table needs `rows`: a list of rows, each a list of cells")))?;
                let mut t = FTable::default();
                for (r, row) in rows.iter().enumerate() {
                    let cells = row.as_array().ok_or_else(|| CmdError::Params(format!("{at}.rows[{r}] is a list of cells")))?;
                    let mut out = Vec::with_capacity(cells.len());
                    for (c, cell) in cells.iter().enumerate() {
                        let q = literal_para(cell, &format!("{at}.rows[{r}][{c}]"))?;
                        out.push(FCell { blocks: vec![FBlock::Para(q)], header: header && r == 0, ..Default::default() });
                    }
                    t.rows.push(out);
                }
                // A table without cells shows nothing (and the flow keeps one block per block).
                if t.rows.iter().all(Vec::is_empty) {
                    continue;
                }
                if std::mem::take(&mut page_break) {
                    // A table right after a break: the break goes in a paragraph of its own.
                    let mut br = FPara::default();
                    br.push_text("\u{000C}", &Fmt::default());
                    flow.blocks.push(FBlock::Para(br));
                    styles.push(None);
                }
                flow.blocks.push(FBlock::Table(t));
                styles.push(None);
            }
            "paragraph" | "para" | "p" | "text" | "heading" | "h" => {
                let mut q = literal_para(b, &at)?;
                let level = p::u64(b, "heading").or_else(|| p::u64(b, "level")).or_else(|| matches!(kind.as_str(), "heading" | "h").then_some(1));
                if let Some(n) = level {
                    q.kind = FKind::Heading(n.clamp(1, 6) as u8);
                }
                q.page_break = std::mem::take(&mut page_break);
                flow.blocks.push(FBlock::Para(q));
                styles.push(p::str(b, "style").map(str::to_string));
            }
            other => return Err(CmdError::Params(format!("{at}: no block type `{other}` (paragraph, heading, table or page_break)"))),
        }
    }
    if page_break {
        flow.blocks.push(FBlock::Para(FPara { page_break: true, ..Default::default() }));
        styles.push(None);
    }
    Ok((flow, styles, breaks))
}

fn import(s: &mut Session, v: &Value) -> CmdResult {
    let blocks = match v.get("blocks") {
        None | Some(Value::Null) => None,
        Some(Value::Array(list)) => Some(list),
        Some(_) => return Err(CmdError::Params("`blocks` is a list of blocks".into())),
    };
    let (source, base) = match (p::str(v, "markdown").or_else(|| p::str(v, "text")), p::str(v, "path")) {
        _ if blocks.is_some() => (String::new(), std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))),
        (Some(m), _) => (m.to_string(), std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))),
        (None, Some(path)) => {
            let bytes = crate::io::read_file(path).map_err(CmdError::Failed)?;
            let base = std::path::Path::new(path).parent().map(std::path::Path::to_path_buf).unwrap_or_default();
            (crate::io::decode_text(&bytes), base)
        }
        (None, None) => {
            return Err(CmdError::Params("`markdown` (the text), `path` (a .md file) or `blocks` (literal text, structured) is required".into()));
        }
    };
    let (fm, body) = front_matter(&source);
    let get = |k: &str| -> Option<String> { p::str(v, k).map(str::to_string).or_else(|| fm.iter().find(|(x, _)| x == k).map(|(_, y)| y.clone())) };
    let truthy = |k: &str| -> Option<bool> {
        p::bool(v, k).or_else(|| get(k).map(|x| matches!(x.to_ascii_lowercase().as_str(), "true" | "yes" | "1" | "on")))
    };
    let mut theme = Theme::from_json(v.get("theme").unwrap_or(&Value::Null));
    // Theme keys in the front matter fill what the argument leaves out.
    let fm_theme = Theme::from_json(&Value::Object(fm.iter().map(|(k, x)| (k.clone(), json!(x))).collect()));
    theme.body_font = theme.body_font.or(fm_theme.body_font);
    theme.heading_font = theme.heading_font.or(fm_theme.heading_font);
    theme.primary = theme.primary.or(fm_theme.primary);
    theme.accent = theme.accent.or(fm_theme.accent);
    theme.text = theme.text.or(fm_theme.text);
    theme.logo =
        theme.logo.or(fm_theme.logo).map(|l| if std::path::Path::new(&l).is_absolute() { l } else { base.join(&l).to_string_lossy().into_owned() });
    let mut notes = Vec::new();
    let mut images = 0usize;
    let page = crate::io::region_page(&s.region());
    let mut literal_breaks = 0;
    let mut doc = match blocks {
        Some(list) => {
            if p::str(v, "markdown").or_else(|| p::str(v, "text")).or_else(|| p::str(v, "path")).is_some() {
                notes.push("`markdown` and `path` were left out: `blocks` gives the document".to_string());
            }
            let (flow, styles, n) = literal_flow(list)?;
            literal_breaks = n;
            let mut doc = wordcraft_formats::model::to_doc_in(&flow, page.clone());
            // One document block per flow block: each takes its style by name or id.
            for (b, style) in doc.body.iter_mut().zip(&styles) {
                let Some(name) = style else { continue };
                match doc.styles.find(name).map(|st| st.id.clone()) {
                    Some(id) => {
                        if let Block::Para(p) = Arc::make_mut(b) {
                            p.props.style = Some(id);
                            p.touch();
                        }
                    }
                    None => notes.push(format!("no style `{name}`: the paragraph kept its own")),
                }
            }
            doc
        }
        None => wordcraft_formats::markdown::import_on(&preprocess(body, &base, &mut notes, &mut images), &page),
    };
    let title = get("title").unwrap_or_default();
    let subtitle = get("subtitle").unwrap_or_default();
    let date = get("date").unwrap_or_default();
    let author = get("author").unwrap_or_default();
    let cover = truthy("cover").unwrap_or(!title.is_empty()) && !title.is_empty();
    // Markers → page breaks, callouts, the TOC's place.
    let mut out: Blocks = Vec::with_capacity(doc.body.len() + 8);
    let mut callout: Option<(Rgb, Rgb)> = None;
    let (mut breaks, mut callouts, mut toc_at) = (literal_breaks, 0usize, None::<usize>);
    let mut break_next = false;
    for b in std::mem::take(&mut doc.body) {
        // Literal blocks carry no markers: their text is the user's, whatever it holds.
        let mark = b.as_para().filter(|_| blocks.is_none()).and_then(|p| parse_marker(&p.plain_text()));
        match mark.as_ref().map(|(k, a)| (k.as_str(), a.as_str())) {
            Some(("pagebreak", _)) => {
                breaks += 1;
                break_next = true;
                continue;
            }
            Some(("toc", _)) => {
                toc_at = Some(out.len());
                out.push(para_block(Paragraph::with_text("", CharProps::default())));
                continue;
            }
            Some(("callout", arg)) => {
                callouts += 1;
                let (kind, title) = arg.split_once('\t').unwrap_or((arg, ""));
                let (accent, fill, default_title) = callout_style(kind);
                let shown = if title.is_empty() { default_title.to_string() } else { title.to_string() };
                let mut t = Paragraph::with_text(&shown, CharProps { bold: Some(true), color: Some(TextColor::Rgb(accent)), ..Default::default() });
                callout_para(&mut t, accent, fill);
                t.props.keep_next = Some(true);
                if std::mem::take(&mut break_next) {
                    t.props.page_break_before = Some(true);
                }
                out.push(para_block(t));
                callout = Some((accent, fill));
                continue;
            }
            Some(("endcallout", _)) => {
                callout = None;
                if let Some(Block::Para(last)) = out.last_mut().map(Arc::make_mut) {
                    last.props.space_after = Some(10.0);
                    last.touch();
                }
                continue;
            }
            _ => {}
        }
        let mut b = b;
        if let Block::Para(p) = Arc::make_mut(&mut b) {
            if let Some((accent, fill)) = callout {
                callout_para(p, accent, fill);
            }
            if std::mem::take(&mut break_next) {
                p.props.page_break_before = Some(true);
                p.touch();
            }
        } else if std::mem::take(&mut break_next) {
            // A table right after a break: the break goes in a paragraph of its own.
            out.push(para_block(Paragraph::with_text("\u{000C}", CharProps::default())));
        }
        out.push(b);
    }
    if out.is_empty() {
        out.push(para_block(Paragraph::new()));
    }
    // The cover block.
    if cover {
        let mut cover_blocks: Blocks = Vec::new();
        let styled = |text: &str, style: &str| {
            let mut p = Paragraph::with_text(text, CharProps::default());
            p.props.style = Some(style.to_string());
            para_block(p)
        };
        cover_blocks.push(styled(&title, "Title"));
        if !subtitle.is_empty() {
            cover_blocks.push(styled(&subtitle, "Subtitle"));
        }
        for line in [&date, &author].into_iter().filter(|x| !x.is_empty()) {
            cover_blocks.push(para_block(Paragraph::with_text(line, CharProps::default())));
        }
        if let Some(Block::Para(first)) = out.first_mut().map(Arc::make_mut) {
            first.props.page_break_before = Some(true);
            first.touch();
        }
        let n = cover_blocks.len();
        cover_blocks.extend(out);
        out = cover_blocks;
        toc_at = toc_at.map(|t| t + n);
        doc.last_section.title_page = true;
    }
    // Table header rows: shaded, bold, repeated.
    let fill = theme.header_fill();
    let ink = theme.header_text();
    let mut tables = 0usize;
    for b in out.iter_mut() {
        if let Block::Table(t) = Arc::make_mut(b) {
            tables += 1;
            // Literal tables have a header row unless they said `header: false`.
            if let Some(row) = t.rows.first_mut().filter(|r| blocks.is_none() || r.props.header) {
                row.props.header = true;
                for cell in row.cells.iter_mut() {
                    cell.props.shading = Some(fill);
                    for cb in cell.blocks.iter_mut() {
                        if let Block::Para(p) = Arc::make_mut(cb) {
                            let len = p.len();
                            let _ = p.format(0, len, &|c| {
                                c.bold = Some(true);
                                if let Some(i) = ink {
                                    c.color = Some(TextColor::Rgb(i));
                                }
                            });
                            p.mark.bold = Some(true);
                        }
                    }
                }
            }
        }
    }
    doc.body = out;
    if !title.is_empty() && doc.core.title.is_empty() {
        doc.core.title = title.clone();
    }
    if !author.is_empty() {
        doc.core.creator = author.clone();
    }
    // Code blocks: shaded, in a fixed-pitch font.
    if let Some(code) = doc.styles.get_mut("Code") {
        code.para.shading = code.para.shading.or(Some(Rgb(0xF4, 0xF5, 0xF7)));
        code.chr.font = code.chr.font.clone().or(Some("Consolas".into()));
    }
    let rtl = auto_direction(&mut doc);
    // A safe point before the open document is replaced.
    crate::whiq_progress::report(1, 3, "markdown: document built");
    crate::whiq_progress::check("stopped before the Markdown document replaced the open one; nothing was changed")?;
    s.set_document(doc);
    s.path = None;
    s.dirty = true;
    crate::whiq_progress::report(2, 3, "markdown: headers, footers and contents");
    // Header and footer.
    let footer = get("footer").unwrap_or_else(|| "Page {page} of {pages}".into());
    let header = get("header").unwrap_or_default();
    let none = |x: &str| matches!(x.trim().to_ascii_lowercase().as_str(), "" | "none" | "false" | "no" | "off");
    if !none(&header) {
        let p = hf_paragraph(&header, &title, &date, Align::Right);
        let id = s.doc.try_add_part(PartKind::Header, vec![para_block(p)])?;
        s.doc.last_section.headers.default = Some(id);
    }
    if !none(&footer) {
        let p = hf_paragraph(&footer, &title, &date, Align::Center);
        let id = s.doc.try_add_part(PartKind::Footer, vec![para_block(p)])?;
        s.doc.last_section.footers.default = Some(id);
    }
    // The theme, as one style batch.
    let applied = if theme.is_empty() { Vec::new() } else { apply_theme(s, &theme)? };
    // The table of contents, from the headings now in place.
    let toc = toc_at.is_some() || truthy("toc").unwrap_or(false);
    if toc {
        let at = toc_at.unwrap_or(0);
        let at = at.min(s.doc.body.len().saturating_sub(1));
        s.sel = Selection::caret(Pos { story: StoryRef::Body, path: wordcraft_doc::Path::top(at), off: 0 });
        let levels = get("toc_levels").and_then(|l| l.parse::<u64>().ok()).unwrap_or(3).clamp(1, 9);
        let before = s.doc.body.len();
        s.join_next_undo();
        s.run("references.toc", &json!({"levels": levels, "hyperlinks": true})).map_err(|e| CmdError::Failed(format!("table of contents: {e}")))?;
        // The TOC went in before the placeholder: after a cover it starts a page of its own, and
        // the content after it starts the next one; the empty placeholder goes.
        let added = s.doc.body.len().saturating_sub(before);
        let place = at + added;
        let placeholder =
            s.doc.body.get(place).and_then(|b| b.as_para()).filter(|p| p.plain_text().is_empty()).map(|p| p.props.page_break_before == Some(true));
        if let Some(had_break) = placeholder {
            s.doc.body.remove(place);
            if let Some(Block::Para(next)) = s.doc.body.get_mut(place).map(Arc::make_mut)
                && (had_break || cover)
            {
                next.props.page_break_before = Some(true);
                next.touch();
            }
        }
        if cover && let Some(Block::Para(first)) = s.doc.body.get_mut(at).map(Arc::make_mut) {
            first.props.page_break_before = Some(true);
            first.touch();
        }
        s.doc.ensure_nonempty();
        s.touch();
        // Page numbers as the pages now fall.
        s.sel = Selection::caret(Pos { story: StoryRef::Body, path: wordcraft_doc::Path::top(at), off: 0 });
        s.join_next_undo();
        s.run("references.toc", &json!({})).map_err(|e| CmdError::Failed(format!("table of contents: {e}")))?;
    }
    s.sel = Selection::caret(s.doc.start_of(StoryRef::Body));
    Ok(json!({
        "paragraphs": s.doc.paragraph_count(), "tables": tables, "images": images, "callouts": callouts, "pageBreaks": breaks,
        "toc": toc, "cover": cover, "header": !none(&header), "footer": !none(&footer), "rightToLeft": rtl, "theme": applied, "warnings": notes,
    }))
}

/// Apply `theme` as one style batch; what was applied.
pub fn apply_theme(s: &mut Session, t: &Theme) -> Result<Vec<String>, CmdError> {
    let mut applied = Vec::new();
    let st = &mut s.doc.styles;
    if let Some(n) = st.get_mut("Normal") {
        if let Some(f) = &t.body_font {
            n.chr.font = Some(f.clone());
            n.chr.font_cs = Some(f.clone());
        }
        if let Some(c) = t.text {
            n.chr.color = Some(TextColor::Rgb(c));
        }
        if let Some(sz) = t.size {
            n.chr.size = Some(sz);
            n.chr.size_cs = Some(sz);
        }
    }
    if t.body_font.is_some() {
        st.default_chr.font = t.body_font.clone();
        applied.push("body font".into());
    }
    if t.text.is_some() {
        applied.push("text colour".into());
    }
    for id in ["Heading1", "Heading2", "Heading3", "Heading4", "Heading5", "Heading6", "Title", "Subtitle", "TOCHeading"] {
        if let Some(h) = st.get_mut(id) {
            if let Some(f) = &t.heading_font {
                h.chr.font = Some(f.clone());
                h.chr.font_cs = Some(f.clone());
            }
            let color = if id == "Subtitle" || id == "Heading3" || id == "Heading4" { t.accent.or(t.primary) } else { t.primary };
            if let Some(c) = color {
                h.chr.color = Some(TextColor::Rgb(c));
            }
        }
    }
    if t.heading_font.is_some() {
        applied.push("heading font".into());
    }
    if t.primary.is_some() {
        applied.push("heading colour".into());
    }
    if let Some(c) = t.accent.or(t.primary)
        && let Some(h) = st.get_mut("Hyperlink")
    {
        h.chr.color = Some(TextColor::Rgb(c));
        applied.push("link colour".into());
    }
    // Table header rows already in the document follow the theme too.
    if t.table_header.is_some() || t.primary.is_some() {
        let fill = t.header_fill();
        let ink = t.header_text();
        for b in s.doc.body.iter_mut() {
            if let Block::Table(tb) = Arc::make_mut(b)
                && let Some(row) = tb.rows.first_mut().filter(|r| r.props.header)
            {
                for cell in row.cells.iter_mut() {
                    cell.props.shading = Some(fill);
                    for cb in cell.blocks.iter_mut() {
                        if let (Block::Para(p), Some(i)) = (Arc::make_mut(cb), ink) {
                            let len = p.len();
                            let _ = p.format(0, len, &|c| c.color = Some(TextColor::Rgb(i)));
                        }
                    }
                }
            }
        }
        applied.push("table header colours".into());
    }
    s.touch();
    if let Some(logo) = &t.logo {
        // The logo, top right of the header (a header is made when there is none).
        let id = match s.doc.last_section.headers.default {
            Some(id) => id,
            None => {
                let mut p = Paragraph::new();
                p.props.align = Some(Align::Right);
                let id = s.doc.try_add_part(PartKind::Header, vec![para_block(p)])?;
                s.doc.last_section.headers.default = Some(id);
                id
            }
        };
        s.sel = Selection::caret(Pos { story: StoryRef::Part(id), path: wordcraft_doc::Path::top(0), off: 0 });
        s.join_next_undo();
        s.run("insert.picture", &json!({"path": logo, "width": 90})).map_err(|e| CmdError::Failed(format!("logo {logo}: {e}")))?;
        if let Ok(p) = s.doc.para_mut(StoryRef::Part(id), &wordcraft_doc::Path::top(0)) {
            p.props.align = Some(Align::Right);
            p.touch();
        }
        s.sel = Selection::caret(s.doc.start_of(StoryRef::Body));
        applied.push("logo".into());
    }
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn import_md(md: &str) -> (Session, Value) {
        let mut s = Session::new(Document::new());
        let r = s.run("file.importMarkdown", &json!({"markdown": md})).unwrap();
        (s, r)
    }

    #[test]
    fn the_platforms_shapes_import_faithfully() {
        let md = "---\ntitle: Annual Review\nsubtitle: 2026\nauthor: Ops Team\ndate: 2026-10-17\nheader: \"{title}\"\n---\n\n[TOC]\n\n# One\n\nText.\n\n## Two\n\n| A | B |\n|---|---|\n| 1 | 2 |\n\n> [!WARNING] Mind the gap\n> Careful here.\n\n\\pagebreak\n\n### Three\n\n!!! tip \"Pro tip\"\n    Indented body.\n\n:::note\nFenced body.\n:::\n\n```\ncode line\n```\n";
        let (s, r) = import_md(md);
        assert_eq!(r["callouts"], 3, "{r}");
        assert_eq!(r["pageBreaks"], 1);
        assert_eq!(
            (r["toc"].as_bool(), r["cover"].as_bool(), r["header"].as_bool(), r["footer"].as_bool()),
            (Some(true), Some(true), Some(true), Some(true))
        );
        let paras: Vec<&Paragraph> = s.doc.body.iter().filter_map(|b| b.as_para()).collect();
        assert_eq!(paras[0].props.style.as_deref(), Some("Title"));
        assert_eq!(paras[1].props.style.as_deref(), Some("Subtitle"));
        assert!(paras.iter().any(|p| p.props.style.as_deref() == Some("TOC1")), "a TOC with entries");
        let warn = paras.iter().find(|p| p.plain_text() == "Mind the gap").unwrap();
        assert!(warn.props.shading.is_some() && warn.props.borders.is_some());
        let body = paras.iter().find(|p| p.plain_text() == "Careful here.").unwrap();
        assert!(body.props.shading.is_some(), "the callout's body is in the box");
        assert!(paras.iter().find(|p| p.plain_text() == "Three").unwrap().props.page_break_before == Some(true));
        assert!(paras.iter().any(|p| p.plain_text() == "Indented body." && p.props.shading.is_some()));
        assert!(paras.iter().any(|p| p.plain_text() == "Fenced body." && p.props.shading.is_some()));
        let t = s.doc.body.iter().find_map(|b| match &**b {
            Block::Table(t) => Some(t),
            _ => None,
        });
        let t = t.unwrap();
        assert!(t.rows[0].props.header && t.rows[0].cells[0].props.shading.is_some());
        assert!(s.doc.last_section.title_page && s.doc.last_section.footers.default.is_some());
        assert_eq!(s.doc.core.title, "Annual Review");
        // Nothing of the markers is left.
        assert!(!s.doc.plain_text(StoryRef::Body).contains(MARK));
    }

    #[test]
    fn theme_and_direction() {
        let mut s = Session::new(Document::new());
        let r = s
            .run(
                "file.importMarkdown",
                &json!({"markdown": "# عنوان\n\nنص عربي.\n\nEnglish text.\n\n| H |\n|---|\n| x |\n", "theme": {"fonts": {"body": "Noto Sans", "heading": "Noto Serif"}, "colors": {"primary": "#1F3864", "accent": "2E75B6"}}, "footer": "none"}),
            )
            .unwrap();
        assert_eq!(r["rightToLeft"], 2, "{r}");
        assert_eq!(s.doc.styles.get("Normal").unwrap().chr.font.as_deref(), Some("Noto Sans"));
        assert_eq!(s.doc.styles.get("Heading1").unwrap().chr.color, Some(TextColor::Rgb(Rgb(0x1F, 0x38, 0x64))));
        assert!(s.doc.last_section.footers.default.is_none());
        let paras: Vec<&Paragraph> = s.doc.body.iter().filter_map(|b| b.as_para()).collect();
        assert_eq!(paras[0].props.bidi, Some(true));
        assert_eq!(paras[2].props.bidi, None);
    }

    #[test]
    fn missing_pictures_keep_their_alt_text_with_a_warning() {
        let (s, r) = import_md("Before ![A chart](no/such/file.png) after.");
        assert!(r["warnings"][0].as_str().unwrap().contains("no/such/file.png"), "{r}");
        assert!(s.doc.plain_text(StoryRef::Body).contains("A chart"));
    }

    #[test]
    fn blocks_keep_user_text_exactly() {
        let note = "  two leading spaces\n\tand a tab; *not* `code`\n# no heading, \\pagebreak, [TOC], <b>&amp;</b>, \\*, ١٢٣";
        let mut s = Session::new(Document::new());
        let r = s
            .run(
                "file.importMarkdown",
                &json!({"blocks": [
                    {"type": "heading", "level": 2, "text": "# Site *diary*"},
                    {"text": note},
                    {"runs": [{"text": "  bold ", "bold": true}, "plain_\\*"], "style": "Quote"},
                    {"type": "page_break"},
                    {"type": "table", "rows": [["| a |", "*b*"], ["  c\nd", {"text": "e", "italic": true}]]},
                    {"type": "table", "header": false, "rows": [["x"]]},
                    {"text": "\u{E0FF}WHIQ:pagebreak:\u{E0FF}"},
                    {"text": "x", "style": "No Such Style"},
                    "a bare string",
                ], "footer": "none"}),
            )
            .unwrap();
        assert_eq!((r["tables"].as_u64(), r["pageBreaks"].as_u64(), r["callouts"].as_u64()), (Some(2), Some(1), Some(0)), "{r}");
        assert!(r["warnings"].to_string().contains("No Such Style"), "{r}");
        let paras: Vec<&Paragraph> = s.doc.body.iter().filter_map(|b| b.as_para()).collect();
        assert_eq!((paras[0].plain_text().as_str(), paras[0].props.style.as_deref()), ("# Site *diary*", Some("Heading2")));
        assert_eq!(paras[1].plain_text(), note, "every character, leading spaces and line breaks as given");
        assert_eq!((paras[2].plain_text().as_str(), paras[2].props.style.as_deref()), ("  bold plain_\\*", Some("Quote")));
        assert_eq!(paras[2].runs[0].props.bold, Some(true));
        assert_eq!(paras[3].plain_text(), "\u{000C}", "the break before a table is a paragraph of its own");
        assert_eq!(paras[4].plain_text(), "\u{E0FF}WHIQ:pagebreak:\u{E0FF}", "no marker is read in literal text");
        assert_eq!(paras[6].plain_text(), "a bare string");
        let tables: Vec<&wordcraft_doc::Table> = s
            .doc
            .body
            .iter()
            .filter_map(|b| match &**b {
                Block::Table(t) => Some(t),
                _ => None,
            })
            .collect();
        let cell = |t: &wordcraft_doc::Table, r: usize, c: usize| tables_text(&t.rows[r].cells[c].blocks);
        assert_eq!((cell(tables[0], 0, 0), cell(tables[0], 1, 0)), ("| a |".to_string(), "  c\nd".to_string()));
        assert!(tables[0].rows[0].props.header && tables[0].rows[0].cells[0].props.shading.is_some(), "a header row, shaded");
        assert!(!tables[1].rows[0].props.header && tables[1].rows[0].cells[0].props.shading.is_none(), "header: false");
        assert!(!s.doc.plain_text(StoryRef::Body).contains("WHIQ:callout"));
        // Mistakes name the block.
        for (bad, want) in [
            (json!({"blocks": [{"type": "chart"}]}), "blocks[0]"),
            (json!({"blocks": ["ok", {"type": "table", "rows": [5]}]}), "blocks[1].rows[0]"),
            (json!({"blocks": [{"runs": [{"bold": true}]}]}), "runs[0]"),
            (json!({"blocks": "text"}), "`blocks`"),
        ] {
            let e = Session::new(Document::new()).run("file.importMarkdown", &bad).unwrap_err().to_string();
            assert!(e.contains(want), "{bad}: {e}");
        }
    }

    fn tables_text(blocks: &Blocks) -> String {
        blocks.iter().filter_map(|b| b.as_para()).map(Paragraph::plain_text).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn markers_survive_hostile_input() {
        for md in
            ["---\n", "---\ntitle: x\n", "> [!NOTE", ":::", "!!! ", "![x](", "![x]()", "\\pagebreak", "[TOC]", "\u{E0FF}WHIQ:callout:zz\u{E0FF}"]
        {
            let mut s = Session::new(Document::new());
            let _ = s.run("file.importMarkdown", &json!({"markdown": md}));
        }
    }
}
