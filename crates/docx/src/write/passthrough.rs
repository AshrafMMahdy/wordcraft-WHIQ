//! WHIQ: writing back the opaque pass-through nodes the reader kept (`read/passthrough.rs`):
//! block-level elements around paragraphs, wrapper tags (balanced: a tag whose partner was
//! edited away is left out), unknown elements among runs, with their relationships renumbered
//! and their parts carried. Markup in a namespace no reader here knows is declared on the part's
//! root and listed in its `mc:Ignorable`, so Word skips what it doesn't know either.

use wordcraft_doc::graphic::{Embedded, EmbeddedRef};
use wordcraft_doc::{Block, Document, InlineObject, Paragraph};

use super::{PartRels, Writer};
use crate::xml::W;

pub const BLOCK: &str = "docx-block";
pub const BLOCK_OPEN: &str = "docx-block-open";
pub const BLOCK_CLOSE: &str = "docx-block-close";
pub const OPEN: &str = "docx-open";
pub const CLOSE: &str = "docx-close";
/// The markers of a block-level `mc:AlternateContent` around its chosen branch's blocks: the
/// element with that branch's content replaced by a slot (`w:wordcraftSlot w:n w:fp`), and the
/// slot's id.
pub const ALT_OPEN: &str = "docx-alt-open";
pub const ALT_CLOSE: &str = "docx-alt-close";

/// `Document::passthrough` key prefix of a kept `mc:AlternateContent` (see [`fingerprint`]).
pub const ALT_PREFIX: &str = "wordcraft:alt:";
/// `Document::passthrough` key prefix of a kept `w:drawing` (see `Reader::keep_drawing`).
pub const DRAWING_PREFIX: &str = "wordcraft:drw:";

