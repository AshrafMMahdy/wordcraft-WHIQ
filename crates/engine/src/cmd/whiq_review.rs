//! WHIQ: review tooling for agents, with the power of the platform's edit_docx.
//!
//! - `review.edit`: a batch of `replace`, `insert` (text before or after an anchor),
//!   `insert_paragraph` (before or after the paragraph holding an anchor), `delete_paragraph`,
//!   `delete` (the anchor text) and `comment` operations, located by anchor text (exact, then
//!   ignoring case, then ignoring quote/dash/space differences, then fuzzy, with a match report),
//!   all as tracked changes by the session's author, dated now. Atomic: when an operation can't be
//!   applied (and `keepGoing` isn't set), nothing changes. Nothing else in the document is
//!   touched. Each operation gets revisions of its own, so its changes can be accepted or rejected
//!   apart from the others'.
//! - `review.changeReport`: every tracked change with a stable id, kind, author, date, text and
//!   location (and the comments).
//! - `review.resolve`: accept or reject changes by id, by author, by kind, or all.
//! - `review.find`: where an anchor is (the match report alone).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};
use wordcraft_doc::para::OBJ;
use wordcraft_doc::{Path, Pos, Revision, RevisionKind, StoryRef};

use crate::{CmdError, CmdResult, CommandSpec, Selection, Session, p};

const EDIT_PARAMS: &str = r#"{"ops": [{"op": "replace" | "insert" | "insert_paragraph" | "delete_paragraph" | "delete" | "comment", "anchor" (or "find"): text to locate, "occurrence"?: n | "all" (replace, delete; default 1), "with"?: replacement (replace), "text"?: text to insert (insert, insert_paragraph; "\n" starts another paragraph) or the comment (comment), "position"?: "before" | "after" (insert, insert_paragraph; default after), "style"?: paragraph style (insert_paragraph), "matchCase"?: bool} | WHIQ structure: {"op": "set_style", anchor, style} | {"op": "insert_table_row", "table"?: n (1-based) or anchor, "row"?: n (after it; default the anchor's or the last), "before"?: bool, "cells"?: [text]} | {"op": "delete_table_row", table or anchor, "row"?: n} | {"op": "set_cell", table or anchor, row, col, text} | {"op": "delete_table", table or anchor} | {"op": "delete_picture", "index"?: n | "alt"?: text | anchor} | {"op": "delete_comment", "id"?: n | anchor} | {"op": "delete_section_break", "section": n}], "tracked"?: bool (default true), "fuzzy"?: bool (default true), "minScore"?: 0.5–1 (fuzzy matches below are refused; default 0.8), "keepGoing"?: bool (apply the operations that can be, report the rest)} → {changes: [{index, op, status, match: {kind: exact|case|normalized|fuzzy, score, found, paragraph}, location, changeIds, commentId}], applied, failed}"#;

pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec::new("review.edit", "Edit by Anchor", "Agents", edit).params(EDIT_PARAMS),
        CommandSpec::new("review.changeReport", "Change Report", "Agents", report)
            .params(r#"{"author"?: string, "kind"?: "insert|delete|format|moveFrom|moveTo"} → {changes: [{id, kind, author, date, text, paragraph, location, context}], comments: [{id, author, date, text, anchor, paragraph}], authors, counts}"#)
            .pure(),
        CommandSpec::new("review.resolve", "Accept or Reject Changes", "Agents", resolve).params(
            r#"{"accept": bool, "ids"?: [change id from review.changeReport], "author"?: string, "kind"?: "insert|delete|format|moveFrom|moveTo", "all"?: bool} → {resolved: [ids], notFound: [ids], remaining}"#,
        ),
        CommandSpec::new("review.find", "Find Anchor", "Agents", find).params(r#"{"anchor": string, "fuzzy"?: bool, "minScore"?: n, "matchCase"?: bool} → {matches: [{kind, score, found, paragraph, location}]}"#).pure(),
    ]
}

/// A paragraph's text as a reader sees it (tracked deletions and objects left out), and where
/// each of its characters is.
struct Vis {
    path: Path,
    chars: std::sync::Arc<Vec<char>>,
    /// Byte offset and length in the paragraph's text of each visible character.
    at: std::sync::Arc<Vec<(usize, usize)>>,
    len: usize,
}

type VisData = (std::sync::Arc<Vec<char>>, std::sync::Arc<Vec<(usize, usize)>>);

thread_local! {
    /// WHIQ: during one `review.edit` batch, each paragraph's visible text by its revision (and
    /// length): an operation rebuilds only the paragraphs the ones before it changed, not the
    /// whole document (300 operations on a 20,000-paragraph document read it once, not 300 times).
    static VIS_CACHE: std::cell::RefCell<Option<std::collections::HashMap<(u64, usize, usize), VisData>>> = const { std::cell::RefCell::new(None) };
}

/// Keep [`visible`]'s per-paragraph results for the batch running now (`on`), or drop them.
fn vis_cache(on: bool) {
    VIS_CACHE.with_borrow_mut(|c| *c = on.then(std::collections::HashMap::new));
}

fn vis_data(p: &wordcraft_doc::Paragraph) -> VisData {
    let mut chars = Vec::with_capacity(p.text.len());
    let mut at = Vec::with_capacity(p.text.len());
    for (range, props) in p.run_ranges() {
        if props.del.is_some() {
            continue;
        }
        let Some(text) = p.text.get(range.clone()) else { continue };
        for (i, c) in text.char_indices() {
            if c == OBJ {
                continue;
            }
            chars.push(c);
            at.push((range.start + i, c.len_utf8()));
        }
    }
    (std::sync::Arc::new(chars), std::sync::Arc::new(at))
}

fn visible(s: &Session) -> Vec<Vis> {
    let mut out = Vec::new();
    VIS_CACHE.with_borrow_mut(|cache| {
        for path in s.doc.para_paths(StoryRef::Body) {
            let Some(p) = s.doc.para(StoryRef::Body, &path) else { continue };
            let key = (p.rev, p.text.len(), p.runs.len());
            let (chars, at) = match cache.as_mut() {
                Some(c) => c.entry(key).or_insert_with(|| vis_data(p)).clone(),
                None => vis_data(p),
            };
            out.push(Vis { path, chars, at, len: p.len() });
        }
    });
    out
}

/// A located anchor: paragraph (index in `visible`), visible character range, how it matched.
#[derive(Clone, Debug)]
struct Hit {
    para: usize,
    start: usize,
    end: usize,
    kind: &'static str,
    score: f64,
}

