//! WHIQ: a streamed layout hands over the pages of the whole layout, one at a time.

use super::*;
use wordcraft_doc::para::InlineObject;
use wordcraft_doc::section::SectionStart;
use wordcraft_doc::{PartKind, para_block};

fn print() -> LayoutOptions {
    LayoutOptions { view: ViewMode::Print, ..Default::default() }
}

fn field(instr: &str) -> InlineObject {
    InlineObject::Field { instr: instr.into(), result: "1".into(), locked: false, dirty: false, code: false, form: String::new() }
}

/// "Page {PAGE} of {NUMPAGES}" (or just the text) as a header or footer part.
fn chrome(d: &mut Document, kind: PartKind, text: &str, totals: bool) -> u32 {
    let mut p = Paragraph::with_text(text, Default::default());
    let end = p.text.len();
    p.insert_object(end, field("PAGE"), &Default::default()).unwrap();
    if totals {
        let end = p.text.len();
        p.insert_object(end, field("NUMPAGES"), &Default::default()).unwrap();
    }
    d.add_part(kind, vec![para_block(p)])
}

fn text_doc(paras: usize) -> Document {
    let text: Vec<String> = (0..paras)
        .map(|i| {
            format!("Clause {i}. The contractor shall provide all labour, materials and plant for the works described in this clause, and shall keep them in good order.")
        })
        .collect();
    Document::from_text(&text.join("\n"))
}

/// The pages of `d` laid out whole and streamed; every page the same, and nothing kept by the stream.
fn same(d: &Document, what: &str) {
    let full = layout(d, &mut LayoutCache::new(), &print());
    let mut got: Vec<Page> = Vec::new();
    let n = layout_streaming(d, &mut LayoutCache::new(), &print(), &mut |i, p| {
        assert_eq!(i, got.len(), "{what}: pages come in order");
        got.push(p);
        true
    });
    assert_eq!(n, Some(full.pages.len()), "{what}: page count");
    assert!(full.pages.len() > 1, "{what}: more than one page");
    for (i, (a, b)) in got.iter().zip(&full.pages).enumerate() {
        assert_eq!(format!("{a:?}"), format!("{b:?}"), "{what}: page {}", i + 1);
    }
}

#[test]
fn a_plain_document_streams_as_it_is_laid_out() {
    same(&text_doc(400), "plain");
}

#[test]
fn headers_footers_page_totals_and_mirrored_margins_are_the_same() {
    let mut d = text_doc(400);
    d.last_section.headers.default = Some(chrome(&mut d, PartKind::Header, "Handbook ", false));
    d.last_section.footers.default = Some(chrome(&mut d, PartKind::Footer, "Page ", true));
    d.last_section.margin_left = 108.0;
    d.last_section.margin_right = 36.0;
    d.settings.mirror_margins = true;
    assert!(shows_page_totals(&d));
    same(&d, "chrome");
}

#[test]
fn a_title_page_and_odd_even_headers_are_the_same() {
    let mut d = text_doc(300);
    let first = chrome(&mut d, PartKind::Header, "First ", false);
    let odd = chrome(&mut d, PartKind::Header, "Odd ", false);
    let even = chrome(&mut d, PartKind::Header, "Even ", false);
    d.last_section.headers.first = Some(first);
    d.last_section.headers.default = Some(odd);
    d.last_section.headers.even = Some(even);
    d.last_section.title_page = true;
    d.settings.even_odd_headers = true;
    same(&d, "title page, odd and even");
}

#[test]
fn sections_that_start_on_odd_pages_and_turn_pages_are_the_same() {
    let mut d = text_doc(450);
    let head = chrome(&mut d, PartKind::Header, "Part ", false);
    for (k, start) in [(120usize, SectionStart::NextPage), (260, SectionStart::OddPage)] {
        let mut s = d.last_section.clone();
        s.headers.default = Some(head);
        s.start = start;
        if let Some(Block::Para(p)) = d.body.get_mut(k).map(Arc::make_mut) {
            p.section = Some(Box::new(s));
        }
    }
    d.last_section.start = SectionStart::OddPage;
    d.last_section.headers.default = Some(head);
    same(&d, "sections");
}

#[test]
fn the_sink_can_stop_the_layout() {
    let d = text_doc(2000);
    let all = layout(&d, &mut LayoutCache::new(), &print()).pages.len();
    let mut seen = 0;
    let n = layout_streaming(&d, &mut LayoutCache::new(), &print(), &mut |_, _| {
        seen += 1;
        seen < 5
    });
    assert_eq!((n, seen), (Some(5), 5));
    assert!(all > 20);
}

#[test]
fn what_cannot_stream_says_so() {
    let d = text_doc(10);
    let balloons = LayoutOptions { comment_balloons: true, ..print() };
    assert!(layout_streaming(&d, &mut LayoutCache::new(), &balloons, &mut |_, _| true).is_none());
    let window = LayoutOptions { window: Some(PageWindow::COUNT), ..print() };
    assert!(layout_streaming(&d, &mut LayoutCache::new(), &window, &mut |_, _| true).is_none());
    let web = LayoutOptions { view: ViewMode::Web, ..Default::default() };
    assert!(layout_streaming(&d, &mut LayoutCache::new(), &web, &mut |_, _| true).is_none());
}

#[test]
fn footnotes_endnotes_tables_and_balanced_columns_are_the_same() {
    use wordcraft_doc::Pos;
    use wordcraft_doc::para::NoteKind;
    let mut d = text_doc(300);
    // A footnote every few paragraphs, a long one that carries over a page, an endnote.
    for k in (3..280).step_by(7) {
        let text = if k == 59 { "A long note. ".repeat(260) } else { format!("Note {k}.") };
        let note = Paragraph::with_text(&text, Default::default()).styled("FootnoteText");
        let id = d.add_part(PartKind::Footnote, vec![para_block(note)]);
        d.insert_object(&Pos::body(k, 4), InlineObject::NoteRef { kind: NoteKind::Footnote, id, custom: String::new() }, &Default::default())
            .unwrap();
    }
    let eid = d.add_part(PartKind::Endnote, vec![para_block(Paragraph::with_text("An endnote.", Default::default()))]);
    d.insert_object(&Pos::body(10, 2), InlineObject::NoteRef { kind: NoteKind::Endnote, id: eid, custom: String::new() }, &Default::default())
        .unwrap();
    // A table in the middle.
    let mut t = wordcraft_doc::Table::new(30, 3, 468.0);
    for (r, row) in t.rows.iter_mut().enumerate() {
        for (c, cell) in row.cells.iter_mut().enumerate() {
            cell.blocks = vec![para_block(Paragraph::with_text(&format!("cell {r}.{c} with some words in it"), Default::default()))];
        }
    }
    d.body.insert(150, Arc::new(Block::Table(t)));
    same(&d, "notes and a table");

    // A two-column section ending in a continuous break (columns balanced on its last page).
    let mut d = text_doc(200);
    let mut cols = d.last_section.clone();
    cols.columns.count = 2;
    cols.columns.space = 36.0;
    if let Some(Block::Para(p)) = d.body.get_mut(120).map(Arc::make_mut) {
        p.section = Some(Box::new(cols));
    }
    d.last_section.start = SectionStart::Continuous;
    same(&d, "balanced columns");
}