/// A fingerprint of an object read from an `mc:AlternateContent`, with the text of the text box
/// it holds: when the object still has it at save time, nothing about it was edited and the
/// original markup (both branches) is written back. `None` for objects that don't qualify.
pub fn fingerprint(doc: &Document, o: &InlineObject) -> Option<String> {
    if !matches!(o, InlineObject::Image { .. } | InlineObject::Shape { .. } | InlineObject::Group { .. } | InlineObject::Graphic { .. }) {
        return None;
    }
    // The drawing id is the object's identity, assigned after it is read: not part of it.
    let mut plain = o.clone();
    clear_ids(&mut plain, 0);
    let mut s = format!("{plain:?}");
    let mut stories = Vec::new();
    collect_stories(o, &mut stories, 0);
    for id in stories {
        let part = doc.parts.get(&id)?;
        for b in &part.blocks {
            match &**b {
                Block::Para(p) => s.push_str(&format!("{:?}{:?}{:?}{:?}{:?}", p.text, p.runs, p.props, p.mark, p.objects)),
                // Tables in a text box: their paragraphs' cache counters make this differ, so the
                // object is written from the model (safe).
                Block::Table(t) => s.push_str(&format!("{t:?}")),
            }
        }
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Some(format!("{ALT_PREFIX}{h:016x}"))
}

fn clear_ids(o: &mut InlineObject, depth: usize) {
    if depth > 8 {
        return;
    }
    match o {
        InlineObject::Shape { extra, .. } => extra.id = 0,
        InlineObject::Group { children, .. } => {
            for c in children.iter_mut() {
                clear_ids(&mut c.obj, depth + 1);
            }
        }
        _ => {}
    }
}

fn collect_stories(o: &InlineObject, out: &mut Vec<u32>, depth: usize) {
    if depth > 8 {
        return;
    }
    match o {
        InlineObject::Shape { story: Some(id), .. } => out.push(*id),
        InlineObject::Group { children, .. } => {
            for c in children {
                collect_stories(&c.obj, out, depth + 1);
            }
        }
        _ => {}
    }
}

/// A kept `mc:AlternateContent` as stored in `Document::passthrough`: its relationships, a blank
/// line, its markup.
pub fn encode_kept(xml: &str, refs: &[EmbeddedRef]) -> Vec<u8> {
    let mut s = String::new();
    for r in refs {
        s.push_str(&format!("R\t{}\t{}\t{}\t{}\n", r.id, r.kind, r.target, if r.external { "E" } else { "I" }));
    }
    s.push('\n');
    s.push_str(xml);
    s.into_bytes()
}

pub fn decode_kept(b: &[u8]) -> Option<(String, Vec<EmbeddedRef>)> {
    let s = std::str::from_utf8(b).ok()?;
    let (head, xml) = s.split_once("\n\n").or_else(|| s.strip_prefix('\n').map(|x| ("", x)))?;
    let mut refs = Vec::new();
    for line in head.lines().filter(|l| !l.is_empty()) {
        let f: Vec<&str> = line.split('\t').collect();
        let ["R", id, kind, target, mode] = f.as_slice() else { return None };
        refs.push(EmbeddedRef { id: (*id).into(), kind: (*kind).into(), target: (*target).into(), external: *mode == "E" });
    }
    Some((xml.to_string(), refs))
}

/// Whether `o` is written around the paragraph (by `para_inner`) rather than among its runs.
pub fn is_block_object(o: &InlineObject) -> bool {
    matches!(o, InlineObject::Opaque { format, .. } if matches!(format.as_str(), BLOCK | BLOCK_OPEN | BLOCK_CLOSE | ALT_OPEN | ALT_CLOSE))
}

/// Objects that mark where kept markup or a content control starts or ends (they may stand for
/// something outside a range of blocks).
fn is_marker(o: &InlineObject) -> bool {
    matches!(o, InlineObject::ControlStart { .. } | InlineObject::ControlEnd)
        || matches!(o, InlineObject::Opaque { format, .. } if matches!(format.as_str(), BLOCK_OPEN | BLOCK_CLOSE | ALT_OPEN | ALT_CLOSE))
}

/// A fingerprint of a range of blocks: text, formatting, objects (drawings by [`fingerprint`]),
/// tables, without the markers of kept markup and content controls. Unchanged while nothing in
/// the range is edited; a block-level alternate content is written back only then.
pub fn blocks_fingerprint(doc: &Document, bl: &[std::sync::Arc<Block>]) -> String {
    fn para(doc: &Document, p: &Paragraph, s: &mut String) {
        s.push_str(&format!("P{:?}{:?}{:?}", p.props, p.mark, p.section));
        let mut last: Option<&wordcraft_doc::props::CharProps> = None;
        let mut k = 0;
        for (range, props) in p.run_ranges() {
            let Some(text) = p.text.get(range) else { continue };
            for c in text.chars() {
                let unit = if c == wordcraft_doc::para::OBJ {
                    let o = p.objects.get(k);
                    k += 1;
                    match o {
                        Some(o) if is_marker(o) => continue,
                        Some(o) => fingerprint(doc, o).unwrap_or_else(|| format!("{o:?}")),
                        None => continue,
                    }
                } else {
                    c.to_string()
                };
                if last != Some(props) {
                    s.push_str(&format!("|{props:?}|"));
                    last = Some(props);
                }
                s.push_str(&unit);
            }
        }
    }
    fn blocks(doc: &Document, bl: &[std::sync::Arc<Block>], s: &mut String, depth: usize) {
        if depth > 16 {
            return;
        }
        for b in bl {
            match &**b {
                Block::Para(p) => para(doc, p, s),
                Block::Table(t) => {
                    s.push_str(&format!("T{:?}{:?}", t.props, t.grid));
                    for r in &t.rows {
                        s.push_str(&format!("R{:?}", r.props));
                        for c in &r.cells {
                            s.push_str(&format!("C{:?}", c.props));
                            blocks(doc, &c.blocks, s, depth + 1);
                        }
                    }
                }
            }
        }
    }
    let mut s = String::new();
    blocks(doc, bl, &mut s, 0);
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// The slot's id and fingerprint in an alternate content marker's markup.
fn slot_attrs(xml: &str) -> Option<(String, String)> {
    let at = xml.find("<w:wordcraftSlot")?;
    let tag = xml.get(at..)?.split_once("/>")?.0;
    let attr = |name: &str| -> Option<String> {
        let v = tag.split_once(&format!(" {name}=\""))?.1;
        Some(v.split_once('"')?.0.to_string())
    };
    Some((attr("w:n")?, attr("w:fp")?))
}

/// Whether block `b` ends alternate content `id`.
fn ends_alternate(b: &Block, id: &str) -> bool {
    match b {
        Block::Para(p) => p.objects.iter().any(|o| matches!(o, InlineObject::Opaque { format, xml, .. } if format == ALT_CLOSE && xml == id)),
        Block::Table(t) => t.wraps.close.iter().any(|x| x.format == ALT_CLOSE && x.xml == id),
    }
}

/// A kept start (of a wrapper or an alternate content) or end, to write around a block.
pub(super) struct Marker<'a> {
    /// The object's byte offset in the paragraph (`None` for a table's).
    pub at: Option<usize>,
    pub format: &'a str,
    pub xml: &'a str,
    pub refs: &'a [EmbeddedRef],
    /// Start: how many of the block's content control starts come before it. End: how many of
    /// the block's content control ends come before it.
    pub controls_before: usize,
}

/// The markers that open at block `b`, in order: a paragraph's leading block-level objects, or a
/// table's kept starts.
pub(super) fn lead_markers(b: &Block) -> Vec<Marker<'_>> {
    let mut out = Vec::new();
    match b {
        Block::Para(p) => {
            let end = leading_end(p);
            let mut starts = 0;
            for (at, o) in objects(p) {
                if at >= end {
                    break;
                }
                match o {
                    InlineObject::ControlStart { .. } => starts += 1,
                    InlineObject::Opaque { format, xml, refs, .. } if matches!(format.as_str(), BLOCK | BLOCK_OPEN | ALT_OPEN) => {
                        out.push(Marker { at: Some(at), format, xml, refs, controls_before: starts })
                    }
                    _ => {}
                }
            }
        }
        Block::Table(t) => {
            let n = t.controls.open.len();
            for x in &t.wraps.open {
                out.push(Marker { at: None, format: &x.format, xml: &x.xml, refs: &x.refs, controls_before: n.saturating_sub(x.inner as usize) });
            }
        }
    }
    out
}

