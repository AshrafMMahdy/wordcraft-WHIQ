//! WHIQ: the round-trip diff tool. Compares a Word package with the same package after
//! open + save: the part list, every part's relationships, binary parts byte for byte, and the
//! story parts (body, headers, footers, notes, comments) as a *normalised* XML tree.
//!
//! The normalised tree names every element and attribute by namespace URI (canonical prefix
//! where one is known), sorts attributes, and leaves out what a save may regenerate without a
//! reader seeing any difference ([`NOISE_ATTRS`], [`NOISE_ELEMENTS`], empty property containers).
//! What remains is compared three ways:
//!
//! - an element inventory: every element name the original story part has must appear in the
//!   saved one at least as often (anything less is a *loss*: something was dropped);
//! - the paragraph texts, in order (any difference is a *text change*);
//! - the whole normalised tree (a difference here that is neither a loss nor a text change is a
//!   *rewrite*: the saved file says the same thing in other words, e.g. `w:b w:val="1"` for
//!   `w:b`).
//!
//! `wordcraft-cli roundtrip-check FILE` runs this on any file; `tests/corpus_roundtrip.rs` runs it
//! on a corpus of hand-written packages covering tracked changes, moves, SmartArt, charts, custom
//! XML, `w14` markup, OLE objects, 3-D models and macros.

use std::collections::{BTreeMap, BTreeSet};

use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;

use crate::DocxError;
use crate::package::Package;

/// Attributes a save may drop or regenerate without any visible change: editing-session ids
/// (`w:rsid*`), paragraph/text ids Word regenerates, and markup-compatibility bookkeeping.
pub const NOISE_ATTRS: &[&str] = &[
    "w:rsidR",
    "w:rsidRPr",
    "w:rsidRDefault",
    "w:rsidP",
    "w:rsidDel",
    "w:rsidSect",
    "w:rsidTr",
    "w14:paraId",
    "w14:textId",
    "mc:Ignorable",
    "xml:space",
];

/// Elements a save may drop without any visible change: proofing state and the page breaks of
/// the last layout Word did (`w:lastRenderedPageBreak`, a layout cache).
pub const NOISE_ELEMENTS: &[&str] = &["w:proofErr", "w:lastRenderedPageBreak"];

/// The parts compared as stories.
fn is_story(part: &str) -> bool {
    let name = part.rsplit('/').next().unwrap_or(part);
    part.starts_with("word/")
        && part.ends_with(".xml")
        && (name == "document.xml"
            || name.starts_with("header")
            || name.starts_with("footer")
            || name == "footnotes.xml"
            || name == "endnotes.xml"
            || name == "comments.xml")
}

/// Known namespace URIs and the prefixes reports use for them.
const PREFIXES: &[(&str, &str)] = &[
    ("w", "http://schemas.openxmlformats.org/wordprocessingml/2006/main"),
    ("r", "http://schemas.openxmlformats.org/officeDocument/2006/relationships"),
    ("wp", "http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"),
    ("a", "http://schemas.openxmlformats.org/drawingml/2006/main"),
    ("c", "http://schemas.openxmlformats.org/drawingml/2006/chart"),
    ("pic", "http://schemas.openxmlformats.org/drawingml/2006/picture"),
    ("mc", "http://schemas.openxmlformats.org/markup-compatibility/2006"),
    ("w14", "http://schemas.microsoft.com/office/word/2010/wordml"),
    ("w15", "http://schemas.microsoft.com/office/word/2012/wordml"),
    ("w16se", "http://schemas.microsoft.com/office/word/2015/wordml/symex"),
    ("w16cid", "http://schemas.microsoft.com/office/word/2016/wordml/cid"),
    ("w16", "http://schemas.microsoft.com/office/word/2018/wordml"),
    ("w16cex", "http://schemas.microsoft.com/office/word/2018/wordml/cex"),
    ("w16du", "http://schemas.microsoft.com/office/word/2023/wordml/word16du"),
    ("wp14", "http://schemas.microsoft.com/office/word/2010/wordprocessingDrawing"),
    ("wps", "http://schemas.microsoft.com/office/word/2010/wordprocessingShape"),
    ("wpg", "http://schemas.microsoft.com/office/word/2010/wordprocessingGroup"),
    ("wpc", "http://schemas.microsoft.com/office/word/2010/wordprocessingCanvas"),
    ("wpi", "http://schemas.microsoft.com/office/word/2010/wordprocessingInk"),
    ("dgm", "http://schemas.openxmlformats.org/drawingml/2006/diagram"),
    ("m", "http://schemas.openxmlformats.org/officeDocument/2006/math"),
    ("v", "urn:schemas-microsoft-com:vml"),
    ("o", "urn:schemas-microsoft-com:office:office"),
    ("w10", "urn:schemas-microsoft-com:office:word"),
    ("am3d", "http://schemas.microsoft.com/office/drawing/2017/model3d"),
    ("a14", "http://schemas.microsoft.com/office/drawing/2010/main"),
    ("xml", "http://www.w3.org/XML/1998/namespace"),
];