impl Hit {
    fn found(&self, vis: &[Vis]) -> String {
        vis.get(self.para).map(|v| v.chars.get(self.start..self.end).unwrap_or(&[]).iter().collect()).unwrap_or_default()
    }
    /// The byte positions of the match.
    fn range(&self, vis: &[Vis]) -> Option<(Pos, Pos)> {
        let v = vis.get(self.para)?;
        let a = if self.start >= v.at.len() { v.len } else { v.at.get(self.start)?.0 };
        let b = match self.end.checked_sub(1).and_then(|i| v.at.get(i)) {
            Some((off, l)) if self.end > self.start => off + l,
            _ => a,
        };
        let pos = |off| Pos { story: StoryRef::Body, path: v.path.clone(), off };
        Some((pos(a), pos(b)))
    }
    fn json(&self, vis: &[Vis]) -> Value {
        let path = vis.get(self.para).map(|v| v.path.0.clone()).unwrap_or_default();
        json!({"kind": self.kind, "score": (self.score * 1000.0).round() / 1000.0, "found": self.found(vis), "paragraph": self.para, "path": path, "start": self.start, "end": self.end})
    }
}

/// Typographic variants read as their plain forms, for the `normalized` match.
fn plain_char(c: char) -> char {
    match c {
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' | '\u{2032}' => '\'',
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' | '\u{2033}' => '"',
        '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
        '\u{00A0}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' | '\t' | '\n' => ' ',
        c => c,
    }
}

/// `chars` normalised (plain quotes and dashes, one space for any run of white space, lower
/// case), with the original index of each normalised character.
fn normalise(chars: &[char]) -> (Vec<char>, Vec<usize>) {
    let mut out = Vec::with_capacity(chars.len());
    let mut origin = Vec::with_capacity(chars.len());
    for (i, c) in chars.iter().enumerate() {
        let c = plain_char(*c);
        if c == ' ' && out.last() == Some(&' ') {
            continue;
        }
        for l in c.to_lowercase() {
            out.push(l);
            origin.push(i);
        }
    }
    (out, origin)
}

fn exact_hits(vis: &[Vis], needle: &[char], fold: bool) -> Vec<Hit> {
    let mut hits = Vec::new();
    if needle.is_empty() {
        return hits;
    }
    let eq = |a: char, b: char| if fold { a.to_lowercase().eq(b.to_lowercase()) } else { a == b };
    for (pi, v) in vis.iter().enumerate() {
        if v.chars.len() < needle.len() {
            continue;
        }
        let mut i = 0;
        while i + needle.len() <= v.chars.len() {
            if v.chars.get(i..i + needle.len()).is_some_and(|w| w.iter().zip(needle).all(|(a, b)| eq(*a, *b))) {
                hits.push(Hit { para: pi, start: i, end: i + needle.len(), kind: if fold { "case" } else { "exact" }, score: 1.0 });
                i += needle.len();
            } else {
                i += 1;
            }
        }
    }
    hits
}

fn normalized_hits(vis: &[Vis], needle: &[char]) -> Vec<Hit> {
    let (n, _) = normalise(needle);
    let n: Vec<char> = {
        let s: String = n.iter().collect();
        s.trim().chars().collect()
    };
    let mut hits = Vec::new();
    if n.is_empty() {
        return hits;
    }
    for (pi, v) in vis.iter().enumerate() {
        let (h, origin) = normalise(&v.chars);
        let mut i = 0;
        while i + n.len() <= h.len() {
            if h.get(i..i + n.len()).is_some_and(|w| w == n.as_slice()) {
                let start = origin.get(i).copied().unwrap_or(0);
                let end = origin.get(i + n.len() - 1).map_or(start, |e| e + 1);
                hits.push(Hit { para: pi, start, end, kind: "normalized", score: 1.0 });
                i += n.len();
            } else {
                i += 1;
            }
        }
    }
    hits
}

/// Edit distance between two char slices (bounded by the callers' lengths).
fn distance(a: &[char], b: &[char]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        if let Some(c0) = cur.first_mut() {
            *c0 = i + 1;
        }
        for (j, cb) in b.iter().enumerate() {
            let sub = prev.get(j).copied().unwrap_or(0) + usize::from(ca != cb);
            let del = prev.get(j + 1).copied().unwrap_or(0) + 1;
            let ins = cur.get(j).copied().unwrap_or(0) + 1;
            if let Some(c) = cur.get_mut(j + 1) {
                *c = sub.min(del).min(ins);
            }
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev.get(b.len()).copied().unwrap_or(usize::MAX)
}

/// The best fuzzy matches of `needle` (windows starting and ending at word boundaries, of about
/// its length), best first.
fn fuzzy_hits(vis: &[Vis], needle: &[char], min_score: f64, limit: usize) -> Vec<Hit> {
    const MAX_NEEDLE: usize = 400;
    const MAX_WINDOWS: usize = 400_000;
    let (n, _) = normalise(needle.get(..needle.len().min(MAX_NEEDLE)).unwrap_or(&[]));
    if n.len() < 3 {
        return Vec::new();
    }
    let (lo, hi) = ((n.len() as f64 * 0.7).floor() as usize, (n.len() as f64 * 1.3).ceil() as usize + 1);
    let mut best: Vec<Hit> = Vec::new();
    let mut windows = 0usize;
    for (pi, v) in vis.iter().enumerate() {
        let (h, origin) = normalise(&v.chars);
        // Windows start at a word's first letter and end after a word's last one.
        let word = |i: usize| h.get(i).is_some_and(|c| c.is_alphanumeric());
        let starts: Vec<usize> = (0..h.len()).filter(|&i| word(i) && (i == 0 || !word(i - 1))).collect();
        for &st in &starts {
            for end in (st + lo.max(1))..=(st + hi).min(h.len()) {
                if !word(end - 1) || word(end) {
                    continue;
                }
                windows += 1;
                if windows > MAX_WINDOWS {
                    break;
                }
                let w = h.get(st..end).unwrap_or(&[]);
                let d = distance(w, &n);
                let score = 1.0 - d as f64 / n.len().max(w.len()) as f64;
                if score >= min_score {
                    let start = origin.get(st).copied().unwrap_or(0);
                    let e = origin.get(end - 1).map_or(start, |x| x + 1);
                    best.push(Hit { para: pi, start, end: e, kind: "fuzzy", score });
                }
            }
        }
    }
    best.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.para.cmp(&b.para)).then(a.start.cmp(&b.start)));
    // Overlapping windows of the same place: keep the best.
    let mut out: Vec<Hit> = Vec::new();
    for h in best {
        if !out.iter().any(|o| o.para == h.para && o.start < h.end && h.start < o.end) {
            out.push(h);
        }
        if out.len() >= limit {
            break;
        }
    }
    out
}

/// Every match of `anchor`, in document order, of the best kind found (exact, then ignoring case
/// unless `match_case`, then normalized, then fuzzy when `fuzzy`).
fn locate(vis: &[Vis], anchor: &str, match_case: bool, fuzzy: bool, min_score: f64) -> Vec<Hit> {
    let needle: Vec<char> = anchor.chars().collect();
    let hits = exact_hits(vis, &needle, false);
    if !hits.is_empty() {
        return hits;
    }
    if !match_case {
        let hits = exact_hits(vis, &needle, true);
        if !hits.is_empty() {
            return hits;
        }
        let hits = normalized_hits(vis, &needle);
        if !hits.is_empty() {
            return hits;
        }
    }
    if fuzzy {
        let mut hits = fuzzy_hits(vis, &needle, min_score, 1);
        hits.truncate(1);
        return hits;
    }
    Vec::new()
}