/// The markers that close at block `b`, in order: a paragraph's trailing wrapper and alternate
/// content ends, or a table's kept ends.
pub(super) fn trail_markers(b: &Block) -> Vec<Marker<'_>> {
    let mut out = Vec::new();
    match b {
        Block::Para(p) => {
            let start = p.text.char_indices().rev().find(|(_, c)| *c != wordcraft_doc::para::OBJ).map_or(0, |(i, c)| i + c.len_utf8());
            let mut ends = 0;
            for (at, o) in objects(p) {
                if at < start {
                    continue;
                }
                match o {
                    InlineObject::ControlEnd => ends += 1,
                    InlineObject::Opaque { format, xml, refs, .. } if matches!(format.as_str(), BLOCK_CLOSE | ALT_CLOSE) => {
                        out.push(Marker { at: Some(at), format, xml, refs, controls_before: ends })
                    }
                    _ => {}
                }
            }
        }
        Block::Table(t) => {
            for x in &t.wraps.close {
                out.push(Marker { at: None, format: &x.format, xml: &x.xml, refs: &x.refs, controls_before: x.inner as usize });
            }
        }
    }
    out
}

/// The paragraph's objects with their byte offsets.
fn objects(p: &Paragraph) -> Vec<(usize, &InlineObject)> {
    let mut out = Vec::new();
    let mut k = 0;
    for (i, c) in p.text.char_indices() {
        if c == wordcraft_doc::para::OBJ {
            if let Some(o) = p.objects.get(k) {
                out.push((i, o));
            }
            k += 1;
        }
    }
    out
}

/// The byte length of the paragraph's leading run of objects (nothing but objects before it).
fn leading_end(p: &Paragraph) -> usize {
    p.text.char_indices().find(|(_, c)| *c != wordcraft_doc::para::OBJ).map_or(p.text.len(), |(i, _)| i)
}

