//! WHIQ contract tests: drive the built `wordcraft-cli mcp` over stdio and prove the shared
//! contract (craft-fork-contract.md §1–10): error shape, busy, jobs, oversized results,
//! auto-save on eviction / panic / SIGTERM / disconnect, `server_status`, forgiving inputs,
//! confinement and `OLD_` saves, caller identity, annotations and `structuredContent`, and the
//! bidirectional base direction.
//!
//! Run: `cargo test -p wordcraft-cli --test mcp_contract` (set `WORDCRAFT_CLI` to test another
//! binary, e.g. the release build).
// Tests: unwrapping and panicking on failure is fine here (AGENTS.md).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod client;
#[path = "../../crates/docx/tests/corpus/mod.rs"]
mod corpus;

use std::time::{Duration, Instant};

use client::{Dir, Mcp};
use serde_json::{Value, json};

fn text_of(m: &mut Mcp) -> String {
    m.ok("get_text", json!({}))["text"].as_str().unwrap_or("").to_string()
}

// §1 Every error carries code, reason and next_step (checked on every call by the client).
#[test]
fn errors_carry_code_reason_and_next_step() {
    let d = Dir::new("errors");
    let mut m = Mcp::start(&d.0, &[]);
    let e = m.err("no_such_tool", json!({}), "unknown_tool");
    assert!(e["next_step"].as_str().unwrap().contains("tools/list") || e["closest"].is_array(), "{e}");
    let e = m.err("execute", json!({"command": "format.blod"}), "unknown_command");
    assert_eq!(e["closest"][0], "format.bold", "{e}");
    let e = m.err("open_document", json!({}), "bad_params");
    assert!(e["next_step"].as_str().unwrap().contains("`path`"), "{e}");
    std::fs::write(d.path("report.docx"), b"x").unwrap();
    let e = m.err("open_document", json!({"path": "reprot.docx"}), "not_found");
    assert!(e["closest"].as_array().unwrap().iter().any(|c| c.as_str().unwrap().ends_with("report.docx")), "{e}");
    let e = m.err("render_page", json!({"page": 9}), "out_of_range");
    assert!(e["next_step"].as_str().unwrap().contains("1–1"), "{e}");
    let e = m.err("execute", json!({"command": "file.encrypt", "params": {"password": 5}}), "bad_params");
    assert!(e["next_step"].as_str().unwrap().contains("file.encrypt"), "{e}");
    m.ok("type_text", json!({"text": "The quarterly report is ready."}));
    let e = m.err("select_text", json!({"text": "quartely reprot"}), "not_found");
    assert_eq!(e["closest"][0], "quarterly report", "{e}");
    // A JSON-RPC level failure carries the same shape in `data`.
    let r = m.request("nope/nope", json!({}));
    assert!(r["error"].is_object(), "{r}");
}

