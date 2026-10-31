//! WHIQ: the document drawn for the platform's Preview pane (`document.preview`, one
//! self-contained HTML page) and read by its agents (`document.markdown`), from the document
//! model, in parts for any size (see `wordcraft_formats::whiq_preview`). A part is a run of
//! top-level blocks holding about `partSize` characters of text; `pages` and `section` name a
//! window of the document instead.

use serde_json::{Value, json};
use wordcraft_formats::whiq_preview::{self as wp, Rendered, Window};

use crate::{CmdError, CmdResult, CommandSpec, Session, p};

pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec::new("document.preview", "Preview Page", "Agents", preview)
            .params(
                r#"{"part"?: 1-based (default 1), "partSize"?: characters of text a part holds (default 200000), "pages"?: "3-5" (the blocks that start on those printed pages, instead of a part), "section"?: 1-based section, "markup"?: bool (tracked changes and comments shown, default true), "pictures"?: bool (default true), "pictureBudget"?: bytes of pictures a part embeds (default 12 MB), "title"?: name shown} → {html, part, parts, nextPart, blocks: {from, to, total}, rows?, stats}"#,
            )
            .pure(),
        CommandSpec::new("document.markdown", "Document as Markdown", "Agents", markdown)
            .params(
                r#"{"part"?: 1-based, "partSize"?: characters of text a part holds (default 40000; no upper limit), "pages"?, "section"?, "markup"?: bool (tracked changes and comments in CriticMarkup, default true), "path"?: file to write (the whole document unless part, pages or section is given)} → {markdown | path, part, parts, nextPart, blocks, stats}"#,
            )
            .pure(),
    ]
}

/// The name the page shows: the file's, else the document's, else "Document".
fn title(s: &Session, v: &Value) -> String {
    if let Some(t) = p::str(v, "title").map(str::trim).filter(|t| !t.is_empty()) {
        return t.chars().take(200).collect();
    }
    if let Some(n) = s.path.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()) {
        return n;
    }
    s.file_name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "Document".into())
}

/// The window `v` names: a page range, a section, or a part (default 1). `Ok(None)` when it names
/// none of them.
fn window(s: &Session, v: &Value) -> Result<Option<(Window, Value)>, CmdError> {
    if let Some(spec) = p::str(v, "pages").map(str::trim).filter(|x| !x.is_empty()) {
        let total = s.page_count(false);
        let pages = crate::print::parse_page_range(spec, total)
            .map_err(|e| CmdError::Params(format!("pages \"{spec}\": {e} (the document has {total} pages)")))?;
        let (Some(&a), Some(&b)) = (pages.iter().min(), pages.iter().max()) else {
            return Err(CmdError::Params(format!("pages \"{spec}\" names no page (the document has {total} pages)")));
        };
        // Lay out up to the page after the window, holding only those pages.
        let lay = s.export_layout_window(false, wordcraft_layout::PageWindow { first: a, last: (b + 1).min(total), stop: true });
        let from = lay.pages.get(a).map_or(0, |pg| pg.first_block);
        let to = lay.pages.get(b + 1).map_or(s.doc.body.len(), |pg| pg.first_block).max(from + 1);
        return Ok(Some((Window::Blocks { from, to }, json!({"pages": {"from": a + 1, "to": b + 1, "total": total}}))));
    }
    if let Some(n) = p::u64(v, "section") {
        let secs = s.doc.sections();
        let k = (n.max(1) - 1) as usize;
        let Some((end, _)) = secs.get(k) else {
            return Err(CmdError::Params(format!("section {n} is out of range: the document has {} sections", secs.len())));
        };
        let from = if k == 0 { 0 } else { secs.get(k - 1).map_or(0, |(e, _)| e + 1) };
        return Ok(Some((Window::Blocks { from, to: end + 1 }, json!({"section": {"index": k + 1, "total": secs.len()}}))));
    }
    Ok(p::u64(v, "part").map(|n| (Window::Part(n.clamp(1, u32::MAX as u64) as usize), json!({}))))
}

fn options(s: &Session, v: &Value, default_chars: usize) -> wp::Options {
    wp::Options {
        title: title(s, v),
        part_chars: p::u64(v, "partSize").map_or(default_chars, |n| (n.min(usize::MAX as u64) as usize).max(wp::MIN_PART_CHARS)),
        picture_budget: p::u64(v, "pictureBudget").map_or(wp::PICTURE_BUDGET, |n| n.clamp(1, 1 << 30)),
        markup: p::bool(v, "markup").unwrap_or(true),
        pictures: p::bool(v, "pictures").unwrap_or(true),
    }
}

/// What every answer says about the part.
fn summary(r: &Rendered, o: &wp::Options, extra: Value) -> Value {
    let st = &r.stats;
    let mut out = json!({
        "part": r.part,
        "parts": r.parts,
        "partSize": o.part_chars,
        "blocks": {"from": r.piece.from, "to": r.piece.to, "total": r.total_blocks},
        "stats": {
            "paragraphs": st.paragraphs, "headings": st.headings, "tables": st.tables,
            "pictures": st.pictures, "picturesOmitted": st.pictures_omitted, "pictureBytes": st.picture_bytes,
            "insertions": st.insertions, "deletions": st.deletions, "formatChanges": st.format_changes,
            "comments": st.comments, "footnotes": st.footnotes, "endnotes": st.endnotes, "characters": st.characters,
        },
        "markup": o.markup,
    });
    if let Some((a, b)) = r.piece.rows {
        out["rows"] = json!({"from": a, "to": b});
    }
    if let Some(n) = r.part {
        out["nextPart"] = if n < r.parts { json!(n + 1) } else { Value::Null };
    }
    if let (Some(m), Some(e)) = (out.as_object_mut(), extra.as_object()) {
        for (k, x) in e {
            m.insert(k.clone(), x.clone());
        }
    }
    if !r.warnings.is_empty() {
        out["warnings"] = json!(r.warnings);
    }
    out
}