impl Writer<'_> {
    /// Opaque markup with relationships: renumbered for this part, its parts queued.
    fn opaque_markup(&mut self, xml: &str, refs: &[wordcraft_doc::graphic::EmbeddedRef], rels: &mut PartRels) -> Option<String> {
        if refs.is_empty() {
            return crate::xml::parse(xml.as_bytes()).is_ok().then(|| xml.to_string());
        }
        let src = Embedded { xml: xml.to_string(), refs: refs.to_vec() };
        self.embedded_tree(&src, rels, None).map(|root| root.to_xml_ext())
    }

    /// The original `mc:AlternateContent` of an object read from one and not edited since, with
    /// its relationships renumbered for this part and fresh drawing ids.
    pub(super) fn kept_alternate(&mut self, o: &InlineObject, rels: &mut PartRels) -> Option<String> {
        if !self.has_kept_alternates {
            return None;
        }
        let key = fingerprint(self.doc, o)?;
        let (xml, refs) = decode_kept(self.doc.passthrough.get(&key)?)?;
        let src = Embedded { xml, refs };
        let mut root = self.embedded_tree(&src, rels, None)?;
        self.fresh_drawing_ids(&mut root, 0);
        Some(root.to_xml_ext())
    }

    /// After writing object `o` from `w.s[start..]`: when the drawing it was read from is kept and
    /// the object is unchanged, give the written `w:drawing` the attributes and elements of the
    /// original the writer left out ([`merge_kept`]).
    pub(super) fn merge_kept_drawing(&self, s: &mut String, start: usize, o: &InlineObject, rels: &mut PartRels) {
        if !self.has_kept_drawings {
            return;
        }
        let Some(alt_key) = fingerprint(self.doc, o) else { return };
        // Written from its kept alternate content: that is the original already.
        if self.has_kept_alternates && self.doc.passthrough.contains_key(&alt_key) {
            return;
        }
        let key = alt_key.replacen(ALT_PREFIX, DRAWING_PREFIX, 1);
        let Some((xml, refs)) = self.doc.passthrough.get(&key).and_then(|b| decode_kept(b)) else { return };
        let Some(orig) = crate::xml::parse(xml.as_bytes()).ok() else { return };
        let Some((from, to)) = s.get(start..).and_then(drawing_span).map(|(a, b)| (start + a, start + b)) else { return };
        let Some(written) = s.get(from..to) else { return };
        let decls: String = crate::xml::NAMESPACES.iter().map(|(p, u)| format!(" xmlns:{p}=\"{u}\"")).collect();
        let Ok(root) = crate::xml::parse(format!("<wordcraftRoot{decls}>{written}</wordcraftRoot>").as_bytes()) else { return };
        let Some(mut d) = root.els().next().cloned() else { return };
        // Markup this parse couldn't name (a namespace declared outside the drawing): leave it.
        if unnamed(&d, 0) {
            return;
        }
        // An external relationship is added for this part when markup using it comes back.
        let mut ids: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let mut map = |old: &str| -> Option<String> {
            if let Some(n) = ids.get(old) {
                return Some(n.clone());
            }
            let r = refs.iter().find(|r| r.id == old && r.external)?;
            let n = rels.add(&r.kind, &r.target, true);
            ids.insert(old.to_string(), n.clone());
            Some(n)
        };
        merge_kept(&mut d, &orig, &mut map, 0);
        let merged = d.to_xml_ext();
        s.replace_range(from..to, &merged);
    }

    /// Give every `wp:docPr` under `e` a fresh id (they must be unique in the document).
    fn fresh_drawing_ids(&mut self, e: &mut crate::xml::El, depth: usize) {
        if depth > crate::xml::MAX_DEPTH {
            return;
        }
        if e.name == "wp:docPr" {
            let id = self.next_docpr();
            match e.attrs.iter_mut().find(|(k, _)| k == "id") {
                Some((_, v)) => *v = id,
                None => e.attrs.push(("id".into(), id)),
            }
        }
        for k in e.kids.iter_mut() {
            if let crate::xml::Node::El(c) = k {
                self.fresh_drawing_ids(c, depth + 1);
            }
        }
    }

    /// Before `<w:p>`: the block-level elements and wrapper starts that lead the paragraph (but
    /// those `blocks` wrote already, among the content controls).
    pub(super) fn opaque_before(&mut self, w: &mut W, p: &Paragraph, rels: &mut PartRels) {
        let end = leading_end(p);
        let done = std::mem::take(&mut self.lead_done);
        for (at, o) in objects(p) {
            if at >= end {
                break;
            }
            let InlineObject::Opaque { format, xml, refs, .. } = o else { continue };
            if !done.contains(&at) {
                self.open_marker(w, &Marker { at: Some(at), format, xml, refs, controls_before: 0 }, rels);
            }
        }
    }

    /// After `</w:p>`: the block-level elements that don't lead it, and wrapper ends (but those
    /// `blocks` writes after the content controls that end here).
    pub(super) fn opaque_after(&mut self, w: &mut W, p: &Paragraph, rels: &mut PartRels) {
        let end = leading_end(p);
        let later = std::mem::take(&mut self.trail_later);
        for (at, o) in objects(p) {
            let InlineObject::Opaque { format, xml, refs, .. } = o else { continue };
            match format.as_str() {
                BLOCK if at >= end => {
                    if let Some(x) = self.opaque_markup(xml, refs, rels) {
                        w.raw(&x);
                    }
                }
                BLOCK_CLOSE | ALT_CLOSE if !later.contains(&at) => self.close_marker(w, format, xml),
                _ => {}
            }
        }
    }

    /// Write a kept start (or an unknown block element) before a block. An alternate content is
    /// written only as decided for this block (`alt_here`, see `decide_alternates`).
    pub(super) fn open_marker(&mut self, w: &mut W, m: &Marker, rels: &mut PartRels) {
        match m.format {
            BLOCK => {
                if let Some(x) = self.opaque_markup(m.xml, m.refs, rels) {
                    w.raw(&x);
                }
            }
            BLOCK_OPEN => {
                if let Some(close) = close_tag(m.xml) {
                    w.raw(m.xml);
                    self.block_wraps.push((close, None));
                }
            }
            ALT_OPEN => {
                let Some((id, _)) = slot_attrs(m.xml) else { return };
                let Some(k) = self.alt_here.iter().position(|(i, _, _)| *i == id) else { return };
                let (id, prefix, suffix) = self.alt_here.remove(k);
                w.raw(&prefix);
                self.block_wraps.push((suffix, Some(id)));
            }
            _ => {}
        }
    }

    /// Write a kept end after a block: the innermost open wrapper's, if it is this one (else it
    /// was closed already, around a content control boundary).
    pub(super) fn close_marker(&mut self, w: &mut W, format: &str, xml: &str) {
        if self.block_wraps.len() <= self.block_wrap_base {
            return;
        }
        let ours = match (format, self.block_wraps.last()) {
            (BLOCK_CLOSE, Some((_, None))) => true,
            (ALT_CLOSE, Some((_, Some(id)))) => id == xml,
            _ => false,
        };
        if ours && let Some((tag, _)) = self.block_wraps.pop() {
            w.raw(&tag);
        }
    }

    /// For the alternate contents that open at block `i` of `bl`: whether each is written back
    /// (its blocks are unchanged), with its markup before and after the chosen branch's blocks.
    pub(super) fn decide_alternates(&mut self, bl: &[std::sync::Arc<Block>], i: usize, rels: &mut PartRels) {
        self.alt_here.clear();
        let Some(b) = bl.get(i) else { return };
        let wanted: Vec<(String, String, String, Vec<EmbeddedRef>)> = lead_markers(b)
            .into_iter()
            .filter(|m| m.format == ALT_OPEN)
            .filter_map(|m| slot_attrs(m.xml).map(|(id, fp)| (id, fp, m.xml.to_string(), m.refs.to_vec())))
            .collect();
        for (id, fp, xml, refs) in wanted {
            let Some(j) = (i..bl.len()).find(|j| bl.get(*j).is_some_and(|b| ends_alternate(b, &id))) else { continue };
            if bl.get(i..=j).is_none_or(|r| blocks_fingerprint(self.doc, r) != fp) {
                continue;
            }
            let Some(markup) = self.opaque_markup(&xml, &refs, rels) else { continue };
            let Some(at) = markup.find("<w:wordcraftSlot") else { continue };
            let Some(len) = markup.get(at..).and_then(|r| r.find("/>")) else { continue };
            let (Some(prefix), Some(suffix)) = (markup.get(..at), markup.get(at + len + 2..)) else { continue };
            self.alt_here.push((id, prefix.to_string(), suffix.to_string()));
        }
    }

    /// Close the block wrappers opened since `mark` (the end of a story or content control).
    pub(super) fn close_block_wraps(&mut self, w: &mut W, mark: usize) {
        while self.block_wraps.len() > mark.max(self.block_wrap_base) {
            if let Some((tag, _)) = self.block_wraps.pop() {
                w.raw(&tag);
            }
        }
    }

    /// An opaque object among the runs: unknown markup, or an inline wrapper's start or end.
    /// Returns false for objects written elsewhere.
    pub(super) fn opaque_inline(&mut self, w: &mut W, o: &InlineObject, rels: &mut PartRels) -> bool {
        let InlineObject::Opaque { format, xml, refs, .. } = o else { return false };
        match format.as_str() {
            OPEN => {
                if let Some(close) = close_tag(xml) {
                    w.raw(xml);
                    self.inline_wraps.push(close);
                }
                true
            }
            CLOSE => {
                if self.inline_wraps.len() > self.inline_wrap_base
                    && let Some(tag) = self.inline_wraps.pop()
                {
                    w.raw(&tag);
                }
                true
            }
            "docx" if !refs.is_empty() => {
                if let Some(x) = self.opaque_markup(xml, refs, rels) {
                    w.raw(&x);
                }
                true
            }
            f if is_block_object(o) || f == BLOCK || f == ALT_OPEN || f == ALT_CLOSE => true,
            _ => false,
        }
    }

    /// Close the inline wrappers still open in this paragraph (before `</w:p>`, a content control
    /// boundary or a hyperlink that must not cross them).
    pub(super) fn close_inline_wraps(&mut self, w: &mut W) {
        while self.inline_wraps.len() > self.inline_wrap_base {
            if let Some(tag) = self.inline_wraps.pop() {
                w.raw(&tag);
            }
        }
    }
}