// §2 Nothing hangs: busy with a retry time, long work as a job whose result survives.
#[test]
fn a_long_call_becomes_a_job_and_others_get_busy() {
    let d = Dir::new("jobs");
    let mut m = Mcp::start(&d.0, &["--job-after-ms", "300"]);
    m.ok("type_text", json!({"text": "Before the job."}));
    let t0 = Instant::now();
    let long = m.send("tools/call", json!({"name": "__sleep", "arguments": {"ms": 2000}}));
    // While it runs: another call is answered at once with busy and a retry time.
    let busy = m.request("tools/call", json!({"name": "get_text", "arguments": {}}));
    let e = &busy["result"]["structuredContent"]["error"];
    assert_eq!(e["code"], "busy", "{busy}");
    assert!(e["retry_after_ms"].as_u64().is_some_and(|ms| ms > 0), "{busy}");
    assert!(t0.elapsed() < Duration::from_millis(1500), "busy came at once");
    // server_status is answered while the call runs and shows it.
    let st = m.ok("server_status", json!({}));
    assert_eq!(st["busy"]["tool"], "__sleep", "{st}");
    // Past job_after_ms the long call answers with a job id.
    let started = m.reply(long);
    let job = started["result"]["structuredContent"].clone();
    assert_eq!(job["status"], "running", "{started}");
    let jid = job["job_id"].as_str().unwrap().to_string();
    assert!(t0.elapsed() < Duration::from_millis(1500), "the job id came before the work finished");
    // Busy now names the job.
    let e = m.err("get_text", json!({}), "busy");
    assert_eq!(e["job_id"], jid.as_str());
    // job_status: running, then done with the result, which stays available.
    let st = m.ok("job_status", json!({"job_id": jid}));
    assert_eq!(st["status"], "running", "{st}");
    let done = loop {
        std::thread::sleep(Duration::from_millis(200));
        let r = m.call("job_status", json!({"job_id": jid.to_uppercase()}));
        if r["structuredContent"]["job"]["status"] == "done" {
            break r;
        }
        assert!(t0.elapsed() < Duration::from_secs(20), "the job never finished");
    };
    assert_eq!(done["structuredContent"]["slept_ms"], 2000, "{done}");
    let again = m.ok("job_status", json!({"job_id": jid}));
    assert_eq!(again["slept_ms"], 2000, "the result survives being read");
    assert_eq!(text_of(&mut m), "Before the job.");
    // background: true makes any call a job at once.
    let j = m.ok("get_text", json!({"background": true}));
    let jid2 = j["job_id"].as_str().unwrap().to_string();
    let r = loop {
        let r = m.ok("job_status", json!({"job_id": jid2}));
        if r["job"]["status"] == "done" {
            break r;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(r["text"], "Before the job.");
    m.err("job_status", json!({"job_id": "job-999"}), "job_not_found");
}

// §3 Work is never lost: SIGTERM auto-saves; the next server names the file.
#[cfg(unix)]
#[test]
fn sigterm_auto_saves_and_the_next_call_names_the_file() {
    let d = Dir::new("sigterm");
    let mut m = Mcp::start(&d.0, &[]);
    m.ok("save_document", json!({"path": "draft.docx"}));
    m.ok("type_text", json!({"text": "Unsaved words"}));
    let pid = m.pid().to_string();
    assert!(std::process::Command::new("kill").args(["-TERM", &pid]).status().unwrap().success());
    client::wait_exit(&mut m.child);
    let recovered: Vec<String> = d.names().into_iter().filter(|n| n.starts_with("draft.recovered-") && n.ends_with(".docx")).collect();
    assert_eq!(recovered.len(), 1, "{:?}", d.names());
    assert!(d.path(".wordcraft-recovered.json").is_file());
    // A new server on the same root: the first call needing the document answers evicted.
    let mut m = Mcp::start(&d.0, &[]);
    let st = m.ok("server_status", json!({}));
    assert!(st["recovered"].to_string().contains(&recovered[0]), "{st}");
    let e = m.err("type_text", json!({"text": "x"}), "evicted");
    assert!(e["next_step"].as_str().unwrap().contains(&recovered[0]), "{e}");
    m.ok("open_document", json!({"path": recovered[0]}));
    assert_eq!(text_of(&mut m), "Unsaved words");
    // Told once.
    m.ok("type_text", json!({"text": "!"}));
    assert!(!d.path(".wordcraft-recovered.json").exists());
}

// §3 The client going away with unsaved changes: auto-saved; without changes: nothing written.
#[test]
fn disconnecting_with_unsaved_work_auto_saves_it() {
    let d = Dir::new("eof");
    let mut m = Mcp::start(&d.0, &[]);
    m.ok("save_document", json!({"path": "kept.docx"}));
    m.close();
    assert_eq!(d.names(), ["kept.docx"], "a clean session leaves nothing behind");
    let mut m = Mcp::start(&d.0, &[]);
    m.ok("open_document", json!({"path": "kept.docx"}));
    m.ok("type_text", json!({"text": "late edit"}));
    m.close();
    assert!(d.names().iter().any(|n| n.starts_with("kept.recovered-")), "{:?}", d.names());
}

// §3 A panic outside any command: auto-saved, answered `internal`, the server keeps working.
#[test]
fn a_panic_is_answered_and_the_work_auto_saved() {
    let d = Dir::new("panic");
    let mut m = Mcp::start(&d.0, &[]);
    m.ok("type_text", json!({"text": "Work before the failure"}));
    let e = m.err("__panic", json!({}), "internal");
    let path = e["recovered"].as_str().unwrap().to_string();
    assert!(std::path::Path::new(&path).is_file(), "{e}");
    assert!(path.starts_with(&d.0.to_string_lossy().into_owned()), "inside the write root: {path}");
    assert_eq!(text_of(&mut m), "Work before the failure", "the server keeps answering");
}

// §3 Memory pressure evicts the document after the call, auto-saved; the next call says so.
#[test]
fn memory_pressure_evicts_with_the_work_auto_saved() {
    let d = Dir::new("evict");
    let mut m = Mcp::start(&d.0, &["--max-memory-mb", "1"]);
    let r = m.ok("type_text", json!({"text": "Precious"}));
    assert!(r["warnings"].to_string().contains("evicted"), "{r}");
    let e = m.err("get_text", json!({}), "evicted");
    let p = e["recovered"].as_str().unwrap().to_string();
    assert!(e["next_step"].as_str().unwrap().contains(&p), "{e}");
    let mut m2 = Mcp::start(&d.0, &[]);
    m2.ok("open_document", json!({"path": p}));
    assert_eq!(text_of(&mut m2), "Precious");
}

// §4 Oversized results are trimmed with a marker and a way to continue; server_status reports.
#[test]
fn oversized_results_are_trimmed_with_continue_from() {
    let d = Dir::new("big");
    let mut m = Mcp::start(&d.0, &[]);
    let lines: Vec<String> = (0..400).map(|i| format!("Paragraph number {i} with some words in it.")).collect();
    m.ok("type_text", json!({"text": lines.join("\n"), "paragraphs": true}));
    let first = m.ok("inspect_document", json!({"max_bytes": 8000}));
    assert_eq!(first["truncated"], true, "{}", &first.to_string()[..300]);
    let total = first["total"].as_u64().unwrap();
    assert_eq!(total, 400);
    let mut seen = first["blocks"].as_array().unwrap().len() as u64;
    let mut from = first["continue_from"].as_u64().unwrap();
    assert_eq!(from, seen);
    let mut rounds = 0;
    while seen < total {
        let next = m.ok("inspect_document", json!({"max_bytes": 8000, "continue_from": from}));
        assert!(next.to_string().len() <= 8000 + 512, "trimmed to the limit");
        let got = next["blocks"].as_array().unwrap();
        assert_eq!(got[0]["text"], format!("Paragraph number {from} with some words in it."));
        seen += got.len() as u64;
        from = next["continue_from"].as_u64().unwrap_or(total);
        rounds += 1;
        assert!(rounds < 200);
    }
    // Text too.
    let t = m.ok("get_text", json!({"max_bytes": 2000}));
    assert_eq!(t["truncated"], true);
    assert!(t["text"].as_str().unwrap().starts_with("Paragraph number 0"));
    // server_status reports memory, the document, jobs, roots and version.
    let st = m.ok("server_status", json!({}));
    for k in ["memory", "document", "jobs", "roots", "version", "recovered", "busy"] {
        assert!(st.get(k).is_some(), "server_status lacks {k}: {st}");
    }
    assert_eq!(st["document"]["paragraphs"], 400);
    assert_eq!(st["roots"]["write"], d.0.to_string_lossy().as_ref());
}

// §5 Forgiving inputs: names in any case and spelling, unknown arguments as warnings.
#[test]
fn inputs_are_forgiving_with_warnings() {
    let d = Dir::new("forgiving");
    let mut m = Mcp::start(&d.0, &[]);
    let r = m.ok("TypeText", json!({"Text": "Hello", "colour": "red"}));
    let w = r["warnings"].to_string();
    assert!(w.contains("read as `type_text`") && w.contains("read as `text`") && w.contains("`colour`"), "{w}");
    let r = m.ok("select-text", json!({"text": "hello", "occurrence": "1"}));
    assert!(r.get("anchor").is_some(), "{r}");
    assert!(r["warnings"].to_string().contains("ignoring case"), "{r}");
    let r = m.ok("execute", json!({"command": "FORMAT_BOLD"}));
    assert!(r["warnings"].to_string().contains("format.bold"), "{r}");
    let r =
        m.ok("batch", json!({"commands": [{"command": "caret.DocEnd"}, {"command": "text.insert", "params": {"text": "!"}}], "keep_going": "true"}));
    assert_eq!(r["ran"], 2, "{r}");
    assert_eq!(text_of(&mut m), "Hello!");
    let r = m.ok("status", json!({}));
    assert!(r["version"].is_string(), "aliases resolve: {r}");
}

// §6 Paths: confined, relative paths inside the write root, atomic saves, OLD_ instead of a
// silent overwrite.
#[test]
fn saves_are_confined_atomic_and_never_silently_overwrite() {
    let d = Dir::new("paths");
    let outside = Dir::new("paths-outside");
    let mut m = Mcp::start(&d.0, &[]);
    m.ok("type_text", json!({"text": "Version one"}));
    let r = m.ok("save_document", json!({"path": "report.docx"}));
    assert!(r.get("kept_previous").is_none());
    assert!(d.path("report.docx").is_file(), "a relative path is inside the write root");
    m.ok("type_text", json!({"text": " and two"}));
    let r = m.ok("save_document", json!({"path": "report.docx"}));
    assert_eq!(r["kept_previous"][0]["kept_as"].as_str().map(|p| p.ends_with("OLD_report.docx")), Some(true), "{r}");
    assert!(r["warnings"].to_string().contains("OLD_report.docx"), "the response says so: {r}");
    // The kept file is the previous version.
    let mut m2 = Mcp::start(&d.0, &[]);
    m2.ok("open_document", json!({"path": "OLD_report.docx"}));
    assert_eq!(text_of(&mut m2), "Version one");
    drop(m2);
    // overwrite: true replaces.
    let r = m.ok("save_document", json!({"path": "report.docx", "overwrite": true}));
    assert!(r.get("kept_previous").is_none(), "{r}");
    // Exports follow the same rule.
    m.ok("export_pdf", json!({"path": "report.pdf"}));
    let r = m.ok("export_pdf", json!({"path": "report.pdf"}));
    assert!(r["kept_previous"].is_array(), "{r}");
    // No temporary files are left behind.
    assert_eq!(d.names(), ["OLD_report.docx", "OLD_report.pdf", "report.docx", "report.pdf"]);
    // Outside the roots: refused, nothing written.
    m.err("save_document", json!({"path": outside.s("x.docx")}), "path_refused");
    m.err("save_document", json!({"path": "../x.docx"}), "path_refused");
    m.err("open_document", json!({"path": outside.s("x.docx")}), "path_refused");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside.0, d.path("escape")).unwrap();
        m.err("save_document", json!({"path": "escape/y.docx"}), "path_refused");
    }
    assert!(outside.names().is_empty());
}

