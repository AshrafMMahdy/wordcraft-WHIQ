//! WHIQ: `file.printPdf` lays a long document out and writes it a page at a time. The PDF is the
//! one the whole layout gives (the same bytes), and the measurement below is the memory it takes.
#![cfg(not(target_arch = "wasm32"))]
#![allow(clippy::unwrap_used)]

use serde_json::json;
use wordcraft_engine::Session;
use wordcraft_engine::doc::{CharProps, Document, Paragraph, para_block};

fn long_doc(paras: usize) -> Document {
    let mut d = Document::new();
    d.core.title = "Long".into();
    d.core.created = "2024-05-06T07:08:09Z".into();
    d.body = (0..paras)
        .map(|i| {
            let style = if i % 90 == 0 { "Heading1" } else { "Normal" };
            para_block(
                Paragraph::with_text(
                    &format!("Clause {i}. The contractor shall provide all labour, materials and plant for the works described in this clause and keep them in good order."),
                    CharProps::default(),
                )
                .styled(style),
            )
        })
        .collect();
    d
}

fn peak_rss_mb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|t| t.lines().find(|l| l.starts_with("VmHWM:")).and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok()))
        .unwrap_or(0)
        >> 10
}

#[test]
fn a_printed_pdf_written_a_page_at_a_time_is_the_pdf_of_the_whole_layout() {
    let s = Session::new(long_doc(2500));
    let whole = s.export_layout();
    assert!(whole.pages.len() > 20, "{}", whole.pages.len());
    let classic = wordcraft_pdf::export_layout(
        &s.doc,
        &whole,
        &wordcraft_pdf::PdfOptions { pages: Some((0..whole.pages.len()).collect()), tagged: true, ..Default::default() },
    )
    .unwrap();
    let (streamed, plan) = s.print_pdf(&json!({})).unwrap();
    assert_eq!(plan.pages.len(), whole.pages.len(), "page count");
    assert!(streamed == classic, "streamed {} bytes, whole {} bytes", streamed.len(), classic.len());
    // A range, and pages that need drawing twice (two copies), give the same pages as ever.
    let (some, plan) = s.print_pdf(&json!({"range": "custom", "pages": "3-5, 9"})).unwrap();
    assert_eq!(plan.pages, vec![2, 3, 4, 8]);
    assert!(some.len() < streamed.len());
    let (twice, plan) = s.print_pdf(&json!({"range": "custom", "pages": "2-3", "copies": 2})).unwrap();
    assert_eq!(plan.pages, vec![1, 2, 1, 2]);
    assert!(twice.starts_with(b"%PDF"));
    // A job with a page past the end is still refused with the page.
    let err = s.print_pdf(&json!({"range": "custom", "pages": "999999"})).unwrap_err().to_string();
    assert!(err.contains("999999") || err.contains("page"), "{err}");
}

/// Measurement, not a check: `GC_PDF=<mode>:<paragraphs>` with mode `whole` (the layout of the
/// whole document, as before) or `stream`; prints the peak resident memory of the export. Run each
/// in a process of its own:
/// `cargo test -p wordcraft-engine --test pdf_stream measure -- --ignored --nocapture`.
#[test]
#[ignore]
fn measure() {
    let Ok(mode) = std::env::var("GC_PDF") else { return };
    let (how, n) = mode.split_once(':').unwrap();
    let doc = long_doc(n.parse().unwrap());
    let mut s = Session::new(doc);
    let base = peak_rss_mb();
    let out = std::env::temp_dir().join(format!("wc-measure-{}.pdf", std::process::id()));
    let (pages, bytes) = if how == "whole" {
        let lay = s.export_layout();
        let pdf = wordcraft_pdf::export_layout(&s.doc, &lay, &wordcraft_pdf::PdfOptions::default()).unwrap();
        (lay.pages.len(), pdf.len())
    } else {
        let r = s.run("file.printPdf", &json!({"path": out.to_str().unwrap()})).unwrap();
        (r["pages"].as_u64().unwrap() as usize, r["bytes"].as_u64().unwrap() as usize)
    };
    let _ = std::fs::remove_file(&out);
    eprintln!("MEASURED {how}: {pages} pages, {} MB PDF, peak RSS {} MB (document alone {base} MB)", bytes >> 20, peak_rss_mb());
}
