//! WHIQ: open + save of every corpus document (`corpus/mod.rs`) keeps every part and
//! relationship and drops nothing from the stories (`wordcraft_docx::roundtrip`).
// Integration tests: unwrapping and panicking on failure is fine here (AGENTS.md).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod corpus;

use wordcraft_docx::roundtrip;

/// Open and save as the file's own kind (a `.docm` stays macro-enabled, as the app saves it).
fn round_trip(name: &str, bytes: &[u8]) -> Vec<u8> {
    let doc = wordcraft_docx::read(bytes).expect("read");
    let flavor = wordcraft_docx::Flavor::from_ext(name.rsplit('.').next().unwrap_or("docx")).unwrap_or_default();
    wordcraft_docx::write_as(&doc, flavor).expect("write")
}

/// The corpus is written to `target/whiq-corpus` (or `$WHIQ_CORPUS_DIR`) with each saved copy
/// beside it, so a person can open both in Word.
fn keep(name: &str, before: &[u8], after: &[u8]) {
    let dir = std::env::var_os("WHIQ_CORPUS_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/whiq-corpus"));
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = std::fs::write(dir.join(name), before);
        let _ = std::fs::write(dir.join(format!("saved-{name}")), after);
    }
}

#[test]
fn every_corpus_document_survives_open_and_save() {
    let mut failures = Vec::new();
    for (name, before) in corpus::corpus() {
        let after = round_trip(name, &before);
        keep(name, &before, &after);
        let report = roundtrip::compare(&before, &after).unwrap();
        if !report.lossless() {
            failures.push(format!("{name}:\n  {}", report.summary().join("\n  ")));
        }
        // A second save changes nothing more: the saved file is stable.
        let again = round_trip(name, &after);
        let second = roundtrip::compare(&after, &again).unwrap();
        if !second.lossless() {
            failures.push(format!("{name} (second save):\n  {}", second.summary().join("\n  ")));
        }
    }
    assert!(failures.is_empty(), "round-trip losses:\n{}", failures.join("\n"));
}

/// The diff tool itself finds what a lossy save would drop.
#[test]
fn the_diff_tool_reports_dropped_parts_relationships_and_elements() {
    let before = corpus::w14_markup();
    // A "save" that drops the future markup and a part: the plain text of the document only.
    let doc = wordcraft_docx::read(&before).unwrap();
    let mut stripped = wordcraft_doc::Document::from_text(&doc.plain_text(wordcraft_doc::StoryRef::Body));
    stripped.styles = doc.styles.clone();
    let after = wordcraft_docx::write(&stripped).unwrap();
    let report = roundtrip::compare(&before, &after).unwrap();
    assert!(!report.lossless());
    let summary = report.summary().join("\n");
    assert!(summary.contains("lost {urn:example:future-markup}marker"), "{summary}");
    assert!(summary.contains("lost w:sdt"), "{summary}");
}