// §7 Caller identity: --author by default, `caller` per call, on tracked changes and comments.
#[test]
fn tracked_changes_and_comments_carry_the_caller() {
    let d = Dir::new("caller");
    let mut m = Mcp::start(&d.0, &["--author", "Agent Default"]);
    m.ok("type_text", json!({"text": "Base text."}));
    m.ok("execute", json!({"command": "review.trackChanges", "params": {"value": true}}));
    m.ok("type_text", json!({"text": " Added by B.", "caller": "Reviewer B"}));
    m.ok("type_text", json!({"text": " Added by default."}));
    let changes = m.ok("execute", json!({"command": "review.changes"}));
    let authors: Vec<String> = changes["result"].as_array().unwrap().iter().map(|c| c["author"].as_str().unwrap_or("").to_string()).collect();
    assert!(authors.contains(&"Reviewer B".to_string()) && authors.contains(&"Agent Default".to_string()), "{changes}");
    m.ok("select_text", json!({"text": "Base"}));
    m.ok("execute", json!({"command": "review.newComment", "params": {"text": "Check"}, "caller": "Reviewer C"}));
    let comments = m.ok("execute", json!({"command": "review.comments"}));
    assert!(comments.to_string().contains("Reviewer C"), "{comments}");
    // Document properties: the last author of a save.
    m.ok("save_document", json!({"path": "signed.docx", "caller": "Reviewer D"}));
    let props = m.ok("execute", json!({"command": "file.properties", "params": {}}));
    assert_eq!(props["lastModifiedBy"], "Reviewer D", "{props}");
    // The default comes back after the call.
    m.ok("type_text", json!({"text": "!"}));
    let changes = m.ok("execute", json!({"command": "review.changes"}));
    assert_eq!(changes["result"].as_array().unwrap().last().unwrap()["author"], "Agent Default");
}