/// Closest phrases for a failed anchor (fuzzy, low threshold), for the error.
fn closest(vis: &[Vis], anchor: &str) -> Vec<Value> {
    let needle: Vec<char> = anchor.chars().collect();
    fuzzy_hits(vis, &needle, 0.4, 5).iter().map(|h| h.json(vis)).collect()
}

/// A text parameter under any of `keys`.
fn text_of<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| p::str(v, k))
}

/// The operation's name in plain form (`insertParagraph`, `insert-paragraph`: `insert_paragraph`).
fn op_name(v: &Value) -> String {
    let raw = p::str(v, "op").or_else(|| p::str(v, "type")).or_else(|| p::str(v, "action")).unwrap_or("");
    let mut out = String::new();
    for (i, c) in raw.trim().chars().enumerate() {
        if c == '-' || c == ' ' {
            out.push('_');
        } else if c.is_uppercase() && i > 0 {
            out.push('_');
            out.extend(c.to_lowercase());
        } else {
            out.extend(c.to_lowercase());
        }
    }
    out
}

/// Revisions of their own for the next edit: `new_revision` reuses the newest revision of the
/// same author, kind and second, so these make one operation's changes apart from another's.
fn fresh_revisions(s: &mut Session) -> [u32; 2] {
    let date = super::now_iso();
    let mut ids = [0u32; 2];
    for (slot, kind) in ids.iter_mut().zip([RevisionKind::Insert, RevisionKind::Delete]) {
        s.doc.revisions.push(Revision { kind, author: s.author.clone(), date: date.clone(), move_name: None });
        *slot = (s.doc.revisions.len() - 1) as u32;
    }
    ids
}

fn edit(s: &mut Session, v: &Value) -> CmdResult {
    let ops = v.get("ops").or_else(|| v.get("operations")).or_else(|| v.get("edits")).and_then(Value::as_array).cloned().unwrap_or_default();
    if ops.is_empty() {
        return Err(CmdError::Params(
            "`ops` must list at least one operation, e.g. [{\"op\": \"replace\", \"anchor\": \"old\", \"with\": \"new\"}]".into(),
        ));
    }
    if ops.len() > 10_000 {
        return Err(CmdError::Params("at most 10000 operations in one batch".into()));
    }
    let tracked = p::bool(v, "tracked").unwrap_or(true);
    let fuzzy = p::bool(v, "fuzzy").unwrap_or(true);
    let min_score = p::f32(v, "minScore").map(f64::from).filter(|x| x.is_finite()).unwrap_or(0.8).clamp(0.5, 1.0);
    let keep_going = p::bool(v, "keepGoing").unwrap_or(false);
    let was_tracking = s.doc.settings.track_changes;
    s.doc.settings.track_changes = tracked || s.doc.settings.protection.as_deref() == Some("trackedChanges");
    vis_cache(true);
    let r = edit_ops(s, &ops, tracked, fuzzy, min_score, keep_going, was_tracking);
    vis_cache(false);
    r
}

#[allow(clippy::too_many_arguments)]
fn edit_ops(s: &mut Session, ops: &[Value], tracked: bool, fuzzy: bool, min_score: f64, keep_going: bool, was_tracking: bool) -> CmdResult {
    let mut results: Vec<Value> = Vec::new();
    // (op index, its revisions, its comment)
    let mut owned: Vec<(usize, [u32; 2], Option<u32>)> = Vec::new();
    let mut failed = 0usize;
    for (i, op) in ops.iter().enumerate() {
        // A safe point: a cancel stops here, and the session puts the document back.
        crate::whiq_progress::report(i as u64, ops.len() as u64, "review edit operations");
        if let Err(e) = crate::whiq_progress::check(&format!("stopped before operation {} of {}; nothing was changed", i + 1, ops.len())) {
            s.doc.settings.track_changes = was_tracking;
            return Err(e);
        }
        match apply(s, op, i, fuzzy, min_score) {
            Ok((mut r, revs, comment)) => {
                if let Some(o) = r.as_object_mut() {
                    o.insert("index".into(), json!(i));
                    o.insert("status".into(), json!("applied"));
                }
                owned.push((i, revs, comment));
                results.push(r);
            }
            Err((reason, extra)) => {
                failed += 1;
                if !keep_going {
                    s.doc.settings.track_changes = was_tracking;
                    // The session puts the document back as it was: nothing of the batch stays.
                    let detail = json!({"failedAt": i, "op": op_name(op), "reason": reason, "closest": extra});
                    return Err(CmdError::Params(format!("operation {} ({}) failed: {reason}; nothing was changed\n{detail}", i + 1, op_name(op))));
                }
                results.push(json!({"index": i, "op": op_name(op), "status": "failed", "reason": reason, "closest": extra}));
            }
        }
    }
    s.doc.settings.track_changes = was_tracking;
    // The changes each operation made, by their report ids.
    let entries = entries(s);
    for (i, revs, comment) in owned {
        let ids: Vec<&str> = entries.iter().filter(|e| e.rev.is_some_and(|r| revs.contains(&r))).map(|e| e.id.as_str()).collect();
        if let Some(r) = results.iter_mut().find(|r| r["index"] == json!(i)) {
            r["changeIds"] = json!(ids);
            if let Some(c) = comment {
                r["commentId"] = json!(c);
            }
        }
    }
    Ok(json!({"changes": results, "applied": ops.len() - failed, "failed": failed, "tracked": tracked, "author": s.author}))
}

type OpResult = Result<(Value, [u32; 2], Option<u32>), (String, Vec<Value>)>;

/// Apply one operation; its report, its revisions and the comment it made.
fn apply(s: &mut Session, op: &Value, _index: usize, fuzzy: bool, min_score: f64) -> OpResult {
    let name = op_name(op);
    let anchor = text_of(op, &["anchor", "find", "target", "old", "search"]).unwrap_or("");
    let match_case = p::bool(op, "matchCase").or_else(|| p::bool(op, "match_case")).unwrap_or(false);
    // WHIQ: tables, pictures, comments, section breaks and styles, by index or by anchor.
    if STRUCTURAL.contains(&name.as_str()) {
        return structural(s, op, &name, anchor, match_case, fuzzy, min_score);
    }
    // Text edits add no content control: when the body has none, its matches skip the checks.
    let _hint = super::controls::assume_no_controls(s, StoryRef::Body);
    apply_text(s, op, &name, anchor, match_case, fuzzy, min_score)
}

