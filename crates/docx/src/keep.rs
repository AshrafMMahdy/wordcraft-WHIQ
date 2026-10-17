//! WHIQ: attributes the model has no field for, kept from the file and written back.
//!
//! When the reader turns an element into model properties (a run's `w:rPr`, a paragraph's
//! `w:pPr`, a table's `w:tblPr`, a section's `w:sectPr`…), it also asks the writer what those
//! properties would be written as, and compares: for each element the writer would write with
//! fewer attributes than the file had (`w:color w:themeColor`, `w:rFonts w:asciiTheme`,
//! `w:lang w:eastAsia`, `wp14:anchorId`, attributes in an unknown namespace…), it keeps an
//! *entry*: the element's path, the attributes the writer writes (its *signature*), and the
//! attributes it leaves out.
//!
//! On save, after writing the same properties, each written element whose path has an entry and
//! whose attributes are exactly the signature (nothing about it was edited) gets the left-out
//! attributes back, and the file's values of attributes the writer writes another way (a shadow's
//! `w14:algn`, a theme colour's cached rgb). An edited element (another colour, another font) is written from the model
//! alone, so a kept attribute can never contradict an edit (`w:themeColor` naming the old colour).
//!
//! Entries are kept as text on the model (`ext_attrs`): one per line, `path \t signature \t
//! attributes`, the path being `name#n/name#n…` (the n-th child of that name) below the
//! properties element, or `^` + a letter for the element that holds them (`^r` the run, `^p` the
//! paragraph, `^h` the hyperlink, `^t` the table, `^w` the row, `^c` the cell, `^s` the section).
//! Attributes are kept as written (escaped, unknown namespaces as `xhhhhhhhh` with their
//! declaration), so writing them back is a string insertion.

use crate::xml::{El, esc, unknown_uri, written_prefix};

/// Attributes never kept: editing-session ids and ids the writer regenerates (they must stay
/// unique), and namespace bookkeeping.
const NOISE: &[&str] = &[
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

/// Longest kept text on one set of properties.
const MAX_KEEP: usize = 16 * 1024;

/// Deepest path kept below the properties element.
const MAX_PATH_DEPTH: usize = 4;

/// One attribute as written: ` name="value"`, plus the declaration an unknown namespace needs.
fn render(name: &str, value: &str) -> Option<String> {
    if name.starts_with("xmlns") || NOISE.contains(&name) {
        return None;
    }
    if !name.starts_with('?') {
        return Some(format!(" {name}=\"{}\"", esc(value)));
    }
    let (p, local) = name.split_once(':')?;
    let w = written_prefix(p)?;
    let uri = unknown_uri(p.trim_start_matches('?'))?;
    Some(format!(" xmlns:{w}=\"{}\" {w}:{local}=\"{}\"", esc(&uri), esc(value)))
}

/// A written start tag, scanned: its name, its attributes (raw, as written), where the attribute
/// list ends (the offset of `>` or `/>`), and whether it is empty (`/>`).
struct Tag {
    start: usize,
    name: String,
    attrs: Vec<(String, String)>,
    end: usize,
    empty: bool,
}

/// Scan the start tag at `s[at..]` (`s[at]` is `<`).
fn scan_tag(s: &str, at: usize) -> Option<Tag> {
    let b = s.as_bytes();
    let mut i = at + 1;
    let start = i;
    while i < b.len() && !matches!(b[i], b' ' | b'\t' | b'\n' | b'\r' | b'/' | b'>') {
        i += 1;
    }
    let name = s.get(start..i)?.to_string();
    let mut attrs = Vec::new();
    loop {
        while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\r') {
            i += 1;
        }
        match b.get(i)? {
            b'>' => return Some(Tag { start: at, name, attrs, end: i, empty: false }),
            b'/' => return Some(Tag { start: at, name, attrs, end: i, empty: true }),
            _ => {}
        }
        let ks = i;
        while i < b.len() && !matches!(b[i], b'=' | b' ' | b'>' | b'/') {
            i += 1;
        }
        let key = s.get(ks..i)?.to_string();
        if b.get(i) != Some(&b'=') {
            return None;
        }
        i += 1;
        let q = *b.get(i)?;
        if q != b'"' && q != b'\'' {
            return None;
        }
        i += 1;
        let vs = i;
        while i < b.len() && b[i] != q {
            i += 1;
        }
        let value = s.get(vs..i)?.to_string();
        i += 1;
        attrs.push((key, value));
    }
}