// §8 Annotations: readOnlyHint only on reads, destructiveHint on tools that delete or
// overwrite, idempotentHint where true; structuredContent on every result (checked by the client).
#[test]
fn annotations_say_what_each_tool_does() {
    let d = Dir::new("annotations");
    let mut m = Mcp::start(&d.0, &[]);
    let tools = m.request("tools/list", json!({}))["result"]["tools"].as_array().unwrap().clone();
    let reads = ["list_commands", "get_text", "inspect_document", "render_page", "parity", "screenshot", "ui_inspect", "server_status", "job_status"];
    for t in &tools {
        let name = t["name"].as_str().unwrap();
        let a = &t["annotations"];
        for k in ["readOnlyHint", "destructiveHint", "idempotentHint"] {
            assert!(a[k].is_boolean(), "{name} lacks {k}");
        }
        let ro = a["readOnlyHint"] == true;
        if reads.contains(&name) {
            assert!(ro && a["destructiveHint"] == false, "{name} reads only: {a}");
        } else if ["list_changes", "compare_documents"].contains(&name) {
            assert!(ro, "{name} reads only");
        } else {
            assert!(!ro, "{name} changes something and must not claim readOnlyHint: {a}");
        }
        assert!(t["inputSchema"]["properties"]["caller"].is_object(), "{name} takes caller");
    }
    let by = |n: &str| tools.iter().find(|t| t["name"] == n).unwrap()["annotations"].clone();
    for n in ["save_document", "execute", "batch", "export_pdf", "type_text", "new_document", "open_document"] {
        assert_eq!(by(n)["destructiveHint"], true, "{n}");
    }
    assert_eq!(by("select_text")["destructiveHint"], false);
    assert_eq!(by("new_document")["idempotentHint"], true);
    assert_eq!(by("save_document")["idempotentHint"], false);
    // Every kind of result carries structuredContent: JSON, images, errors.
    let img = m.call("render_page", json!({"page": 1, "dpi": 36}));
    assert_eq!(img["content"][0]["type"], "image");
    assert_eq!(img["structuredContent"]["pages"][0]["page"], 1);
}

