//! Range fields: a [`InlineObject::FieldStart`] … [`InlineObject::FieldEnd`] pair whose result is
//! the ordinary content between the two markers, possibly spanning paragraphs (a Zotero
//! bibliography). Pairs nest like brackets and never cross a block list: a pair opened in a
//! table cell closes in that cell.
//!
//! Editing can leave a marker without its partner (deleting a selection that holds only one
//! end). Such orphans are harmless in the model and in layout; writers call
//! [`Document::balance_field_ranges`] so a saved file always has whole fields.

use crate::para::OBJ;
use crate::{Block, Blocks, Document, InlineObject, Path, Pos, StoryRef};

/// Deepest table nesting scanned (matches `walk` in the crate root).
const MAX_DEPTH: usize = 16;

/// A whole range field.
#[derive(Clone, Debug, PartialEq)]
pub struct FieldRange {
    pub instr: String,
    pub locked: bool,
    /// Where the `FieldStart` marker is.
    pub start: Pos,
    /// Where the `FieldEnd` marker is (the result is everything between the two).
    pub end: Pos,
    /// Number of enclosing range fields.
    pub depth: usize,
}

#[derive(Default)]
struct Scan {
    ranges: Vec<FieldRange>,
    orphans: Vec<(Path, usize)>,
}

fn scan(story: StoryRef, bl: &Blocks, prefix: &mut Vec<u32>, depth: usize, out: &mut Scan) {
    if depth > MAX_DEPTH {
        return;
    }
    // Open starts: (index into out.ranges once closed is unknown, so keep the data here).
    let mut open: Vec<(String, bool, Pos)> = Vec::new();
    for (i, b) in bl.iter().enumerate() {
        prefix.push(u32::try_from(i).unwrap_or(u32::MAX));
        match &**b {
            Block::Para(p) => {
                let path = Path(prefix.clone());
                for off in p.object_offsets() {
                    match p.object_at(off) {
                        Some(InlineObject::FieldStart { instr, locked, .. }) => {
                            open.push((instr.clone(), *locked, Pos::new(story, path.clone(), off)));
                        }
                        Some(InlineObject::FieldEnd) => match open.pop() {
                            Some((instr, locked, start)) => {
                                let depth = open.len();
                                out.ranges.push(FieldRange { instr, locked, start, end: Pos::new(story, path.clone(), off), depth });
                            }
                            None => out.orphans.push((path.clone(), off)),
                        },
                        _ => {}
                    }
                }
            }
            Block::Table(t) => {
                for (r, row) in t.rows.iter().enumerate() {
                    for (c, cell) in row.cells.iter().enumerate() {
                        prefix.push(u32::try_from(r).unwrap_or(u32::MAX));
                        prefix.push(u32::try_from(c).unwrap_or(u32::MAX));
                        scan(story, &cell.blocks, prefix, depth + 1, out);
                        prefix.pop();
                        prefix.pop();
                    }
                }
            }
        }
        prefix.pop();
    }
    out.orphans.extend(open.into_iter().map(|(_, _, p)| (p.path, p.off)));
}

fn scan_story(story: StoryRef, bl: &Blocks) -> Scan {
    let mut out = Scan::default();
    scan(story, bl, &mut Vec::new(), 0, &mut out);
    out.ranges.sort_by(|a, b| a.start.path.cmp(&b.start.path).then(a.start.off.cmp(&b.start.off)));
    out
}

impl Document {
    fn story_refs(&self) -> Vec<StoryRef> {
        std::iter::once(StoryRef::Body).chain(self.parts.keys().map(|k| StoryRef::Part(*k))).collect()
    }

    /// The whole range fields of a story, in document order of their starts.
    pub fn field_ranges(&self, story: StoryRef) -> Vec<FieldRange> {
        self.story(story).map(|bl| scan_story(story, bl).ranges).unwrap_or_default()
    }

    /// `p` moved past the range-field ends right after it. A click just after a field's result
    /// lands before its end marker (the marker takes no room); typing or inserting there belongs
    /// after the field, as in Word, not in its result.
    pub fn past_field_ends(&self, p: &Pos) -> Pos {
        let Some(para) = self.para_at(p) else { return p.clone() };
        let mut off = p.off;
        while matches!(para.object_at(off), Some(InlineObject::FieldEnd)) {
            off += OBJ.len_utf8();
        }
        Pos { off, ..p.clone() }
    }

    /// Whether any story holds a range-field marker without its partner.
    pub fn has_unbalanced_field_ranges(&self) -> bool {
        self.story_refs().into_iter().any(|s| self.story(s).is_some_and(|bl| !scan_story(s, bl).orphans.is_empty()))
    }