fn apply_text(s: &mut Session, op: &Value, name: &str, anchor: &str, match_case: bool, fuzzy: bool, min_score: f64) -> OpResult {
    let name = name.to_string();
    if anchor.is_empty() {
        return Err((format!("`{name}` needs an `anchor` (the text to find)"), Vec::new()));
    }
    let vis = visible(s);
    let hits = locate(&vis, anchor, match_case, fuzzy, min_score);
    if hits.is_empty() {
        return Err((format!("anchor `{anchor}` not found"), closest(&vis, anchor)));
    }
    let all = op.get("occurrence").and_then(Value::as_str).is_some_and(|o| o.eq_ignore_ascii_case("all"));
    let nth = op.get("occurrence").and_then(Value::as_u64).unwrap_or(1).max(1) as usize;
    let chosen: Vec<Hit> = if all {
        hits.clone()
    } else {
        match hits.get(nth - 1) {
            Some(h) => vec![h.clone()],
            None => {
                return Err((
                    format!("anchor `{anchor}` occurs {} time(s), not {nth}", hits.len()),
                    hits.iter().take(5).map(|h| h.json(&vis)).collect(),
                ));
            }
        }
    };
    let matches: Vec<Value> = chosen.iter().map(|h| h.json(&vis)).collect();
    let tracked = s.doc.settings.track_changes;
    let revs = if tracked && name != "comment" { fresh_revisions(s) } else { [u32::MAX; 2] };
    let mut comment = None;
    let mut locations = Vec::new();
    let position = p::str(op, "position").unwrap_or("after").to_ascii_lowercase();
    let before = position.starts_with("before") || position == "above";
    // Applied from the last match back, so earlier positions stay valid.
    for h in chosen.iter().rev() {
        let Some((a, b)) = h.range(&vis) else { continue };
        match name.as_str() {
            "replace" | "replace_text" => {
                let with = text_of(op, &["with", "replace", "replacement", "new", "text"])
                    .ok_or_else(|| ("`replace` needs `with` (the new text)".to_string(), Vec::new()))?;
                s.sel = Selection { anchor: a.clone(), focus: b };
                super::type_text(s, &with.replace('\n', "\r")).map_err(|e| (e.to_string(), Vec::new()))?;
                locations.push(loc(&a));
            }
            "delete" | "delete_text" => {
                s.sel = Selection { anchor: a.clone(), focus: b };
                super::delete_selection(s).map_err(|e| (e.to_string(), Vec::new()))?;
                locations.push(loc(&a));
            }
            "insert" | "insert_text" => {
                let text = text_of(op, &["text", "with", "insert"]).ok_or_else(|| ("`insert` needs `text`".to_string(), Vec::new()))?;
                let at = if before { a.clone() } else { b };
                s.sel = Selection::caret(at.clone());
                super::type_text(s, &text.replace('\n', "\r")).map_err(|e| (e.to_string(), Vec::new()))?;
                locations.push(loc(&at));
            }
            "insert_paragraph" | "add_paragraph" => {
                let text = text_of(op, &["text", "with", "insert", "paragraph"])
                    .ok_or_else(|| ("`insert_paragraph` needs `text`".to_string(), Vec::new()))?;
                let len = s.doc.para(a.story, &a.path).map_or(0, |p| p.len());
                let body = text.replace('\n', "\r");
                let new_path = if before {
                    s.sel = Selection::caret(Pos { off: 0, ..a.clone() });
                    super::type_text(s, &format!("{body}\r")).map_err(|e| (e.to_string(), Vec::new()))?;
                    a.path.clone()
                } else {
                    s.sel = Selection::caret(Pos { off: len, ..a.clone() });
                    super::type_text(s, &format!("\r{body}")).map_err(|e| (e.to_string(), Vec::new()))?;
                    s.sel.focus.path.clone()
                };
                if let Some(style) = p::str(op, "style") {
                    s.sel = Selection::caret(Pos { story: a.story, path: new_path.clone(), off: 0 });
                    s.join_next_undo();
                    s.run("para.style", &json!({"style": style})).map_err(|e| (format!("style `{style}`: {e}"), Vec::new()))?;
                }
                locations.push(loc(&Pos { story: a.story, path: new_path, off: 0 }));
            }
            "delete_paragraph" | "remove_paragraph" => {
                let start = Pos { off: 0, ..a.clone() };
                let next = a.path.with_last(a.path.last().saturating_add(1));
                let end = if super::has_next_para(s, a.story, &a.path) {
                    Pos { story: a.story, path: next, off: 0 }
                } else {
                    Pos { off: s.doc.para(a.story, &a.path).map_or(0, |p| p.len()), ..a.clone() }
                };
                s.sel = Selection { anchor: start.clone(), focus: end };
                super::delete_selection(s).map_err(|e| (e.to_string(), Vec::new()))?;
                locations.push(loc(&start));
            }
            "comment" | "add_comment" => {
                let text = text_of(op, &["text", "comment", "body", "with"]).ok_or_else(|| ("`comment` needs `text`".to_string(), Vec::new()))?;
                s.sel = Selection { anchor: a.clone(), focus: b };
                let before_ids: BTreeSet<u32> = s.doc.comments.keys().copied().collect();
                s.join_next_undo();
                s.run("review.newComment", &json!({"text": text})).map_err(|e| (e.to_string(), Vec::new()))?;
                comment = s.doc.comments.keys().copied().find(|k| !before_ids.contains(k));
                locations.push(loc(&a));
            }
            other => {
                return Err((
                    format!(
                        "unknown operation `{other}`: use replace, insert, insert_paragraph, delete_paragraph, delete, comment, {}",
                        STRUCTURAL.join(", ")
                    ),
                    Vec::new(),
                ));
            }
        }
    }
    locations.reverse();
    let mut r = json!({"op": name, "anchor": anchor, "match": matches.first().cloned().unwrap_or(Value::Null), "location": locations.first().cloned().unwrap_or(Value::Null)});
    if chosen.len() > 1 {
        r["matches"] = json!(matches);
        r["locations"] = json!(locations);
    }
    Ok((r, revs, comment))
}

/// WHIQ: operations on the document's structure rather than on anchor text.
const STRUCTURAL: &[&str] =
    &["set_style", "insert_table_row", "delete_table_row", "set_cell", "delete_table", "delete_picture", "delete_comment", "delete_section_break"];

type StepErr = (String, Vec<Value>);

/// The paragraph `anchor` is in (the first match), or the error naming the closest text.
fn anchor_pos(s: &Session, anchor: &str, match_case: bool, fuzzy: bool, min_score: f64) -> Result<(Pos, Value), StepErr> {
    if anchor.is_empty() {
        return Err(("needs an `anchor` (text to find) or an index".into(), Vec::new()));
    }
    let vis = visible(s);
    let hits = locate(&vis, anchor, match_case, fuzzy, min_score);
    let Some(h) = hits.first() else { return Err((format!("anchor `{anchor}` not found"), closest(&vis, anchor))) };
    let (a, _) = h.range(&vis).ok_or_else(|| (format!("anchor `{anchor}` not found"), Vec::new()))?;
    Ok((a, h.json(&vis)))
}