// §10 Bidi: a paragraph's base direction from its first strong character; Arabic is shaped in
// PNG and PDF with embedded font subsets.
#[test]
fn arabic_paragraphs_are_right_to_left_and_exported_with_embedded_fonts() {
    let d = Dir::new("bidi");
    let mut m = Mcp::start(&d.0, &[]);
    m.ok("type_text", json!({"text": "مرحبا بالعالم\nHello world\n١٢٣ ثم نص", "paragraphs": true}));
    let doc = m.ok("inspect_document", json!({}));
    let blocks = doc["blocks"].as_array().unwrap();
    let rtl = |i: usize| blocks[i]["props"]["bidi"] == true;
    assert!(rtl(0), "Arabic first: right to left: {}", blocks[0]);
    assert!(!rtl(1), "English line: left to right: {}", blocks[1]);
    assert!(rtl(2), "digits then Arabic: right to left: {}", blocks[2]);
    let img = m.call("render_page", json!({"dpi": 72}));
    assert_eq!(img["isError"], false);
    m.ok("export_pdf", json!({"path": "arabic.pdf"}));
    let pdf = std::fs::read(d.path("arabic.pdf")).unwrap();
    let s = String::from_utf8_lossy(&pdf);
    assert!(s.contains("/FontFile") || s.contains("/FontFile2") || s.contains("/FontFile3"), "fonts are embedded");
    // Subset fonts are named with a six-letter tag (`ABCDEF+Name`).
    let subset = s.split("/BaseFont").skip(1).any(|t| t.trim_start().trim_start_matches('/').chars().take(7).collect::<String>().ends_with('+'));
    assert!(subset, "fonts are embedded as subsets");
}