fn qualify(uri: Option<&[u8]>, local: &[u8]) -> String {
    let local = String::from_utf8_lossy(local);
    match uri {
        None => local.into_owned(),
        Some(u) => match PREFIXES.iter().find(|(_, x)| x.as_bytes() == u) {
            Some((p, _)) => format!("{p}:{local}"),
            None => format!("{{{}}}{local}", String::from_utf8_lossy(u)),
        },
    }
}

/// A namespace-expanded element.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Node {
    pub name: String,
    pub attrs: BTreeMap<String, String>,
    pub kids: Vec<Node>,
    pub text: String,
}

/// Parse `bytes` into a namespace-expanded tree (bounded depth; `None` when it isn't XML).
pub fn parse_expanded(bytes: &[u8]) -> Option<Node> {
    let mut r = NsReader::from_reader(bytes);
    r.config_mut().trim_text(false);
    let mut stack: Vec<Node> = Vec::new();
    let mut buf = Vec::new();
    let mut root = None;
    loop {
        buf.clear();
        let (res, ev) = r.read_resolved_event_into(&mut buf).ok()?;
        let uri = match &res {
            ResolveResult::Bound(ns) => Some(ns.as_ref().to_vec()),
            _ => None,
        };
        match ev {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let mut n = Node { name: qualify(uri.as_deref(), e.local_name().as_ref()), ..Default::default() };
                for a in e.attributes().with_checks(false).flatten() {
                    let raw = a.key.as_ref();
                    if raw == b"xmlns" || raw.starts_with(b"xmlns:") {
                        continue;
                    }
                    let (ares, alocal) = r.resolver().resolve_attribute(a.key);
                    let aname = match ares {
                        ResolveResult::Bound(ns) => qualify(Some(ns.as_ref()), alocal.as_ref()),
                        _ => String::from_utf8_lossy(alocal.as_ref()).into_owned(),
                    };
                    let value = a
                        .normalized_value(quick_xml::XmlVersion::Explicit1_0)
                        .map(|v| v.into_owned())
                        .unwrap_or_else(|_| String::from_utf8_lossy(&a.value).into_owned());
                    n.attrs.insert(aname, value);
                }
                if matches!(ev, Event::Start(_)) && stack.len() < 512 {
                    stack.push(n);
                } else {
                    match stack.last_mut() {
                        Some(p) => p.kids.push(n),
                        None => root = Some(n),
                    }
                }
            }
            Event::End(_) => {
                let n = stack.pop()?;
                match stack.last_mut() {
                    Some(p) => p.kids.push(n),
                    None => root = Some(n),
                }
            }
            Event::Text(t) => {
                if let (Some(top), Ok(s)) = (stack.last_mut(), t.decode()) {
                    top.text.push_str(&s);
                }
            }
            Event::GeneralRef(g) => {
                if let Some(top) = stack.last_mut() {
                    if let Ok(Some(c)) = g.resolve_char_ref() {
                        top.text.push(c);
                    } else if let Ok(n) = g.decode()
                        && let Some(s) = quick_xml::escape::resolve_predefined_entity(&n)
                    {
                        top.text.push_str(s);
                    }
                }
            }
            Event::CData(t) => {
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&String::from_utf8_lossy(&t));
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    root
}

/// The tree without noise: [`NOISE_ATTRS`], [`NOISE_ELEMENTS`], empty `…Pr` containers, and
/// whitespace-only text outside `w:t`-like elements.
pub fn normalise(n: &Node) -> Node {
    fn go(n: &Node, depth: usize) -> Node {
        let mut out = Node { name: n.name.clone(), ..Default::default() };
        for (k, v) in &n.attrs {
            if !NOISE_ATTRS.contains(&k.as_str()) {
                out.attrs.insert(k.clone(), v.clone());
            }
        }
        let keeps_space = matches!(n.name.as_str(), "w:t" | "w:delText" | "w:instrText" | "w:delInstrText" | "m:t" | "a:t");
        out.text = if keeps_space || !n.text.trim().is_empty() { n.text.clone() } else { String::new() };
        if depth < 512 {
            for k in &n.kids {
                if NOISE_ELEMENTS.contains(&k.name.as_str()) {
                    continue;
                }
                let k = go(k, depth + 1);
                if k.name.ends_with("Pr") && k.kids.is_empty() && k.attrs.is_empty() && k.text.is_empty() {
                    continue;
                }
                out.kids.push(k);
            }
        }
        out
    }
    go(n, 0)
}

/// The normalised tree as indented lines (one element or text per line), for diffs.
pub fn lines(n: &Node) -> Vec<String> {
    fn go(n: &Node, depth: usize, out: &mut Vec<String>) {
        let pad = "  ".repeat(depth.min(64));
        let attrs: Vec<String> = n.attrs.iter().map(|(k, v)| format!(" {k}=\"{v}\"")).collect();
        out.push(format!("{pad}<{}{}>", n.name, attrs.concat()));
        if !n.text.is_empty() {
            out.push(format!("{pad}  \"{}\"", n.text));
        }
        if depth < 512 {
            for k in &n.kids {
                go(k, depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    go(n, 0, &mut out);
    out
}

/// Containers a save may merge or split without changing anything a reader sees (their
/// contents are compared: child elements by name, text by paragraph).
const CONTAINERS: &[&str] = &["w:r", "w:t", "w:rPr", "w:pPr", "w:instrText"];

/// A run's properties (`w:r/w:rPr`): compared per character ([`run_format_losses`]), since a
/// save may merge runs that look the same.
fn is_run_props(parent: &Node, n: &Node) -> bool {
    parent.name == "w:r" && n.name == "w:rPr"
}

fn inventory(n: &Node, out: &mut BTreeMap<String, usize>) {
    // A field is one field whichever way it's written: `w:fldSimple`, or `w:fldChar` begin …
    // separate … end around `w:instrText`.
    let key = match n.name.as_str() {
        "w:fldSimple" => Some("(field)".to_string()),
        "w:fldChar" => (n.attrs.get("w:fldCharType").map(String::as_str) == Some("begin")).then(|| "(field)".to_string()),
        name if CONTAINERS.contains(&name) => None,
        name => Some(name.to_string()),
    };
    if let Some(k) = key {
        *out.entry(k).or_default() += 1;
    }
    for k in &n.kids {
        if !is_run_props(n, k) {
            inventory(k, out);
        }
    }
}

/// Every attribute, as `element@attribute`, with how often it occurs. A field's attributes count
/// the same however it is written (its instruction is compared as text elsewhere).
fn attr_inventory(n: &Node, out: &mut BTreeMap<String, usize>) {
    let field = n.name == "w:fldSimple" || (n.name == "w:fldChar" && n.attrs.get("w:fldCharType").map(String::as_str) == Some("begin"));
    for k in n.attrs.keys() {
        let key = match k.as_str() {
            "w:instr" | "w:fldCharType" if field => continue,
            _ if field => format!("(field)@{k}"),
            _ => format!("{}@{k}", n.name),
        };
        *out.entry(key).or_default() += 1;
    }
    for k in &n.kids {
        if !is_run_props(n, k) {
            attr_inventory(k, out);
        }
    }
}

/// Per paragraph, each character's run properties: the set of `element@attribute=value` (and
/// bare `element`) of its run's `w:rPr`.
fn char_formats(n: &Node) -> Vec<Vec<BTreeSet<String>>> {
    fn props(rpr: &Node, prefix: &str, out: &mut BTreeSet<String>, depth: usize) {
        if depth > 8 {
            return;
        }
        for k in &rpr.kids {
            let name = format!("{prefix}{}", k.name);
            out.insert(name.clone());
            for (a, v) in &k.attrs {
                // Revision ids are renumbered on save; an rgb value beside a theme colour is
                // that colour's cache (readers recompute it from the theme).
                let cache = (matches!(a.as_str(), "w:val" | "w:color") && k.attrs.contains_key("w:themeColor"))
                    || (a == "w:fill" && k.attrs.contains_key("w:themeFill"));
                if a != "w:id" && !cache {
                    out.insert(format!("{name}@{a}={v}"));
                }
            }
            props(k, &format!("{name}/"), out, depth + 1);
        }
    }
    fn chars(n: &Node, cur: &BTreeSet<String>, out: &mut Vec<BTreeSet<String>>) {
        match n.name.as_str() {
            "w:t" | "w:delText" => out.extend(n.text.chars().map(|_| cur.clone())),
            "w:tab" | "w:br" | "w:cr" => out.push(cur.clone()),
            "w:p" | "w:txbxContent" => {}
            "w:r" => {
                let mut set = BTreeSet::new();
                if let Some(rpr) = n.kids.iter().find(|k| k.name == "w:rPr") {
                    props(rpr, "", &mut set, 0);
                }
                for k in n.kids.iter().filter(|k| k.name != "w:rPr") {
                    chars(k, &set, out);
                }
            }
            _ => {
                for k in &n.kids {
                    chars(k, cur, out);
                }
            }
        }
    }
    fn go(n: &Node, out: &mut Vec<Vec<BTreeSet<String>>>) {
        if n.name == "w:p" {
            let mut v = Vec::new();
            for k in &n.kids {
                chars(k, &BTreeSet::new(), &mut v);
            }
            out.push(v);
        }
        for k in &n.kids {
            go(k, out);
        }
    }
    let mut out = Vec::new();
    go(n, &mut out);
    out
}

/// Run properties a character had before the save and not after (paragraphs whose text is
/// unchanged): (property, characters).
fn run_format_losses(a: &Node, b: &Node) -> Vec<(String, usize)> {
    let (fa, fb) = (char_formats(a), char_formats(b));
    let mut lost: BTreeMap<String, usize> = BTreeMap::new();
    for (pa, pb) in fa.iter().zip(&fb) {
        if pa.len() != pb.len() {
            continue;
        }
        for (ca, cb) in pa.iter().zip(pb) {
            for p in ca.difference(cb) {
                *lost.entry(p.clone()).or_default() += 1;
            }
        }
    }
    lost.into_iter().collect()
}

/// The text of each paragraph (`w:p`), in document order (nested paragraphs, as in text boxes
/// and tables, each on their own).
pub fn paragraph_texts(n: &Node) -> Vec<String> {
    fn text_of(n: &Node, s: &mut String) {
        match n.name.as_str() {
            "w:t" | "w:delText" => s.push_str(&n.text),
            "w:tab" => s.push('\t'),
            "w:br" | "w:cr" => s.push('\n'),
            // Nested paragraphs are their own entries.
            "w:p" | "w:txbxContent" => return,
            _ => {}
        }
        for k in &n.kids {
            text_of(k, s);
        }
    }
    fn go(n: &Node, out: &mut Vec<String>) {
        if n.name == "w:p" {
            let mut s = String::new();
            for k in &n.kids {
                text_of(k, &mut s);
            }
            out.push(s);
        }
        for k in &n.kids {
            go(k, out);
        }
    }
    let mut out = Vec::new();
    go(n, &mut out);
    out
}

/// How one story part compares.
#[derive(Clone, Debug, Default)]
pub struct StoryDiff {
    pub part: String,
    /// Element names with fewer occurrences after the save: (name, before, after).
    pub losses: Vec<(String, usize, usize)>,
    /// Attributes (`element@attribute`) with fewer occurrences after the save: (name, before,
    /// after).
    pub attr_losses: Vec<(String, usize, usize)>,
    /// Run properties (`element`, `element@attribute=value`) some characters lost: (property,
    /// characters).
    pub format_losses: Vec<(String, usize)>,
    /// Paragraph texts that differ: (index, before, after).
    pub text_changes: Vec<(usize, String, String)>,
    /// The normalised trees are identical.
    pub identical: bool,
    /// First differing lines of the normalised trees (before, after), when not identical.
    pub first_difference: Option<(usize, String, String)>,
}

/// How a package compares with its re-saved self.
#[derive(Clone, Debug, Default)]
pub struct Report {
    pub missing_parts: Vec<String>,
    pub added_parts: Vec<String>,
    /// Non-XML parts whose bytes differ.
    pub changed_binary_parts: Vec<String>,
    /// Relationships of a part that the save lost: (source part, type, target).
    pub missing_rels: Vec<(String, String, String)>,
    pub stories: Vec<StoryDiff>,
    /// Parts saved under another name with the same bytes (media renumbered): (before, after).
    pub renamed_parts: Vec<(String, String)>,
}

impl Report {
    /// Nothing a reader can see changed and nothing was dropped.
    pub fn lossless(&self) -> bool {
        self.missing_parts.is_empty()
            && self.changed_binary_parts.is_empty()
            && self.missing_rels.is_empty()
            && self.stories.iter().all(|s| s.losses.is_empty() && s.attr_losses.is_empty() && s.format_losses.is_empty() && s.text_changes.is_empty())
    }

    /// One line per problem (empty when [`Report::lossless`]), then one line per rewritten story.
    pub fn summary(&self) -> Vec<String> {
        let mut out = Vec::new();
        out.extend(self.missing_parts.iter().map(|p| format!("missing part {p}")));
        out.extend(self.changed_binary_parts.iter().map(|p| format!("binary part changed {p}")));
        out.extend(self.missing_rels.iter().map(|(s, t, x)| format!("missing relationship {s} -> {x} ({t})")));
        for s in &self.stories {
            out.extend(s.losses.iter().map(|(n, a, b)| format!("{}: lost {n} ({a} -> {b})", s.part)));
            out.extend(s.attr_losses.iter().map(|(n, a, b)| format!("{}: lost attribute {n} ({a} -> {b})", s.part)));
            out.extend(s.format_losses.iter().map(|(p, n)| format!("{}: lost run property {p} on {n} characters", s.part)));
            out.extend(s.text_changes.iter().map(|(i, a, b)| format!("{}: paragraph {i} text {a:?} -> {b:?}", s.part)));
        }
        out
    }

    /// The report as JSON-friendly data (for `wordcraft-cli roundtrip-check`).
    pub fn to_json(&self) -> String {
        fn esc(s: &str) -> String {
            let mut o = String::from("\"");
            for c in s.chars() {
                match c {
                    '"' => o.push_str("\\\""),
                    '\\' => o.push_str("\\\\"),
                    '\n' => o.push_str("\\n"),
                    c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
                    c => o.push(c),
                }
            }
            o.push('"');
            o
        }
        let list = |v: &[String]| format!("[{}]", v.iter().map(|s| esc(s)).collect::<Vec<_>>().join(","));
        let stories: Vec<String> = self
            .stories
            .iter()
            .map(|s| {
                let diff = s
                    .first_difference
                    .as_ref()
                    .map_or("null".to_string(), |(i, a, b)| format!("{{\"line\":{i},\"before\":{},\"after\":{}}}", esc(a), esc(b)));
                format!(
                    "{{\"part\":{},\"identical\":{},\"losses\":{},\"textChanges\":{},\"firstDifference\":{diff}}}",
                    esc(&s.part),
                    s.identical,
                    list(&s.losses.iter().map(|(n, a, b)| format!("{n} {a}->{b}")).collect::<Vec<_>>()),
                    s.text_changes.len()
                )
            })
            .collect();
        format!(
            "{{\"lossless\":{},\"missingParts\":{},\"addedParts\":{},\"changedBinaryParts\":{},\"missingRelationships\":{},\"stories\":[{}],\"problems\":{}}}",
            self.lossless(),
            list(&self.missing_parts),
            list(&self.added_parts),
            list(&self.changed_binary_parts),
            list(&self.missing_rels.iter().map(|(s, t, x)| format!("{s} -> {x} ({t})")).collect::<Vec<_>>()),
            stories.join(","),
            list(&self.summary())
        )
    }
}

/// Parts that are bookkeeping of the package or of WordCraft's writer and may legitimately be
/// regenerated (their content is compared through the model elsewhere).
fn regenerated(part: &str) -> bool {
    part == "[Content_Types].xml" || part.ends_with(".rels") || part.starts_with("docProps/")
}

fn rel_set(pkg: &Package, part: &str) -> BTreeSet<(String, String)> {
    pkg.rels(part).list.iter().map(|r| (r.kind.clone(), r.target.clone())).collect()
}

/// Open `bytes` (a Word package named `name`: its extension picks `.docm`, `.dotx`…) and save it
/// again, as WordCraft does, and compare: the report and the saved bytes.
pub fn check(bytes: &[u8], name: &str) -> Result<(Report, Vec<u8>), DocxError> {
    let doc = crate::read(bytes)?;
    let ext = name.rsplit('.').next().unwrap_or("docx").to_ascii_lowercase();
    let saved = crate::write_as(&doc, crate::Flavor::from_ext(&ext).unwrap_or_default())?;
    Ok((compare(bytes, &saved)?, saved))
}

/// Compare `before` (the original package) with `after` (the same document saved again).
pub fn compare(before: &[u8], after: &[u8]) -> Result<Report, DocxError> {
    let a = Package::open(before)?;
    let b = Package::open(after)?;
    let (an, bn) = (a.part_names(), b.part_names());
    let mut rep =
        Report { missing_parts: an.difference(&bn).cloned().collect(), added_parts: bn.difference(&an).cloned().collect(), ..Default::default() };
    // A part saved under another name with the same bytes is renamed, not lost.
    let mut renames: BTreeMap<String, String> = BTreeMap::new();
    rep.missing_parts.retain(|m| {
        let bytes = a.get(m).unwrap_or_default();
        let to = rep.added_parts.iter().position(|x| !renames.values().any(|v| v == x) && b.get(x).unwrap_or_default() == bytes);
        match to {
            Some(i) if !m.ends_with(".rels") => {
                renames.insert(m.clone(), rep.added_parts.get(i).cloned().unwrap_or_default());
                false
            }
            _ => true,
        }
    });
    rep.added_parts.retain(|x| !renames.values().any(|v| v == x));
    rep.renamed_parts = renames.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let rename = |t: &str| renames.get(t).cloned().unwrap_or_else(|| t.to_string());
    // Headers and footers are paired by the section and slot that use them (a save may number
    // them differently).
    let (ha, hb) = (header_footer_slots(&a), header_footer_slots(&b));
    let mut paired: BTreeMap<String, String> = BTreeMap::new();
    for (slot, pa) in &ha {
        if let Some(pb) = hb.get(slot) {
            paired.entry(pa.clone()).or_insert_with(|| pb.clone());
        }
    }
    for (pa, pb) in &paired {
        if pa == pb {
            continue;
        }
        if let (Some(x), Some(y)) = (a.get(pa).and_then(parse_expanded), b.get(pb).and_then(parse_expanded)) {
            rep.stories.push(story_diff(&format!("{pa} (saved as {pb})"), &x, &y));
        }
        rep.missing_parts.retain(|m| m != pa);
        rep.added_parts.retain(|m| m != pb);
    }
    for part in an.intersection(&bn) {
        let (x, y) = (a.get(part).unwrap_or_default(), b.get(part).unwrap_or_default());
        if !part.ends_with(".rels") {
            // The relationships of this part, by type and target (ids may be renumbered).
            let ra: BTreeSet<(String, String)> = rel_set(&a, part).into_iter().map(|(t, x)| (t, rename(&x))).collect();
            let rb = rel_set(&b, part);
            for (t, x) in ra.difference(&rb) {
                rep.missing_rels.push((part.clone(), t.clone(), x.clone()));
            }
        }
        if is_story(part) {
            // A header or footer saved under another name was compared above.
            let same = paired.get(part).is_some_and(|p| p == part);
            if !same && (paired.contains_key(part) || paired.values().any(|p| p == part)) {
                continue;
            }
            let (Some(ta), Some(tb)) = (parse_expanded(x), parse_expanded(y)) else { continue };
            rep.stories.push(story_diff(part, &ta, &tb));
        } else if !regenerated(part) && !part.ends_with(".xml") && x != y {
            rep.changed_binary_parts.push(part.clone());
        }
    }
    // The package relationships (from `_rels/.rels`).
    let ra: BTreeSet<(String, String)> = rel_set(&a, "").into_iter().map(|(t, x)| (t, rename(&x))).collect();
    let rb = rel_set(&b, "");
    for (t, x) in ra.difference(&rb) {
        rep.missing_rels.push(("(package)".into(), t.clone(), x.clone()));
    }
    Ok(rep)
}

/// The header and footer parts the main document's sections use: (section, `header`/`footer`,
/// type) → part name.
fn header_footer_slots(pkg: &Package) -> BTreeMap<(usize, String, String), String> {
    let mut out = BTreeMap::new();
    let main = "word/document.xml";
    let Some(doc) = pkg.get(main).and_then(parse_expanded) else { return out };
    let rels = pkg.rels(main);
    let mut sections = Vec::new();
    fn find<'a>(n: &'a Node, out: &mut Vec<&'a Node>, depth: usize) {
        if depth > 64 {
            return;
        }
        if n.name == "w:sectPr" {
            out.push(n);
            return;
        }
        for k in &n.kids {
            find(k, out, depth + 1);
        }
    }
    find(&doc, &mut sections, 0);
    for (i, s) in sections.iter().enumerate() {
        for k in &s.kids {
            let kind = match k.name.as_str() {
                "w:headerReference" => "header",
                "w:footerReference" => "footer",
                _ => continue,
            };
            let ty = k.attrs.get("w:type").cloned().unwrap_or_else(|| "default".into());
            let Some(target) = k.attrs.get("r:id").and_then(|id| rels.by_id(id)).map(|r| r.target.clone()) else { continue };
            out.insert((i, kind.to_string(), ty), target);
        }
    }
    out
}

fn story_diff(part: &str, a: &Node, b: &Node) -> StoryDiff {
    let (na, nb) = (normalise(a), normalise(b));
    let (mut ia, mut ib) = (BTreeMap::new(), BTreeMap::new());
    inventory(&na, &mut ia);
    inventory(&nb, &mut ib);
    let losses = ia
        .iter()
        .filter_map(|(k, n)| {
            let m = ib.get(k).copied().unwrap_or(0);
            (m < *n).then(|| (k.clone(), *n, m))
        })
        .collect();
    let (mut aa, mut ab) = (BTreeMap::new(), BTreeMap::new());
    attr_inventory(&na, &mut aa);
    attr_inventory(&nb, &mut ab);
    let attr_losses = aa
        .iter()
        .filter_map(|(k, n)| {
            let m = ab.get(k).copied().unwrap_or(0);
            (m < *n).then(|| (k.clone(), *n, m))
        })
        .collect();
    let (pa, pb) = (paragraph_texts(&na), paragraph_texts(&nb));
    let mut text_changes = Vec::new();
    for i in 0..pa.len().max(pb.len()) {
        let (x, y) = (pa.get(i).cloned().unwrap_or_default(), pb.get(i).cloned().unwrap_or_default());
        if x != y {
            text_changes.push((i, x, y));
        }
    }
    let (la, lb) = (lines(&na), lines(&nb));
    let first_difference = (0..la.len().max(lb.len()))
        .find(|i| la.get(*i) != lb.get(*i))
        .map(|i| (i, la.get(i).cloned().unwrap_or_default(), lb.get(i).cloned().unwrap_or_default()));
    let format_losses = run_format_losses(&na, &nb);
    StoryDiff { part: part.to_string(), losses, attr_losses, format_losses, text_changes, identical: first_difference.is_none(), first_difference }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalising_drops_noise_and_keeps_meaning() {
        let x = br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:q="urn:x"><w:body><w:p w:rsidR="00AB"><w:pPr/><w:proofErr w:type="spellStart"/><w:r><w:t xml:space="preserve"> Hi </w:t></w:r><q:thing q:a="1"/></w:p></w:body></w:document>"#;
        let n = normalise(&parse_expanded(x).unwrap());
        let l = lines(&n);
        assert!(l.iter().any(|s| s.contains("{urn:x}thing") && s.contains("{urn:x}a=\"1\"")), "{l:?}");
        assert!(!l.iter().any(|s| s.contains("rsid") || s.contains("proofErr") || s.contains("pPr")), "{l:?}");
        assert_eq!(paragraph_texts(&n), [" Hi "]);
    }
}