/// The first `w:drawing` element in `s`: (start, end) byte offsets, nested drawings (in text
/// boxes) included.
fn drawing_span(s: &str) -> Option<(usize, usize)> {
    let start = s.find("<w:drawing")?;
    let mut depth = 0usize;
    let mut i = start;
    while let Some(off) = s.get(i..).and_then(|r| r.find("<w:drawing").into_iter().chain(r.find("</w:drawing>")).min()) {
        let at = i + off;
        if s.get(at..).is_some_and(|r| r.starts_with("</w:drawing>")) {
            depth = depth.checked_sub(1)?;
            i = at + "</w:drawing>".len();
            if depth == 0 {
                return Some((start, i));
            }
        } else {
            // `<w:drawing>` or `<w:drawing …>`, not `<w:drawingX`.
            let next = s.as_bytes().get(at + "<w:drawing".len()).copied();
            if matches!(next, Some(b'>' | b' ' | b'/')) {
                if s.get(at..).and_then(|r| r.find('>')).is_some_and(|g| s.as_bytes().get(at + g - 1) == Some(&b'/')) {
                    if depth == 0 {
                        return None;
                    }
                } else {
                    depth += 1;
                }
            }
            i = at + 1;
        }
    }
    None
}

/// Whether `e` or anything in it has a name the parser couldn't resolve (`?`, `?:…`).
fn unnamed(e: &crate::xml::El, depth: usize) -> bool {
    depth > crate::xml::MAX_DEPTH
        || e.name == "?"
        || e.name.starts_with("?:")
        || e.attrs.iter().any(|(k, _)| k == "?" || k.starts_with("?:"))
        || e.els().any(|c| unnamed(c, depth + 1))
}