/// The table an operation names: `table` (1-based, body tables in order, nested ones after their
/// table) or the innermost table holding `anchor`; with the row (0-based) the anchor is in.
fn table_of(s: &Session, op: &Value, anchor: &str, mc: bool, fz: bool, ms: f64) -> Result<(Path, Option<usize>, Value), StepErr> {
    if let Some(n) = p::u64(op, "table") {
        let mut tables = Vec::new();
        collect_tables(&s.doc.body, &[], &mut tables, 0);
        let count = tables.len();
        return tables
            .into_iter()
            .nth((n.max(1) - 1) as usize)
            .map(|t| (t, None, Value::Null))
            .ok_or_else(|| (format!("table {n} is out of range: the document has {count} tables"), Vec::new()));
    }
    let (a, m) = anchor_pos(s, anchor, mc, fz, ms)?;
    let (tp, row, _) = a.path.cell().ok_or_else(|| (format!("anchor `{anchor}` is not in a table"), Vec::new()))?;
    Ok((tp, Some(row), m))
}

fn collect_tables(bl: &wordcraft_doc::Blocks, prefix: &[u32], out: &mut Vec<Path>, depth: usize) {
    for (i, b) in bl.iter().enumerate() {
        if let wordcraft_doc::Block::Table(t) = &**b {
            let mut here = prefix.to_vec();
            here.push(i as u32);
            out.push(Path(here.clone()));
            if depth < 8 {
                for (r, row) in t.rows.iter().enumerate() {
                    for (c, cell) in row.cells.iter().enumerate() {
                        let mut inner = here.clone();
                        inner.extend([r as u32, c as u32]);
                        collect_tables(&cell.blocks, &inner, out, depth + 1);
                    }
                }
            }
        }
    }
}

/// The first paragraph of cell (`row`, `col`) of the table at `tp`.
fn cell_start(tp: &Path, row: usize, col: usize) -> Pos {
    let mut v = tp.0.clone();
    v.extend([row as u32, col as u32, 0]);
    Pos { story: StoryRef::Body, path: Path(v), off: 0 }
}

/// Select the whole text of cell (`row`, `col`).
fn select_cell_text(s: &mut Session, tp: &Path, row: usize, col: usize) -> Result<(), StepErr> {
    let n = s.doc.table(StoryRef::Body, tp).and_then(|t| t.rows.get(row)).and_then(|r| r.cells.get(col)).map(|c| c.blocks.len());
    let Some(n) = n else { return Err((format!("no cell at row {} column {}", row + 1, col + 1), Vec::new())) };
    let a = cell_start(tp, row, col);
    let mut last = a.path.clone();
    last = last.with_last(n.saturating_sub(1));
    let len = s.doc.para(StoryRef::Body, &last).map_or(0, |p| p.len());
    s.sel = Selection { anchor: a, focus: Pos { story: StoryRef::Body, path: last, off: len } };
    Ok(())
}

fn rows_of(s: &Session, tp: &Path) -> usize {
    s.doc.table(StoryRef::Body, tp).map_or(0, |t| t.rows.len())
}

/// 1-based `key` of `op` as a 0-based index, or `fallback`.
fn index(op: &Value, key: &str, fallback: Option<usize>) -> Option<usize> {
    p::u64(op, key).map(|n| (n.max(1) - 1) as usize).or(fallback)
}

fn run_step(s: &mut Session, id: &str, v: &Value) -> Result<Value, StepErr> {
    s.join_next_undo();
    s.run(id, v).map_err(|e| (format!("{id}: {e}"), Vec::new()))
}

/// Every picture in the body in document order: (paragraph path, byte offset, alt text).
fn pictures(s: &Session) -> Vec<(Path, usize, String)> {
    let mut out = Vec::new();
    for path in s.doc.para_paths(StoryRef::Body) {
        let Some(p) = s.doc.para(StoryRef::Body, &path) else { continue };
        let mut k = 0usize;
        for (off, c) in p.text.char_indices() {
            if c != OBJ {
                continue;
            }
            if let Some(o @ wordcraft_doc::para::InlineObject::Image { .. }) = p.objects.get(k)
                && p.props_of_char(off).del.is_none()
            {
                out.push((path.clone(), off, o.alt_text().to_string()));
            }
            k += 1;
        }
    }
    out
}