/// The start tags of `s[from..]` with their paths (`name#n/…`, relative to the fragment).
fn tags_with_paths(s: &str, from: usize) -> Vec<(String, Tag)> {
    let mut out = Vec::new();
    // Per open element: its path and the count of each child name seen so far.
    let mut stack: Vec<(String, Vec<(String, usize)>)> = vec![(String::new(), Vec::new())];
    let mut i = from;
    let b = s.as_bytes();
    while let Some(off) = s.get(i..).and_then(|r| r.find('<')) {
        let at = i + off;
        match b.get(at + 1) {
            Some(b'/') => {
                if stack.len() > 1 {
                    stack.pop();
                }
                i = at + 2;
                continue;
            }
            Some(b'?') | Some(b'!') => {
                i = at + 2;
                continue;
            }
            None => break,
            _ => {}
        }
        let Some(tag) = scan_tag(s, at) else {
            i = at + 1;
            continue;
        };
        let Some((parent, counts)) = stack.last_mut() else { break };
        let n = match counts.iter_mut().find(|(k, _)| *k == tag.name) {
            Some((_, c)) => {
                *c += 1;
                *c - 1
            }
            None => {
                counts.push((tag.name.clone(), 1));
                0
            }
        };
        let path = if parent.is_empty() { format!("{}#{n}", tag.name) } else { format!("{parent}/{}#{n}", tag.name) };
        i = tag.end + 1;
        let empty = tag.empty;
        out.push((path.clone(), tag));
        if !empty {
            stack.push((path, Vec::new()));
        }
    }
    out
}

/// The signature of written attributes: ` k="v"` each, in order.
fn signature(attrs: &[(String, String)]) -> String {
    attrs.iter().map(|(k, v)| format!(" {k}=\"{v}\"")).collect()
}

/// The written name of a model attribute name (`?hhhhhhhh:x` → `xhhhhhhhh:x`).
fn written_name(name: &str) -> Option<String> {
    if !name.starts_with('?') {
        return Some(name.to_string());
    }
    let (p, local) = name.split_once(':')?;
    Some(format!("{}:{local}", written_prefix(p)?))
}

/// The attributes of `orig` the written tag (`written`) doesn't have, or has with another value,
/// rendered.
fn left_out(orig: &El, written: &[(String, String)]) -> String {
    let mut s = String::new();
    for (k, v) in &orig.attrs {
        let Some(wn) = written_name(k) else { continue };
        if let Some((_, wv)) = written.iter().find(|(x, _)| *x == wn) {
            // Written the same (compared as written: escaped).
            if *wv == esc(v) || wv.is_empty() {
                continue;
            }
        }
        if let Some(r) = render(k, v) {
            s.push_str(&r);
        }
    }
    s
}

/// The start tag `tag` with `extra`'s attributes: values replaced, the others added.
fn with_attrs(tag: &Tag, extra: &str) -> Option<String> {
    let extra = split_rendered(extra);
    if extra.is_empty() {
        return None;
    }
    let mut attrs = tag.attrs.clone();
    let mut added = String::new();
    for (k, r) in extra {
        match attrs.iter_mut().find(|(x, _)| *x == k) {
            Some((_, v)) => {
                let nv = r.rsplit_once("=\"").map(|(_, x)| x.trim_end_matches('"').to_string())?;
                *v = nv;
            }
            None => added.push_str(&r),
        }
    }
    Some(format!("<{}{}{added}", tag.name, signature(&attrs)))
}

fn push_entry(out: &mut String, path: &str, sig: &str, extra: &str) {
    if extra.is_empty() || path.contains(['\t', '\n']) || sig.contains(['\t', '\n']) || extra.contains(['\t', '\n']) {
        return;
    }
    out.push_str(path);
    out.push('\t');
    out.push_str(sig);
    out.push('\t');
    out.push_str(extra);
    out.push('\n');
}