    /// Remove range-field markers that have no partner. Returns how many were removed.
    pub fn balance_field_ranges(&mut self) -> usize {
        let mut removed = 0;
        for s in self.story_refs() {
            let Some(bl) = self.story(s) else { continue };
            let mut orphans = scan_story(s, bl).orphans;
            // Last first, so earlier offsets in the same paragraph stay valid.
            orphans.sort();
            for (path, off) in orphans.into_iter().rev() {
                if let Ok(p) = self.para_mut(s, &path)
                    && p.delete(off, off + OBJ.len_utf8()).is_ok()
                {
                    removed += 1;
                }
            }
        }
        removed
    }
}

// ---- WHIQ: fields nested in another field's code ----

/// WHIQ (corpus gate W1): a field nested in another field's code (ECMA-376 §17.16.1:
/// `{ IF { PAGE } = { NUMPAGES } "last page" "" }`) is kept inside the outer field's `instr`
/// between these markers — Unicode noncharacters, which are reserved for a program's internal use
/// and never occur in a file's text: [`NEST_BEGIN`], then [`NEST_LOCKED`] and [`NEST_DIRTY`] when
/// the nested field's begin carries those flags, its own code (nested fields again in it), [`NEST_SEP`],
/// its cached result, [`NEST_END`]. Writers write it back as a field ([`instr_parts`]); the code
/// view shows it in braces as Word does ([`instr_code`]).
pub const NEST_BEGIN: char = '\u{FDD0}';
/// Between a nested field's code and its cached result.
pub const NEST_SEP: char = '\u{FDD1}';
/// After a nested field's cached result.
pub const NEST_END: char = '\u{FDD2}';
/// Right after [`NEST_BEGIN`]: the nested field is locked (`w:fldLock`).
pub const NEST_LOCKED: char = '\u{FDD3}';
/// Right after [`NEST_BEGIN`] (and [`NEST_LOCKED`]): the nested field's result is stale (`w:dirty`).
pub const NEST_DIRTY: char = '\u{FDD4}';

/// Deepest nesting of fields in fields read or written (Word's own limit is 20).
pub const MAX_NEST: usize = 20;

fn is_nest_marker(c: char) -> bool {
    matches!(c, NEST_BEGIN | NEST_SEP | NEST_END | NEST_LOCKED | NEST_DIRTY)
}

/// `text` without nesting markers (text that can't hold them: a nested field's result, or code
/// from outside).
pub fn strip_nest_markers(text: &str) -> String {
    text.chars().filter(|c| !is_nest_marker(*c)).collect()
}

/// A nested field as it goes into the outer field's `instr` (see [`NEST_BEGIN`]).
pub fn nest(instr: &str, result: &str, locked: bool, dirty: bool) -> String {
    let mut s = String::with_capacity(instr.len() + result.len() + 12);
    s.push(NEST_BEGIN);
    if locked {
        s.push(NEST_LOCKED);
    }
    if dirty {
        s.push(NEST_DIRTY);
    }
    s.push_str(instr);
    s.push(NEST_SEP);
    s.push_str(&strip_nest_markers(result));
    s.push(NEST_END);
    s
}

/// Does this field code hold a nested field?
pub fn has_nested(instr: &str) -> bool {
    instr.contains(NEST_BEGIN)
}

/// A piece of a field's code: text, or a nested field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstrPart<'a> {
    Text(&'a str),
    /// A nested field: its code (which may hold nested fields of its own) and cached result.
    Field {
        instr: &'a str,
        result: &'a str,
        locked: bool,
        dirty: bool,
    },
}

/// A field's code split into text and the fields nested in it (one level; a nested field's
/// `instr` splits the same way). Markers out of place (an unclosed field, a stray separator) are
/// left in the text pieces, which [`strip_nest_markers`] cleans for writing.
pub fn instr_parts(instr: &str) -> Vec<InstrPart<'_>> {
    let mut parts = Vec::new();
    let mut text_from = 0;
    let mut i = 0;
    while let Some(rel) = instr.get(i..).and_then(|r| r.find(NEST_BEGIN)) {
        let start = i + rel;
        let Some((end, part)) = nested_at(instr, start) else {
            i = start + NEST_BEGIN.len_utf8();
            continue;
        };
        if start > text_from {
            parts.push(InstrPart::Text(instr.get(text_from..start).unwrap_or("")));
        }
        parts.push(part);
        text_from = end;
        i = end;
    }
    if text_from < instr.len() {
        parts.push(InstrPart::Text(instr.get(text_from..).unwrap_or("")));
    }
    parts
}