fn structural(s: &mut Session, op: &Value, name: &str, anchor: &str, mc: bool, fz: bool, ms: f64) -> OpResult {
    let mut r = json!({"op": name});
    let mut comment = None;
    let tracked = s.doc.settings.track_changes;
    let revs = if tracked && !matches!(name, "delete_comment" | "set_style" | "delete_section_break") { fresh_revisions(s) } else { [u32::MAX; 2] };
    match name {
        "set_style" => {
            let style = p::str(op, "style").ok_or_else(|| ("`set_style` needs `style` (a paragraph style name or id)".to_string(), Vec::new()))?;
            let (a, m) = anchor_pos(s, anchor, mc, fz, ms)?;
            s.sel = Selection::caret(a.clone());
            run_step(s, "para.style", &json!({"style": style}))?;
            r["match"] = m;
            r["location"] = loc(&a);
        }
        "insert_table_row" => {
            let (tp, arow, m) = table_of(s, op, anchor, mc, fz, ms)?;
            let n = rows_of(s, &tp);
            // After `row` (1-based), else after the anchor's row, else at the end; `before: true`
            // puts it above that row.
            let at = index(op, "row", arow).unwrap_or(n.saturating_sub(1)).min(n.saturating_sub(1));
            let above = p::bool(op, "before").unwrap_or(false);
            s.sel = Selection::caret(cell_start(&tp, at, 0));
            run_step(s, if above { "table.insertRowAbove" } else { "table.insertRowBelow" }, &json!({}))?;
            let new_row = if above { at } else { at + 1 };
            let cells: Vec<String> = op
                .get("cells")
                .and_then(Value::as_array)
                .map(|v| v.iter().map(|c| c.as_str().map_or_else(|| c.to_string(), str::to_string)).collect())
                .unwrap_or_default();
            for (c, text) in cells.iter().enumerate() {
                if text.is_empty() {
                    continue;
                }
                select_cell_text(s, &tp, new_row, c)?;
                super::type_text(s, &text.replace('\n', "\r")).map_err(|e| (e.to_string(), Vec::new()))?;
            }
            r["table"] = json!(tp.0);
            r["row"] = json!(new_row + 1);
            r["match"] = m;
        }
        "delete_table_row" => {
            let (tp, arow, m) = table_of(s, op, anchor, mc, fz, ms)?;
            let n = rows_of(s, &tp);
            let row = index(op, "row", arow)
                .ok_or_else(|| ("`delete_table_row` needs `row` (1-based) or an `anchor` in the row".to_string(), Vec::new()))?;
            if row >= n {
                return Err((format!("row {} is out of range: the table has {n} rows", row + 1), Vec::new()));
            }
            s.sel = Selection::caret(cell_start(&tp, row, 0));
            run_step(s, "table.deleteRow", &json!({}))?;
            r["table"] = json!(tp.0);
            r["row"] = json!(row + 1);
            r["match"] = m;
        }
        "set_cell" => {
            let (tp, arow, m) = table_of(s, op, anchor, mc, fz, ms)?;
            let row = index(op, "row", arow).ok_or_else(|| ("`set_cell` needs `row` and `col` (1-based)".to_string(), Vec::new()))?;
            let col = index(op, "col", None)
                .or_else(|| index(op, "column", None))
                .ok_or_else(|| ("`set_cell` needs `col` (1-based)".to_string(), Vec::new()))?;
            let text = text_of(op, &["text", "with", "value"]).unwrap_or("");
            select_cell_text(s, &tp, row, col)?;
            if s.sel.anchor == s.sel.focus && text.is_empty() {
                // Nothing to replace.
            } else if text.is_empty() {
                super::delete_selection(s).map_err(|e| (e.to_string(), Vec::new()))?;
            } else {
                super::type_text(s, &text.replace('\n', "\r")).map_err(|e| (e.to_string(), Vec::new()))?;
            }
            r["table"] = json!(tp.0);
            r["row"] = json!(row + 1);
            r["col"] = json!(col + 1);
            r["match"] = m;
        }
        "delete_table" => {
            let (tp, _, m) = table_of(s, op, anchor, mc, fz, ms)?;
            s.sel = Selection::caret(cell_start(&tp, 0, 0));
            run_step(s, "table.deleteTable", &json!({}))?;
            r["table"] = json!(tp.0);
            r["match"] = m;
        }
        "delete_picture" => {
            let list = pictures(s);
            let count = list.len();
            let alt = p::str(op, "alt").map(str::to_lowercase);
            let chosen = match (index(op, "index", None), alt) {
                (Some(i), _) => list.into_iter().nth(i),
                (None, Some(a)) => list.into_iter().find(|(_, _, t)| t.to_lowercase().contains(&a)),
                (None, None) if !anchor.is_empty() => {
                    let (a, _) = anchor_pos(s, anchor, mc, fz, ms)?;
                    list.into_iter().find(|(path, _, _)| *path == a.path)
                }
                _ => {
                    return Err((
                        "`delete_picture` needs `index` (1-based), `alt` (its alt text) or an `anchor` in its paragraph".into(),
                        Vec::new(),
                    ));
                }
            };
            let (path, off, alt) = chosen.ok_or_else(|| (format!("no such picture: the document has {count}"), Vec::new()))?;
            let a = Pos { story: StoryRef::Body, path: path.clone(), off };
            s.sel = Selection { anchor: a.clone(), focus: Pos { off: off + OBJ.len_utf8(), ..a.clone() } };
            super::delete_selection(s).map_err(|e| (e.to_string(), Vec::new()))?;
            r["alt"] = json!(alt);
            r["location"] = loc(&a);
        }
        "delete_comment" => {
            let id = match p::u64(op, "id") {
                Some(id) => id as u32,
                None => {
                    let (a, _) = anchor_pos(s, anchor, mc, fz, ms)?;
                    s.sel = Selection::caret(a);
                    super::review::comment_at_caret(s).ok_or_else(|| (format!("no comment on `{anchor}`"), Vec::new()))?
                }
            };
            if !s.doc.comments.contains_key(&id) {
                return Err((format!("no comment {id}"), Vec::new()));
            }
            run_step(s, "review.deleteComment", &json!({"id": id}))?;
            comment = Some(id);
            r["deleted"] = json!(id);
        }
        "delete_section_break" => {
            let n = p::u64(op, "section")
                .ok_or_else(|| ("`delete_section_break` needs `section` (1-based: the break ending that section)".to_string(), Vec::new()))?;
            let secs: Vec<usize> = s.doc.sections().into_iter().map(|(i, _)| i).collect();
            let k = (n.max(1) - 1) as usize;
            if k + 1 >= secs.len() {
                return Err((format!("section {n} has no break after it: the document has {} sections", secs.len()), Vec::new()));
            }
            let Some(&i) = secs.get(k) else { return Err(("no such section".into(), Vec::new())) };
            let path = Path::top(i);
            let p = s.doc.para_mut(StoryRef::Body, &path).map_err(|e| (e.to_string(), Vec::new()))?;
            p.section = None;
            p.touch();
            r["section"] = json!(n);
            r["sections"] = json!(secs.len() - 1);
        }
        _ => return Err((format!("unknown operation `{name}`"), Vec::new())),
    }
    Ok((r, revs, comment))
}

fn loc(p: &Pos) -> Value {
    json!({"story": "body", "path": p.path.0, "off": p.off})
}

/// One tracked change as the report gives it.
struct Entry {
    id: String,
    kind: &'static str,
    rev: Option<u32>,
    author: String,
    date: String,
    text: String,
    a: Pos,
    b: Pos,
}