/// Entries for the descendants of `orig` (the properties element as read) against `written`
/// (what the writer writes for the properties read from it: the content of the element, without
/// its own tags).
pub fn capture(orig: &El, written: &str) -> String {
    let tags = tags_with_paths(written, 0);
    let mut out = String::new();
    fn go(e: &El, parent: &str, tags: &[(String, Tag)], out: &mut String, depth: usize) {
        if depth >= MAX_PATH_DEPTH {
            return;
        }
        let mut counts: Vec<(&str, usize)> = Vec::new();
        for k in e.els() {
            let Some(wn) = written_name(&k.name) else { continue };
            let n = match counts.iter_mut().find(|(x, _)| *x == k.name) {
                Some((_, c)) => {
                    *c += 1;
                    *c - 1
                }
                None => {
                    counts.push((&k.name, 1));
                    0
                }
            };
            let path = if parent.is_empty() { format!("{wn}#{n}") } else { format!("{parent}/{wn}#{n}") };
            if let Some((_, t)) = tags.iter().find(|(p, _)| *p == path) {
                push_entry(out, &path, &signature(&t.attrs), &left_out(k, &t.attrs));
            }
            go(k, &path, tags, out, depth + 1);
        }
    }
    go(orig, "", &tags, &mut out, 0);
    out
}

/// The entry for the element holding the properties (`key` = `^r`, `^p`…): the attributes of
/// `orig` not in `written` (the attribute names the writer writes on that tag).
pub fn capture_tag(key: &str, orig: &El, written: &[&str]) -> String {
    let w: Vec<(String, String)> = written.iter().map(|k| ((*k).to_string(), String::new())).collect();
    let mut out = String::new();
    push_entry(&mut out, key, "", &left_out(orig, &w));
    out
}

/// Children of `w:rPr` / `w:pPr` written elsewhere than by `rpr_inner` / `ppr_inner` (tracked
/// changes, the paragraph mark, the section): never kept as missing.
pub const RPR_ELSEWHERE: &[&str] = &["w:rPrChange", "w:ins", "w:del", "w:moveFrom", "w:moveTo"];
pub const PPR_ELSEWHERE: &[&str] = &["w:rPr", "w:sectPr", "w:pPrChange"];

/// CT_RPr and CT_PPrBase element order (kept elements go where the schema puts them).
pub const RPR_ORDER: &[&str] = &[
    "w:rStyle",
    "w:rFonts",
    "w:b",
    "w:bCs",
    "w:i",
    "w:iCs",
    "w:caps",
    "w:smallCaps",
    "w:strike",
    "w:dstrike",
    "w:outline",
    "w:shadow",
    "w:emboss",
    "w:imprint",
    "w:noProof",
    "w:snapToGrid",
    "w:vanish",
    "w:webHidden",
    "w:color",
    "w:spacing",
    "w:w",
    "w:kern",
    "w:position",
    "w:sz",
    "w:szCs",
    "w:highlight",
    "w:u",
    "w:effect",
    "w:bdr",
    "w:shd",
    "w:fitText",
    "w:vertAlign",
    "w:rtl",
    "w:cs",
    "w:em",
    "w:lang",
    "w:eastAsianLayout",
    "w:specVanish",
    "w:oMath",
];
pub const PPR_ORDER: &[&str] = &[
    "w:pStyle",
    "w:keepNext",
    "w:keepLines",
    "w:pageBreakBefore",
    "w:framePr",
    "w:widowControl",
    "w:numPr",
    "w:suppressLineNumbers",
    "w:pBdr",
    "w:shd",
    "w:tabs",
    "w:suppressAutoHyphens",
    "w:kinsoku",
    "w:wordWrap",
    "w:overflowPunct",
    "w:topLinePunct",
    "w:autoSpaceDE",
    "w:autoSpaceDN",
    "w:bidi",
    "w:adjustRightInd",
    "w:snapToGrid",
    "w:spacing",
    "w:ind",
    "w:contextualSpacing",
    "w:mirrorIndents",
    "w:suppressOverlap",
    "w:jc",
    "w:textDirection",
    "w:textAlignment",
    "w:textboxTightWrap",
    "w:outlineLvl",
    "w:divId",
    "w:cnfStyle",
];

