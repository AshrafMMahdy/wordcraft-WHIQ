//! WHIQ: the document drawn for the platform's Preview pane, and read by its agents, from the
//! document model itself, in parts for a document of any size.
//!
//! - [`render_html`]: one self-contained HTML page (nothing fetched: pictures are data URIs,
//!   charts inline SVG, equations MathML), in the shape the platform's own Word previewer
//!   returned (`.file-head`, `.page`, `.warn`, the same page colours), so its Preview pane shows it
//!   unchanged. Drawn from WordCraft's model: styles and direct formatting resolved (fonts, sizes,
//!   colours, bold, italic, underline, highlight), list labels numbered as Word numbers them,
//!   tables with merged cells, shading, borders and table-style regions, pictures cropped and
//!   floated, headers and footers on each sheet, page and section breaks as new sheets, tracked
//!   insertions, deletions, moves and formatting marked with author and date, comments
//!   highlighted and listed with their replies, footnotes and endnotes, right-to-left paragraphs.
//! - [`render_markdown`]: the same walk as Markdown for an agent to read: headings, list labels,
//!   GFM tables (merged cells noted under the table), tracked changes and comments in
//!   CriticMarkup (`{++…++}`, `{--…--}`, `{==…==}{>>…<<}`), footnotes (`[^1]`), page breaks as
//!   `<!-- pagebreak -->` (what `import_markdown` reads).
//! - [`plan`]: where the parts are: top-level blocks taken in order until a part holds
//!   `part_chars` characters of text (and, for the page, `picture_budget` bytes of pictures); a
//!   table larger than a part is split between rows, its header rows repeated in each piece.
//!   List numbers, note numbers and open comments carry over from the parts before.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use wordcraft_doc::graphic::{Graphic, GraphicItem, PathSeg, TextAlign};
use wordcraft_doc::numbering::{Counters, Level};
use wordcraft_doc::para::{COLUMN_BREAK, Float, FloatAlign, InlineObject, LINE_BREAK, NoteKind, OBJ, PAGE_BREAK, SOFT_HYPHEN, ShapeKind, Wrap};
use wordcraft_doc::props::{
    Align, Border, BorderStyle, Borders, CharProps, HeightRule, LineSpacing, NumRef, ParaProps, Rgb, TextColor, TextDirection, Underline, VAlign,
    VMerge, VertAlign,
};
use wordcraft_doc::resolve::ResolvedChar;
use wordcraft_doc::section::SectionStart;
use wordcraft_doc::styles::{PartSet, TablePart, TableStyleProps};
use wordcraft_doc::{Block, Blocks, Document, Paragraph, SectionProps, Table};

use crate::html::esc;
use crate::model::{base64_encode, decode_bounded, mime_of, sniff_image};

/// Characters of text in a part of the page (about 60 printed pages).
pub const PART_CHARS: usize = 200_000;
/// Characters of text in a part of the Markdown (about 10,000 tokens).
pub const MARKDOWN_PART_CHARS: usize = 40_000;
/// Bytes of pictures embedded in a part of the page.
pub const PICTURE_BUDGET: u64 = 12 << 20;
/// The smallest part a caller may ask for (a smaller number is read as this).
pub const MIN_PART_CHARS: usize = 200;
/// Tables in tables, text boxes in text boxes: deeper ones are flattened to text.
const MAX_DEPTH: usize = 8;
const MAX_BOX_DEPTH: usize = wordcraft_doc::MAX_TEXT_BOX_DEPTH;

/// What a part covers: top-level body blocks `from..to`, and for one table split between rows,
/// its rows `rows.0..rows.1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Piece {
    pub from: usize,
    pub to: usize,
    pub rows: Option<(usize, usize)>,
}

/// Which part to draw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Window {
    /// Part `n` (1-based) of [`plan`].
    Part(usize),
    /// Top-level body blocks `from..to` (a page or section window the caller worked out).
    Blocks { from: usize, to: usize },
}

#[derive(Clone, Debug)]
pub struct Options {
    /// The file name shown at the top of the page.
    pub title: String,
    /// Characters of text a part holds (see [`plan`]).
    pub part_chars: usize,
    /// Bytes of pictures a part of the page embeds; past it a part ends (or, in a window, the
    /// pictures show as placeholders).
    pub picture_budget: u64,
    /// Show tracked changes and comments (Print Markup); false: the document as it would be with
    /// every change accepted, without comments.
    pub markup: bool,
    /// Embed pictures (false: placeholders of their size).
    pub pictures: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options { title: "Document".into(), part_chars: PART_CHARS, picture_budget: PICTURE_BUDGET, markup: true, pictures: true }
    }
}

/// What a part holds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub paragraphs: usize,
    pub headings: usize,
    pub tables: usize,
    pub pictures: usize,
    pub pictures_omitted: usize,
    pub picture_bytes: u64,
    pub insertions: usize,
    pub deletions: usize,
    pub format_changes: usize,
    pub comments: usize,
    pub footnotes: usize,
    pub endnotes: usize,
    /// Characters of document text drawn.
    pub characters: usize,
}

