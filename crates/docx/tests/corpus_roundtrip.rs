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

/// The first body paragraph whose text contains `needle`, to edit.
fn para_with<'a>(doc: &'a mut wordcraft_doc::Document, needle: &str) -> &'a mut wordcraft_doc::Paragraph {
    let i = doc.body.iter().position(|b| b.as_para().is_some_and(|p| p.plain_text().contains(needle))).expect("paragraph");
    match std::sync::Arc::make_mut(&mut doc.body[i]) {
        wordcraft_doc::Block::Para(p) => p,
        _ => unreachable!(),
    }
}

/// WHIQ: kept markup never contradicts an edit. An alternate content whose shown branch was
/// edited is written as the edited blocks alone (its other branch would show the old text); the
/// unedited one beside it keeps both branches. A run whose colour was changed loses the theme
/// colour it was read with; the untouched runs keep theirs.
#[test]
fn kept_markup_gives_way_to_edits() {
    let mut doc = wordcraft_docx::read(&corpus::block_alternate()).unwrap();
    let p = para_with(&mut doc, "Fallback paragraph.");
    let at = p.text.find("Fallback").unwrap();
    p.insert_text(at, "Edited ", &Default::default()).unwrap();
    let saved = wordcraft_docx::write(&doc).unwrap();
    let xml = String::from_utf8(zip_part(&saved, "word/document.xml")).unwrap();
    assert!(xml.contains("Edited Fallback paragraph."), "{xml}");
    assert_eq!(xml.matches("<mc:AlternateContent").count(), 1, "{xml}");
    assert!(!xml.contains("Choice paragraph one."), "{xml}");
    assert!(xml.contains("w14 choice table.") && xml.contains("Fallback for the table."), "{xml}");

    let mut doc = wordcraft_docx::read(&corpus::unknown_attributes()).unwrap();
    let p = para_with(&mut doc, "Theme formatted.");
    let red = wordcraft_doc::props::TextColor::Rgb(wordcraft_doc::props::Rgb(0xFF, 0, 0));
    for r in p.runs.iter_mut() {
        if r.props.color.is_some() && r.props.size.is_some() {
            r.props.color = Some(red);
        }
    }
    let saved = wordcraft_docx::write(&doc).unwrap();
    let xml = String::from_utf8(zip_part(&saved, "word/document.xml")).unwrap();
    assert!(xml.contains(r#"<w:color w:val="FF0000"/>"#), "{xml}");
    assert_eq!(xml.matches(r#"w:themeColor="accent1" w:themeShade="BF""#).count(), 1, "{xml}");
    // The paragraph mark's and the table cell's theme colours are untouched.
    assert!(xml.contains(r#"<w:rPr><w:rFonts w:ascii="Aptos" w:hAnsi="Aptos" w:cs="Arial" w:hint="eastAsia"/><w:color w:val="0F4761" w:themeColor="accent1" w:themeShade="BF"/>"#), "{xml}");
    assert!(xml.contains(r#"w:themeColor="background1""#), "{xml}");
}

fn zip_part(pkg: &[u8], name: &str) -> Vec<u8> {
    use std::io::Read;
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(pkg)).unwrap();
    let mut f = z.by_name(name).unwrap();
    let mut v = Vec::new();
    f.read_to_end(&mut v).unwrap();
    v
}

/// WHIQ: real Word files, which the repository must not carry (AGENTS.md: never commit files
/// produced by Word): every `.docx`/`.docm` in `$WHIQ_REAL_DOCX_DIR`, when set. Open + save must
/// not lose anything this fork keeps (markup and attributes in extension or unknown namespaces,
/// `mc:AlternateContent`, `w:customXml`); every other difference is printed, as the list of what
/// the model doesn't carry yet.
#[test]
fn real_word_files_keep_what_the_fork_keeps() {
    let Some(dir) = std::env::var_os("WHIQ_REAL_DOCX_DIR") else { return };
    let mut entries: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.path()).collect();
    entries.sort();
    // A loss of kept markup: an element, attribute or run property in an extension or unknown
    // namespace, or alternate content / custom XML (not a text change).
    let kept = |p: &str| {
        let Some((_, what)) = p.split_once(": lost ") else { return false };
        what.contains("{http") || what.contains("{urn") || ["mc:", "w14:", "w15:", "w16", "wp14:", "w:customXml"].iter().any(|k| what.contains(k))
    };
    let (mut failures, mut others, mut checked) = (Vec::new(), Vec::new(), 0);
    for path in entries {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        if !(name.ends_with(".docx") || name.ends_with(".docm")) {
            continue;
        }
        let bytes = std::fs::read(&path).unwrap();
        // Files that don't open (damaged or fuzzed) are not this test's business.
        let Ok((report, _)) = roundtrip::check(&bytes, &name) else { continue };
        checked += 1;
        for p in report.summary() {
            if kept(&p) { failures.push(format!("{name}: {p}")) } else { others.push(format!("{name}: {p}")) }
        }
    }
    eprintln!("{checked} real files; differences outside what the fork keeps ({}):\n{}", others.len(), others.join("\n"));
    assert!(failures.is_empty(), "lost what the fork keeps:\n{}", failures.join("\n"));
}