/// `e` without its `i`-th child element.
pub fn without_child(e: &El, i: usize) -> El {
    let mut k = 0;
    let kids = e
        .kids
        .iter()
        .filter(|n| match n {
            crate::xml::Node::El(_) => {
                k += 1;
                k - 1 != i
            }
            crate::xml::Node::Text(_) => true,
        })
        .cloned()
        .collect();
    El { name: e.name.clone(), attrs: e.attrs.clone(), kids }
}

/// Entries for the children of `orig` the writer writes nothing for (`written`: the properties'
/// content as written) that hold nothing the model reads (`inert(i)`: the model read without
/// child `i` is the same), but those in `elsewhere`: `+name` entries holding the element's markup,
/// written back where the schema puts it unless the writer writes one of that name
/// ([`restore_missing`]). Their absence can't be an edit: there is nothing in the model to edit.
pub fn capture_missing(orig: &El, written: &str, elsewhere: &[&str], inert: &dyn Fn(usize) -> bool) -> String {
    let names: Vec<String> = top_elements(written).into_iter().map(|(n, _)| n).collect();
    let mut out = String::new();
    for (i, k) in orig.els().enumerate() {
        let Some(wn) = written_name(&k.name) else { continue };
        if elsewhere.contains(&k.name.as_str()) || names.contains(&wn) || (k.attrs.is_empty() && k.kids.is_empty()) || !inert(i) {
            continue;
        }
        push_entry(&mut out, &format!("+{wn}"), "", &k.to_xml_ext());
    }
    out
}

/// Write back the kept elements (`+name` entries) the writer wrote nothing for in `s[from..]`.
pub fn restore_missing(s: &mut String, from: usize, keep: Option<&str>, order: &[&str]) {
    let Some(keep) = keep.filter(|k| k.contains("\n+") || k.starts_with('+')) else { return };
    let present: Vec<String> = s.get(from..).map(|r| top_elements(r).into_iter().map(|(n, _)| n).collect()).unwrap_or_default();
    let mut ext = String::new();
    for l in keep.lines() {
        let mut f = l.splitn(3, '\t');
        let (Some(p), Some(_), Some(x)) = (f.next(), f.next(), f.next()) else { continue };
        let Some(name) = p.strip_prefix('+') else { continue };
        if !present.iter().any(|n| n == name) {
            ext.push_str(x);
        }
    }
    insert_ordered(s, from, Some(&ext), order);
}

/// Add `extra` (entries) to the kept entries in `slot`.
pub fn add(slot: &mut Option<String>, extra: &str) {
    if extra.is_empty() {
        return;
    }
    let mut s = slot.take().unwrap_or_default();
    if s.len() + extra.len() <= MAX_KEEP {
        s.push_str(extra);
    }
    *slot = (!s.is_empty()).then_some(s);
}

/// Join kept entries (`None` when there are none or too many).
pub fn join(parts: &[&str]) -> Option<String> {
    let s: String = parts.concat();
    (!s.is_empty() && s.len() <= MAX_KEEP).then_some(s)
}

/// The attributes kept for holding element `key`.
pub fn tag_extra<'a>(keep: Option<&'a str>, key: &str) -> Option<&'a str> {
    keep?.lines().find_map(|l| {
        let mut f = l.splitn(3, '\t');
        (f.next()? == key).then_some(())?;
        let _sig = f.next()?;
        f.next().filter(|x| !x.is_empty())
    })
}

/// Give the written elements of `s[from..]` their kept attributes back (see the module docs).
pub fn restore(s: &mut String, from: usize, keep: Option<&str>) {
    let Some(keep) = keep.filter(|k| k.lines().any(|l| !l.starts_with('^'))) else { return };
    let mut inserts: Vec<(usize, usize, String)> = Vec::new();
    for (path, tag) in tags_with_paths(s, from) {
        for l in keep.lines() {
            let mut f = l.splitn(3, '\t');
            let (Some(p), Some(sig), Some(extra)) = (f.next(), f.next(), f.next()) else { continue };
            if p != path || signature(&tag.attrs) != sig {
                continue;
            }
            if let Some(new) = with_attrs(&tag, extra) {
                inserts.push((tag.start, tag.end, new));
            }
        }
    }
    inserts.sort_by_key(|(at, _, _)| std::cmp::Reverse(*at));
    for (from, to, x) in inserts {
        if s.is_char_boundary(from) && s.is_char_boundary(to) {
            s.replace_range(from..to, &x);
        }
    }
}