/// The nested field whose [`NEST_BEGIN`] is at byte `start`: where it ends, and the field.
fn nested_at(instr: &str, start: usize) -> Option<(usize, InstrPart<'_>)> {
    let body = start + NEST_BEGIN.len_utf8();
    let mut code_from = body;
    let (mut locked, mut dirty) = (false, false);
    for c in instr.get(body..)?.chars() {
        match c {
            NEST_LOCKED if !locked && !dirty => locked = true,
            NEST_DIRTY if !dirty => dirty = true,
            _ => break,
        }
        code_from += c.len_utf8();
    }
    // The separator at this level: nested fields' own markers are skipped by depth.
    let mut depth = 0usize;
    let mut sep = None;
    for (k, c) in instr.get(code_from..)?.char_indices() {
        match c {
            NEST_BEGIN => depth += 1,
            NEST_END if depth > 0 => depth -= 1,
            NEST_END => return None,
            NEST_SEP if depth == 0 => {
                sep = Some(code_from + k);
                break;
            }
            _ => {}
        }
    }
    let sep = sep?;
    let res_from = sep + NEST_SEP.len_utf8();
    let rel_end = instr.get(res_from..)?.find(|c: char| c == NEST_END || c == NEST_BEGIN || c == NEST_SEP)?;
    let res_end = res_from + rel_end;
    if !instr.get(res_end..)?.starts_with(NEST_END) {
        return None;
    }
    let part = InstrPart::Field { instr: instr.get(code_from..sep)?, result: instr.get(res_from..res_end)?, locked, dirty };
    Some((res_end + NEST_END.len_utf8(), part))
}

/// A field's code as Word shows it: nested fields in braces, `IF { PAGE } = { NUMPAGES } "…" ""`.
pub fn instr_code(instr: &str) -> String {
    fn go(instr: &str, out: &mut String, depth: usize) {
        for part in instr_parts(instr) {
            match part {
                InstrPart::Text(t) => out.push_str(&strip_nest_markers(t)),
                InstrPart::Field { instr, .. } if depth < MAX_NEST => {
                    out.push_str("{ ");
                    go(instr.trim(), out, depth + 1);
                    out.push_str(" }");
                }
                InstrPart::Field { instr, .. } => out.push_str(&strip_nest_markers(instr)),
            }
        }
    }
    if !has_nested(instr) {
        return instr.to_string();
    }
    let mut out = String::with_capacity(instr.len());
    go(instr, &mut out, 0);
    out
}

/// A field's code with each nested field replaced by its cached result, `IF 1 = 1 "…" ""`: what
/// a program that evaluates the outer field reads.
pub fn instr_flat(instr: &str) -> String {
    if !has_nested(instr) {
        return instr.to_string();
    }
    instr_parts(instr)
        .into_iter()
        .map(|part| match part {
            InstrPart::Text(t) => strip_nest_markers(t),
            InstrPart::Field { result, .. } => result.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod nest_tests {
    use super::*;

    #[test]
    fn nested_fields_split_show_and_flatten() {
        let page = nest(" PAGE ", "3", false, false);
        let total = nest(" NUMPAGES ", "9", true, true);
        let instr = format!("IF {page} = {total} \"last\" \"\"");
        assert!(has_nested(&instr));
        let parts = instr_parts(&instr);
        assert_eq!(parts.len(), 5, "{parts:?}");
        assert_eq!(parts[1], InstrPart::Field { instr: " PAGE ", result: "3", locked: false, dirty: false });
        assert_eq!(parts[3], InstrPart::Field { instr: " NUMPAGES ", result: "9", locked: true, dirty: true });
        assert_eq!(instr_code(&instr), "IF { PAGE } = { NUMPAGES } \"last\" \"\"");
        assert_eq!(instr_flat(&instr), "IF 3 = 9 \"last\" \"\"");
        // Two deep: the outer split sees one field, whose code splits again.
        let deep = format!("QUOTE {} \\* Upper", nest(&format!(" IF {page} > 0 \"y\" "), "y", false, false));
        let parts = instr_parts(&deep);
        assert_eq!(parts.len(), 3, "{parts:?}");
        let InstrPart::Field { instr: inner, .. } = parts[1] else { panic!("{parts:?}") };
        assert_eq!(instr_parts(inner).len(), 3);
        assert_eq!(instr_code(&deep), "QUOTE { IF { PAGE } > 0 \"y\" } \\* Upper");
        assert_eq!(instr_code("PAGE \\* roman"), "PAGE \\* roman");
    }

    #[test]
    fn stray_markers_are_text_and_never_panic() {
        for hostile in
            ["\u{FDD0}", "\u{FDD0}PAGE", "a\u{FDD1}b\u{FDD2}", "\u{FDD0}\u{FDD3}\u{FDD4}\u{FDD1}\u{FDD2}\u{FDD2}", "\u{FDD0}\u{FDD0}x\u{FDD1}"]
        {
            let parts = instr_parts(hostile);
            let _ = (instr_code(hostile), instr_flat(hostile));
            let joined: usize = parts
                .iter()
                .map(|p| match p {
                    InstrPart::Text(t) => t.len(),
                    InstrPart::Field { instr, result, .. } => instr.len() + result.len(),
                })
                .sum();
            assert!(joined <= hostile.len(), "{hostile:?}");
        }
        assert!(instr_parts("\u{FDD0}PAGE").iter().all(|p| matches!(p, InstrPart::Text(_))));
    }
}