/// The round trip through MCP: every corpus document (hand-written packages with tracked changes,
/// moves, SmartArt, a chart, custom XML, `w14` and future markup, OLE, a 3-D model, headers,
/// footers, notes, macros…) passes `roundtrip_check`, and opened and saved through the server
/// keeps every part, relationship and story element.
#[test]
fn every_corpus_document_survives_open_and_save_through_mcp() {
    let d = Dir::new("corpus");
    let mut m = Mcp::start(&d.0, &[]);
    for (name, bytes) in corpus::corpus() {
        std::fs::write(d.path(name), &bytes).unwrap();
        let rc = m.ok("roundtrip_check", json!({"path": name}));
        assert_eq!(rc["lossless"], true, "{name}: {rc}");
        m.ok("open_document", json!({"path": name}));
        let saved = format!("saved-{name}");
        m.ok("save_document", json!({"path": saved}));
        let after = std::fs::read(d.path(&saved)).unwrap();
        let report = wordcraft_docx::roundtrip::compare(&bytes, &after).unwrap();
        assert!(report.lossless(), "{name}: {:?}", report.summary());
        // An edit elsewhere in the document keeps the unknown markup too.
        m.ok("execute", json!({"command": "caret.docEnd"}));
        m.ok("type_text", json!({"text": " (edited)"}));
        let edited = format!("edited-{name}");
        m.ok("save_document", json!({"path": edited}));
        let after = std::fs::read(d.path(&edited)).unwrap();
        let report = wordcraft_docx::roundtrip::compare(&bytes, &after).unwrap();
        let losses: Vec<String> = report.summary().into_iter().filter(|l| !l.contains("paragraph") || !l.contains("(edited)")).collect();
        assert!(losses.is_empty(), "{name} after an edit: {losses:?}");
    }
    let e = m.err("roundtrip_check", json!({"path": "missing.docx"}), "not_found");
    assert!(!e["next_step"].as_str().unwrap().is_empty());
}

/// Hostile input never takes the server down.
#[test]
fn garbage_lines_get_answers_and_the_server_stays_up() {
    let d = Dir::new("garbage");
    let mut m = Mcp::start(&d.0, &[]);
    // Hostile numbers are clamped or refused, never fatal.
    let id = m.send("tools/call", json!({"name": "execute", "arguments": {"command": "insert.table", "params": {"rows": 1e30, "cols": -5}}}));
    let r = m.reply(id);
    client::check_shape("execute", &r["result"]);
    let r = m.call("execute", json!({"command": "text.insert", "params": [1, 2, 3]}));
    client::check_shape("execute", &r);
    let r = m.request("tools/call", json!({"name": 42, "arguments": "nope"}));
    assert!(r.get("error").is_some() || r["result"]["isError"] == true, "{r}");
    m.ok("new_document", json!({}));
    m.ok("type_text", json!({"text": "still here"}));
    assert_eq!(text_of(&mut m), "still here");
    let _: Value = m.ok("server_status", json!({}));
}