/// Every tracked change in the body, with ids made of what the change is (kind, author, date,
/// text, the n-th such change): stable while other changes are accepted or rejected.
fn entries(s: &Session) -> Vec<Entry> {
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut out = Vec::new();
    for c in super::review::changes(s) {
        let rev = c.rev.and_then(|r| s.doc.revisions.get(r as usize));
        let author = rev.map(|r| r.author.clone()).unwrap_or_default();
        let date = rev.map(|r| r.date.clone()).unwrap_or_default();
        let text = range_text(s, &c.a, &c.b);
        let key = format!("{}|{author}|{date}|{text}|{}", c.kind, Value::Array(c.props.clone()));
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in key.bytes() {
            h ^= u64::from(byte);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        let base = format!("chg-{:08x}", h as u32);
        let n = seen.entry(base.clone()).or_insert(0);
        *n += 1;
        let id = if *n == 1 { base } else { format!("{base}-{n}") };
        out.push(Entry { id, kind: c.kind, rev: c.rev, author, date, text, a: c.a, b: c.b });
    }
    out
}

/// The text between `a` and `b`, as `copy_range(a, b).plain_text()` gives it (objects as their
/// plain text), without building a document for a range inside one paragraph: a report of
/// thousands of changes reads each change's text this way.
fn range_text(s: &Session, a: &Pos, b: &Pos) -> String {
    if a.path != b.path || a.story != b.story {
        return s.doc.copy_range(a, b).plain_text();
    }
    let Some(p) = s.doc.para(a.story, &a.path) else { return String::new() };
    let (from, to) = (a.off.min(b.off), a.off.max(b.off));
    let mut out = String::new();
    let mut k = 0usize;
    for (i, c) in p.text.char_indices() {
        let inside = i >= from && i < to;
        if c == OBJ {
            if inside && let Some(o) = p.objects.get(k) {
                out.push_str(o.plain_text());
            }
            k += 1;
        } else if inside {
            out.push(c);
        }
        if i >= to {
            break;
        }
    }
    out
}

fn paragraph_index(s: &Session, path: &Path) -> Option<usize> {
    s.doc.para_paths(StoryRef::Body).iter().position(|p| p == path)
}

/// Up to 40 characters around a change, for people and agents to see where it is.
fn context(s: &Session, a: &Pos, b: &Pos) -> String {
    let Some(p) = s.doc.para(a.story, &a.path) else { return String::new() };
    let text = &p.text;
    let start = (0..=a.off.min(text.len())).rev().filter(|i| text.is_char_boundary(*i)).nth(40).unwrap_or(0);
    let end_off = if b.path == a.path { b.off } else { text.len() };
    let end = (end_off.min(text.len())..=text.len()).filter(|i| text.is_char_boundary(*i)).nth(40).unwrap_or(text.len());
    text.get(start..end).unwrap_or("").chars().filter(|c| *c != OBJ).collect()
}

fn kind_matches(want: &str, kind: &str) -> bool {
    let w = want.to_ascii_lowercase().replace(['_', '-'], "");
    let k = kind.to_ascii_lowercase();
    w == k
        || (w == "insertion" && k == "insert")
        || (w == "deletion" && k == "delete")
        || (w == "move" && k.starts_with("move"))
        || (w == "formatting" && k == "format")
}

fn report(s: &mut Session, v: &Value) -> CmdResult {
    let author = p::str(v, "author");
    let kind = p::str(v, "kind");
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut authors: BTreeSet<String> = BTreeSet::new();
    let mut list = Vec::new();
    for e in entries(s) {
        authors.insert(e.author.clone());
        if author.is_some_and(|a| !a.eq_ignore_ascii_case(&e.author)) || kind.is_some_and(|k| !kind_matches(k, e.kind)) {
            continue;
        }
        *counts.entry(e.kind).or_default() += 1;
        list.push(json!({
            "id": e.id, "kind": e.kind, "author": e.author, "date": e.date, "text": e.text,
            "paragraph": paragraph_index(s, &e.a.path), "location": {"start": loc(&e.a), "end": loc(&e.b)},
            "context": context(s, &e.a, &e.b),
        }));
    }
    let comments: Vec<Value> = super::review::comment_list(s)
        .into_iter()
        .filter_map(|(id, at)| {
            let c = s.doc.comments.get(&id)?;
            let body = s.doc.parts.get(&c.part).map(|p| p.blocks.iter().filter_map(|b| b.as_para()).map(|p| p.plain_text()).collect::<Vec<_>>().join("\n")).unwrap_or_default();
            Some(json!({"id": id, "author": c.author, "date": c.date, "text": body, "resolved": c.resolved, "parent": c.parent, "paragraph": at.as_ref().and_then(|a| paragraph_index(s, &a.path)), "location": at.as_ref().map(loc)}))
        })
        .collect();
    Ok(json!({"changes": list, "comments": comments, "authors": authors, "counts": counts}))
}

fn resolve(s: &mut Session, v: &Value) -> CmdResult {
    let accept = p::bool(v, "accept").ok_or_else(|| CmdError::Params("`accept` is required: true accepts, false rejects".into()))?;
    let ids: Vec<String> = v
        .get("ids")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.trim().to_ascii_lowercase())).collect())
        .unwrap_or_default();
    let author = p::str(v, "author").map(str::trim).filter(|a| !a.is_empty());
    let kind = p::str(v, "kind");
    let all = p::bool(v, "all").unwrap_or(false);
    if ids.is_empty() && author.is_none() && kind.is_none() && !all {
        return Err(CmdError::Params("say which changes: `ids` (from review.changeReport), `author`, `kind`, or `all: true`".into()));
    }
    let wanted = |e: &Entry| {
        (ids.is_empty() || ids.contains(&e.id.to_ascii_lowercase()))
            && author.is_none_or(|a| a.eq_ignore_ascii_case(&e.author))
            && kind.is_none_or(|k| kind_matches(k, e.kind))
    };
    let mut done: Vec<String> = Vec::new();
    let mut tried: BTreeSet<String> = BTreeSet::new();
    // One change at a time, from the end of the document back, the list read again after each
    // (resolving one end of a move resolves the other).
    for _ in 0..100_000 {
        let list = entries(s);
        let Some(e) = list.iter().rev().find(|e| wanted(e) && !tried.contains(&e.id)) else { break };
        tried.insert(e.id.clone());
        s.sel = Selection { anchor: e.a.clone(), focus: e.b.clone() };
        super::review::resolve_sel(s, accept)?;
        done.push(e.id.clone());
    }
    let not_found: Vec<&String> = ids.iter().filter(|i| !done.iter().any(|d| d.eq_ignore_ascii_case(i))).collect();
    let remaining = entries(s).len();
    s.clamp_selection();
    Ok(json!({"accepted": accept, "resolved": done, "notFound": not_found, "remaining": remaining}))
}