fn preview(s: &mut Session, v: &Value) -> CmdResult {
    let o = options(s, v, wp::PART_CHARS);
    let (w, extra) = window(s, v)?.unwrap_or((Window::Part(1), json!({})));
    let r = wp::render_html(&s.doc, &o, w).map_err(CmdError::Params)?;
    let mut out = summary(&r, &o, extra);
    out["bytes"] = json!(r.text.len());
    out["html"] = json!(r.text);
    Ok(out)
}

fn markdown(s: &mut Session, v: &Value) -> CmdResult {
    let path = p::str(v, "path").map(str::trim).filter(|x| !x.is_empty());
    let mut o = options(s, v, wp::MARKDOWN_PART_CHARS);
    let named = window(s, v)?;
    // A file of the whole document unless a part or window is named.
    let whole = path.is_some() && named.is_none() && p::u64(v, "partSize").is_none();
    if whole {
        o.part_chars = usize::MAX;
    }
    let (w, extra) = named.unwrap_or((Window::Part(1), json!({})));
    let r = wp::render_markdown(&s.doc, &o, w).map_err(CmdError::Params)?;
    let mut out = summary(&r, &o, extra);
    if whole {
        out["partSize"] = Value::Null;
    }
    out["characters"] = json!(r.text.chars().count());
    match path {
        Some(path) => {
            crate::io::write_file(path, r.text.as_bytes()).map_err(CmdError::Failed)?;
            out["path"] = json!(path);
            out["bytes"] = json!(r.text.len());
        }
        None => out["markdown"] = json!(r.text),
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wordcraft_doc::Document;

    use crate::Session;

    fn session(paragraphs: usize) -> Session {
        let mut s = Session::new(Document::new());
        let text: Vec<String> = (0..paragraphs).map(|i| format!("Paragraph {i} {}", "lorem ipsum ".repeat(12))).collect();
        s.run("document.setText", &json!({"text": text.join("\n")})).unwrap();
        s
    }

    #[test]
    fn the_preview_answers_with_the_page_and_its_parts() {
        let mut s = session(300);
        let before = s.doc.clone();
        let r = s.run("document.preview", &json!({"partSize": 10000})).unwrap();
        let parts = r["parts"].as_u64().unwrap();
        assert!(parts > 2, "{r}");
        assert_eq!((r["part"].clone(), r["nextPart"].clone()), (json!(1), json!(2)));
        assert!(r["html"].as_str().unwrap().contains("<div class=\"page\""));
        assert_eq!(r["blocks"]["from"], 0);
        let last = s.run("document.preview", &json!({"partSize": 10000, "part": parts})).unwrap();
        assert_eq!(last["nextPart"], json!(null));
        assert_eq!(last["blocks"]["to"], 300);
        assert!(s.run("document.preview", &json!({"partSize": 10000, "part": parts + 1})).unwrap_err().to_string().contains("out of range"));
        assert!(s.doc == before, "a preview changes nothing");
    }

    #[test]
    fn pages_and_sections_name_windows() {
        let mut s = session(400);
        let total = s.page_count(false);
        assert!(total > 4, "{total}");
        let r = s.run("document.preview", &json!({"pages": "2-3"})).unwrap();
        let (from, to) = (r["blocks"]["from"].as_u64().unwrap(), r["blocks"]["to"].as_u64().unwrap());
        assert!(from > 0 && to > from && to < 400, "{}", r["blocks"]);
        assert_eq!(r["pages"], json!({"from": 2, "to": 3, "total": total}));
        assert!(r["html"].as_str().unwrap().contains(&format!("Paragraph {from} ")));
        let m = s.run("document.markdown", &json!({"pages": "2"})).unwrap();
        assert!(m["markdown"].as_str().unwrap().starts_with(&format!("Paragraph {from} ")), "{}", m["markdown"]);
        let one = s.run("document.markdown", &json!({"section": 1})).unwrap();
        assert_eq!(one["blocks"], json!({"from": 0, "to": 400, "total": 400}));
        assert!(s.run("document.preview", &json!({"section": 2})).is_err());
        assert!(s.run("document.preview", &json!({"pages": "900"})).is_err());
    }

    #[test]
    fn markdown_goes_to_a_file_whole_or_in_parts_of_any_size() {
        let mut s = session(500);
        let dir = std::env::temp_dir().join(format!("wc-md-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("all.md");
        let r = s.run("document.markdown", &json!({"path": path.to_string_lossy()})).unwrap();
        assert_eq!((r["parts"].clone(), r.get("markdown").is_none()), (json!(1), true), "{r}");
        let all = std::fs::read_to_string(&path).unwrap();
        assert!(all.starts_with("Paragraph 0 ") && all.trim_end().ends_with(&format!("Paragraph 499 {}", "lorem ipsum ".repeat(12).trim_end())));
        // A part size larger than the document: one part, nothing capped.
        let r = s.run("document.markdown", &json!({"partSize": 50_000_000u64})).unwrap();
        assert_eq!(r["parts"], 1);
        assert_eq!(r["markdown"].as_str().unwrap().trim_end(), all.trim_end());
        // Parts put together are the whole document.
        let mut joined = String::new();
        let mut part = 1;
        loop {
            let r = s.run("document.markdown", &json!({"partSize": 3000, "part": part})).unwrap();
            joined.push_str(r["markdown"].as_str().unwrap());
            joined.push_str("\n\n");
            match r["nextPart"].as_u64() {
                Some(n) => part = n,
                None => break,
            }
        }
        assert!(part > 10);
        assert_eq!(joined.trim_end(), all.trim_end());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