/// `e` with its relationship references renumbered by `map`; `None` when one can't be (an
/// internal relationship: the writer's).
fn renumbered(e: &crate::xml::El, map: &mut dyn FnMut(&str) -> Option<String>, depth: usize) -> Option<crate::xml::El> {
    use crate::xml::Node;
    if depth > crate::xml::MAX_DEPTH {
        return None;
    }
    let mut out = crate::xml::El { name: e.name.clone(), attrs: Vec::new(), kids: Vec::new() };
    for (k, v) in &e.attrs {
        let v = if k.starts_with("r:") { map(v)? } else { v.clone() };
        out.attrs.push((k.clone(), v));
    }
    for k in &e.kids {
        out.kids.push(match k {
            Node::El(c) => Node::El(renumbered(c, map, depth + 1)?),
            Node::Text(t) => Node::Text(t.clone()),
        });
    }
    Some(out)
}

/// Merge `orig` (a kept drawing) into `w` (the same drawing as written): attributes `w` lacks are
/// added; children `w` lacks are inserted where they were (after the written counterpart of the
/// child before them), unless `w` has a child the original hasn't at that level (the writer says
/// it another way: nothing is added beside it) or they refer to relationships. Text boxes'
/// content is the writer's.
pub fn merge_kept(w: &mut crate::xml::El, orig: &crate::xml::El, map: &mut dyn FnMut(&str) -> Option<String>, depth: usize) {
    use crate::xml::Node;
    if depth > 64 {
        return;
    }
    for (k, v) in &orig.attrs {
        if !w.attrs.iter().any(|(x, _)| x == k) {
            let v = if k.starts_with("r:") { map(v) } else { Some(v.clone()) };
            if let Some(v) = v {
                w.attrs.push((k.clone(), v));
            }
        }
    }
    if orig.name == "w:txbxContent" {
        return;
    }
    // Children by name and index among those of that name.
    fn keys(kids: &[Node]) -> Vec<Option<(String, usize)>> {
        let mut seen: Vec<(String, usize)> = Vec::new();
        kids.iter()
            .map(|n| match n {
                Node::El(e) => {
                    let c = match seen.iter_mut().find(|(x, _)| *x == e.name) {
                        Some((_, c)) => {
                            *c += 1;
                            *c - 1
                        }
                        None => {
                            seen.push((e.name.clone(), 1));
                            0
                        }
                    };
                    Some((e.name.clone(), c))
                }
                Node::Text(_) => None,
            })
            .collect()
    }
    let wk = keys(&w.kids);
    let ok = keys(&orig.kids);
    let extra_written = wk.iter().flatten().any(|k| !ok.iter().flatten().any(|o| o == k));
    let mut inserts: Vec<(usize, crate::xml::El)> = Vec::new();
    let mut after: usize = 0;
    for (oi, key) in ok.iter().enumerate() {
        let (Some(key), Some(Node::El(oc))) = (key, orig.kids.get(oi)) else { continue };
        match wk.iter().position(|k| k.as_ref() == Some(key)) {
            Some(wi) => {
                if let Some(Node::El(wc)) = w.kids.get_mut(wi) {
                    merge_kept(wc, oc, map, depth + 1);
                }
                after = wi + 1;
            }
            None if !extra_written => {
                if let Some(e) = renumbered(oc, map, 0) {
                    inserts.push((after, e));
                }
            }
            None => {}
        }
    }
    for (at, e) in inserts.into_iter().rev() {
        let at = at.min(w.kids.len());
        w.kids.insert(at, Node::El(e));
    }
}