/// Add `extra` (rendered attributes) to the start tag at `s[at..]`, leaving out attributes it
/// already has.
pub fn extend_tag(s: &mut String, at: usize, extra: Option<&str>) {
    let Some(extra) = extra.filter(|x| !x.is_empty()) else { return };
    if s.as_bytes().get(at) != Some(&b'<') {
        return;
    }
    let Some(tag) = scan_tag(s, at) else { return };
    let fresh: String = split_rendered(extra).into_iter().filter(|(k, _)| !tag.attrs.iter().any(|(x, _)| x == k)).map(|(_, r)| r).collect();
    if !fresh.is_empty() && s.is_char_boundary(tag.end) {
        s.insert_str(tag.end, &fresh);
    }
}

/// Add `extra` to the last start tag written in `s` (just written: `s` ends with it).
pub fn extend_last_tag(s: &mut String, extra: Option<&str>) {
    if let Some(at) = s.rfind('<') {
        extend_tag(s, at, extra);
    }
}

/// Rendered attributes split per attribute (a declaration goes with the attribute after it):
/// (attribute name, its rendering).
fn split_rendered(x: &str) -> Vec<(String, String)> {
    let Some(tag) = scan_tag(&format!("<x{x}>"), 0) else { return Vec::new() };
    let mut out = Vec::new();
    let mut pending = String::new();
    for (k, v) in tag.attrs {
        let r = format!(" {k}=\"{v}\"");
        if k.starts_with("xmlns:") {
            pending.push_str(&r);
        } else {
            out.push((k, format!("{pending}{r}")));
            pending.clear();
        }
    }
    out
}

/// `Document::passthrough` key of the revisions' kept attributes: one line per author and date,
/// `author \t date \t attributes`.
pub const REVISION_ATTRS: &str = "wordcraft:revision-attrs";

/// Keep `attrs` for the revisions by `author` at `date` (the first seen wins).
pub fn note_revision(doc: &mut wordcraft_doc::Document, author: &str, date: &str, attrs: &str) {
    if author.contains(['\t', '\n']) || date.contains(['\t', '\n']) {
        return;
    }
    let mut s = doc.passthrough.get(REVISION_ATTRS).map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
    if s.lines().any(|l| l.split('\t').take(2).eq([author, date])) || s.len() > 1024 * 1024 {
        return;
    }
    s.push_str(&format!("{author}\t{date}\t{attrs}\n"));
    doc.passthrough.insert(REVISION_ATTRS.into(), std::sync::Arc::new(s.into_bytes()));
}

/// The revisions' kept attributes by (author, date).
pub fn revision_attrs(doc: &wordcraft_doc::Document) -> std::collections::HashMap<(String, String), String> {
    let Some(b) = doc.passthrough.get(REVISION_ATTRS) else { return Default::default() };
    String::from_utf8_lossy(b)
        .lines()
        .filter_map(|l| {
            let mut f = l.splitn(3, '\t');
            Some(((f.next()?.to_string(), f.next()?.to_string()), f.next()?.to_string()))
        })
        .collect()
}

/// WHIQ: insert kept property markup `ext` (elements written as markup) among the properties
/// written in `s[from..]`, each before the first written one the schema `order` puts after it
/// (at the end when it isn't in `order`, or nothing written comes after it).
pub fn insert_ordered(s: &mut String, from: usize, ext: Option<&str>, order: &[&str]) {
    let Some(ext) = ext.filter(|x| !x.is_empty()) else { return };
    let rank = |n: &str| order.iter().position(|o| *o == n);
    // The written top-level elements: (offset, name).
    let mut written: Vec<(usize, String)> = Vec::new();
    let mut depth = 0usize;
    let mut i = from;
    while let Some(off) = s.get(i..).and_then(|r| r.find('<')) {
        let at = i + off;
        if s.as_bytes().get(at + 1) == Some(&b'/') {
            depth = depth.saturating_sub(1);
            i = at + 2;
            continue;
        }
        let Some(tag) = scan_tag(s, at) else { break };
        if depth == 0 {
            written.push((at, tag.name.clone()));
        }
        if !tag.empty {
            depth += 1;
        }
        i = tag.end + 1;
    }
    let end = s.len();
    let mut inserts: Vec<(usize, String)> = Vec::new();
    for (name, xml) in top_elements(ext) {
        let at = match rank(&name) {
            Some(r) => written.iter().find(|(_, n)| rank(n).is_some_and(|x| x > r)).map_or(end, |(o, _)| *o),
            None => end,
        };
        inserts.push((at, xml));
    }
    // Same place: in the order kept.
    for (at, x) in inserts.into_iter().rev() {
        if s.is_char_boundary(at) {
            s.insert_str(at, &x);
        }
    }
}

