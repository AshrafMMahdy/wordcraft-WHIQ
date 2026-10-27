//! WHIQ: page windows lay a long document out holding a few pages at a time.

use super::*;

fn long_doc(paras: usize) -> Document {
    let text: Vec<String> =
        (0..paras).map(|i| format!("Clause {i}. The contractor shall provide all labour, materials and plant for the works.")).collect();
    Document::from_text(&text.join("\n"))
}

fn print(window: Option<PageWindow>) -> LayoutOptions {
    LayoutOptions { view: ViewMode::Print, window, ..Default::default() }
}

fn lines_on(p: &Page) -> usize {
    p.items.iter().map(|it| if let Placed::Lines { l0, l1, .. } = it { l1 - l0 } else { 0 }).sum()
}

#[test]
fn counting_keeps_no_page_and_finds_every_page() {
    let d = long_doc(3000);
    let full = layout(&d, &mut LayoutCache::new(), &print(None));
    assert!(full.pages.len() > 40, "{}", full.pages.len());
    let counted = layout(&d, &mut LayoutCache::new(), &print(Some(PageWindow::COUNT)));
    assert_eq!(counted.pages.len(), full.pages.len());
    assert!(counted.pages.iter().all(|p| p.items.is_empty() && p.header.is_empty()), "no page keeps its lines");
    assert_eq!(counted.pages.iter().map(|p| p.number).collect::<Vec<_>>(), full.pages.iter().map(|p| p.number).collect::<Vec<_>>());
}

#[test]
fn one_page_is_laid_out_as_in_the_whole_document_and_the_rest_is_not_kept() {
    let d = long_doc(3000);
    let full = layout(&d, &mut LayoutCache::new(), &print(None));
    let n = full.pages.len() / 2;
    let w = PageWindow { first: n, last: n, stop: true };
    let one = layout(&d, &mut LayoutCache::new(), &print(Some(w)));
    assert!(one.pages.len() <= n + 3, "pagination stopped after the page: {} of {}", one.pages.len(), full.pages.len());
    let (a, b) = (&one.pages[n], &full.pages[n]);
    assert_eq!((a.number, a.first_block, lines_on(a)), (b.number, b.first_block, lines_on(b)));
    assert!(lines_on(a) > 0);
    assert!(one.pages.iter().enumerate().all(|(i, p)| i == n || p.items.is_empty()), "only the window keeps its lines");
}

#[test]
fn a_document_showing_page_totals_is_known_to() {
    let d = long_doc(5);
    assert!(!shows_page_totals(&d));
}