/// What is open around rows (or cells) while a table is written: a content control (`None`) or
/// a kept wrapper (its end tag).
pub(super) type Around = Vec<Option<String>>;

/// Before a row or cell: its content controls and kept wrappers, nested as read.
pub(super) fn open_around(
    w: &mut W,
    controls: &[wordcraft_doc::control::ContentControl],
    wraps: &[wordcraft_doc::table::WrapTag],
    stack: &mut Around,
) {
    let n = controls.len();
    let mut k = 0;
    for (i, c) in controls.iter().enumerate() {
        while let Some(x) = wraps.get(k).filter(|x| n.saturating_sub(x.inner as usize) <= i) {
            open_wrap(w, x, stack);
            k += 1;
        }
        super::sdt::open(w, c);
        stack.push(None);
    }
    for x in wraps.get(k..).unwrap_or(&[]) {
        open_wrap(w, x, stack);
    }
}

fn open_wrap(w: &mut W, x: &wordcraft_doc::table::WrapTag, stack: &mut Around) {
    if x.format == BLOCK_OPEN
        && let Some(close) = close_tag(&x.xml)
    {
        w.raw(&x.xml);
        stack.push(Some(close));
    }
}

/// After a row or cell: `n` content controls end, and its kept wrappers, nested as read. A
/// control's end first ends the wrappers still open inside it; a wrapper's end that isn't the
/// innermost open thing is left for later (the end of the table at the latest).
pub(super) fn close_around(w: &mut W, n: u32, wraps: &[wordcraft_doc::table::WrapTag], stack: &mut Around) {
    let mut k = 0;
    for i in 0..n as usize {
        while let Some(x) = wraps.get(k).filter(|x| x.inner as usize <= i) {
            close_wrap(w, x, stack);
            k += 1;
        }
        if !stack.iter().any(Option::is_none) {
            break;
        }
        while let Some(top) = stack.pop() {
            match top {
                Some(tag) => w.raw(&tag),
                None => {
                    super::sdt::close(w);
                    break;
                }
            }
        }
    }
    for x in wraps.get(k..).unwrap_or(&[]) {
        close_wrap(w, x, stack);
    }
}

fn close_wrap(w: &mut W, x: &wordcraft_doc::table::WrapTag, stack: &mut Around) {
    if x.format == BLOCK_CLOSE
        && matches!(stack.last(), Some(Some(_)))
        && let Some(Some(tag)) = stack.pop()
    {
        w.raw(&tag);
    }
}

/// End everything still open around rows or cells.
pub(super) fn close_all(w: &mut W, stack: &mut Around) {
    while let Some(top) = stack.pop() {
        match top {
            Some(tag) => w.raw(&tag),
            None => super::sdt::close(w),
        }
    }
}

/// `</name>` for a start tag `<name …>`.
fn close_tag(open: &str) -> Option<String> {
    let rest = open.strip_prefix('<')?;
    let name: String = rest.chars().take_while(|c| !matches!(c, ' ' | '/' | '>' | '\t' | '\n' | '\r')).collect();
    (!name.is_empty() && open.ends_with('>') && !open.ends_with("/>")).then(|| format!("</{name}>"))
}