/// The top-level elements of markup `x`: (name, markup).
fn top_elements(x: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut name = String::new();
    let mut i = 0;
    while let Some(off) = x.get(i..).and_then(|r| r.find('<')) {
        let at = i + off;
        if x.as_bytes().get(at + 1) == Some(&b'/') {
            depth = depth.saturating_sub(1);
            let close = x.get(at..).and_then(|r| r.find('>')).map_or(x.len(), |g| at + g + 1);
            if depth == 0
                && let Some(m) = x.get(start..close)
            {
                out.push((name.clone(), m.to_string()));
            }
            i = close;
            continue;
        }
        let Some(tag) = scan_tag(x, at) else { break };
        if depth == 0 {
            start = at;
            name = tag.name.clone();
        }
        if tag.empty {
            if depth == 0
                && let Some(m) = x.get(start..tag.end + 2)
            {
                out.push((name.clone(), m.to_string()));
            }
        } else {
            depth += 1;
        }
        i = tag.end + 1;
    }
    out
}

/// `Document::passthrough` key of the main part root's markup-compatibility attributes
/// (`mc:PreserveAttributes`, `mc:PreserveElements`, `mc:ProcessContent`), their names resolved:
/// one line per name, `attribute \t namespace URI \t local name`.
pub const ROOT_COMPAT: &str = "wordcraft:root-compat";

const MC_URI: &str = "http://schemas.openxmlformats.org/markup-compatibility/2006";

/// Keep the markup-compatibility attributes of the root of the main part `bytes`.
pub fn note_root_compat(doc: &mut wordcraft_doc::Document, bytes: Option<&[u8]>) {
    let Some(b) = bytes else { return };
    let head = String::from_utf8_lossy(b.get(..b.len().min(64 * 1024)).unwrap_or(b)).into_owned();
    let mut at = 0;
    let tag = loop {
        let Some(off) = head.get(at..).and_then(|r| r.find('<')) else { return };
        let i = at + off;
        match head.as_bytes().get(i + 1) {
            Some(b'?') | Some(b'!') => at = i + 1,
            Some(_) => match scan_tag(&head, i) {
                Some(t) => break t,
                None => return,
            },
            None => return,
        }
    };
    let decl = |p: &str| tag.attrs.iter().find(|(k, _)| k.strip_prefix("xmlns:") == Some(p)).map(|(_, v)| v.clone());
    let mut out = String::new();
    for (k, v) in &tag.attrs {
        let Some((p, local)) = k.split_once(':') else { continue };
        if p == "xmlns" || decl(p).as_deref() != Some(MC_URI) || !matches!(local, "PreserveAttributes" | "PreserveElements" | "ProcessContent") {
            continue;
        }
        for token in v.split_whitespace() {
            let Some((tp, tl)) = token.split_once(':') else { continue };
            let Some(uri) = decl(tp) else { continue };
            if !uri.contains(['\t', '\n']) && !tl.contains(['\t', '\n']) {
                out.push_str(&format!("{local}\t{uri}\t{tl}\n"));
            }
        }
    }
    if !out.is_empty() && out.len() <= MAX_KEEP {
        doc.passthrough.insert(ROOT_COMPAT.into(), std::sync::Arc::new(out.into_bytes()));
    }
}