fn find(s: &mut Session, v: &Value) -> CmdResult {
    let anchor = text_of(v, &["anchor", "text", "find"]).ok_or_else(|| CmdError::Params("`anchor` is required".into()))?;
    let fuzzy = p::bool(v, "fuzzy").unwrap_or(true);
    let min_score = p::f32(v, "minScore").map(f64::from).filter(|x| x.is_finite()).unwrap_or(0.8).clamp(0.3, 1.0);
    let vis = visible(s);
    let hits = locate(&vis, anchor, p::bool(v, "matchCase").unwrap_or(false), fuzzy, min_score);
    let list: Vec<Value> = hits.iter().map(|h| h.json(&vis)).collect();
    let closest = if hits.is_empty() { closest(&vis, anchor) } else { Vec::new() };
    Ok(json!({"matches": list, "closest": closest}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wordcraft_doc::Document;

    fn session(text: &str) -> Session {
        let mut s = Session::new(Document::from_text(text));
        s.author = "Reviewer".into();
        s
    }

    #[test]
    fn a_locked_content_control_still_refuses_a_replace_all() {
        // WHIQ: the controls hint skips the checks only for a body that has no controls.
        let mut s = session("Fee: TEN dollars. Also TEN here.");
        s.run("select.text", &json!({"text": "TEN", "occurrence": 1})).unwrap();
        s.run("developer.plainText", &json!({"tag": "fee"})).unwrap();
        s.run("developer.properties", &json!({"control": "fee", "lockEdit": true})).unwrap();
        let before = s.doc.clone();
        let e = s.run("review.edit", &json!({"ops": [{"op": "replace", "anchor": "TEN", "with": "TWELVE", "occurrence": "all"}]}));
        assert!(e.is_err(), "{e:?}");
        assert!(s.doc == before, "nothing changed");
        // Without controls the same batch goes through.
        let mut s = session("Fee: TEN dollars. Also TEN here.");
        let r = s
            .run("review.edit", &json!({"ops": [{"op": "replace", "anchor": "TEN", "with": "TWELVE", "occurrence": "all"}], "tracked": false}))
            .unwrap();
        assert_eq!(r["applied"], 1);
        assert_eq!(s.doc.final_text(StoryRef::Body), "Fee: TWELVE dollars. Also TWELVE here.");
    }

    #[test]
    fn structure_operations_edit_tables_pictures_comments_and_sections() {
        let mut s = session("Intro\nAfter");
        s.run("caret.docEnd", &json!({})).unwrap();
        s.run("insert.table", &json!({"rows": 2, "cols": 2})).unwrap();
        let r = s
            .run(
                "review.edit",
                &json!({"tracked": false, "ops": [
                    {"op": "set_cell", "table": 1, "row": 1, "col": 1, "text": "A1"},
                    {"op": "set_cell", "table": 1, "row": 2, "col": 2, "text": "B2"},
                    {"op": "insert_table_row", "table": 1, "cells": ["C1", "C2"]},
                    {"op": "delete_table_row", "anchor": "B2"},
                    {"op": "set_style", "anchor": "Intro", "style": "Heading 1"},
                ]}),
            )
            .unwrap();
        assert_eq!(r["applied"], 5, "{r}");
        let t = s.doc.body.iter().find_map(|b| b.as_table()).unwrap();
        let cell = |r: usize, c: usize| t.rows[r].cells[c].blocks[0].as_para().unwrap().text.clone();
        assert_eq!((t.rows.len(), cell(0, 0), cell(1, 0), cell(1, 1)), (2, "A1".into(), "C1".into(), "C2".into()));
        assert_eq!(s.doc.body[0].as_para().unwrap().props.style.as_deref(), Some("Heading1"));
        let e = s.run("review.edit", &json!({"ops": [{"op": "delete_table_row", "table": 9, "row": 1}]})).unwrap_err().to_string();
        assert!(e.contains("table 9 is out of range"), "{e}");
        let e = s.run("review.edit", &json!({"ops": [{"op": "delete_section_break", "section": 1}]})).unwrap_err().to_string();
        assert!(e.contains("no break after it"), "{e}");
    }

    #[test]
    fn a_batch_of_tracked_edits_by_anchor_reports_each_change() {
        let mut s = session("The quick brown fox.\nIt jumps over the lazy dog.\nThe end.");
        let r = s
            .run(
                "review.edit",
                &json!({"ops": [
                    {"op": "replace", "anchor": "quick brown", "with": "slow red"},
                    {"op": "insert_paragraph", "anchor": "lazy dog", "position": "after", "text": "A new paragraph."},
                    {"op": "comment", "anchor": "The end", "text": "Too short?"},
                    {"op": "delete_paragraph", "anchor": "It jumps"},
                ]}),
            )
            .unwrap();
        assert_eq!(r["applied"], 4, "{r}");
        for c in r["changes"].as_array().unwrap() {
            assert_eq!(c["status"], "applied", "{c}");
            assert!(c["location"]["path"].is_array(), "{c}");
        }
        assert!(r["changes"][2]["commentId"].is_u64());
        let rep = s.run("review.changeReport", &json!({})).unwrap();
        let changes = rep["changes"].as_array().unwrap();
        assert!(changes.iter().all(|c| c["author"] == "Reviewer"), "{rep}");
        assert_eq!(rep["comments"][0]["text"], "Too short?");
        // The final text, all changes accepted.
        let mut accepted = Session::new(s.doc.clone());
        accepted.run("review.acceptAll", &json!({})).unwrap();
        assert_eq!(accepted.doc.plain_text(StoryRef::Body), "The slow red fox.\nA new paragraph.\nThe end.");
        // Rejecting everything gives the original back.
        s.run("review.resolve", &json!({"accept": false, "all": true})).unwrap();
        assert_eq!(s.doc.plain_text(StoryRef::Body), "The quick brown fox.\nIt jumps over the lazy dog.\nThe end.");
    }

    #[test]
    fn one_operation_can_be_accepted_apart_from_the_others() {
        let mut s = session("Alpha beta gamma.\nDelta epsilon.");
        let r = s
            .run(
                "review.edit",
                &json!({"ops": [{"op": "replace", "anchor": "beta", "with": "BETA"}, {"op": "replace", "anchor": "epsilon", "with": "EPSILON"}]}),
            )
            .unwrap();
        let first: Vec<String> = r["changes"][0]["changeIds"].as_array().unwrap().iter().map(|x| x.as_str().unwrap().to_string()).collect();
        assert_eq!(first.len(), 2, "a deletion and an insertion: {r}");
        s.run("review.resolve", &json!({"accept": true, "ids": first})).unwrap();
        let rep = s.run("review.changeReport", &json!({})).unwrap();
        assert_eq!(rep["changes"].as_array().unwrap().len(), 2, "the second operation's changes are still there: {rep}");
        s.run("review.resolve", &json!({"accept": false, "author": "reviewer"})).unwrap();
        assert_eq!(s.doc.plain_text(StoryRef::Body), "Alpha BETA gamma.\nDelta epsilon.");
    }

    #[test]
    fn anchors_match_ignoring_case_typography_and_small_differences() {
        let mut s = session("He said \u{201C}wait\u{201D} \u{2014} then left the building quietly.");
        let f = s.run("review.find", &json!({"anchor": "HE SAID"})).unwrap();
        assert_eq!(f["matches"][0]["kind"], "case");
        let f = s.run("review.find", &json!({"anchor": "said \"wait\" - then"})).unwrap();
        assert_eq!(f["matches"][0]["kind"], "normalized", "{f}");
        let f = s.run("review.find", &json!({"anchor": "left the bilding quitely"})).unwrap();
        assert_eq!(f["matches"][0]["kind"], "fuzzy", "{f}");
        assert_eq!(f["matches"][0]["found"], "left the building quietly");
        // Not there at all: refused, nothing changed, with the closest phrases.
        let before = s.doc.plain_text(StoryRef::Body);
        let e = s
            .run(
                "review.edit",
                &json!({"ops": [{"op": "replace", "anchor": "then left", "with": "x"}, {"op": "replace", "anchor": "zebra crossing", "with": "y"}]}),
            )
            .unwrap_err();
        assert!(e.to_string().contains("nothing was changed"), "{e}");
        assert_eq!(s.doc.plain_text(StoryRef::Body), before, "atomic: the first operation was undone too");
        // keepGoing applies what it can.
        let r = s.run("review.edit", &json!({"keepGoing": true, "ops": [{"op": "replace", "anchor": "then left", "with": "x"}, {"op": "replace", "anchor": "zebra crossing", "with": "y"}]})).unwrap();
        assert_eq!((r["applied"].as_u64(), r["failed"].as_u64()), (Some(1), Some(1)), "{r}");
    }

    #[test]
    fn hostile_batches_are_errors_not_panics() {
        let mut s = session("Text.");
        for v in [
            json!({}),
            json!({"ops": "x"}),
            json!({"ops": [{}]}),
            json!({"ops": [{"op": "replace", "anchor": ""}]}),
            json!({"ops": [{"op": "frobnicate", "anchor": "Text"}]}),
            json!({"ops": [{"op": "replace", "anchor": "Text", "occurrence": 99, "with": "x"}]}),
        ] {
            assert!(s.run("review.edit", &v).is_err(), "{v}");
        }
        assert!(s.run("review.resolve", &json!({"accept": true})).is_err());
        assert_eq!(s.doc.plain_text(StoryRef::Body), "Text.");
    }
}