/// Declarations `xmlns:xhhhhhhhh="…"` in a written part (markup in namespaces no reader here
/// knows) are repeated on the part's root, and their prefixes added to the root's
/// `mc:Ignorable`, as Word requires of namespaces it may not understand.
pub fn hoist_namespaces(bytes: &mut Vec<u8>) {
    const NEEDLE: &[u8] = b" xmlns:x";
    let mut found: Vec<(String, String)> = Vec::new();
    let mut i = 0;
    while let Some(off) = bytes.get(i..).and_then(|b| b.windows(NEEDLE.len()).position(|w| w == NEEDLE)) {
        let start = i + off + NEEDLE.len();
        i = start;
        let Some(hex) = bytes.get(start..start + 8) else { break };
        if !hex.iter().all(u8::is_ascii_hexdigit) || bytes.get(start + 8..start + 10) != Some(b"=\"") {
            continue;
        }
        let vstart = start + 10;
        let Some(len) = bytes.get(vstart..).and_then(|b| b.iter().position(|c| *c == b'"')) else { break };
        let prefix = format!("x{}", String::from_utf8_lossy(hex));
        let uri = String::from_utf8_lossy(bytes.get(vstart..vstart + len).unwrap_or_default()).into_owned();
        if !found.iter().any(|(p, _)| *p == prefix) {
            found.push((prefix, uri));
        }
        if found.len() > 256 {
            break;
        }
    }
    if found.is_empty() {
        return;
    }
    // The root's start tag: the first element after the prolog.
    let mut at = 0;
    let root_start = loop {
        let Some(lt) = bytes.get(at..).and_then(|b| b.iter().position(|c| *c == b'<')) else { return };
        let s = at + lt;
        match bytes.get(s + 1) {
            Some(b'?') | Some(b'!') => at = s + 1,
            Some(_) => break s,
            None => return,
        }
    };
    let Some(gt) = bytes.get(root_start..).and_then(|b| b.iter().position(|c| *c == b'>')) else { return };
    let mut root_end = root_start + gt;
    if bytes.get(root_end.saturating_sub(1)) == Some(&b'/') {
        root_end -= 1;
    }
    let tag = String::from_utf8_lossy(bytes.get(root_start..root_end).unwrap_or_default()).into_owned();
    let mut add = String::new();
    for (p, u) in &found {
        if !tag.contains(&format!(" xmlns:{p}=")) {
            add.push_str(&format!(" xmlns:{p}=\"{u}\""));
        }
    }
    let prefixes: Vec<&str> = found.iter().map(|(p, _)| p.as_str()).collect();
    let mut new_tag = tag.clone();
    if let Some(pos) = new_tag.find(" mc:Ignorable=\"") {
        let vstart = pos + " mc:Ignorable=\"".len();
        if let Some(vlen) = new_tag.get(vstart..).and_then(|r| r.find('"')) {
            let current: Vec<String> = new_tag.get(vstart..vstart + vlen).unwrap_or("").split_whitespace().map(str::to_string).collect();
            let mut list = current.clone();
            for p in &prefixes {
                if !current.iter().any(|c| c == p) {
                    list.push((*p).to_string());
                }
            }
            new_tag.replace_range(vstart..vstart + vlen, &list.join(" "));
        }
    } else {
        if !new_tag.contains(" xmlns:mc=") {
            add.push_str(" xmlns:mc=\"http://schemas.openxmlformats.org/markup-compatibility/2006\"");
        }
        add.push_str(&format!(" mc:Ignorable=\"{}\"", prefixes.join(" ")));
    }
    new_tag.push_str(&add);
    bytes.splice(root_start..root_end, new_tag.into_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_namespaces_are_declared_on_the_root_and_ignorable() {
        let mut b = br#"<?xml version="1.0"?><w:document xmlns:w="urn:w" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" mc:Ignorable="w14 w15"><w:body><x0a0b0c0d:t xmlns:x0a0b0c0d="urn:fut"/></w:body></w:document>"#.to_vec();
        hoist_namespaces(&mut b);
        let s = String::from_utf8(b).unwrap();
        assert!(s.contains(r#"mc:Ignorable="w14 w15 x0a0b0c0d" xmlns:x0a0b0c0d="urn:fut">"#), "{s}");
        let mut b = br#"<w:styles xmlns:w="urn:w"><x0a0b0c0d:t xmlns:x0a0b0c0d="urn:fut"/></w:styles>"#.to_vec();
        hoist_namespaces(&mut b);
        let s = String::from_utf8(b).unwrap();
        assert!(s.starts_with(r#"<w:styles xmlns:w="urn:w" xmlns:x0a0b0c0d="urn:fut" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" mc:Ignorable="x0a0b0c0d">"#), "{s}");
        // Nothing to do: unchanged.
        let mut b = b"<w:document xmlns:w=\"urn:w\"/>".to_vec();
        hoist_namespaces(&mut b);
        assert_eq!(b, b"<w:document xmlns:w=\"urn:w\"/>");
    }

    #[test]
    fn close_tags() {
        assert_eq!(close_tag("<w:customXml w:element=\"a\">").as_deref(), Some("</w:customXml>"));
        assert_eq!(close_tag("<w:bdo/>"), None);
        assert_eq!(close_tag("nonsense"), None);
    }
}