/// The kept markup-compatibility attributes for the main part's root, rendered with the prefixes
/// the writer uses (and declarations of those it doesn't declare on the root anyway).
pub fn root_compat_attrs(doc: &wordcraft_doc::Document) -> Option<String> {
    let b = doc.passthrough.get(ROOT_COMPAT)?;
    let text = String::from_utf8_lossy(b).into_owned();
    let mut values: Vec<(String, Vec<String>)> = Vec::new();
    let mut decls = String::new();
    for l in text.lines() {
        let mut f = l.splitn(3, '\t');
        let (Some(attr), Some(uri), Some(local)) = (f.next(), f.next(), f.next()) else { continue };
        let Some(p) = crate::xml::written_prefix_for_uri(uri) else { continue };
        if !crate::xml::NAMESPACES.iter().any(|(x, _)| *x == p) && !decls.contains(&format!(" xmlns:{p}=")) {
            decls.push_str(&format!(" xmlns:{p}=\"{}\"", esc(uri)));
        }
        let name = format!("{p}:{local}");
        match values.iter_mut().find(|(a, _)| a == attr) {
            Some((_, v)) => v.push(name),
            None => values.push((attr.to_string(), vec![name])),
        }
    }
    let attrs: String = values.iter().map(|(a, v)| format!(" mc:{a}=\"{}\"", esc(&v.join(" ")))).collect();
    (!attrs.is_empty()).then(|| format!("{decls}{attrs}"))
}

/// The attributes of `e` beyond `known` (and the noise), rendered: for an element whose
/// attributes the model reads by name (a revision: `w:id`, `w:author`, `w:date`).
pub fn extra_attrs(e: &El, known: &[&str]) -> Option<String> {
    let mut s = String::new();
    for (k, v) in &e.attrs {
        if known.contains(&k.as_str()) {
            continue;
        }
        if let Some(r) = render(k, v) {
            s.push_str(&r);
        }
    }
    (!s.is_empty() && s.len() <= MAX_KEEP && !s.contains(['\t', '\n'])).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: &str = "xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"";

    #[test]
    fn left_out_attributes_come_back_on_unedited_elements_only() {
        let orig = crate::xml::parse(
            format!(r#"<w:rPr {W}><w:color w:val="2F5496" w:themeColor="accent1" w:themeShade="BF"/><w:b/><w:lang w:val="en-US" w:eastAsia="zh-CN"/></w:rPr>"#)
                .as_bytes(),
        )
        .unwrap();
        let written = r#"<w:b/><w:color w:val="2F5496"/><w:lang w:val="en-US"/>"#;
        let keep = capture(&orig, written);
        assert!(keep.contains("w:color#0\t w:val=\"2F5496\"\t w:themeColor=\"accent1\" w:themeShade=\"BF\""), "{keep}");
        // Unchanged: the attributes come back.
        let mut s = format!("<w:rPr>{written}</w:rPr>");
        restore(&mut s, "<w:rPr>".len(), Some(&keep));
        assert_eq!(
            s,
            r#"<w:rPr><w:b/><w:color w:val="2F5496" w:themeColor="accent1" w:themeShade="BF"/><w:lang w:val="en-US" w:eastAsia="zh-CN"/></w:rPr>"#
        );
        // An edited colour: no stale theme colour.
        let mut s = r#"<w:color w:val="FF0000"/><w:lang w:val="en-US"/>"#.to_string();
        restore(&mut s, 0, Some(&keep));
        assert_eq!(s, r#"<w:color w:val="FF0000"/><w:lang w:val="en-US" w:eastAsia="zh-CN"/>"#);
    }

    #[test]
    fn tags_and_unknown_namespaces() {
        let orig =
            crate::xml::parse(format!(r#"<w:p {W} xmlns:q="urn:example:q" w:rsidR="00AA" w14:paraId="1" xmlns:w14="http://schemas.microsoft.com/office/word/2010/wordml" q:mark="1"/>"#).as_bytes())
                .unwrap();
        let keep = capture_tag("^p", &orig, &[]);
        let extra = tag_extra(Some(&keep), "^p").unwrap();
        assert!(extra.contains(":mark=\"1\"") && extra.contains("xmlns:x") && !extra.contains("rsid") && !extra.contains("paraId"), "{extra}");
        let mut s = "<w:p w14:paraId=\"2\"><w:r/>".to_string();
        extend_tag(&mut s, 0, Some(extra));
        assert!(s.starts_with("<w:p w14:paraId=\"2\" xmlns:x") && s.ends_with(":mark=\"1\"><w:r/>"), "{s}");
        // Not twice.
        let before = s.clone();
        extend_tag(&mut s, 0, Some(extra));
        assert_eq!(s, before);
    }
}