/// A drawn part.
#[derive(Clone, Debug)]
pub struct Rendered {
    /// The HTML page or the Markdown.
    pub text: String,
    /// The part drawn (1-based), `None` for a block window.
    pub part: Option<usize>,
    /// How many parts the document has at this part size.
    pub parts: usize,
    pub piece: Piece,
    pub total_blocks: usize,
    pub stats: Stats,
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------------------------
// Parts

fn add(a: (usize, u64), b: (usize, u64)) -> (usize, u64) {
    (a.0.saturating_add(b.0), a.1.saturating_add(b.1))
}

fn picture_len(doc: &Document, o: &InlineObject) -> u64 {
    match o {
        InlineObject::Image { media, .. } => doc.media.byte_len(media).unwrap_or(0),
        InlineObject::Group { children, .. } => children.iter().map(|c| picture_len(doc, &c.obj)).sum(),
        _ => 0,
    }
}

fn measure_para(doc: &Document, p: &Paragraph, depth: usize) -> (usize, u64) {
    let mut m = (p.text.chars().count() + 1, 0u64);
    for o in &p.objects {
        m.1 = m.1.saturating_add(picture_len(doc, o));
        if depth < MAX_BOX_DEPTH {
            for id in o.text_boxes() {
                if let Some(part) = doc.parts.get(&id) {
                    m = add(m, measure_blocks(doc, &part.blocks, depth + 1));
                }
            }
        }
    }
    m
}

fn measure_blocks(doc: &Document, bl: &Blocks, depth: usize) -> (usize, u64) {
    bl.iter().fold((0, 0), |m, b| add(m, measure_block(doc, b, depth)))
}

fn measure_row(doc: &Document, r: &wordcraft_doc::table::Row, depth: usize) -> (usize, u64) {
    r.cells.iter().fold((0, 0), |m, c| add(m, measure_blocks(doc, &c.blocks, depth + 1)))
}

fn measure_block(doc: &Document, b: &Block, depth: usize) -> (usize, u64) {
    match b {
        Block::Para(p) => measure_para(doc, p, depth),
        Block::Table(t) if depth < MAX_DEPTH => t.rows.iter().fold((0, 0), |m, r| add(m, measure_row(doc, r, depth))),
        Block::Table(_) => (0, 0),
    }
}

/// The parts of `doc`: top-level blocks in order until a part would pass `part_chars` characters
/// of text or `picture_budget` bytes of pictures; a table that alone passes them is split between
/// rows. Every block is in exactly one part; a document has at least one part.
pub fn plan(doc: &Document, part_chars: usize, picture_budget: u64) -> Vec<Piece> {
    let limit = part_chars.max(MIN_PART_CHARS);
    let budget = picture_budget.max(1);
    let over = |m: (usize, u64)| m.0 > limit || m.1 > budget;
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut acc = (0usize, 0u64);
    for (i, b) in doc.body.iter().enumerate() {
        let m = measure_block(doc, b, 0);
        if i > start && over(add(acc, m)) {
            out.push(Piece { from: start, to: i, rows: None });
            start = i;
            acc = (0, 0);
        }
        if over(m)
            && let Block::Table(t) = &**b
            && t.rows.len() > 1
        {
            let heads = header_rows(t);
            let mut r0 = 0usize;
            let mut racc = (0usize, 0u64);
            for (ri, row) in t.rows.iter().enumerate() {
                let rm = measure_row(doc, row, 0);
                if ri > r0 && ri > heads && over(add(racc, rm)) {
                    out.push(Piece { from: i, to: i + 1, rows: Some((r0, ri)) });
                    r0 = ri;
                    racc = (0, 0);
                }
                racc = add(racc, rm);
            }
            out.push(Piece { from: i, to: i + 1, rows: Some((r0, t.rows.len())) });
            start = i + 1;
            acc = (0, 0);
            continue;
        }
        acc = add(acc, m);
    }
    if start < doc.body.len() || out.is_empty() {
        out.push(Piece { from: start, to: doc.body.len().max(start), rows: None });
    }
    out
}

/// How many rows at the top of `t` repeat as header rows.
fn header_rows(t: &Table) -> usize {
    t.rows.iter().take_while(|r| r.props.header).count()
}

fn pick(doc: &Document, opts: &Options, window: Window, budget: u64) -> Result<(Option<usize>, usize, Piece), String> {
    let pieces = plan(doc, opts.part_chars, budget);
    let parts = pieces.len();
    match window {
        Window::Part(n) => {
            let n = n.max(1);
            pieces.get(n - 1).map(|p| (Some(n), parts, *p)).ok_or_else(|| {
                format!(
                    "part {n} is out of range: the document has {parts} part{} of up to {} characters",
                    if parts == 1 { "" } else { "s" },
                    opts.part_chars.max(MIN_PART_CHARS)
                )
            })
        }
        Window::Blocks { from, to } => {
            let len = doc.body.len();
            if from >= len {
                return Err(format!("block {from} is out of range: the document has {len} blocks"));
            }
            Ok((None, parts, Piece { from, to: to.clamp(from + 1, len), rows: None }))
        }
    }
}

/// The part of `doc` that `window` names as one self-contained HTML page.
pub fn render_html(doc: &Document, opts: &Options, window: Window) -> Result<Rendered, String> {
    let (part, parts, piece) = pick(doc, opts, window, opts.picture_budget)?;
    let mut w = Walker::new(doc, opts, false);
    w.skip_to(&piece);
    let body = w.body_html(&piece);
    let notes = w.notes_html();
    let mut o = String::with_capacity(body.len() + notes.len() + 4096);
    o.push_str("<!DOCTYPE html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    let _ = write!(o, "<meta name=\"generator\" content=\"WordCraft render_preview\"><title>{}</title><style>{PAGE_CSS}", esc(&opts.title));
    for (class, uri) in &w.css_pictures {
        let _ = write!(o, ".{class}{{background-image:url({uri})}}");
    }
    o.push_str("</style></head><body>\n");
    let _ = write!(o, "<div class=\"file-head\">{}", esc(&opts.title));
    if parts > 1 || part.is_none() {
        match part {
            Some(n) => {
                let _ = write!(o, " · part {n} of {parts}");
            }
            None => {
                let _ = write!(o, " · blocks {}–{} of {}", piece.from + 1, piece.to, doc.body.len());
            }
        }
    }
    o.push_str("</div>\n");
    if w.stats.pictures_omitted > 0 {
        let _ = writeln!(
            o,
            "<div class=\"warn\">⚠ {} picture(s) are shown as placeholders{}.</div>",
            w.stats.pictures_omitted,
            if opts.pictures { " (over this part's picture budget: show a smaller part)" } else { "" }
        );
    }
    o.push_str(&body);
    o.push_str(&notes);
    o.push_str("</body></html>\n");
    Ok(Rendered { text: o, part, parts, piece, total_blocks: doc.body.len(), stats: w.stats, warnings: w.warnings })
}

/// The part of `doc` that `window` names as Markdown for reading.
pub fn render_markdown(doc: &Document, opts: &Options, window: Window) -> Result<Rendered, String> {
    let (part, parts, piece) = pick(doc, opts, window, u64::MAX)?;
    let mut w = Walker::new(doc, opts, true);
    w.skip_to(&piece);
    let mut o = tighten(&w.body_md(&piece));
    let notes = w.notes_md();
    if !notes.is_empty() {
        o.push_str(&notes);
    }
    while o.ends_with('\n') {
        o.pop();
    }
    Ok(Rendered { text: o, part, parts, piece, total_blocks: doc.body.len(), stats: w.stats, warnings: w.warnings })
}

/// How a list item's Markdown ends: a blank line that [`tighten`] takes out between two items.
const LIST_END: &str = "\n\u{1}\n";

/// A tight list: no blank line between two list items; elsewhere the marker is a blank line.
fn tighten(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    let mut rest = md;
    while let Some(i) = rest.find(LIST_END) {
        out.push_str(rest.get(..i).unwrap_or(""));
        let after = rest.get(i + LIST_END.len()..).unwrap_or("");
        let next_is_item = after.trim_start_matches(' ').starts_with("- ") || {
            let t = after.trim_start_matches(' ');
            let n = t.chars().take_while(|c| !c.is_whitespace()).count();
            n > 0 && n <= 12 && t.chars().take(n).any(|c| c.is_alphanumeric()) && t.chars().take(n).last().is_some_and(|c| matches!(c, '.' | ')'))
        };
        out.push_str(if next_is_item { "\n" } else { "\n\n" });
        rest = after;
    }
    out.push_str(rest);
    out
}

const PAGE_CSS: &str = "
body{margin:0;background:#e8e6e1;font-family:Calibri,Carlito,'Segoe UI',Arial,sans-serif;color:#1c1c1c}
.page{position:relative;box-sizing:border-box;max-width:100%;margin:24px auto;background:#fff;box-shadow:0 2px 14px rgba(0,0,0,.18);display:flex;flex-direction:column;overflow:hidden;font-size:11pt;line-height:1.2}
.page .body{flex:1}
.page p,.page h1,.page h2,.page h3,.page h4,.page h5,.page h6{margin:0;font-size:inherit;font-weight:inherit;white-space:pre-wrap;overflow-wrap:break-word}
.page table{border-collapse:collapse;margin:0}
.page td{vertical-align:top;padding:0 5.4pt}
.page img{max-width:100%}
.hf{color:#555;opacity:.85}
.hf.hdr{border-bottom:1px dashed #d8d4cc;margin-bottom:6pt}
.hf.ftr{border-top:1px dashed #d8d4cc;margin-top:6pt}
.ln{display:inline-block;text-indent:0}
.tab{display:inline-block;min-width:2.5em}
.pgbrk{display:block;border-top:1px dashed #b9b4aa;margin:6pt 0;height:0}
.colbrk{display:block;border-top:1px dotted #d8d4cc;margin:4pt 0;height:0}
ins.rv-ins{color:#1a5fb4;text-decoration:underline}
del.rv-del{color:#c01c28;text-decoration:line-through}
ins.rv-mv{color:#26a269;text-decoration:underline double}
del.rv-mv{color:#26a269;text-decoration:line-through double}
.rv-fmt{border-bottom:1px dotted #9141ac}
.pm{font-size:.8em;color:#1a5fb4}
mark.cm{background:#fdeca6;color:inherit}
.cmref,.fnref{font-size:.7em;vertical-align:super;line-height:0}
.cmref a,.fnref a{text-decoration:none;color:#a66f00}
.fnref a{color:#1a5fb4}
.pic-ph{display:inline-block;box-sizing:border-box;border:1px dashed #b9b4aa;background:#f6f5f2;color:#777;font:11px/1.3 Consolas,monospace;padding:4px;overflow:hidden;vertical-align:bottom;max-width:100%}
.pic{display:inline-block;background-size:100% 100%;background-repeat:no-repeat;vertical-align:bottom;max-width:100%}
.tbx{box-sizing:border-box;margin:4pt 0;padding:3.6pt 7.2pt;max-width:100%}
.wm{position:absolute;left:0;right:0;top:40%;text-align:center;font-size:64pt;opacity:.12;transform:rotate(-35deg);pointer-events:none;white-space:nowrap}
.notes{display:block;font-size:10pt}
.notes h3{font:600 12px/1.4 'Segoe UI',Arial,sans-serif;color:#555;margin:12pt 0 4pt;border-bottom:1px solid #e0ddd6;white-space:normal}
.notes ol{margin:0;padding-inline-start:2em}
.notes li{margin:4pt 0}
.cmeta{font:11px/1.4 'Segoe UI',Arial,sans-serif;color:#666}
.creply{margin:4pt 0 0 1.5em;border-inline-start:2px solid #eee;padding-inline-start:6pt}
.file-head{max-width:850px;margin:14px auto 0;font:12px/1.4 Consolas,monospace;color:#666}
.warn{max-width:850px;margin:8px auto;font:12px/1.4 Consolas,monospace;color:#a66f00}
@media (max-width:900px){.page{margin:0;padding:24px 18px !important;width:auto !important;min-height:0 !important}}
";

// ---------------------------------------------------------------------------------------------
// The walk

/// A piece of a paragraph, between the paragraph's text and its HTML or Markdown.
enum Seg {
    Text {
        text: String,
        rc: Box<ResolvedChar>,
    },
    Tab,
    Br,
    PageBreak,
    ColumnBreak,
    /// Ready markup (a picture, a note reference, an equation…), with the run's changes.
    Raw {
        html: String,
        md: String,
        ins: Option<u32>,
        del: Option<u32>,
    },
    CommentStart(u32),
    CommentEnd(u32),
    Anchor(String),
}

/// The table-style formatting a cell's paragraphs take.
#[derive(Clone, Copy, Default)]
struct Ctx<'t> {
    para: Option<&'t ParaProps>,
    chr: Option<&'t CharProps>,
    /// Inside a table, a text box, a header, a note or a comment: page breaks stay inline.
    nested: bool,
}

struct Walker<'a> {
    doc: &'a Document,
    md: bool,
    markup: bool,
    pictures: bool,
    picture_budget: u64,
    counters: Counters,
    /// List counters of headers, text boxes, notes and comments (not counted with the body's).
    aux_counters: Counters,
    aux: usize,
    style_num: HashMap<String, Option<NumRef>>,
    comment_no: HashMap<u32, usize>,
    open: Vec<u32>,
    footnotes: u32,
    endnotes: u32,
    /// Notes referenced in this part: (kind, story, label).
    notes: Vec<(NoteKind, u32, String)>,
    /// Comments starting in this part, in order.
    comments: Vec<u32>,
    in_note: bool,
    /// Drawing headers and footers: pictures become CSS classes, embedded once.
    in_hf: bool,
    css_pictures: Vec<(String, String)>,
    css_by_media: HashMap<String, String>,
    hf_cache: HashMap<u32, String>,
    revs: [HashSet<u32>; 3],
    stats: Stats,
    warnings: Vec<String>,
}

fn sections(doc: &Document) -> Vec<(usize, &SectionProps)> {
    doc.sections()
}

impl<'a> Walker<'a> {
    fn new(doc: &'a Document, opts: &Options, md: bool) -> Self {
        Walker {
            doc,
            md,
            markup: opts.markup,
            pictures: opts.pictures,
            picture_budget: opts.picture_budget,
            counters: Counters::default(),
            aux_counters: Counters::default(),
            aux: 0,
            style_num: HashMap::new(),
            comment_no: HashMap::new(),
            open: Vec::new(),
            footnotes: 0,
            endnotes: 0,
            notes: Vec::new(),
            comments: Vec::new(),
            in_note: false,
            in_hf: false,
            css_pictures: Vec::new(),
            css_by_media: HashMap::new(),
            hf_cache: HashMap::new(),
            revs: Default::default(),
            stats: Stats::default(),
            warnings: Vec::new(),
        }
    }

    // --- counting what comes before the part -------------------------------------------------

    fn skip_to(&mut self, piece: &Piece) {
        let doc = self.doc;
        for b in doc.body.iter().take(piece.from) {
            self.count_block(b, 0);
        }
        if let Some((r0, _)) = piece.rows
            && let Some(Block::Table(t)) = doc.body.get(piece.from).map(|b| &**b)
        {
            for row in t.rows.iter().take(r0) {
                for c in &row.cells {
                    for b in c.blocks.iter() {
                        self.count_block(b, 1);
                    }
                }
            }
        }
    }

    fn count_block(&mut self, b: &Block, depth: usize) {
        match b {
            Block::Para(p) => {
                let _ = self.list_label(p);
                for o in &p.objects {
                    match o {
                        InlineObject::NoteRef { kind, custom, .. } => {
                            let _ = self.note_label(*kind, custom);
                        }
                        InlineObject::CommentStart { id } => {
                            self.comment_ordinal(*id);
                            if !self.open.contains(id) {
                                self.open.push(*id);
                            }
                        }
                        InlineObject::CommentEnd { id } => self.open.retain(|c| c != id),
                        _ => {}
                    }
                }
            }
            Block::Table(t) if depth < MAX_DEPTH => {
                for r in &t.rows {
                    for c in &r.cells {
                        for b in c.blocks.iter() {
                            self.count_block(b, depth + 1);
                        }
                    }
                }
            }
            Block::Table(_) => {}
        }
    }

    fn numref(&mut self, p: &Paragraph) -> Option<NumRef> {
        if let Some(n) = p.props.numbering {
            return (n.num != 0).then_some(n);
        }
        let style = p.props.style.as_deref().unwrap_or("Normal");
        if let Some(v) = self.style_num.get(style) {
            return *v;
        }
        let v = self.doc.styles.para_style_props(style).0.numbering.filter(|n| n.num != 0);
        self.style_num.insert(style.to_string(), v);
        v
    }

    fn list_label(&mut self, p: &Paragraph) -> Option<(String, Level)> {
        let n = self.numref(p)?;
        let doc = self.doc;
        let c = if self.aux > 0 { &mut self.aux_counters } else { &mut self.counters };
        c.next_label(&doc.numbering, n.num, n.level)
    }

    fn note_label(&mut self, kind: NoteKind, custom: &str) -> String {
        if !custom.is_empty() {
            return custom.chars().filter(|c| !c.is_control()).take(16).collect();
        }
        let (n, format) = match kind {
            NoteKind::Footnote => {
                self.footnotes = self.footnotes.saturating_add(1);
                (self.footnotes, self.doc.settings.footnote_format)
            }
            NoteKind::Endnote => {
                self.endnotes = self.endnotes.saturating_add(1);
                (self.endnotes, self.doc.settings.endnote_format)
            }
        };
        let s: String = format.format(n).trim().chars().take(16).collect();
        if s.is_empty() { n.to_string() } else { s }
    }

    fn comment_ordinal(&mut self, id: u32) -> usize {
        let next = self.comment_no.len() + 1;
        *self.comment_no.entry(id).or_insert(next)
    }

    // --- body ----------------------------------------------------------------------------------

    /// For each body block of the piece: the section it is in (index into `sections`).
    fn section_of(secs: &[(usize, &SectionProps)], i: usize) -> usize {
        secs.iter().position(|(end, _)| *end >= i).unwrap_or(secs.len().saturating_sub(1))
    }

    /// The header and footer stories section `k` shows: its own, else those it inherits.
    fn hf_of(secs: &[(usize, &SectionProps)], k: usize, first_sheet: bool) -> (Option<u32>, Option<u32>) {
        let (mut hd, mut hf, mut fd, mut ff) = (None, None, None, None);
        for (_, s) in secs.iter().take(k + 1) {
            hd = s.headers.default.or(hd);
            hf = s.headers.first.or(hf);
            fd = s.footers.default.or(fd);
            ff = s.footers.first.or(ff);
        }
        let title = secs.get(k).is_some_and(|(_, s)| s.title_page);
        if first_sheet && title { (hf, ff) } else { (hd, fd) }
    }

    fn sheet_open(&mut self, sect: &SectionProps, header: Option<u32>, out: &mut String) {
        let num = |v: f32, lo: f32, hi: f32| if v.is_finite() { v.clamp(lo, hi) } else { lo };
        let w = num(sect.page_w, 72.0, 4000.0);
        let h = num(sect.page_h, 72.0, 4000.0);
        let (mt, mb) = (num(sect.margin_top.abs(), 0.0, 1000.0), num(sect.margin_bottom.abs(), 0.0, 1000.0));
        let (ml, mr) = (num(sect.margin_left, 0.0, 1000.0), num(sect.margin_right, 0.0, 1000.0));
        let (hd, fd) = (num(sect.header, 0.0, mt), num(sect.footer, 0.0, mb));
        let bg = self.doc.settings.page_color.map(|c| format!("background:#{};", c.hex())).unwrap_or_default();
        let _ = write!(
            out,
            "<div class=\"page\" style=\"width:{}pt;min-height:{}pt;padding:{}pt {}pt {}pt {}pt;{bg}\">",
            f(w),
            f(h),
            f(hd),
            f(mr),
            f(fd),
            f(ml)
        );
        if let Some(wm) = &self.doc.settings.watermark
            && !wm.text.trim().is_empty()
        {
            let _ = write!(out, "<div class=\"wm\" style=\"color:#{}\">{}</div>", wm.color.hex(), esc(&wm.text));
        }
        let room = (mt - hd).max(0.0);
        out.push_str(&format!("<div class=\"hf hdr\" style=\"min-height:{}pt\">", f(room)));
        if let Some(id) = header {
            out.push_str(&self.hf_html(id));
        }
        out.push_str("</div>");
        let cols = sect.columns.count.clamp(1, 12);
        if cols > 1 {
            let _ = write!(out, "<div class=\"body\" style=\"column-count:{cols};column-gap:{}pt\">", f(num(sect.columns.space, 0.0, 200.0)));
        } else {
            out.push_str("<div class=\"body\">");
        }
    }

    fn sheet_close(&mut self, sect: &SectionProps, footer: Option<u32>, out: &mut String) {
        let mb = if sect.margin_bottom.is_finite() { sect.margin_bottom.abs().clamp(0.0, 1000.0) } else { 72.0 };
        let fd = if sect.footer.is_finite() { sect.footer.clamp(0.0, mb) } else { 0.0 };
        out.push_str("</div>");
        let _ = write!(out, "<div class=\"hf ftr\" style=\"min-height:{}pt\">", f((mb - fd).max(0.0)));
        if let Some(id) = footer {
            out.push_str(&self.hf_html(id));
        }
        out.push_str("</div></div>\n");
    }

    fn hf_html(&mut self, id: u32) -> String {
        if let Some(h) = self.hf_cache.get(&id) {
            return h.clone();
        }
        let doc = self.doc;
        let Some(part) = doc.parts.get(&id) else { return String::new() };
        let was = self.in_hf;
        self.in_hf = true;
        self.aux += 1;
        let mut o = String::new();
        self.blocks_html(&part.blocks, Ctx { nested: true, ..Ctx::default() }, 0, &mut o);
        self.aux -= 1;
        self.in_hf = was;
        self.hf_cache.insert(id, o.clone());
        o
    }

    fn body_html(&mut self, piece: &Piece) -> String {
        let doc = self.doc;
        let secs = sections(doc);
        let mut out = String::new();
        let mut k = Self::section_of(&secs, piece.from);
        let sect_start = |k: usize| if k == 0 { 0 } else { secs.get(k - 1).map_or(0, |(e, _)| e + 1) };
        let first_sheet = piece.from == sect_start(k) && piece.rows.is_none_or(|(r0, _)| r0 == 0);
        let fallback = SectionProps::default();
        let sect = |k: usize| secs.get(k).map_or(&fallback, |(_, s)| *s);
        let (mut hdr, mut ftr) = Self::hf_of(&secs, k, first_sheet);
        self.sheet_open(sect(k), hdr, &mut out);
        let mut at_start = true;
        for i in piece.from..piece.to {
            let Some(b) = doc.body.get(i) else { break };
            match &**b {
                Block::Para(p) => {
                    let rp_break = p.props.page_break_before.unwrap_or(false) || doc.styles.resolve_para(&p.props).page_break_before;
                    let neighbours = (style_of(doc.body.get(i.wrapping_sub(1))), style_of(doc.body.get(i + 1)));
                    let po = self.para_html(p, Ctx::default(), neighbours);
                    if (rp_break || po.break_before) && !at_start {
                        self.sheet_close(sect(k), ftr, &mut out);
                        (hdr, ftr) = Self::hf_of(&secs, k, false);
                        self.sheet_open(sect(k), hdr, &mut out);
                    }
                    out.push_str(&po.html);
                    at_start = po.html.is_empty() && at_start;
                    if po.break_after {
                        self.sheet_close(sect(k), ftr, &mut out);
                        (hdr, ftr) = Self::hf_of(&secs, k, false);
                        self.sheet_open(sect(k), hdr, &mut out);
                        at_start = true;
                    }
                }
                Block::Table(t) => {
                    let rows = if i == piece.from { piece.rows } else { None };
                    self.table_html(t, rows, 0, &mut out);
                    at_start = false;
                }
            }
            // The end of a section: a new sheet unless the next one starts on the same page.
            if let Some((end, _)) = secs.get(k)
                && *end == i
                && i + 1 < piece.to
                && k + 1 < secs.len()
            {
                k += 1;
                if sect(k).start != SectionStart::Continuous {
                    self.sheet_close(sect(k - 1), ftr, &mut out);
                    (hdr, ftr) = Self::hf_of(&secs, k, true);
                    self.sheet_open(sect(k), hdr, &mut out);
                    at_start = true;
                }
            }
        }
        self.sheet_close(sect(k), ftr, &mut out);
        out
    }

    fn blocks_html(&mut self, bl: &Blocks, ctx: Ctx<'_>, depth: usize, out: &mut String) {
        for (i, b) in bl.iter().enumerate() {
            match &**b {
                Block::Para(p) => {
                    let neighbours = (style_of(bl.get(i.wrapping_sub(1))), style_of(bl.get(i + 1)));
                    let po = self.para_html(p, ctx, neighbours);
                    out.push_str(&po.html);
                }
                Block::Table(t) if depth < MAX_DEPTH => self.table_html(t, None, depth + 1, out),
                Block::Table(t) => {
                    for r in &t.rows {
                        for c in &r.cells {
                            self.blocks_html(&c.blocks, ctx, depth, out);
                        }
                    }
                }
            }
        }
    }

    // --- paragraphs ----------------------------------------------------------------------------

    fn segments(&mut self, p: &Paragraph, style: &str, chr: Option<&CharProps>, deferred: &mut Vec<(String, String)>) -> Vec<Seg> {
        let doc = self.doc;
        let mut segs = Vec::new();
        let mut k = 0usize;
        for (range, props) in p.run_ranges() {
            let Some(text) = p.text.get(range) else { continue };
            let rc = doc.styles.resolve_char_in(Some(style), chr, props);
            let hidden = rc.hidden || (rc.del.is_some() && !self.markup);
            let mut buf = String::new();
            let flush = |buf: &mut String, segs: &mut Vec<Seg>, rc: &ResolvedChar| {
                if !buf.is_empty() {
                    segs.push(Seg::Text { text: symbol_text(&rc.font, &std::mem::take(buf)), rc: Box::new(rc.clone()) });
                }
            };
            for c in text.chars() {
                if c == OBJ {
                    let obj = p.objects.get(k);
                    k += 1;
                    flush(&mut buf, &mut segs, &rc);
                    if let Some(o) = obj {
                        self.object(o, &rc, hidden, &mut segs, deferred);
                    }
                    continue;
                }
                if hidden {
                    continue;
                }
                match c {
                    LINE_BREAK => {
                        flush(&mut buf, &mut segs, &rc);
                        segs.push(Seg::Br);
                    }
                    PAGE_BREAK => {
                        flush(&mut buf, &mut segs, &rc);
                        segs.push(Seg::PageBreak);
                    }
                    COLUMN_BREAK => {
                        flush(&mut buf, &mut segs, &rc);
                        segs.push(Seg::ColumnBreak);
                    }
                    '\t' => {
                        flush(&mut buf, &mut segs, &rc);
                        segs.push(Seg::Tab);
                    }
                    '\r' => {}
                    c if c.is_control() => {}
                    c => buf.push(c),
                }
            }
            flush(&mut buf, &mut segs, &rc);
        }
        segs
    }

    fn object(&mut self, o: &InlineObject, rc: &ResolvedChar, hidden: bool, segs: &mut Vec<Seg>, deferred: &mut Vec<(String, String)>) {
        let raw = |html: String, md: String| Seg::Raw { html, md, ins: rc.ins, del: rc.del };
        match o {
            InlineObject::CommentStart { id } => {
                if !self.markup {
                    return;
                }
                if self.aux == 0 {
                    self.comment_ordinal(*id);
                }
                segs.push(Seg::CommentStart(*id));
            }
            InlineObject::CommentEnd { id } => {
                if self.markup {
                    segs.push(Seg::CommentEnd(*id));
                }
            }
            InlineObject::BookmarkStart { name } if !hidden && name != "_GoBack" => segs.push(Seg::Anchor(name.clone())),
            InlineObject::NoteRef { kind, id, custom } => {
                if self.in_note {
                    if !hidden {
                        segs.push(raw(String::new(), String::new()));
                    }
                    return;
                }
                let label = self.note_label(*kind, custom);
                if hidden {
                    return;
                }
                let anchor = format!("{}-{}", if *kind == NoteKind::Footnote { "fn" } else { "en" }, id);
                self.notes.push((*kind, *id, label.clone()));
                match kind {
                    NoteKind::Footnote => self.stats.footnotes += 1,
                    NoteKind::Endnote => self.stats.endnotes += 1,
                }
                segs.push(raw(
                    format!("<span class=\"fnref\"><a href=\"#{anchor}\" id=\"r{anchor}\">{}</a></span>", esc(&label)),
                    format!("[^{}{}]", if *kind == NoteKind::Footnote { "" } else { "e" }, md_ref(&label)),
                ));
            }
            _ if hidden => {}
            InlineObject::Image { media, w, h, alt, float, crop, .. } => {
                if float.hidden {
                    return;
                }
                let (html, md) = self.picture(media, *w, *h, alt, *crop, Some(float));
                segs.push(raw(html, md));
            }
            InlineObject::Graphic { w, h, alt, graphic, float } => {
                if float.hidden {
                    return;
                }
                let noun = graphic.kind.noun();
                let label = if alt.trim().is_empty() { noun.to_string() } else { alt.trim().to_string() };
                let html = self.graphic_svg(graphic, *w, *h, &label);
                segs.push(raw(html, format!("[{noun}: {}]", md_text(&label))));
            }
            InlineObject::Shape { kind, w, h, fill, stroke, stroke_width, float, story, .. } => {
                if float.hidden {
                    return;
                }
                match story.and_then(|id| self.doc.parts.get(&id).map(|p| (id, p))) {
                    Some((_, part)) if self.aux < MAX_BOX_DEPTH => {
                        // After the paragraph it is anchored in.
                        deferred.push(self.text_box(&part.blocks, *w, *fill, *stroke, *stroke_width));
                    }
                    _ => {
                        let svg = shape_svg(*kind, *w, *h, *fill, *stroke, *stroke_width);
                        segs.push(raw(svg, String::new()));
                    }
                }
            }
            InlineObject::Group { children, float, .. } => {
                if float.hidden {
                    return;
                }
                let (mut html, mut md) = (String::from("<div class=\"tbx\">"), String::new());
                for c in children {
                    match &c.obj {
                        InlineObject::Image { media, w, h, alt, crop, .. } => {
                            let (a, b) = self.picture(media, *w, *h, alt, *crop, None);
                            html.push_str(&a);
                            md.push_str(&b);
                            md.push(' ');
                        }
                        InlineObject::Shape { w, fill, stroke, stroke_width, story: Some(id), .. } if self.aux < MAX_BOX_DEPTH => {
                            if let Some(part) = self.doc.parts.get(id) {
                                let (a, b) = self.text_box(&part.blocks, *w, *fill, *stroke, *stroke_width);
                                html.push_str(&a);
                                md.push_str(&b);
                            }
                        }
                        _ => {}
                    }
                }
                html.push_str("</div>");
                deferred.push((html, md.trim().to_string()));
            }
            InlineObject::Field { instr, result, .. } => {
                let shown = if result.is_empty() {
                    match instr.split_whitespace().next().unwrap_or("").to_ascii_uppercase().as_str() {
                        "PAGE" | "NUMPAGES" | "SECTIONPAGES" | "SECTION" => "#".to_string(),
                        _ => String::new(),
                    }
                } else {
                    result.clone()
                };
                let shown: String = shown.chars().filter(|c| !c.is_control() || *c == '\t').collect();
                if !shown.is_empty() {
                    segs.push(Seg::Text { text: symbol_text(&rc.font, &shown), rc: Box::new(rc.clone()) });
                }
            }
            InlineObject::Equation { linear, display, math } => {
                let linear = if linear.is_empty() { wordcraft_doc::math::to_linear(&math.nodes) } else { linear.clone() };
                let html = if math.nodes.is_empty() {
                    crate::mathml::linear_to_mathml(&linear, *display)
                } else {
                    crate::mathml::to_mathml(&math.nodes, *display, &linear)
                };
                let md = if *display { format!("$${}$$", linear) } else { format!("${}$", linear) };
                segs.push(raw(html, md));
            }
            InlineObject::Ruby { base, ruby, .. } => {
                segs.push(raw(format!("<ruby>{}<rt>{}</rt></ruby>", esc(base), esc(ruby)), format!("{} ({})", md_text(base), md_text(ruby))));
            }
            InlineObject::Opaque { text, .. } => {
                if !text.is_empty() {
                    let t: String = text.chars().filter(|c| !c.is_control()).collect();
                    segs.push(Seg::Text { text: symbol_text(&rc.font, &t), rc: Box::new(rc.clone()) });
                }
            }
            other => {
                let t: String = other.plain_text().chars().filter(|c| !c.is_control()).collect();
                if !t.is_empty() {
                    segs.push(Seg::Text { text: symbol_text(&rc.font, &t), rc: Box::new(rc.clone()) });
                }
            }
        }
    }

    fn text_box(&mut self, blocks: &Blocks, w: f32, fill: Option<Rgb>, stroke: Option<Rgb>, sw: f32) -> (String, String) {
        self.aux += 1;
        let mut html = String::new();
        let mut md = String::new();
        if self.md {
            md = self.blocks_md(blocks, true, 0).trim().to_string();
        } else {
            let mut css = format!("width:{}pt;", f(w.clamp(10.0, 4000.0)));
            if let Some(c) = fill {
                let _ = write!(css, "background:#{};", c.hex());
            }
            if let Some(c) = stroke {
                let _ = write!(css, "border:{}pt solid #{};", f(sw.clamp(0.25, 6.0)), c.hex());
            }
            let _ = write!(html, "<div class=\"tbx\" style=\"{css}\">");
            self.blocks_html(blocks, Ctx { nested: true, ..Ctx::default() }, 0, &mut html);
            html.push_str("</div>");
        }
        self.aux -= 1;
        (html, if md.is_empty() { md } else { format!("> [text box] {}", md.replace('\n', "\n> ")) })
    }

    fn picture(&mut self, media: &str, w: f32, h: f32, alt: &str, crop: [f32; 4], float: Option<&Float>) -> (String, String) {
        let w = if w.is_finite() { w.clamp(1.0, 4000.0) } else { 72.0 };
        let h = if h.is_finite() { h.clamp(1.0, 4000.0) } else { 72.0 };
        let md = format!("![{}](picture:{} {}x{}pt)", md_text(alt).replace(']', "\\]"), media.replace([' ', ')'], "_"), f(w), f(h));
        if self.md {
            self.stats.pictures += 1;
            return (String::new(), md);
        }
        let placeholder = |why: &str| {
            format!(
                "<span class=\"pic-ph\" style=\"width:{}pt;height:{}pt\" title=\"{}\">{}</span>",
                f(w),
                f(h),
                esc(alt),
                esc(if alt.trim().is_empty() { why } else { alt })
            )
        };
        if !self.pictures {
            self.stats.pictures_omitted += 1;
            return (placeholder("picture"), md);
        }
        // Repeated pictures (headers, footers) are embedded once, as a class.
        let css_class = if self.in_hf { self.css_by_media.get(media).cloned() } else { None };
        let uri = match css_class {
            Some(_) => None,
            None => {
                let len = self.doc.media.byte_len(media).unwrap_or(0);
                if self.stats.picture_bytes.saturating_add(len) > self.picture_budget && self.stats.pictures > 0 {
                    self.stats.pictures_omitted += 1;
                    return (placeholder("picture (over the part's picture budget)"), md);
                }
                let Some(bytes) = self.doc.media.get(media) else {
                    self.stats.pictures_omitted += 1;
                    return (placeholder("picture (missing)"), md);
                };
                match browser_picture(&bytes) {
                    Some((mime, data)) => {
                        self.stats.picture_bytes = self.stats.picture_bytes.saturating_add(data.len() as u64);
                        Some(format!("data:{mime};base64,{}", base64_encode(&data)))
                    }
                    None => {
                        self.stats.pictures_omitted += 1;
                        let kind = if bytes.get(..4) == Some(&[1, 0, 0, 0]) { "EMF picture" } else { "picture (format browsers do not show)" };
                        return (placeholder(kind), md);
                    }
                }
            }
        };
        self.stats.pictures += 1;
        let mut box_css = String::new();
        if let Some(fl) = float {
            match fl.wrap {
                Wrap::Square | Wrap::Tight | Wrap::Through => {
                    let right = fl.h_align == Some(FloatAlign::End) || (fl.h_align.is_none() && fl.x > 200.0);
                    box_css.push_str(if right { "float:right;margin:0 0 6pt 9pt;" } else { "float:left;margin:0 9pt 6pt 0;" });
                }
                Wrap::TopAndBottom => {
                    box_css.push_str(if fl.h_align == Some(FloatAlign::Center) {
                        "display:block;margin:6pt auto;"
                    } else {
                        "display:block;margin:6pt 0;"
                    });
                }
                _ => {}
            }
        }
        let cropped = crop.iter().any(|c| c.is_finite() && *c > 0.0005);
        let html = if self.in_hf {
            let class = match self.css_by_media.get(media) {
                Some(c) => c.clone(),
                None => {
                    let c = format!("hfpic{}", self.css_pictures.len() + 1);
                    self.css_pictures.push((c.clone(), uri.unwrap_or_default()));
                    self.css_by_media.insert(media.to_string(), c.clone());
                    c
                }
            };
            format!(
                "<span class=\"pic {class}\" role=\"img\" aria-label=\"{}\" style=\"width:{}pt;height:{}pt;{box_css}\"></span>",
                esc(alt),
                f(w),
                f(h)
            )
        } else if cropped {
            let c = |v: f32| if v.is_finite() { v.clamp(-0.99, 0.99) } else { 0.0 };
            let (l, t, r, b) = (c(crop[0]), c(crop[1]), c(crop[2]), c(crop[3]));
            let fw = w / (1.0 - l - r).clamp(0.01, 10.0);
            let fh = h / (1.0 - t - b).clamp(0.01, 10.0);
            format!(
                "<span style=\"display:inline-block;position:relative;overflow:hidden;width:{}pt;height:{}pt;vertical-align:bottom;{box_css}\"><img src=\"{}\" alt=\"{}\" style=\"position:absolute;max-width:none;left:{}pt;top:{}pt;width:{}pt;height:{}pt\"></span>",
                f(w),
                f(h),
                uri.unwrap_or_default(),
                esc(alt),
                f(-l * fw),
                f(-t * fh),
                f(fw),
                f(fh)
            )
        } else {
            format!(
                "<img src=\"{}\" alt=\"{}\" style=\"width:{}pt;aspect-ratio:{}/{};height:auto;vertical-align:bottom;{box_css}\">",
                uri.unwrap_or_default(),
                esc(alt),
                f(w),
                f(w),
                f(h)
            )
        };
        (html, md)
    }

    fn graphic_svg(&mut self, g: &Graphic, w: f32, h: f32, label: &str) -> String {
        let w = if w.is_finite() { w.clamp(1.0, 4000.0) } else { 200.0 };
        let h = if h.is_finite() { h.clamp(1.0, 4000.0) } else { 150.0 };
        if g.items.is_empty() {
            return format!("<span class=\"pic-ph\" style=\"width:{}pt;height:{}pt\">{}</span>", f(w), f(h), esc(label));
        }
        let (gw, gh) = if g.w > 0.0 && g.h > 0.0 && g.w.is_finite() && g.h.is_finite() { (g.w, g.h) } else { (w, h) };
        let mut o = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {} {}\" width=\"{}pt\" height=\"{}pt\" role=\"img\" aria-label=\"{}\" style=\"max-width:100%;height:auto;vertical-align:bottom\">",
            f(gw),
            f(gh),
            f(w),
            f(h),
            esc(label)
        );
        let paint = |fill: Option<Rgb>, stroke: Option<Rgb>, sw: f32| {
            format!(
                "fill=\"{}\" stroke=\"{}\" stroke-width=\"{}\"",
                fill.map_or("none".to_string(), |c| format!("#{}", c.hex())),
                stroke.map_or("none".to_string(), |c| format!("#{}", c.hex())),
                f(if sw.is_finite() { sw.clamp(0.0, 50.0) } else { 0.0 })
            )
        };
        for it in g.items.iter().take(20_000) {
            match it {
                GraphicItem::Shape { rect, kind, fill, stroke, stroke_width } => {
                    let [x, y, rw, rh] = rect.map(|v| if v.is_finite() { v } else { 0.0 });
                    let p = paint(*fill, *stroke, *stroke_width);
                    if matches!(kind, ShapeKind::Ellipse | ShapeKind::FlowchartOr) {
                        let _ = write!(
                            o,
                            "<ellipse cx=\"{}\" cy=\"{}\" rx=\"{}\" ry=\"{}\" {p}/>",
                            f(x + rw / 2.0),
                            f(y + rh / 2.0),
                            f(rw / 2.0),
                            f(rh / 2.0)
                        );
                    } else {
                        let _ = write!(o, "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" {p}/>", f(x), f(y), f(rw.max(0.0)), f(rh.max(0.0)));
                    }
                }
                GraphicItem::Path { segs, fill, stroke, stroke_width } => {
                    let mut d = String::new();
                    for s in segs.iter().take(100_000) {
                        match s {
                            PathSeg::Move(x, y) => {
                                let _ = write!(d, "M{} {}", f(*x), f(*y));
                            }
                            PathSeg::Line(x, y) => {
                                let _ = write!(d, "L{} {}", f(*x), f(*y));
                            }
                            PathSeg::Cubic(a, b, c, e, x, y) => {
                                let _ = write!(d, "C{} {} {} {} {} {}", f(*a), f(*b), f(*c), f(*e), f(*x), f(*y));
                            }
                            PathSeg::Close => d.push('Z'),
                        }
                    }
                    let _ = write!(o, "<path d=\"{d}\" {}/>", paint(*fill, *stroke, *stroke_width));
                }
                GraphicItem::Text { rect, text, size, color, bold, align, font } => {
                    let [x, y, rw, rh] = rect.map(|v| if v.is_finite() { v } else { 0.0 });
                    let (ax, anchor) = match align {
                        TextAlign::Left => (x, "start"),
                        TextAlign::Center => (x + rw / 2.0, "middle"),
                        TextAlign::Right => (x + rw, "end"),
                    };
                    let fam = font.as_deref().map(|n| format!(" font-family=\"{}\"", esc(&clean_font(n)))).unwrap_or_default();
                    let _ = write!(
                        o,
                        "<text x=\"{}\" y=\"{}\" font-size=\"{}\" fill=\"#{}\" text-anchor=\"{anchor}\" dominant-baseline=\"middle\"{}{fam}>{}</text>",
                        f(ax),
                        f(y + rh / 2.0),
                        f(if size.is_finite() { size.clamp(1.0, 400.0) } else { 10.0 }),
                        color.hex(),
                        if *bold { " font-weight=\"700\"" } else { "" },
                        esc(text)
                    );
                }
                GraphicItem::Image { rect, media } => {
                    let [x, y, rw, rh] = rect.map(|v| if v.is_finite() { v } else { 0.0 });
                    let len = self.doc.media.byte_len(media).unwrap_or(0);
                    if self.pictures
                        && self.stats.picture_bytes.saturating_add(len) <= self.picture_budget
                        && let Some((mime, data)) = self.doc.media.get(media).and_then(|b| browser_picture(&b))
                    {
                        self.stats.picture_bytes = self.stats.picture_bytes.saturating_add(data.len() as u64);
                        let _ = write!(
                            o,
                            "<image x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" preserveAspectRatio=\"none\" href=\"data:{mime};base64,{}\"/>",
                            f(x),
                            f(y),
                            f(rw.max(0.0)),
                            f(rh.max(0.0)),
                            base64_encode(&data)
                        );
                    }
                }
            }
        }
        o.push_str("</svg>");
        o
    }

    /// A paragraph as HTML: its element, and whether a page break leads or ends it.
    fn para_html(&mut self, p: &Paragraph, ctx: Ctx<'_>, neighbours: (Option<String>, Option<String>)) -> ParaOut {
        let doc = self.doc;
        let rp = doc.styles.resolve_para_in(&p.props, ctx.para);
        let style = p.props.style.clone().unwrap_or_else(|| "Normal".into());
        let label = self.list_label(p);
        let mut deferred = Vec::new();
        let mut segs = self.segments(p, &style, ctx.chr, &mut deferred);
        let mut out = ParaOut::default();
        if !ctx.nested && self.aux == 0 {
            while matches!(segs.first(), Some(Seg::PageBreak)) {
                segs.remove(0);
                out.break_before = true;
            }
            while matches!(segs.last(), Some(Seg::PageBreak)) {
                segs.pop();
                out.break_after = true;
            }
        }
        let visible = segs.iter().any(|s| matches!(s, Seg::Text { .. } | Seg::Raw { .. } | Seg::Tab | Seg::Br | Seg::PageBreak | Seg::ColumnBreak));
        // A paragraph holding only a page break, or deleted whole without markup.
        if !visible && label.is_none() && ((out.break_before || out.break_after) || (p.mark.del.is_some() && !self.markup)) {
            self.close_comments_in(&segs);
            for (h, _) in deferred {
                out.html.push_str(&h);
            }
            return out;
        }
        let rtl = if rp.bidi {
            true
        } else if p.props.bidi == Some(false) {
            false
        } else {
            has_letters(&p.text) && wordcraft_doc::bidi::first_strong_is_rtl(&p.text)
        };
        let mut base = doc.styles.resolve_char_in(Some(&style), ctx.chr, &CharProps::default());
        if rtl {
            base = base.complex();
        }
        let level = rp.outline_level.map(|l| (l as usize + 1).min(6));
        let tag = match level {
            Some(l) => format!("h{l}"),
            None => "p".to_string(),
        };
        self.stats.paragraphs += 1;
        if level.is_some() {
            self.stats.headings += 1;
        }
        // Indents: the paragraph's, else the list level's.
        let (mut start, end, mut first) = (rp.indent_left, rp.indent_right, rp.indent_first);
        if let Some((_, lv)) = &label {
            let (left_set, first_set) = doc.styles.indents_over_list(&style);
            if p.props.indent_left.is_none() && !left_set {
                start = lv.indent;
            }
            if p.props.indent_first.is_none() && !first_set {
                first = -lv.hanging;
            }
        }
        let mut css = String::new();
        full_font_css(&base, &mut css);
        let (mut before, mut after) = (rp.space_before, rp.space_after);
        if rp.contextual_spacing {
            if neighbours.0.as_deref() == Some(style.as_str()) {
                before = 0.0;
            }
            if neighbours.1.as_deref() == Some(style.as_str()) {
                after = 0.0;
            }
        }
        let _ = write!(css, "margin-block:{}pt {}pt;", f(before), f(after));
        if start != 0.0 || end != 0.0 {
            let _ = write!(css, "margin-inline:{}pt {}pt;", f(start), f(end));
        }
        if first != 0.0 {
            let _ = write!(css, "text-indent:{}pt;", f(first));
        }
        match rp.align {
            Align::Left => {}
            Align::Center => css.push_str("text-align:center;"),
            Align::Right => css.push_str("text-align:end;"),
            Align::Justify | Align::Distribute => css.push_str("text-align:justify;"),
        }
        match rp.line_spacing {
            LineSpacing::Multiple(m) if (m - 1.0).abs() > 0.001 => {
                let _ = write!(css, "line-height:{};", f(1.2 * m));
            }
            LineSpacing::Exactly(v) => {
                let _ = write!(css, "line-height:{}pt;", f(v));
            }
            LineSpacing::AtLeast(v) if v > 0.0 => {
                let _ = write!(css, "line-height:max(1.2em,{}pt);", f(v));
            }
            _ => {}
        }
        if let Some(c) = rp.shading {
            let _ = write!(css, "background:#{};", c.hex());
        }
        if let Some(b) = &rp.borders {
            borders_css(b, &mut css, true);
        }
        let mut html = format!("<{tag}");
        if rtl {
            html.push_str(" dir=\"rtl\"");
        }
        if let Some(name) = doc.styles.get(&style).map(|s| s.name.as_str()).filter(|n| !n.is_empty() && *n != "Normal") {
            let _ = write!(html, " data-style=\"{}\"", esc(name));
        }
        let _ = write!(html, " style=\"{css}\">");
        if let Some((text, lv)) = &label {
            // The label's font: the paragraph mark's with the level's over it, as layout draws it.
            let lc = doc.styles.resolve_char_in(Some(&style), ctx.chr, &p.mark.clone().overlaid(&lv.chr));
            let lcss = char_css(&lc, &base);
            let hang = if first < 0.0 { -first } else { 0.0 };
            let shown = label_text(text, &lc.font, lv.format == wordcraft_doc::section::NumFormat::Bullet);
            let _ = write!(html, "<span class=\"ln\" style=\"min-width:{}pt;{lcss}\">{}</span>", f(hang), esc(&shown));
            if hang <= 0.0 {
                html.push(' ');
            }
        }
        let inner = self.segs_html(&segs, &base);
        html.push_str(&inner);
        if self.markup
            && let Some(r) = p.mark.ins.or(p.mark.del)
        {
            let ins = p.mark.ins.is_some();
            let _ = write!(html, "<span class=\"pm {}\" title=\"{}\">¶</span>", if ins { "rv-ins" } else { "rv-del" }, esc(&self.rev_title(r, ins)));
            self.count_rev(if ins { 0 } else { 1 }, r);
        }
        if inner.is_empty() && label.is_none() {
            html.push_str("<br>");
        }
        let _ = write!(html, "</{tag}>");
        out.html = html;
        for (h, _) in deferred {
            out.html.push_str(&h);
        }
        out
    }

    fn close_comments_in(&mut self, segs: &[Seg]) {
        for s in segs {
            match s {
                Seg::CommentStart(id) => self.start_comment(*id),
                Seg::CommentEnd(id) => self.open.retain(|c| c != id),
                _ => {}
            }
        }
    }

    fn start_comment(&mut self, id: u32) {
        if !self.open.contains(&id) {
            self.open.push(id);
        }
        if self.aux == 0 && !self.comments.contains(&id) {
            self.comments.push(id);
            self.stats.comments += 1;
        }
    }

    fn rev_title(&self, r: u32, ins: bool) -> String {
        let rev = self.doc.revisions.get(r as usize);
        let what = match rev.and_then(|v| v.move_name.as_ref()) {
            Some(_) if ins => "Moved here",
            Some(_) => "Moved from here",
            None if ins => "Inserted",
            None => "Deleted",
        };
        let who = rev.map(|v| v.author.trim()).filter(|a| !a.is_empty()).unwrap_or("unknown author");
        match rev.map(|v| short_date(&v.date)).filter(|d| !d.is_empty()) {
            Some(d) => format!("{what} by {who}, {d}"),
            None => format!("{what} by {who}"),
        }
    }

    fn count_rev(&mut self, k: usize, r: u32) {
        if let Some(set) = self.revs.get_mut(k)
            && set.insert(r)
        {
            match k {
                0 => self.stats.insertions += 1,
                1 => self.stats.deletions += 1,
                _ => self.stats.format_changes += 1,
            }
        }
    }

    fn wrap_rev(&mut self, mut s: String, ins: Option<u32>, del: Option<u32>, fmt: Option<u32>) -> String {
        if !self.markup {
            return s;
        }
        if let Some(r) = fmt {
            let who = self.doc.revisions.get(r as usize).map(|v| v.author.clone()).unwrap_or_default();
            s = format!("<span class=\"rv-fmt\" title=\"Formatted by {}\">{s}</span>", esc(&who));
            self.count_rev(2, r);
        }
        if let Some(r) = del {
            let mv = self.doc.move_name(Some(r)).is_some();
            s = format!("<del class=\"{}\" title=\"{}\">{s}</del>", if mv { "rv-mv" } else { "rv-del" }, esc(&self.rev_title(r, false)));
            self.count_rev(1, r);
        }
        if let Some(r) = ins {
            let mv = self.doc.move_name(Some(r)).is_some();
            s = format!("<ins class=\"{}\" title=\"{}\">{s}</ins>", if mv { "rv-mv" } else { "rv-ins" }, esc(&self.rev_title(r, true)));
            self.count_rev(0, r);
        }
        s
    }

    fn mark_comments(&self, s: String) -> String {
        if self.open.is_empty() || !self.markup {
            return s;
        }
        let nos: Vec<String> = self.open.iter().filter_map(|id| self.comment_no.get(id)).map(|n| n.to_string()).collect();
        format!("<mark class=\"cm\" data-comment=\"{}\">{s}</mark>", nos.join(" "))
    }

    fn segs_html(&mut self, segs: &[Seg], base: &ResolvedChar) -> String {
        let mut o = String::new();
        for s in segs {
            match s {
                Seg::Text { text, rc } => {
                    self.stats.characters += text.chars().count();
                    let eff =
                        if text.chars().find(|c| !c.is_whitespace()).is_some_and(|c| rc.uses_complex(c)) { rc.complex() } else { (**rc).clone() };
                    let mut h = esc_text(text);
                    let css = char_css(&eff, base);
                    if !css.is_empty() {
                        h = format!("<span style=\"{css}\">{h}</span>");
                    }
                    match eff.vert_align {
                        VertAlign::Superscript => h = format!("<sup>{h}</sup>"),
                        VertAlign::Subscript => h = format!("<sub>{h}</sub>"),
                        VertAlign::Baseline => {}
                    }
                    if eff.rtl {
                        h = format!("<span dir=\"rtl\">{h}</span>");
                    }
                    h = self.mark_comments(h);
                    h = self.wrap_rev(h, rc.ins, rc.del, rc.fmt);
                    if let Some(href) = rc.link.as_deref().and_then(safe_href) {
                        h = format!(
                            "<a href=\"{}\"{}>{h}</a>",
                            esc(&href),
                            if href.starts_with('#') { "" } else { " target=\"_blank\" rel=\"noopener\"" }
                        );
                    }
                    o.push_str(&h);
                }
                Seg::Tab => o.push_str("<span class=\"tab\">\t</span>"),
                Seg::Br => o.push_str("<br>"),
                Seg::PageBreak => o.push_str("<span class=\"pgbrk\" role=\"separator\" title=\"Page break\"></span>"),
                Seg::ColumnBreak => o.push_str("<span class=\"colbrk\" role=\"separator\" title=\"Column break\"></span>"),
                Seg::Raw { html, ins, del, .. } => {
                    let h = self.mark_comments(html.clone());
                    let h = self.wrap_rev(h, *ins, *del, None);
                    o.push_str(&h);
                }
                Seg::CommentStart(id) => self.start_comment(*id),
                Seg::CommentEnd(id) => {
                    self.open.retain(|c| c != id);
                    if let Some(n) = self.comment_no.get(id) {
                        let _ =
                            write!(o, "<span class=\"cmref\"><a href=\"#cm-{id}\" title=\"{}\">[C{n}]</a></span>", esc(&self.comment_summary(*id)));
                    }
                }
                Seg::Anchor(name) => {
                    let _ = write!(o, "<a id=\"{}\"></a>", esc(&anchor_id(name)));
                }
            }
        }
        o
    }

    fn comment_summary(&self, id: u32) -> String {
        let Some(c) = self.doc.comments.get(&id) else { return String::new() };
        let text = self.doc.parts.get(&c.part).map(|p| blocks_text(&p.blocks)).unwrap_or_default();
        let text: String = text.chars().take(300).collect();
        format!("{}: {}", if c.author.trim().is_empty() { "Comment" } else { c.author.trim() }, text)
    }

    // --- tables --------------------------------------------------------------------------------

    fn table_html(&mut self, t: &Table, window: Option<(usize, usize)>, depth: usize, out: &mut String) {
        let doc = self.doc;
        self.stats.tables += 1;
        let ts: Option<TableStyleProps> = t.props.style.as_deref().and_then(|s| doc.styles.table_style(s));
        let ncols = t.cols().max(1);
        let nrows = t.rows.len();
        let (r0, r1) = window.map_or((0, nrows), |(a, b)| (a.min(nrows), b.min(nrows)));
        let heads = if r0 > 0 { header_rows(t).min(r0) } else { 0 };
        let rows: Vec<usize> = (0..heads).chain(r0..r1).collect();
        // Table-level borders: the style's whole-table borders under the table's own.
        let mut tb = ts.as_ref().and_then(|s| s.parts.borders).unwrap_or_default();
        if let Some(own) = &t.props.borders {
            tb.overlay(own);
        }
        let margins = t.props.cell_margins.or(ts.as_ref().and_then(|s| s.parts.cell_margins)).unwrap_or([0.0, 5.4, 0.0, 5.4]);
        let grid_total: f32 = t.grid.iter().filter(|w| w.is_finite() && **w > 0.0).sum();
        let mut css = String::new();
        if let Some(pct) = t.props.width_pct.filter(|v| v.is_finite() && *v > 0.0) {
            let _ = write!(css, "width:{}%;", f(pct.min(100.0)));
        } else if let Some(w) = t.props.width.filter(|v| v.is_finite() && *v > 0.0) {
            let _ = write!(css, "width:{}pt;", f(w.min(4000.0)));
        } else if grid_total > 0.0 {
            let _ = write!(css, "width:{}pt;", f(grid_total.min(4000.0)));
        }
        css.push_str("max-width:100%;");
        match t.props.align {
            Some(Align::Center) => css.push_str("margin-inline:auto;"),
            Some(Align::Right) => css.push_str("margin-inline-start:auto;"),
            _ => {
                if let Some(i) = t.props.indent.filter(|v| v.is_finite() && *v != 0.0) {
                    let _ = write!(css, "margin-inline-start:{}pt;", f(i.clamp(-1000.0, 1000.0)));
                }
            }
        }
        if let Some(c) = t.props.shading {
            let _ = write!(css, "background:#{};", c.hex());
        }
        let _ = write!(out, "<table{} style=\"{css}\">", if t.props.bidi_visual { " dir=\"rtl\"" } else { "" });
        if grid_total > 0.0 {
            out.push_str("<colgroup>");
            for w in &t.grid {
                let _ = write!(out, "<col style=\"width:{}pt\">", f(if w.is_finite() { w.clamp(0.0, 4000.0) } else { 0.0 }));
            }
            out.push_str("</colgroup>");
        }
        let band = ts.as_ref().and_then(|s| s.parts.band_size).unwrap_or(1);
        let last = rows.last().copied().unwrap_or(0);
        for (pos, &ri) in rows.iter().enumerate() {
            let Some(row) = t.rows.get(ri) else { continue };
            let mut rcss = String::new();
            if let Some(hgt) = row.props.height.filter(|v| v.is_finite() && *v > 0.0) {
                let _ = write!(rcss, "height:{}pt;", f(hgt.min(4000.0)));
                if row.props.height_rule == HeightRule::Exact {
                    rcss.push_str("overflow:hidden;");
                }
            }
            if rcss.is_empty() {
                out.push_str("<tr>");
            } else {
                let _ = write!(out, "<tr style=\"{rcss}\">");
            }
            let mut g = row.props.grid_before as usize;
            if g > 0 {
                let _ = write!(out, "<td colspan=\"{}\" style=\"border:none;padding:0\"></td>", g.min(63));
            }
            for (ci, cell) in row.cells.iter().enumerate() {
                let span = cell.span();
                // A cell continuing a vertical merge is drawn by the cell above (unless that one is
                // in an earlier part).
                if cell.props.vmerge == VMerge::Continue && pos > 0 && pos != heads {
                    g += span;
                    continue;
                }
                let mut rowspan = 1usize;
                if cell.props.vmerge != VMerge::None || pos == heads {
                    for &below in rows.iter().skip(pos + 1) {
                        if below < ri || (pos < heads && below >= r0) {
                            break;
                        }
                        let cont = t
                            .cell_at_grid(below, t.grid_col(ri, ci))
                            .and_then(|k| t.rows.get(below).and_then(|r| r.cells.get(k)))
                            .is_some_and(|c| c.props.vmerge == VMerge::Continue);
                        if !cont {
                            break;
                        }
                        rowspan += 1;
                    }
                }
                let region = PartSet::of_cell(&t.props.look, band, (ri, nrows), (g, span, ncols));
                let mut fill = None;
                let mut rb = Borders::default();
                let mut chr = CharProps::default();
                let mut para = ts.as_ref().map(|s| s.para.clone()).unwrap_or_default();
                let mut valign = None;
                if let Some(st) = &ts {
                    for part in region.iter() {
                        let pf = st.part(part);
                        chr.overlay(&pf.chr);
                        if pf.align.is_some() {
                            para.align = pf.align;
                        }
                        fill = pf.fill.or(fill);
                        valign = pf.v_align.or(valign);
                        if part != TablePart::WholeTable
                            && let Some(b) = pf.borders
                        {
                            rb.overlay(&b);
                        }
                    }
                }
                let mut cb = Borders::default();
                let outer_top = pos == 0;
                let outer_bottom = ri + rowspan.saturating_sub(1) >= last;
                cb.top = if outer_top { tb.top } else { tb.between };
                cb.bottom = if outer_bottom { tb.bottom } else { tb.between };
                cb.left = if g == 0 { tb.left } else { tb.inside_v };
                cb.right = if g + span >= ncols { tb.right } else { tb.inside_v };
                cb.overlay(&rb);
                if let Some(own) = &cell.props.borders {
                    cb.overlay(own);
                }
                let mut ccss = String::new();
                if let Some(c) = cell.props.shading.or(fill) {
                    let _ = write!(ccss, "background:#{};", c.hex());
                }
                borders_css(&cb, &mut ccss, false);
                let m = cell.props.margins.unwrap_or(margins);
                let mm = |v: f32| if v.is_finite() { v.clamp(0.0, 200.0) } else { 0.0 };
                let _ = write!(ccss, "padding:{}pt {}pt {}pt {}pt;", f(mm(m[0])), f(mm(m[3])), f(mm(m[2])), f(mm(m[1])));
                match cell.props.valign.or(valign) {
                    Some(VAlign::Center) => ccss.push_str("vertical-align:middle;"),
                    Some(VAlign::Bottom) => ccss.push_str("vertical-align:bottom;"),
                    _ => {}
                }
                match cell.props.text_direction {
                    TextDirection::Down => ccss.push_str("writing-mode:vertical-rl;"),
                    TextDirection::Up => ccss.push_str("writing-mode:vertical-rl;transform:rotate(180deg);"),
                    TextDirection::Horizontal => {}
                }
                out.push_str("<td");
                if span > 1 {
                    let _ = write!(out, " colspan=\"{span}\"");
                }
                if rowspan > 1 {
                    let _ = write!(out, " rowspan=\"{rowspan}\"");
                }
                let _ = write!(out, " style=\"{ccss}\">");
                let has_style = ts.is_some();
                let ctx = Ctx { para: has_style.then_some(&para), chr: has_style.then_some(&chr), nested: true };
                if depth < MAX_DEPTH {
                    self.blocks_html(&cell.blocks, ctx, depth, out);
                }
                out.push_str("</td>");
                g += span;
            }
            if row.props.grid_after > 0 {
                let _ = write!(out, "<td colspan=\"{}\" style=\"border:none;padding:0\"></td>", (row.props.grid_after as usize).min(63));
            }
            out.push_str("</tr>");
        }
        out.push_str("</table>\n");
    }

    // --- notes and comments --------------------------------------------------------------------

    fn notes_html(&mut self) -> String {
        let doc = self.doc;
        let notes = std::mem::take(&mut self.notes);
        let comments = std::mem::take(&mut self.comments);
        if notes.is_empty() && comments.is_empty() {
            return String::new();
        }
        let mut o = String::from("<div class=\"page notes\" style=\"width:850px;padding:24pt 54pt\">");
        self.aux += 1;
        self.in_note = true;
        for (kind, title) in [(NoteKind::Footnote, "Footnotes"), (NoteKind::Endnote, "Endnotes")] {
            let list: Vec<&(NoteKind, u32, String)> = notes.iter().filter(|n| n.0 == kind).collect();
            if list.is_empty() {
                continue;
            }
            let _ = write!(o, "<h3>{title}</h3><ol style=\"list-style:none;padding-inline-start:0\">");
            for (_, id, label) in list {
                let anchor = format!("{}-{}", if kind == NoteKind::Footnote { "fn" } else { "en" }, id);
                let _ = write!(o, "<li id=\"{anchor}\"><a href=\"#r{anchor}\" class=\"cmeta\">{}</a> ", esc(label));
                if let Some(part) = doc.parts.get(id) {
                    self.blocks_html(&part.blocks, Ctx { nested: true, ..Ctx::default() }, 0, &mut o);
                }
                o.push_str("</li>");
            }
            o.push_str("</ol>");
        }
        self.in_note = false;
        if !comments.is_empty() {
            o.push_str("<h3>Comments</h3><ol style=\"list-style:none;padding-inline-start:0\">");
            for id in comments {
                self.comment_html(id, 0, &mut o);
            }
            o.push_str("</ol>");
        }
        self.aux -= 1;
        o.push_str("</div>\n");
        o
    }

    fn comment_html(&mut self, id: u32, depth: usize, o: &mut String) {
        let doc = self.doc;
        let Some(c) = doc.comments.get(&id) else { return };
        let no = self.comment_no.get(&id).map(|n| format!("[C{n}] ")).unwrap_or_default();
        let _ = write!(
            o,
            "<li id=\"cm-{id}\"{}><div class=\"cmeta\">{no}<b>{}</b> {}{}</div>",
            if depth > 0 { " class=\"creply\"" } else { "" },
            esc(if c.author.trim().is_empty() { "Unknown author" } else { c.author.trim() }),
            esc(&short_date(&c.date)),
            if c.resolved { " · resolved" } else { "" }
        );
        if let Some(part) = doc.parts.get(&c.part) {
            let saved = std::mem::take(&mut self.open);
            self.blocks_html(&part.blocks, Ctx { nested: true, ..Ctx::default() }, 0, o);
            self.open = saved;
        }
        if depth < 8 {
            let replies: Vec<u32> = doc.comments.iter().filter(|(_, r)| r.parent == Some(id)).map(|(k, _)| *k).collect();
            if !replies.is_empty() {
                o.push_str("<ol style=\"list-style:none;padding-inline-start:0\">");
                for r in replies {
                    self.comment_html(r, depth + 1, o);
                }
                o.push_str("</ol>");
            }
        }
        o.push_str("</li>");
    }

    // --- Markdown -------------------------------------------------------------------------------

    fn body_md(&mut self, piece: &Piece) -> String {
        let doc = self.doc;
        let secs = sections(doc);
        let mut out = String::new();
        let mut k = Self::section_of(&secs, piece.from);
        let (hdr, _) = Self::hf_of(&secs, k, false);
        self.hf_md(hdr, "Header", &mut out);
        let mut pending_footer = Self::hf_of(&secs, k, false).1;
        for i in piece.from..piece.to {
            let Some(b) = doc.body.get(i) else { break };
            match &**b {
                Block::Para(p) => {
                    let page_before = p.props.page_break_before.unwrap_or(false) && i > piece.from;
                    let (text, before, after) = self.para_md(p, Ctx::default());
                    if (before || page_before) && !out.is_empty() {
                        out.push_str("<!-- pagebreak -->\n\n");
                    }
                    out.push_str(&text);
                    if after {
                        out.push_str("<!-- pagebreak -->\n\n");
                    }
                }
                Block::Table(t) => {
                    let rows = if i == piece.from { piece.rows } else { None };
                    out.push_str(&self.table_md(t, rows, 0));
                }
            }
            if let Some((end, _)) = secs.get(k)
                && *end == i
                && i + 1 < piece.to
                && k + 1 < secs.len()
            {
                self.hf_md(pending_footer, "Footer", &mut out);
                k += 1;
                if secs.get(k).is_some_and(|(_, s)| s.start != SectionStart::Continuous) {
                    out.push_str("<!-- pagebreak -->\n\n");
                }
                out.push_str("<!-- section break -->\n\n");
                let (h, ft) = Self::hf_of(&secs, k, false);
                self.hf_md(h, "Header", &mut out);
                pending_footer = ft;
            }
        }
        self.hf_md(pending_footer, "Footer", &mut out);
        out
    }

    fn hf_md(&mut self, id: Option<u32>, what: &str, out: &mut String) {
        let doc = self.doc;
        let Some(part) = id.and_then(|id| doc.parts.get(&id)) else { return };
        self.aux += 1;
        let text = self.blocks_md(&part.blocks, true, 0);
        self.aux -= 1;
        let text = text.trim();
        if !text.is_empty() {
            let _ = write!(out, "> **{what}:** {}\n\n", text.replace('\n', " "));
        }
    }

    fn blocks_md(&mut self, bl: &Blocks, inline: bool, depth: usize) -> String {
        let mut out = String::new();
        for b in bl.iter() {
            match &**b {
                Block::Para(p) => {
                    let (t, _, _) = self.para_md(p, Ctx { nested: true, ..Ctx::default() });
                    let t = t.replace(LIST_END, "\n\n");
                    if inline {
                        let t = t.trim();
                        if !t.is_empty() {
                            if !out.is_empty() {
                                out.push_str("<br>");
                            }
                            out.push_str(t);
                        }
                    } else {
                        out.push_str(&t);
                    }
                }
                Block::Table(t) if !inline && depth < MAX_DEPTH => out.push_str(&self.table_md(t, None, depth + 1)),
                Block::Table(t) => {
                    let mut rows = Vec::new();
                    for r in &t.rows {
                        let cells: Vec<String> = r.cells.iter().map(|c| self.blocks_md(&c.blocks, true, depth + 1)).collect();
                        rows.push(cells.join(" \\| "));
                    }
                    if !out.is_empty() {
                        out.push_str("<br>");
                    }
                    let _ = write!(out, "[table: {}]", rows.join("; "));
                }
            }
        }
        out
    }

    /// A paragraph as Markdown (ending in a blank line), and whether page breaks lead or end it.
    fn para_md(&mut self, p: &Paragraph, ctx: Ctx<'_>) -> (String, bool, bool) {
        let doc = self.doc;
        let rp = doc.styles.resolve_para_in(&p.props, ctx.para);
        let style = p.props.style.clone().unwrap_or_else(|| "Normal".into());
        let label = self.list_label(p);
        let mut deferred = Vec::new();
        let mut segs = self.segments(p, &style, ctx.chr, &mut deferred);
        let (mut before, mut after) = (false, false);
        if !ctx.nested {
            while matches!(segs.first(), Some(Seg::PageBreak)) {
                segs.remove(0);
                before = true;
            }
            while matches!(segs.last(), Some(Seg::PageBreak)) {
                segs.pop();
                after = true;
            }
        }
        let mut body = self.segs_md(&segs);
        if self.markup
            && let Some(r) = p.mark.ins.or(p.mark.del)
        {
            let ins = p.mark.ins.is_some();
            self.count_rev(if ins { 0 } else { 1 }, r);
            body.push_str(if ins { "{++¶++}" } else { "{--¶--}" });
        }
        let mut out = String::new();
        let trimmed = body.trim();
        if trimmed.is_empty() && label.is_none() {
            for (_, m) in deferred {
                if !m.is_empty() {
                    let _ = write!(out, "{m}\n\n");
                }
            }
            return (out, before, after);
        }
        self.stats.paragraphs += 1;
        if let Some(l) = rp.outline_level {
            self.stats.headings += 1;
            let _ = write!(out, "{} ", "#".repeat((l as usize + 1).min(6)));
        } else if let Some((text, lv)) = &label {
            let lvl = p.props.numbering.or_else(|| self.numref(p)).map_or(0, |n| n.level.min(8) as usize);
            out.push_str(&"  ".repeat(lvl));
            // A bullet level is a Markdown bullet whatever its glyph (•, ▪, ◦, a Symbol or
            // Wingdings character, a picture bullet).
            let bullet =
                lv.format == wordcraft_doc::section::NumFormat::Bullet || (text.chars().count() <= 1 && !text.chars().any(|c| c.is_alphanumeric()));
            if bullet {
                out.push_str("- ");
            } else {
                let _ = write!(out, "{} ", md_text(text.trim()));
            }
        } else if line_starts_like_markdown(trimmed) {
            out.push('\\');
        }
        out.push_str(trimmed);
        out.push_str(if label.is_some() && deferred.is_empty() { LIST_END } else { "\n\n" });
        for (_, m) in deferred {
            if !m.is_empty() {
                let _ = write!(out, "{m}\n\n");
            }
        }
        (out, before, after)
    }

    fn segs_md(&mut self, segs: &[Seg]) -> String {
        let mut o = String::new();
        // Comments opened in this paragraph whose text is highlighted (`{==`).
        let mut here: Vec<u32> = Vec::new();
        for s in segs {
            match s {
                Seg::Text { text, rc } => {
                    self.stats.characters += text.chars().count();
                    let mut t = md_text(text);
                    let lead = t.len() - t.trim_start().len();
                    let trail = t.len() - t.trim_end().len();
                    let core = t.trim().to_string();
                    if !core.is_empty() {
                        let mut c = core;
                        if rc.strike || rc.double_strike {
                            c = format!("~~{c}~~");
                        }
                        if rc.italic {
                            c = format!("*{c}*");
                        }
                        if rc.bold {
                            c = format!("**{c}**");
                        }
                        match rc.vert_align {
                            VertAlign::Superscript => c = format!("<sup>{c}</sup>"),
                            VertAlign::Subscript => c = format!("<sub>{c}</sub>"),
                            VertAlign::Baseline => {}
                        }
                        if let Some(href) = rc.link.as_deref().and_then(safe_href) {
                            c = format!("[{c}]({})", href.replace(' ', "%20").replace(')', "%29"));
                        }
                        t = format!("{}{c}{}", " ".repeat(lead.min(1)), " ".repeat(trail.min(1)));
                    }
                    o.push_str(&self.md_rev(t, rc.ins, rc.del));
                }
                Seg::Tab => o.push('\t'),
                Seg::Br => o.push_str("  \n"),
                Seg::PageBreak => o.push_str("\n\n<!-- pagebreak -->\n\n"),
                Seg::ColumnBreak => o.push_str("  \n"),
                Seg::Raw { md, ins, del, .. } => {
                    if !md.is_empty() {
                        let m = self.md_rev(md.clone(), *ins, *del);
                        o.push_str(&m);
                    }
                }
                Seg::CommentStart(id) => {
                    self.start_comment(*id);
                    here.push(*id);
                    o.push_str("{==");
                }
                Seg::CommentEnd(id) => {
                    self.open.retain(|c| c != id);
                    if here.contains(id) {
                        o.push_str("==}");
                        here.retain(|c| c != id);
                    }
                    let _ = write!(o, "{{>>{}<<}}", self.comment_md(*id));
                }
                Seg::Anchor(_) => {}
            }
        }
        // A comment that runs on past this paragraph: its highlight ends here.
        for _ in here {
            o.push_str("==}");
        }
        o
    }

    fn md_rev(&mut self, t: String, ins: Option<u32>, del: Option<u32>) -> String {
        if !self.markup || t.trim().is_empty() {
            return t;
        }
        if let Some(r) = del {
            self.count_rev(1, r);
            return format!("{{--{t}--}}");
        }
        if let Some(r) = ins {
            self.count_rev(0, r);
            return format!("{{++{t}++}}");
        }
        t
    }

    fn comment_md(&mut self, id: u32) -> String {
        let doc = self.doc;
        let mut parts = Vec::new();
        let mut stack = vec![(id, 0usize)];
        while let Some((cid, depth)) = stack.pop() {
            let Some(c) = doc.comments.get(&cid) else { continue };
            let text = doc.parts.get(&c.part).map(|p| blocks_text(&p.blocks)).unwrap_or_default();
            let who = if c.author.trim().is_empty() { "Unknown author" } else { c.author.trim() };
            let no = self.comment_no.get(&cid).map(|n| format!("C{n} ")).unwrap_or_default();
            let lead = if depth == 0 { no } else { "reply: ".to_string() };
            parts.push(format!(
                "{lead}{who}{}{}: {}",
                short_date(&c.date).split(' ').next().map(|d| format!(" ({d})")).filter(|d| d != " ()").unwrap_or_default(),
                if c.resolved { ", resolved" } else { "" },
                text.replace("<<", "« ").replace('\n', " ")
            ));
            if depth < 8 {
                for (k, r) in doc.comments.iter().rev() {
                    if r.parent == Some(cid) {
                        stack.push((*k, depth + 1));
                    }
                }
            }
        }
        parts.join(" | ")
    }

    fn notes_md(&mut self) -> String {
        let doc = self.doc;
        let notes = std::mem::take(&mut self.notes);
        self.comments.clear();
        let mut o = String::new();
        self.aux += 1;
        self.in_note = true;
        for (kind, id, label) in notes {
            let text = doc.parts.get(&id).map(|p| self.blocks_md(&p.blocks, true, 0)).unwrap_or_default();
            let _ = writeln!(o, "[^{}{}]: {}", if kind == NoteKind::Footnote { "" } else { "e" }, md_ref(&label), text.trim());
        }
        self.in_note = false;
        self.aux -= 1;
        if !o.is_empty() {
            o.push('\n');
        }
        o
    }

    fn table_md(&mut self, t: &Table, window: Option<(usize, usize)>, depth: usize) -> String {
        self.stats.tables += 1;
        let nrows = t.rows.len();
        let ncols = t.cols().max(1);
        let (r0, r1) = window.map_or((0, nrows), |(a, b)| (a.min(nrows), b.min(nrows)));
        let heads = if r0 > 0 { header_rows(t).min(r0) } else { 0 };
        let rows: Vec<usize> = (0..heads).chain(r0..r1).collect();
        let mut grid: Vec<Vec<String>> = Vec::new();
        let mut merges = Vec::new();
        let mut nested = Vec::new();
        for (pos, &ri) in rows.iter().enumerate() {
            let Some(row) = t.rows.get(ri) else { continue };
            let mut line = vec![String::new(); ncols];
            let mut g = row.props.grid_before as usize;
            for (ci, cell) in row.cells.iter().enumerate() {
                let span = cell.span();
                if cell.props.vmerge == VMerge::Continue && pos > 0 {
                    g += span;
                    continue;
                }
                if cell.props.vmerge == VMerge::Restart {
                    let mut n = 1;
                    for &below in rows.iter().skip(pos + 1) {
                        let cont = t
                            .cell_at_grid(below, t.grid_col(ri, ci))
                            .and_then(|k| t.rows.get(below).and_then(|r| r.cells.get(k)))
                            .is_some_and(|c| c.props.vmerge == VMerge::Continue);
                        if !cont {
                            break;
                        }
                        n += 1;
                    }
                    if n > 1 {
                        merges.push(format!("R{}C{} spans {n} rows", pos + 1, g + 1));
                    }
                }
                if span > 1 {
                    merges.push(format!("R{}C{} spans {span} columns", pos + 1, g + 1));
                }
                for b in cell.blocks.iter() {
                    if let Block::Table(nt) = &**b
                        && depth < MAX_DEPTH
                    {
                        nested.push((pos + 1, g + 1, nt.clone()));
                    }
                }
                let text = self.blocks_md(&cell.blocks, true, depth + 1).replace('|', "\\|").replace('\n', " ");
                if let Some(slot) = line.get_mut(g) {
                    *slot = text;
                }
                g += span;
            }
            grid.push(line);
        }
        let mut o = String::new();
        for (i, line) in grid.iter().enumerate() {
            let _ = writeln!(o, "| {} |", line.join(" | "));
            if i == 0 {
                let _ = writeln!(o, "|{}", " --- |".repeat(ncols));
            }
        }
        if grid.is_empty() {
            return String::new();
        }
        if !merges.is_empty() {
            let _ = writeln!(o, "\n_Merged cells: {}._", merges.join("; "));
        }
        o.push('\n');
        for (r, c, nt) in nested {
            let _ = writeln!(o, "_Table in R{r}C{c}:_\n");
            o.push_str(&self.table_md(&nt, None, depth + 1));
        }
        o
    }
}

#[derive(Default)]
struct ParaOut {
    html: String,
    break_before: bool,
    break_after: bool,
}

// ---------------------------------------------------------------------------------------------
// Helpers

/// A number for CSS: at most two decimals, no trailing zeros.
fn f(v: f32) -> String {
    if !v.is_finite() {
        return "0".into();
    }
    let s = format!("{:.2}", v);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" || s.is_empty() { "0".into() } else { s.to_string() }
}

fn style_of(b: Option<&std::sync::Arc<Block>>) -> Option<String> {
    b.and_then(|b| b.as_para()).map(|p| p.props.style.clone().unwrap_or_else(|| "Normal".into()))
}

fn has_letters(s: &str) -> bool {
    s.chars().any(|c| c.is_alphabetic() || c == '\u{200E}' || c == '\u{200F}')
}

fn esc_text(s: &str) -> String {
    esc(s).replace(SOFT_HYPHEN, "&shy;")
}

/// Text for Markdown: the characters Markdown would read as markup are escaped.
fn md_text(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '\\' | '*' | '_' | '`' | '[' | ']' | '<' | '>' | '~' | '{' | '}' => {
                o.push('\\');
                o.push(c);
            }
            SOFT_HYPHEN => {}
            c if c.is_control() && c != '\t' => {}
            c => o.push(c),
        }
    }
    o
}

fn md_ref(label: &str) -> String {
    let s: String = label.chars().filter(|c| c.is_alphanumeric()).collect();
    if s.is_empty() { "x".into() } else { s }
}

/// A paragraph whose text starts like a Markdown block marker (`#`, `- `, `1. `, `>`, `|`).
fn line_starts_like_markdown(s: &str) -> bool {
    let first = s.chars().next().unwrap_or(' ');
    if matches!(first, '#' | '>' | '|' | '=') {
        return true;
    }
    if (s.starts_with("- ") || s.starts_with("+ ") || s.starts_with("---")) && first != '\\' {
        return true;
    }
    let digits = s.chars().take_while(|c| c.is_ascii_digit()).count();
    digits > 0 && s.get(digits..).is_some_and(|r| r.starts_with(". ") || r.starts_with(") "))
}

fn blocks_text(bl: &Blocks) -> String {
    let mut parts = Vec::new();
    for b in bl.iter() {
        match &**b {
            Block::Para(p) => parts.push(p.final_text().trim().to_string()),
            Block::Table(t) => {
                for r in &t.rows {
                    for c in &r.cells {
                        parts.push(blocks_text(&c.blocks));
                    }
                }
            }
        }
    }
    parts.retain(|p| !p.is_empty());
    parts.join("\n")
}

/// `YYYY-MM-DD HH:MM` from an ISO 8601 date.
fn short_date(d: &str) -> String {
    let d = d.trim();
    let date = d.get(..10).unwrap_or(d);
    match d.get(11..16) {
        Some(t) if t != "00:00" => format!("{date} {t}"),
        _ => date.to_string(),
    }
}

/// A link that may go into the page: web and mail links and bookmarks only.
fn safe_href(link: &str) -> Option<String> {
    let l = link.trim();
    let lower = l.to_ascii_lowercase();
    if l.starts_with('#') {
        return Some(format!("#{}", anchor_id(l.trim_start_matches('#'))));
    }
    if lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("mailto:") {
        return Some(l.chars().filter(|c| !c.is_control() && *c != '"').collect());
    }
    if !l.is_empty() && !l.contains(':') && !l.contains('/') && !l.contains('\\') {
        // A bookmark name (internal hyperlink).
        return Some(format!("#{}", anchor_id(l)));
    }
    None
}

fn anchor_id(name: &str) -> String {
    let s: String = name.chars().map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).take(120).collect();
    format!("bm-{s}")
}

/// A picture's MIME type and bytes as a browser shows them: PNG, JPEG, GIF, WebP, BMP and SVG as
/// they are; TIFF and other raster formats the `image` crate reads, as PNG; metafiles: none.
fn browser_picture(bytes: &[u8]) -> Option<(&'static str, Vec<u8>)> {
    if let Some(ext) = sniff_image(bytes) {
        return Some((mime_of(ext), bytes.to_vec()));
    }
    let head = String::from_utf8_lossy(bytes.get(..256).unwrap_or(bytes)).to_ascii_lowercase();
    if head.contains("<svg") {
        return Some(("image/svg+xml", bytes.to_vec()));
    }
    let img = decode_bounded(bytes)?;
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(("image/png", out.into_inner()))
}

/// Text set in one of Word's symbol fonts (Symbol, Wingdings, Wingdings 2 and 3, Webdings) as
/// the Unicode characters Word shows for its codes (U+F0B7 in Symbol: •, U+F0A7 in Wingdings: ▪,
/// U+F0D8 in Wingdings: ➢…); other text unchanged.
fn symbol_text(font: &str, text: &str) -> String {
    wordcraft_doc::symbol_fonts::map_text(font, text)
}

/// A list label as Word shows it: a symbol-font bullet as its Unicode character; the classic
/// second-level bullet, `o` in Courier New, as ◦; a private-use bullet whose font is not a
/// symbol font (a damaged or hand-made list) read as Symbol, else Wingdings.
fn label_text(text: &str, font: &str, bullet: bool) -> String {
    if wordcraft_doc::symbol_fonts::is_symbol_font(font) {
        return symbol_text(font, text);
    }
    if !bullet {
        return text.to_string();
    }
    if text == "o" && font.trim().eq_ignore_ascii_case("courier new") {
        return "◦".to_string();
    }
    text.chars()
        .map(|c| match u32::from(c) {
            0xF020..=0xF0FF => wordcraft_doc::symbol_fonts::symbol_char("Symbol", c)
                .or_else(|| wordcraft_doc::symbol_fonts::symbol_char("Wingdings", c))
                .unwrap_or('•'),
            _ => c,
        })
        .collect()
}

fn clean_font(name: &str) -> String {
    name.chars().filter(|c| !matches!(c, '\'' | '"' | '\\' | '<' | '>' | ';' | '{' | '}') && !c.is_control()).take(64).collect()
}

/// `font-family` with open metric-compatible faces after a proprietary one, and a generic family.
fn font_family(name: &str) -> String {
    // Word's symbol fonts (proprietary, never installed with the page) are shown as the Unicode
    // characters [`symbol_text`] maps their codes to, in fonts that have those characters.
    if wordcraft_doc::symbol_fonts::is_symbol_font(name) {
        return "'Segoe UI Symbol','Noto Sans Symbols 2','Noto Sans Symbols','DejaVu Sans',sans-serif".to_string();
    }
    let n = clean_font(name);
    let lower = n.to_ascii_lowercase();
    let subst = match lower.as_str() {
        "calibri" => Some("Carlito"),
        "cambria" => Some("Caladea"),
        "arial" | "helvetica" => Some("Liberation Sans"),
        "times new roman" | "times" => Some("Liberation Serif"),
        "courier new" | "courier" => Some("Liberation Mono"),
        "aptos" => Some("Carlito"),
        _ => None,
    };
    let generic = if ["courier", "consolas", "mono", "code"].iter().any(|k| lower.contains(k)) {
        "monospace"
    } else if ["times", "cambria", "georgia", "garamond", "book", "serif", "naskh", "traditional arabic", "minion", "palatino"]
        .iter()
        .any(|k| lower.contains(k))
        && !lower.contains("sans")
    {
        "serif"
    } else {
        "sans-serif"
    };
    match subst {
        Some(s) => format!("'{n}','{s}',{generic}"),
        None if n.is_empty() => generic.to_string(),
        None => format!("'{n}',{generic}"),
    }
}

fn decoration(rc: &ResolvedChar) -> String {
    let mut lines = Vec::new();
    if rc.underline != Underline::None {
        lines.push("underline");
    }
    if rc.strike || rc.double_strike {
        lines.push("line-through");
    }
    if lines.is_empty() {
        return String::new();
    }
    let style = match rc.underline {
        Underline::Double => " double",
        Underline::Dotted => " dotted",
        Underline::Dash | Underline::DotDash | Underline::DotDotDash => " dashed",
        Underline::Wave | Underline::DoubleWave => " wavy",
        _ if rc.double_strike => " double",
        _ => "",
    };
    let color = rc.underline_color.map(|c| format!(" #{}", c.hex())).unwrap_or_default();
    format!("{}{style}{color}", lines.join(" "))
}

fn full_font_css(rc: &ResolvedChar, css: &mut String) {
    let _ = write!(css, "font-family:{};font-size:{}pt;", font_family(&rc.font), f(rc.size));
    if let TextColor::Rgb(c) = rc.color {
        let _ = write!(css, "color:#{};", c.hex());
    }
    if rc.bold {
        css.push_str("font-weight:700;");
    }
    if rc.italic {
        css.push_str("font-style:italic;");
    }
    let d = decoration(rc);
    if !d.is_empty() {
        let _ = write!(css, "text-decoration:{d};");
    }
    if rc.caps {
        css.push_str("text-transform:uppercase;");
    }
    if rc.small_caps {
        css.push_str("font-variant:small-caps;");
    }
}

/// The CSS that makes a run of `rc` look as it does over its paragraph's `base`.
fn char_css(rc: &ResolvedChar, base: &ResolvedChar) -> String {
    let mut c = String::new();
    if rc.font != base.font {
        let _ = write!(c, "font-family:{};", font_family(&rc.font));
    }
    if (rc.size - base.size).abs() > 0.01 {
        let _ = write!(c, "font-size:{}pt;", f(rc.size));
    }
    if rc.bold != base.bold {
        c.push_str(if rc.bold { "font-weight:700;" } else { "font-weight:400;" });
    }
    if rc.italic != base.italic {
        c.push_str(if rc.italic { "font-style:italic;" } else { "font-style:normal;" });
    }
    if rc.color != base.color {
        match rc.color {
            TextColor::Rgb(col) => {
                let _ = write!(c, "color:#{};", col.hex());
            }
            TextColor::Auto => c.push_str("color:#000;"),
        }
    }
    let (d, bd) = (decoration(rc), decoration(base));
    if d != bd {
        let _ = write!(c, "text-decoration:{};", if d.is_empty() { "none" } else { d.as_str() });
    }
    if let Some(h) = rc.highlight.or(rc.shading).filter(|_| rc.highlight != base.highlight || rc.shading != base.shading) {
        let _ = write!(c, "background:#{};", h.hex());
    }
    if rc.caps != base.caps {
        c.push_str(if rc.caps { "text-transform:uppercase;" } else { "text-transform:none;" });
    }
    if rc.small_caps != base.small_caps {
        c.push_str(if rc.small_caps { "font-variant:small-caps;" } else { "font-variant:normal;" });
    }
    if rc.spacing.is_finite() && rc.spacing.abs() > 0.01 && (rc.spacing - base.spacing).abs() > 0.01 {
        let _ = write!(c, "letter-spacing:{}pt;", f(rc.spacing.clamp(-20.0, 50.0)));
    }
    if rc.position.is_finite() && rc.position.abs() > 0.01 && (rc.position - base.position).abs() > 0.01 {
        let _ = write!(c, "vertical-align:{}pt;", f(rc.position.clamp(-200.0, 200.0)));
    }
    if let Some(b) = rc.border.filter(|b| b.is_visible()) {
        let _ = write!(c, "border:{};", border_css(&b));
    }
    c
}

fn border_css(b: &Border) -> String {
    if !b.is_visible() {
        return "none".into();
    }
    let style = match b.style {
        BorderStyle::Double | BorderStyle::Triple => "double",
        BorderStyle::Dotted => "dotted",
        BorderStyle::Dashed | BorderStyle::DotDash => "dashed",
        _ => "solid",
    };
    let mut w = if b.width.is_finite() { b.width.clamp(0.25, 12.0) } else { 0.5 };
    if style == "double" {
        w = w.max(2.25);
    }
    if b.style == BorderStyle::Thick {
        w = w.max(1.5);
    }
    format!("{}pt {style} #{}", f(w), b.color.unwrap_or(Rgb::BLACK).hex())
}

/// Edge borders as CSS (`padding` from each border's space when `space` is asked for).
fn borders_css(b: &Borders, css: &mut String, space: bool) {
    for (edge, side) in [(b.top, "top"), (b.bottom, "bottom"), (b.left, "left"), (b.right, "right")] {
        if let Some(e) = edge {
            let _ = write!(css, "border-{side}:{};", border_css(&e));
            if space && e.is_visible() && e.space.is_finite() && e.space > 0.0 {
                let _ = write!(css, "padding-{side}:{}pt;", f(e.space.clamp(0.0, 31.0)));
            }
        }
    }
}

fn shape_svg(kind: ShapeKind, w: f32, h: f32, fill: Option<Rgb>, stroke: Option<Rgb>, sw: f32) -> String {
    let w = if w.is_finite() { w.clamp(1.0, 2000.0) } else { 36.0 };
    let h = if h.is_finite() { h.clamp(1.0, 2000.0) } else { 36.0 };
    let fill = fill.map_or("none".to_string(), |c| format!("#{}", c.hex()));
    let stroke = stroke.map_or("none".to_string(), |c| format!("#{}", c.hex()));
    let sw = f(if sw.is_finite() { sw.clamp(0.0, 20.0) } else { 0.75 });
    let (fw, fh) = (f(w), f(h));
    let body = match kind {
        ShapeKind::Ellipse | ShapeKind::FlowchartOr => {
            format!("<ellipse cx=\"{}\" cy=\"{}\" rx=\"{}\" ry=\"{}\"/>", f(w / 2.0), f(h / 2.0), f(w / 2.0), f(h / 2.0))
        }
        ShapeKind::Triangle => format!("<polygon points=\"{} 0 {fw} {fh} 0 {fh}\"/>", f(w / 2.0)),
        ShapeKind::RightTriangle => format!("<polygon points=\"0 0 {fw} {fh} 0 {fh}\"/>"),
        ShapeKind::Diamond => format!("<polygon points=\"{} 0 {fw} {} {} {fh} 0 {}\"/>", f(w / 2.0), f(h / 2.0), f(w / 2.0), f(h / 2.0)),
        ShapeKind::Line | ShapeKind::StraightConnector | ShapeKind::ElbowConnector | ShapeKind::CurvedConnector => {
            format!("<line x1=\"0\" y1=\"0\" x2=\"{fw}\" y2=\"{fh}\"/>")
        }
        ShapeKind::RoundedRectangle => format!("<rect width=\"{fw}\" height=\"{fh}\" rx=\"{}\"/>", f(w.min(h) / 6.0)),
        _ => format!("<rect width=\"{fw}\" height=\"{fh}\"/>"),
    };
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{fw}pt\" height=\"{fh}pt\" viewBox=\"0 0 {fw} {fh}\" style=\"vertical-align:bottom;max-width:100%\" fill=\"{fill}\" stroke=\"{stroke}\" stroke-width=\"{sw}\">{body}</svg>"
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use wordcraft_doc::para::InlineObject;
    use wordcraft_doc::props::{CharProps, NumRef, ParaProps, Rgb, VMerge};
    use wordcraft_doc::{Block, Comment, Document, Paragraph, Part, PartKind, Revision, RevisionKind, Table, para_block};

    use super::*;

    fn para(text: &str) -> Arc<Block> {
        para_block(Paragraph::with_text(text, CharProps::default()))
    }
    fn styled(text: &str, style: &str) -> Arc<Block> {
        para_block(Paragraph::with_text(text, CharProps::default()).styled(style))
    }
    fn doc_of(blocks: Vec<Arc<Block>>) -> Document {
        let mut d = Document::new();
        d.body = blocks;
        d
    }
    fn html(d: &Document) -> String {
        render_html(d, &Options::default(), Window::Part(1)).unwrap().text
    }
    fn md(d: &Document) -> String {
        render_markdown(d, &Options { part_chars: MARKDOWN_PART_CHARS, ..Options::default() }, Window::Part(1)).unwrap().text
    }
    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(w, h, image::Rgb([200, 30, 30]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img).write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn the_page_has_the_previewers_shape_and_headings_with_their_style() {
        let d = doc_of(vec![styled("Annual report", "Heading1"), styled("Scope", "Heading2"), para("Body text & <more>.")]);
        let h = html(&d);
        assert!(h.starts_with("<!DOCTYPE html><html><head><meta charset=\"utf-8\">"), "{}", &h[..80]);
        assert!(h.contains("<div class=\"file-head\">Document</div>"));
        assert!(h.contains("<div class=\"page\" style=\"width:"), "sheets carry the section's page size");
        assert!(h.contains("data-style=\"heading 1\"") || h.contains("data-style=\"Heading 1\""), "{h}");
        let h1 = h.find("<h1").unwrap();
        assert!(h[h1..].starts_with("<h1 data-style"), "{}", &h[h1..h1 + 60]);
        assert!(h[h1..].contains(">Annual report</h1>"));
        assert!(h.contains(">Scope</h2>"));
        // The heading's resolved size and colour, not the browser's.
        let h1_tag = &h[h1..h[h1..].find('>').unwrap() + h1];
        assert!(h1_tag.contains("font-size:") && h1_tag.contains("color:#"), "{h1_tag}");
        assert!(h.contains("Body text &amp; &lt;more&gt;."));
        assert!(!h.contains("http://") && !h.contains("https://") || h.contains("xmlns"), "nothing fetched");
        let m = md(&d);
        assert!(m.starts_with("# Annual report\n\n## Scope\n\nBody text & \\<more\\>."), "{m}");
    }

    #[test]
    fn runs_keep_bold_italic_underline_colour_and_size() {
        let mut p = Paragraph::with_text("plain ", CharProps::default());
        let bold = CharProps { bold: Some(true), color: Some(TextColor::Rgb(Rgb(0x12, 0x34, 0x56))), size: Some(20.0), ..Default::default() };
        p.insert_text(p.len(), "loud", &bold).unwrap();
        let it = CharProps { italic: Some(true), underline: Some(Underline::Single), ..Default::default() };
        p.insert_text(p.len(), " soft", &it).unwrap();
        let h = html(&doc_of(vec![para_block(p)]));
        assert!(h.contains("<span style=\"font-size:20pt;font-weight:700;color:#123456;\">loud</span>"), "{h}");
        assert!(h.contains("font-style:italic;text-decoration:underline;\"> soft</span>"), "{h}");
        let m = md(&doc_of(vec![para_block({
            let mut p = Paragraph::with_text("a ", CharProps::default());
            p.insert_text(2, "b", &bold).unwrap();
            p
        })]));
        assert_eq!(m, "a **b**");
    }

    fn merged_table() -> Table {
        let mut t = Table::new(3, 3, 300.0);
        // Row 0: cells 0 and 1 merged across; column 2 merged down rows 0-2.
        t.merge_right(0, 0, 1);
        if let Some(c) = t.rows[0].cells.last_mut() {
            c.props.vmerge = VMerge::Restart;
            c.props.shading = Some(Rgb(0xDD, 0xEE, 0xFF));
            c.blocks = vec![para("tall")];
        }
        for r in 1..3 {
            let c = t.rows[r].cells.last_mut().unwrap();
            c.props.vmerge = VMerge::Continue;
        }
        t.rows[0].cells[0].blocks = vec![para("wide")];
        t.rows[1].cells[0].blocks = vec![para("a")];
        t.rows[2].cells[1].blocks = vec![para("b|c")];
        t
    }

    #[test]
    fn tables_keep_merged_cells_shading_and_borders() {
        let d = doc_of(vec![para_block(Paragraph::new()), Arc::new(Block::Table(merged_table())), para("after")]);
        let h = html(&d);
        assert!(h.contains("<td colspan=\"2\""), "{h}");
        assert!(h.contains("<td rowspan=\"3\" style=\"background:#DDEEFF;"), "{h}");
        assert_eq!(h.matches("<tr>").count(), 3);
        assert_eq!(h.matches("<td").count(), 2 + 2 + 2, "covered cells are not drawn");
        assert!(h.contains("border-top:0.5pt solid #000000"), "Table Grid borders: {h}");
        let m = md(&d);
        assert!(m.contains("| wide |  | tall |\n| --- | --- | --- |\n| a |  |  |\n|  | b\\|c |  |"), "{m}");
        assert!(
            m.contains("_Merged cells: R1C3 spans 3 rows; R1C1 spans 2 columns._")
                || m.contains("_Merged cells: R1C1 spans 2 columns; R1C3 spans 3 rows._"),
            "{m}"
        );
    }

    #[test]
    fn pictures_are_embedded_as_data_uris_and_charts_drawn() {
        let mut d = Document::new();
        let bytes = png(4, 2);
        d.media.insert("image1.png".into(), Arc::new(bytes.clone()));
        let mut p = Paragraph::with_text("Logo: ", CharProps::default());
        let img = InlineObject::Image {
            media: "image1.png".into(),
            w: 40.0,
            h: 20.0,
            alt: "the logo".into(),
            float: Default::default(),
            crop: [0.0; 4],
            ole: None,
        };
        p.insert_object(p.len(), img, &CharProps::default()).unwrap();
        d.body = vec![para_block(p)];
        let r = render_html(&d, &Options::default(), Window::Part(1)).unwrap();
        let uri = format!("data:image/png;base64,{}", base64_encode(&bytes));
        assert!(r.text.contains(&format!("<img src=\"{uri}\" alt=\"the logo\" style=\"width:40pt;aspect-ratio:40/20;")), "{}", r.text);
        assert_eq!((r.stats.pictures, r.stats.picture_bytes), (1, bytes.len() as u64));
        assert!(md(&d).contains("![the logo](picture:image1.png 40x20pt)"));
        // Without pictures: a placeholder of its size.
        let r = render_html(&d, &Options { pictures: false, ..Options::default() }, Window::Part(1)).unwrap();
        assert!(r.text.contains("class=\"pic-ph\" style=\"width:40pt;height:20pt\"") && !r.text.contains("base64"));
        assert_eq!(r.stats.pictures_omitted, 1);
    }

    fn tracked_doc() -> Document {
        let mut d = Document::new();
        d.revisions.push(Revision::new(RevisionKind::Insert, "Alice", "2026-10-30T09:15:00Z"));
        d.revisions.push(Revision::new(RevisionKind::Delete, "Bob", "2026-10-29T08:00:00Z"));
        let mut p = Paragraph::with_text("The fee is ", CharProps::default());
        p.insert_text(p.len(), "ten", &CharProps { del: Some(1), ..Default::default() }).unwrap();
        p.insert_text(p.len(), "twelve", &CharProps { ins: Some(0), ..Default::default() }).unwrap();
        p.insert_text(p.len(), " dollars.", &CharProps::default()).unwrap();
        d.body = vec![para_block(p)];
        d
    }

    #[test]
    fn tracked_changes_are_marked_with_author_and_date() {
        let d = tracked_doc();
        let r = render_html(&d, &Options::default(), Window::Part(1)).unwrap();
        assert!(r.text.contains("<del class=\"rv-del\" title=\"Deleted by Bob, 2026-10-29 08:00\">ten</del>"), "{}", r.text);
        assert!(r.text.contains("<ins class=\"rv-ins\" title=\"Inserted by Alice, 2026-10-30 09:15\">twelve</ins>"));
        assert_eq!((r.stats.insertions, r.stats.deletions), (1, 1));
        // Without markup: the text as if accepted.
        let r = render_html(&d, &Options { markup: false, ..Options::default() }, Window::Part(1)).unwrap();
        assert!(
            r.text.contains("The fee is twelve dollars.") && !r.text.contains(">ten<") && !r.text.contains("<ins") && !r.text.contains("<del"),
            "{}",
            r.text
        );
        assert_eq!(md(&d), "The fee is {--ten--}{++twelve++} dollars.");
    }

    #[test]
    fn comments_are_highlighted_and_listed_with_replies() {
        let mut d = Document::new();
        d.parts.insert(10, Part { kind: PartKind::Comment, blocks: vec![para("Check this figure")], ..Default::default() });
        d.parts.insert(11, Part { kind: PartKind::Comment, blocks: vec![para("Agreed")], ..Default::default() });
        d.comments.insert(1, Comment { author: "Carol".into(), date: "2026-10-30T12:00:00Z".into(), part: 10, ..Default::default() });
        d.comments.insert(2, Comment { author: "Dan".into(), date: "2026-10-30T13:00:00Z".into(), part: 11, parent: Some(1), ..Default::default() });
        let mut p = Paragraph::with_text("Revenue rose 5%.", CharProps::default());
        p.insert_object(8, InlineObject::CommentStart { id: 1 }, &CharProps::default()).unwrap();
        let end = p.len();
        p.insert_object(end, InlineObject::CommentEnd { id: 1 }, &CharProps::default()).unwrap();
        d.body = vec![para_block(p)];
        let r = render_html(&d, &Options::default(), Window::Part(1)).unwrap();
        assert!(r.text.contains("<mark class=\"cm\" data-comment=\"1\">rose 5%.</mark>"), "{}", r.text);
        assert!(r.text.contains("<a href=\"#cm-1\" title=\"Carol: Check this figure\">[C1]</a>"));
        assert!(r.text.contains("<h3>Comments</h3>") && r.text.contains("<b>Carol</b>") && r.text.contains("<b>Dan</b>"));
        assert!(r.text.contains("Agreed"));
        assert_eq!(r.stats.comments, 1);
        assert_eq!(md(&d), "Revenue {==rose 5%.==}{>>C1 Carol (2026-10-30): Check this figure | reply: Dan (2026-10-30): Agreed<<}");
    }

    #[test]
    fn arabic_paragraphs_are_right_to_left() {
        let d = doc_of(vec![para("مرحبا بالعالم"), para("Hello")]);
        let h = html(&d);
        let ar = h.find("مرحبا").unwrap();
        let open = h[..ar].rfind("<p").unwrap();
        assert!(h[open..ar].starts_with("<p dir=\"rtl\""), "{}", &h[open..ar]);
        let en = h.find("Hello").unwrap();
        let open = h[..en].rfind("<p").unwrap();
        assert!(!h[open..en].contains("dir="), "an English paragraph stays left to right");
        // An explicitly right-to-left paragraph aligned "left" is aligned to its start.
        let mut p = Paragraph::with_text("abc", CharProps::default());
        p.props = ParaProps { bidi: Some(true), align: Some(Align::Right), ..Default::default() };
        let h = html(&doc_of(vec![para_block(p)]));
        assert!(h.contains("<p dir=\"rtl\"") && h.contains("text-align:end;"), "{h}");
        assert_eq!(md(&doc_of(vec![para("مرحبا بالعالم")])), "مرحبا بالعالم");
    }

    #[test]
    fn lists_are_numbered_as_word_numbers_them_across_parts() {
        let mut d = Document::new();
        let num = d.numbering.add_list(wordcraft_doc::numbering::ListKind::Numbered);
        let item = |t: &str| {
            let mut p = Paragraph::with_text(t, CharProps::default());
            p.props.numbering = Some(NumRef { num, level: 0 });
            para_block(p)
        };
        d.body = (0..30).map(|i| item(&format!("Item {i} with enough words to fill a line of text here."))).collect();
        let h = html(&d);
        assert!(h.contains(">1.</span>Item 0") && h.contains(">30.</span>Item 29"), "{}", &h[..2000.min(h.len())]);
        // Small parts: the numbers go on from the parts before.
        let o = Options { part_chars: 500, ..Options::default() };
        let r = render_html(&d, &o, Window::Part(3)).unwrap();
        let first = r.piece.from;
        assert!(first > 0 && r.text.contains(&format!(">{}.</span>Item {first}", first + 1)), "{}", r.text);
        let m = render_markdown(&d, &o, Window::Part(2)).unwrap();
        assert!(m.text.starts_with(&format!("{}. Item {}", m.piece.from + 1, m.piece.from)), "{}", m.text);
    }

    #[test]
    fn symbol_and_wingdings_bullets_show_as_word_shows_them() {
        // Word writes its bullets in Symbol and Wingdings as private-use codes (U+F0B7 •,
        // U+F0A7 ▪, U+F0D8 ⮚) and its second level as `o` in Courier New; a w:sym character in
        // a run (Wingdings U+F0FC ✓) the same way.
        let mut d = Document::new();
        let mut levels = wordcraft_doc::numbering::levels_for(wordcraft_doc::numbering::ListKind::Bullet);
        for (lv, (text, font)) in levels.iter_mut().zip([
            ("\u{F0B7}", "Symbol"),
            ("o", "Courier New"),
            ("\u{F0A7}", "Wingdings"),
            ("\u{F0D8}", "Wingdings"),
            ("\u{F076}", "Wingdings"),
        ]) {
            lv.text = text.into();
            lv.chr.font = Some(font.into());
        }
        let num = d.numbering.try_add_levels(None, levels).unwrap();
        let item = |t: &str, level: u8| {
            let mut p = Paragraph::with_text(t, CharProps::default());
            p.props.numbering = Some(NumRef { num, level });
            para_block(p)
        };
        let mut tick = Paragraph::with_text("Done ", CharProps::default());
        tick.insert_text(tick.len(), "\u{F0FC}", &CharProps { font: Some("Wingdings".into()), ..Default::default() }).unwrap();
        d.body = vec![item("First", 0), item("Second", 1), item("Third", 2), item("Fourth", 3), item("Fifth", 4), para_block(tick)];
        let h = html(&d);
        for (label, text) in [("•", "First"), ("◦", "Second"), ("▪", "Third"), ("⮚", "Fourth"), ("❖", "Fifth")] {
            assert!(h.contains(&format!(">{label}</span>{text}")), "{label} {text}: {h}");
        }
        assert!(h.contains("Done <span style=\"font-family:'Segoe UI Symbol'") && h.contains(">✓</span>"), "{h}");
        assert!(!h.chars().any(|c| ('\u{F000}'..='\u{F0FF}').contains(&c)), "no private-use character is left to show as a box");
        assert!(!h.contains("font-family:'Symbol'") && !h.contains("font-family:'Wingdings'"));
        let m = md(&d);
        assert_eq!(m, "- First\n  - Second\n    - Third\n      - Fourth\n        - Fifth\n\nDone ✓", "{m}");
    }

    #[test]
    fn a_long_document_comes_in_parts_that_cover_every_block_once() {
        let mut blocks: Vec<Arc<Block>> = (0..5000).map(|i| para(&format!("Paragraph {i}: {}", "word ".repeat(30)))).collect();
        // A big table in the middle is split between its rows.
        let mut t = Table::new(400, 2, 300.0);
        t.rows[0].props.header = true;
        t.rows[0].cells[0].blocks = vec![para("Head")];
        for (i, r) in t.rows.iter_mut().enumerate().skip(1) {
            r.cells[0].blocks = vec![para(&format!("row {i} {}", "cell ".repeat(40)))];
        }
        blocks.insert(2500, Arc::new(Block::Table(t)));
        let d = doc_of(blocks);
        let o = Options { part_chars: 20_000, ..Options::default() };
        let pieces = plan(&d, o.part_chars, o.picture_budget);
        assert!(pieces.len() > 20, "{}", pieces.len());
        let mut next = (0usize, None::<usize>);
        for p in &pieces {
            match p.rows {
                Some((a, b)) => {
                    assert_eq!(p.from, 2500);
                    assert_eq!(a, next.1.unwrap_or(0), "rows follow on");
                    next = if b == 400 { (2501, None) } else { (2500, Some(b)) };
                }
                None => {
                    assert_eq!(p.from, next.0, "parts follow on");
                    next = (p.to, None);
                }
            }
        }
        assert_eq!(next.0, d.body.len(), "every block is in a part");
        let table_parts: Vec<&Piece> = pieces.iter().filter(|p| p.rows.is_some()).collect();
        assert!(table_parts.len() > 1);
        for (n, p) in pieces.iter().enumerate() {
            let r = render_html(&d, &o, Window::Part(n + 1)).unwrap();
            assert_eq!((r.part, r.parts, r.piece), (Some(n + 1), pieces.len(), *p));
            assert!(r.stats.characters <= 2 * o.part_chars, "part {} holds {}", n + 1, r.stats.characters);
            if let Some((a, _)) = p.rows
                && a > 0
            {
                assert!(r.text.contains(">Head</p>"), "header rows repeat");
            }
        }
        let last = render_markdown(&d, &o, Window::Part(pieces.len())).unwrap();
        assert!(last.text.trim_end().ends_with(&format!("Paragraph 4999: {}", "word ".repeat(30).trim_end())), "{}", last.text);
        let e = render_html(&d, &o, Window::Part(pieces.len() + 1)).unwrap_err();
        assert!(e.contains("out of range") && e.contains(&format!("{} parts", pieces.len())), "{e}");
        // One part holds everything when the part is large enough.
        assert_eq!(plan(&d, usize::MAX, u64::MAX).len(), 1);
    }

    #[test]
    fn headers_footers_and_page_breaks_make_sheets() {
        let mut d = Document::new();
        d.parts.insert(5, Part { kind: PartKind::Header, blocks: vec![para("Company header")], ..Default::default() });
        d.parts.insert(6, Part { kind: PartKind::Footer, blocks: vec![para("Confidential")], ..Default::default() });
        d.last_section.headers.default = Some(5);
        d.last_section.footers.default = Some(6);
        let mut brk = Paragraph::with_text("", CharProps::default());
        brk.insert_text(0, "\u{000C}", &CharProps::default()).unwrap();
        d.body = vec![para("Page one"), para_block(brk), para("Page two")];
        let h = html(&d);
        assert_eq!(h.matches("<div class=\"page\"").count(), 2, "{h}");
        assert_eq!(h.matches("Company header").count(), 2);
        assert_eq!(h.matches("Confidential").count(), 2);
        let m = md(&d);
        assert_eq!(m, "> **Header:** Company header\n\nPage one\n\n<!-- pagebreak -->\n\nPage two\n\n> **Footer:** Confidential");
    }

    #[test]
    fn hostile_numbers_and_empty_documents_never_panic() {
        let mut d = Document::new();
        d.body.clear();
        for w in [Window::Part(0), Window::Part(1), Window::Blocks { from: 0, to: 0 }] {
            let _ = render_html(&d, &Options::default(), w);
            let _ = render_markdown(&d, &Options::default(), w);
        }
        let mut t = Table::new(2, 2, 100.0);
        t.rows[0].cells[0].props.span = 999;
        t.rows[1].cells[0].props.vmerge = VMerge::Continue;
        t.grid = vec![f32::NAN, -5.0];
        let mut p = Paragraph::with_text("x", CharProps { size: Some(f32::INFINITY), ..Default::default() });
        p.props.indent_left = Some(f32::NAN);
        let d = doc_of(vec![Arc::new(Block::Table(t)), para_block(p)]);
        assert!(render_html(&d, &Options { part_chars: 0, picture_budget: 0, ..Options::default() }, Window::Part(usize::MAX)).is_err());
        assert!(render_html(&d, &Options::default(), Window::Part(1)).is_ok());
        assert!(render_markdown(&d, &Options::default(), Window::Blocks { from: 1, to: usize::MAX }).is_ok());
    }
}
