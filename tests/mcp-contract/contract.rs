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
    let e = m.err("no_such_tool", json!({}), "not_found");
    assert!(e["next_step"].as_str().unwrap().contains("tools/list") || e["closest"].is_array(), "{e}");
    let e = m.err("execute", json!({"command": "format.blod"}), "not_found");
    assert_eq!(e["closest"][0], "format.bold", "{e}");
    let e = m.err("open_document", json!({}), "invalid_argument");
    assert!(e["next_step"].as_str().unwrap().contains("`path`"), "{e}");
    std::fs::write(d.path("report.docx"), b"x").unwrap();
    let e = m.err("open_document", json!({"path": "reprot.docx"}), "not_found");
    assert!(e["closest"].as_array().unwrap().iter().any(|c| c.as_str().unwrap().ends_with("report.docx")), "{e}");
    let e = m.err("render_page", json!({"page": 9}), "out_of_range");
    assert!(e["next_step"].as_str().unwrap().contains("1–1"), "{e}");
    let e = m.err("execute", json!({"command": "file.encrypt", "params": {"password": 5}}), "invalid_argument");
    assert!(e["next_step"].as_str().unwrap().contains("file.encrypt"), "{e}");
    m.ok("type_text", json!({"text": "The quarterly report is ready."}));
    let e = m.err("select_text", json!({"text": "quartely reprot"}), "not_found");
    assert_eq!(e["closest"][0], "quarterly report", "{e}");
    // A JSON-RPC level failure carries the same shape in `data`.
    let r = m.request("nope/nope", json!({}));
    assert_eq!(r["error"]["data"]["code"], "not_found", "{r}");
    assert!(r["error"]["data"]["next_step"].is_string(), "{r}");
    // So do the protocol errors the worker answers (a tools/call without a name).
    let r = m.request("tools/call", json!({"arguments": {}}));
    assert_eq!(r["error"]["data"]["code"], "invalid_argument", "{r}");
    assert!(r["error"]["data"]["reason"].as_str().is_some_and(|s| s.contains("name")), "{r}");
    // The text block an agent reads first: `[code] reason` and the next step.
    let r = m.call("render_page", json!({"page": 9}));
    let text = r["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("[out_of_range] ") && text.contains("\nnext_step: "), "{text}");
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
    assert_eq!(st["current_call"]["tool"], "__sleep", "{st}");
    // Past job_after_ms the long call answers with a job id.
    let started = m.reply(long);
    let job = started["result"]["structuredContent"].clone();
    assert_eq!(job["status"], "running", "{started}");
    let jid = job["job_id"].as_str().unwrap().to_string();
    assert!(t0.elapsed() < Duration::from_millis(1500), "the job id came before the work finished");
    // Busy now names the job.
    let e = m.err("get_text", json!({}), "busy");
    assert_eq!(e["job_id"], jid.as_str());
    // job_status: running (with progress in the shared shape), then done with the result,
    // which stays available.
    let st = m.ok("job_status", json!({"job_id": jid}));
    assert_eq!(st["status"], "running", "{st}");
    for k in ["job_id", "tool", "elapsed_ms", "progress", "stage", "steps", "stages", "retry_after_ms", "next_step"] {
        assert!(st.get(k).is_some(), "job_status lacks {k}: {st}");
    }
    let done = loop {
        std::thread::sleep(Duration::from_millis(200));
        let r = m.ok("job_status", json!({"job_id": jid.to_uppercase()}));
        if r["status"] == "done" {
            break r;
        }
        assert!(t0.elapsed() < Duration::from_secs(20), "the job never finished");
    };
    assert_eq!(done["result"]["slept_ms"], 2000, "{done}");
    assert_eq!(done["progress"], 1.0, "{done}");
    let again = m.ok("job_status", json!({"job_id": jid}));
    assert_eq!(again["result"]["slept_ms"], 2000, "the result survives being read");
    assert_eq!(text_of(&mut m), "Before the job.");
    // async: true makes any call a job at once (`background`, the older name, too).
    for flag in ["async", "background"] {
        let j = m.ok("get_text", json!({flag: true}));
        let jid2 = j["job_id"].as_str().unwrap().to_string();
        let r = loop {
            let r = m.ok("job_status", json!({"job_id": jid2}));
            if r["status"] == "done" {
                break r;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(r["result"]["text"], "Before the job.");
    }
    // Every job, through list_jobs and job_status without an id.
    let all = m.ok("list_jobs", json!({}));
    assert_eq!(all["jobs"].as_array().unwrap().len(), 3, "{all}");
    assert_eq!(m.ok("job_status", json!({}))["jobs"], all["jobs"]);
    let e = m.err("job_status", json!({"job_id": "job-999"}), "not_found");
    assert!(e["closest"].is_array(), "{e}");
}

// §2 Opening a very large file is a job from the start (`--open-job-mb`); a call that waits behind
// another is served once it ends (within `--busy-after-ms`), else answered busy.
#[test]
fn a_big_open_is_a_job_and_pipelined_calls_wait_their_turn() {
    let d = Dir::new("bigopen");
    let mut m = Mcp::start(&d.0, &["--open-job-mb", "0.001"]);
    m.ok("type_text", json!({"text": "A document a little over a kilobyte. ".repeat(40)}));
    m.ok("save_document", json!({"path": "big.docx"}));
    let started = m.ok("open_document", json!({"path": "big.docx"}));
    let jid = started["job_id"].as_str().expect("a job id at once").to_string();
    let done = job_result(&mut m, &jid);
    assert_eq!(done["structuredContent"]["status"], "done", "{done}");
    // Pipelined: the second call waits for the first (it is short) instead of answering busy.
    let a = m.send("tools/call", json!({"name": "__sleep", "arguments": {"ms": 100}}));
    let b = m.send("tools/call", json!({"name": "get_text", "arguments": {}}));
    assert_eq!(m.reply(a)["result"]["isError"], false);
    assert_eq!(m.reply(b)["result"]["isError"], false, "served after the first call");
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
    // A new server on the same root lists it and names it on its first answer (once).
    let mut m = Mcp::start(&d.0, &[]);
    let st = m.ok("server_status", json!({}));
    assert!(st["recovered_files"].to_string().contains(&recovered[0]), "{st}");
    let r = m.ok("get_text", json!({}));
    assert!(r["recovered_files"].to_string().contains(&recovered[0]), "{r}");
    assert_eq!(r["recovered_files"][0]["reason"], "found");
    assert!(m.ok("get_text", json!({})).get("recovered_files").is_none(), "named once");
    m.ok("open_document", json!({"path": recovered[0]}));
    assert_eq!(text_of(&mut m), "Unsaved words");
    assert!(!d.path(".wordcraft-recovered.json").exists());
}

/// Wait (bounded) for job `jid` to finish: its result.
fn job_result(m: &mut Mcp, jid: &str) -> Value {
    wait_for(&format!("job {jid}"), || {
        let r = m.call("job_status", json!({"job_id": jid}));
        let sc = &r["structuredContent"];
        (sc["status"] != "running").then(|| r.clone())
    })
}

// §5 Every long call reports progress (job_status, server_status, notifications/progress when
// asked for), and `cancel` stops it at its next safe point with the document consistent.
#[test]
fn long_calls_report_progress_and_cancel_stops_them_cleanly() {
    let d = Dir::new("cancel");
    let mut m = Mcp::start(&d.0, &[]);
    // Nothing running: nothing to stop.
    let c = m.ok("cancel", json!({}));
    assert_eq!((c["cancelled"].as_bool(), c["status"].as_str()), (Some(false), Some("not_running")), "{c}");
    // Progress notifications for a call that asks for them, as it runs.
    let id = m.send("tools/call", json!({"name": "__sleep", "arguments": {"ms": 2500}, "_meta": {"progressToken": "tok-1"}}));
    let r = m.reply(id);
    assert_eq!(r["result"]["isError"], false, "{r}");
    let notes: Vec<&Value> = m.notifications.iter().filter(|n| n["params"]["progressToken"] == "tok-1").collect();
    assert!(notes.len() >= 2, "{:?}", m.notifications);
    let values: Vec<f64> = notes.iter().map(|n| n["params"]["progress"].as_f64().unwrap()).collect();
    assert!(values.windows(2).all(|w| w[1] > w[0]), "progress only grows: {values:?}");
    assert!(notes[0]["params"]["message"].as_str().unwrap().contains("__sleep"), "{:?}", notes[0]);

    // A long multi-paragraph type_text as a job: progress counts paragraphs; cancel stops it
    // between paragraphs and the document is as it was before the call.
    let lines: Vec<String> = (0..40_000).map(|i| format!("Line {i}")).collect();
    let id = m.send(
        "tools/call",
        json!({"name": "type_text", "arguments": {"text": lines.join("\n"), "paragraphs": true, "background": true}, "_meta": {"progressToken": 7}}),
    );
    let started = m.reply(id);
    let jid = started["result"]["structuredContent"]["job_id"].as_str().unwrap().to_string();
    let running = wait_for("progress", || {
        let st = m.ok("job_status", json!({"job_id": jid}));
        // Progress is rounded to 0.001: a few paragraphs of 40 000 still read 0.
        (st["steps"]["done"].as_u64().unwrap_or(0) > 0 && st["progress"].as_f64().unwrap_or(0.0) > 0.0).then_some(st)
    });
    assert!(running["stages"].to_string().contains("typing paragraphs"), "{running}");
    assert!(running["progress"].as_f64().unwrap() > 0.0, "{running}");
    let c = m.ok("cancel", json!({"job_id": jid}));
    assert_eq!((c["cancelled"].as_bool(), c["status"].as_str()), (Some(true), Some("cancelling")), "{c}");
    let done = job_result(&mut m, &jid);
    assert_eq!(done["structuredContent"]["status"], "cancelled", "{done}");
    let e = &done["structuredContent"]["error"];
    assert_eq!(e["code"], "cancelled", "{done}");
    assert_eq!(e["rolled_back"], true, "{e}");
    assert_eq!(text_of(&mut m), "", "nothing of the stopped call stays");
    // An ended job can't be cancelled.
    let again = m.ok("cancel", json!({"job_id": jid}));
    assert_eq!((again["cancelled"].as_bool(), again["status"].as_str()), (Some(false), Some("cancelled")), "{again}");
    // A batch stopped part way is undone as a whole.
    m.ok("type_text", json!({"text": "kept"}));
    let cmds: Vec<Value> = (0..20_000).map(|_| json!({"command": "text.insert", "params": {"text": "x"}})).collect();
    let id = m.send("tools/call", json!({"name": "batch", "arguments": {"commands": cmds, "async": true}}));
    let jid = m.reply(id)["result"]["structuredContent"]["job_id"].as_str().unwrap().to_string();
    wait_for("the batch to start", || (m.ok("job_status", json!({"job_id": jid}))["steps"]["done"].as_u64().unwrap_or(0) > 0).then_some(()));
    m.ok("cancel", json!({}));
    let done = job_result(&mut m, &jid);
    assert_eq!(done["structuredContent"]["error"]["code"], "cancelled", "{done}");
    assert_eq!(text_of(&mut m), "kept");
    m.ok("new_document", json!({}));

    // A review edit batch stopped part way changes nothing (all or nothing).
    m.ok("new_document", json!({}));
    m.ok("type_text", json!({"text": "alpha beta gamma delta"}));
    let ops: Vec<Value> = (0..4000).map(|i| json!({"op": "comment", "anchor": "gamma", "text": format!("note {i}")})).collect();
    let id = m.send("tools/call", json!({"name": "review_edit", "arguments": {"ops": ops, "background": true}}));
    let jid = m.reply(id)["result"]["structuredContent"]["job_id"].as_str().unwrap().to_string();
    m.ok("cancel", json!({}));
    let done = job_result(&mut m, &jid);
    assert_eq!(done["structuredContent"]["error"]["code"], "cancelled", "{done}");
    assert_eq!(text_of(&mut m), "alpha beta gamma delta");
    assert_eq!(m.ok("list_changes", json!({}))["comments"].as_array().map_or(0, Vec::len), 0, "nothing of the batch stayed");
    // An unknown job.
    m.err("cancel", json!({"job_id": "job-99"}), "not_found");
}

/// Wait (bounded) until `f` finds something.
fn wait_for<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let until = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < until, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The text of a .docx file on disk.
fn file_text(path: &std::path::Path) -> String {
    let doc = wordcraft_docx::read(&std::fs::read(path).unwrap()).unwrap();
    doc.plain_text(wordcraft_doc::StoryRef::Body).trim_end().to_string()
}

// §3 A kill that can't be caught (SIGKILL here; TerminateProcess on Windows, the OOM killer):
// unsaved work is checkpointed every N edits, so it loses at most the edits since the last one,
// and the next server on the root reports the checkpoint like any recovered work.
#[test]
fn a_hard_kill_loses_at_most_the_edits_since_the_last_checkpoint() {
    let d = Dir::new("checkpoint");
    let mut m = Mcp::start(&d.0, &["--checkpoint-edits", "3", "--checkpoint-secs", "0"]);
    m.ok("save_document", json!({"path": "work.docx"}));
    for w in ["one ", "two ", "three"] {
        m.ok("type_text", json!({"text": w}));
    }
    let ck = wait_for("the checkpoint", || d.names().into_iter().find(|n| n.starts_with("work.recovered-") && n.ends_with(".docx")));
    let st = wait_for("the checkpoint in server_status", || {
        let st = m.ok("server_status", json!({}));
        st["checkpoints"]["files"][0]["path"].as_str().is_some_and(|p| p.ends_with(&ck)).then_some(st)
    });
    assert_eq!(st["checkpoints"]["every_edits"], 3, "{st}");
    assert_eq!(st["checkpoints"]["writes"], 1, "{st}");
    assert!(d.path(".wordcraft-recovered.json").is_file());
    assert_eq!(file_text(&d.path(&ck)), "one two three");
    // After the checkpoint: this edit is what a hard kill loses.
    m.ok("type_text", json!({"text": " four"}));
    m.child.kill().unwrap();
    client::wait_exit(&mut m.child);
    let recovered: Vec<String> = d.names().into_iter().filter(|n| n.starts_with("work.recovered-")).collect();
    assert_eq!(recovered, std::slice::from_ref(&ck), "{:?}", d.names());
    let mut m = Mcp::start(&d.0, &[]);
    let st = m.ok("server_status", json!({}));
    assert!(st["recovered_files"].to_string().contains(&ck), "the next server names it: {st}");
    m.ok("open_document", json!({"path": ck}));
    assert_eq!(text_of(&mut m), "one two three");
}

// §3 Checkpoints by time: one file per stretch of unsaved work (rewritten in place), gone once
// the work is saved.
#[test]
fn checkpoints_follow_the_clock_and_go_once_the_work_is_saved() {
    let d = Dir::new("checkpoint-clock");
    let mut m = Mcp::start(&d.0, &["--checkpoint-edits", "0", "--checkpoint-secs", "1"]);
    m.ok("save_document", json!({"path": "clock.docx"}));
    m.ok("type_text", json!({"text": "tick"}));
    let ck = wait_for("the checkpoint", || d.names().into_iter().find(|n| n.starts_with("clock.recovered-") && n.ends_with(".docx")));
    m.ok("type_text", json!({"text": " tock"}));
    wait_for("the second checkpoint", || (file_text(&d.path(&ck)) == "tick tock").then_some(()));
    let recovered: Vec<String> = d.names().into_iter().filter(|n| n.starts_with("clock.recovered-")).collect();
    assert_eq!(recovered, std::slice::from_ref(&ck), "one file for the whole stretch: {:?}", d.names());
    m.ok("save_document", json!({"path": "clock.docx", "overwrite": true}));
    wait_for("the checkpoint to go", || (!d.names().iter().any(|n| n.starts_with("clock.recovered-"))).then_some(()));
    assert!(!d.path(".wordcraft-recovered.json").exists(), "{:?}", d.names());
    // Nothing unsaved: a hard kill now loses nothing and leaves nothing behind.
    m.child.kill().unwrap();
    client::wait_exit(&mut m.child);
    assert_eq!(d.names(), ["clock.docx"], "{:?}", d.names());
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
    let path = e["recovered_path"].as_str().unwrap().to_string();
    assert!(std::path::Path::new(&path).is_file(), "{e}");
    assert!(path.starts_with(&d.0.to_string_lossy().into_owned()), "inside the write root: {path}");
    assert_eq!(text_of(&mut m), "Work before the failure", "the server keeps answering");
}

// §3 Memory pressure evicts the document after the call, auto-saved; the next call says so.
#[test]
fn memory_pressure_evicts_with_the_work_auto_saved() {
    let d = Dir::new("evict");
    let mut m = Mcp::start(&d.0, &["--max-memory-mb", "1"]);
    // The process's resident memory as the operating system counts it (not an estimate).
    let st = m.ok("server_status", json!({}));
    let rss = st["memory"]["rss_bytes"].as_u64().unwrap_or(0);
    assert!(rss > 1024 * 1024, "{st}");
    assert!(st["memory"]["source"].as_str().is_some_and(|s| !s.is_empty()), "{st}");
    assert_eq!(st["memory"]["measured"], true, "{st}");
    assert_eq!(st["memory"]["limit_bytes"], 1024 * 1024, "{st}");
    let r = m.ok("type_text", json!({"text": "Precious"}));
    assert!(r["warnings"].to_string().contains("evicted"), "{r}");
    let e = m.err("get_text", json!({}), "evicted");
    let p = e["recovered_path"].as_str().unwrap().to_string();
    assert!(e["next_step"].as_str().unwrap().contains(&p), "{e}");
    // Until a document is opened or started, every call that needs one says so.
    m.err("get_text", json!({}), "evicted");
    let mut m2 = Mcp::start(&d.0, &[]);
    m2.ok("open_document", json!({"path": p}));
    assert_eq!(text_of(&mut m2), "Precious");
}

// WHIQ §3: a save is never lost to an eviction: the save that comes after one writes the
// auto-saved work where it was asked, byte for byte.
#[test]
fn a_save_after_a_memory_eviction_writes_the_auto_saved_work() {
    let d = Dir::new("evict-save");
    let mut m = Mcp::start(&d.0, &["--max-memory-mb", "1"]);
    let r = m.ok("type_text", json!({"text": "Saved after the eviction"}));
    assert!(r["warnings"].to_string().contains("evicted"), "{r}");
    let e = m.err("get_text", json!({}), "evicted");
    let rec = e["recovered_path"].as_str().unwrap().to_string();
    let out = d.0.join("after-eviction.docx");
    let s = m.ok("save_document", json!({"path": out.to_string_lossy()}));
    let name = std::path::Path::new(&rec).file_name().unwrap().to_string_lossy().into_owned();
    assert!(s["saved_from"].as_str().unwrap().ends_with(&name), "{s}");
    assert_eq!(std::fs::read(&out).unwrap(), std::fs::read(&rec).unwrap());
    let mut m2 = Mcp::start(&d.0, &[]);
    m2.ok("open_document", json!({"path": out.to_string_lossy()}));
    assert_eq!(text_of(&mut m2), "Saved after the eviction");
}

// WHIQ: a save after an eviction saves the document the call NAMES, not the first one closed.
#[test]
fn a_save_after_an_eviction_saves_the_document_it_names() {
    let d = Dir::new("evict-names");
    let mut m = Mcp::start(&d.0, &[]);
    let a = m.ok("new_document", json!({}))["document"].as_str().unwrap().to_string();
    m.ok("type_text", json!({"text": "work of A", "document": a}));
    let b = m.ok("new_document", json!({}))["document"].as_str().unwrap().to_string();
    m.ok("type_text", json!({"text": "work of B", "document": b}));
    assert_ne!(a, b);
    m.ok("evict", json!({}));
    let (out_b, out_a) = (d.0.join("b-saved.docx"), d.0.join("a-saved.docx"));
    let s = m.ok("save_document", json!({"path": out_b.to_string_lossy(), "document": b}));
    assert!(s["saved_from"].is_string(), "{s}");
    m.ok("save_document", json!({"path": out_a.to_string_lossy(), "document": a}));
    let mut m2 = Mcp::start(&d.0, &[]);
    m2.ok("open_document", json!({"path": out_b.to_string_lossy()}));
    assert_eq!(text_of(&mut m2), "work of B");
    drop(m2);
    let mut m3 = Mcp::start(&d.0, &[]);
    m3.ok("open_document", json!({"path": out_a.to_string_lossy()}));
    assert_eq!(text_of(&mut m3), "work of A");
    // A handle the eviction did not close is an error, not another document's work.
    m.err("save_document", json!({"path": d.0.join("c.docx").to_string_lossy(), "document": "doc-999"}), "failed");
}

// WHIQ: every block of a long document is reachable through inspect_document's continuation
// (the engine answered the first 2,000 and no more), and export_text writes the whole text.
#[test]
fn inspect_document_reaches_every_block_and_export_text_writes_it_all() {
    let d = Dir::new("long");
    let mut m = Mcp::start(&d.0, &[]);
    let lines: Vec<String> = (0..2600).map(|i| format!("Paragraph {i}.")).collect();
    m.ok("type_text", json!({"text": lines.join("\n"), "paragraphs": true}));
    let mut seen = 0usize;
    let mut from = 0u64;
    let mut last = String::new();
    for _ in 0..200 {
        let r = m.ok("inspect_document", json!({"continue_from": from}));
        let blocks = r["blocks"].as_array().unwrap();
        assert!(!blocks.is_empty(), "{r}");
        assert_eq!(blocks[0]["index"].as_u64().unwrap(), from, "the continuation is absolute");
        seen += blocks.len();
        last = blocks.last().unwrap()["text"].as_str().unwrap_or("").to_string();
        match r["continue_from"].as_u64() {
            Some(n) => from = n,
            None => break,
        }
    }
    assert_eq!(seen, 2600);
    assert_eq!(last, "Paragraph 2599.");
    let out = d.0.join("all.txt");
    let t = m.ok("export_text", json!({"path": out.to_string_lossy()}));
    assert_eq!(t["path"], json!(out.to_string_lossy()));
    let text = std::fs::read_to_string(&out).unwrap();
    assert!(text.starts_with("Paragraph 0.") && text.trim_end().ends_with("Paragraph 2599."), "{}", &text[..40]);
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
    for k in [
        "server",
        "version",
        "protocol",
        "pid",
        "uptime_ms",
        "author",
        "memory",
        "fonts",
        "roots",
        "documents",
        "current_call",
        "jobs",
        "recovered_files",
        "evicted",
        "checkpoints",
        "limits",
    ] {
        assert!(st.get(k).is_some(), "server_status lacks {k}: {st}");
    }
    assert_eq!(st["documents"][0]["paragraphs"], 400);
    assert!(st["fonts"]["faces"].is_number(), "{st}");
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
    assert!(r.get("previous_files_renamed").is_none());
    assert!(d.path("report.docx").is_file(), "a relative path is inside the write root");
    m.ok("type_text", json!({"text": " and two"}));
    let r = m.ok("save_document", json!({"path": "report.docx"}));
    assert_eq!(r["previous_files_renamed"][0]["previous"].as_str().map(|p| p.ends_with("OLD_report.docx")), Some(true), "{r}");
    assert!(r["notice"].as_str().unwrap().contains("OLD_report.docx"), "the response says so: {r}");
    assert!(r["warnings"].to_string().contains("OLD_report.docx"), "the response says so: {r}");
    // The kept file is the previous version.
    let mut m2 = Mcp::start(&d.0, &[]);
    m2.ok("open_document", json!({"path": "OLD_report.docx"}));
    assert_eq!(text_of(&mut m2), "Version one");
    drop(m2);
    // overwrite: true replaces.
    let r = m.ok("save_document", json!({"path": "report.docx", "overwrite": true}));
    assert!(r.get("previous_files_renamed").is_none(), "{r}");
    // Exports follow the same rule.
    m.ok("export_pdf", json!({"path": "report.pdf"}));
    let r = m.ok("export_pdf", json!({"path": "report.pdf"}));
    assert!(r["previous_files_renamed"].is_array(), "{r}");
    // No temporary files are left behind.
    assert_eq!(d.names(), ["OLD_report.docx", "OLD_report.pdf", "report.docx", "report.pdf"]);
    // Outside the roots: refused, nothing written.
    let e = m.err("save_document", json!({"path": outside.s("x.docx")}), "outside_root");
    assert_eq!(e["write_root"], d.0.to_string_lossy().as_ref(), "{e}");
    m.err("save_document", json!({"path": "../x.docx"}), "outside_root");
    m.err("open_document", json!({"path": outside.s("x.docx")}), "outside_root");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside.0, d.path("escape")).unwrap();
        m.err("save_document", json!({"path": "escape/y.docx"}), "outside_root");
    }
    assert!(outside.names().is_empty());
}

// §6 For a supervisor that keeps the backups itself: --automation-no-old-copies, no OLD_ files.
#[test]
fn old_copies_can_be_turned_off_for_a_supervisor() {
    let d = Dir::new("no-old");
    let mut m = Mcp::start(&d.0, &["--automation-no-old-copies"]);
    assert_eq!(m.ok("server_status", json!({}))["limits"]["old_copies"], false);
    m.ok("type_text", json!({"text": "Version one"}));
    m.ok("save_document", json!({"path": "report.docx"}));
    m.ok("export_pdf", json!({"path": "report.pdf"}));
    m.ok("type_text", json!({"text": " and two"}));
    for (tool, path) in [("save_document", "report.docx"), ("export_pdf", "report.pdf")] {
        let r = m.ok(tool, json!({"path": path}));
        assert!(r.get("previous_files_renamed").is_none() && r.get("notice").is_none(), "{r}");
    }
    assert_eq!(d.names(), ["report.docx", "report.pdf"], "replaced in place, no OLD_ copies");
    drop(m);
    let mut m = Mcp::start(&d.0, &[]);
    m.ok("open_document", json!({"path": "report.docx"}));
    assert_eq!(text_of(&mut m), "Version one and two");
    assert_eq!(m.ok("server_status", json!({}))["limits"]["old_copies"], true, "on by default");
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
    assert_eq!(props["creator"], "Reviewer D", "a document with no author gets the caller (as in every craft app): {props}");
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
    let reads = [
        "list_commands",
        "get_text",
        "inspect_document",
        "render_page",
        "parity",
        "screenshot",
        "ui_inspect",
        "server_status",
        "job_status",
        "list_jobs",
        "render_preview",
    ];
    for t in &tools {
        let name = t["name"].as_str().unwrap();
        let a = &t["annotations"];
        for k in ["readOnlyHint", "destructiveHint", "idempotentHint", "openWorldHint"] {
            assert!(a[k].is_boolean(), "{name} lacks {k}");
        }
        let ro = a["readOnlyHint"] == true;
        if reads.contains(&name) {
            assert!(ro && a["destructiveHint"] == false, "{name} reads only: {a}");
        } else if ["list_changes", "find_anchor", "roundtrip_check"].contains(&name) {
            assert!(ro, "{name} reads only");
        } else {
            assert!(!ro, "{name} changes something and must not claim readOnlyHint: {a}");
        }
        assert!(t["inputSchema"]["properties"]["caller"].is_object(), "{name} takes caller");
    }
    let by = |n: &str| tools.iter().find(|t| t["name"] == n).unwrap()["annotations"].clone();
    for n in [
        "save_document",
        "execute",
        "batch",
        "export_pdf",
        "type_text",
        "new_document",
        "open_document",
        "review_edit",
        "accept_changes",
        "reject_changes",
        "compare_documents",
    ] {
        assert_eq!(by(n)["destructiveHint"], true, "{n}");
    }
    assert_eq!(by("select_text")["destructiveHint"], false);
    for n in ["cancel", "evict"] {
        assert_eq!(
            (by(n)["readOnlyHint"].as_bool(), by(n)["destructiveHint"].as_bool(), by(n)["idempotentHint"].as_bool()),
            (Some(false), Some(false), Some(true)),
            "{n}"
        );
    }
    assert_eq!(by("new_document")["idempotentHint"], true);
    assert_eq!(by("save_document")["idempotentHint"], false);
    // `_meta["whiq/files"]`: the arguments naming files a call reads, and whether it opens the document.
    let files = |n: &str| tools.iter().find(|t| t["name"] == n).unwrap()["_meta"]["whiq/files"].clone();
    assert_eq!(files("open_document"), json!({"read": ["path"], "opens": true}));
    assert_eq!(files("import_markdown"), json!({"read": ["path"], "opens": true}));
    assert_eq!(files("new_document"), json!({"read": [], "opens": true}));
    assert_eq!(files("roundtrip_check"), json!({"read": ["path"], "opens": false}));
    assert_eq!(files("compare_documents"), json!({"read": ["revised", "original"], "opens": false}));
    assert!(files("save_document").is_null() && files("export_pdf").is_null(), "a save's path is written, not read");
    for t in &tools {
        for k in t["_meta"]["whiq/files"]["read"].as_array().into_iter().flatten() {
            assert!(t["inputSchema"]["properties"][k.as_str().unwrap()].is_object(), "{}: `{k}` is in its schema", t["name"]);
        }
    }
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

// Review tooling: a batch of anchored, tracked edits by the caller; a change report with
// locations; accept/reject per change and per author.
#[test]
fn review_edits_by_anchor_are_tracked_reported_and_resolvable() {
    let d = Dir::new("review");
    let mut m = Mcp::start(&d.0, &["--author", "Drafting Agent"]);
    m.ok(
        "type_text",
        json!({"text": "Payment is due within thirty days.\nThe supplier may terminate on notice.\nGoverning law: England.", "paragraphs": true}),
    );
    m.ok("save_document", json!({"path": "contract.docx"}));
    let r = m.ok(
        "review_edit",
        json!({"caller": "Legal Reviewer", "ops": [
            {"op": "replace", "anchor": "thirty days", "with": "forty-five days"},
            {"op": "insert_paragraph", "anchor": "terminate on notice", "position": "after", "text": "Notice must be given in writing."},
            {"op": "comment", "anchor": "Governing law", "text": "Confirm with the client."},
            {"op": "delete", "anchor": "The suplier may"},
        ]}),
    );
    assert_eq!(r["applied"], 4, "{r}");
    let changes = r["changes"].as_array().unwrap();
    assert_eq!(changes[3]["match"]["kind"], "fuzzy", "a fuzzy anchor is reported as such: {}", changes[3]);
    for c in changes {
        assert_eq!(c["status"], "applied");
        assert!(c["location"]["path"].is_array(), "every change has a location: {c}");
    }
    let replace_ids = changes[0]["changeIds"].as_array().unwrap().clone();
    assert_eq!(replace_ids.len(), 2, "a deletion and an insertion");
    // The report: every change by the caller, with location and context; the comment too.
    let rep = m.ok("list_changes", json!({}));
    assert!(rep["changes"].as_array().unwrap().iter().all(|c| c["author"] == "Legal Reviewer"), "{rep}");
    assert!(rep["changes"].as_array().unwrap().iter().all(|c| c["location"]["start"]["path"].is_array() && c["context"].is_string()));
    assert_eq!(rep["comments"][0]["author"], "Legal Reviewer");
    // A second reviewer's edit, then accept one change, reject by author.
    m.ok("review_edit", json!({"caller": "Second Reviewer", "ops": [{"op": "replace", "anchor": "England", "with": "England and Wales"}]}));
    let r = m.ok("accept_changes", json!({"ids": replace_ids}));
    assert_eq!(r["resolved"].as_array().unwrap().len(), 2, "{r}");
    let r = m.ok("reject_changes", json!({"author": "second reviewer"}));
    assert!(!r["resolved"].as_array().unwrap().is_empty(), "{r}");
    let text = m.ok("get_text", json!({}))["text"].as_str().unwrap().to_string();
    assert!(text.contains("forty-five days") && text.contains("Governing law: England.") && !text.contains("Wales"), "{text}");
    // A batch with an anchor that isn't there changes nothing and says what is close.
    let before = m.ok("get_text", json!({}))["text"].clone();
    let e = m.err("review_edit", json!({"ops": [{"op": "replace", "anchor": "forty-five", "with": "sixty"}, {"op": "replace", "anchor": "zebra crossing rules", "with": "x"}]}), "not_found");
    assert_eq!(e["failed_at"], 1, "{e}");
    assert_eq!(m.ok("get_text", json!({}))["text"], before, "all or nothing");
    let f = m.ok("find_anchor", json!({"anchor": "GOVERNING LAW"}));
    assert_eq!(f["matches"][0]["kind"], "case");
    // Saved, the changes are in the file with the reviewer's name.
    m.ok("save_document", json!({"path": "reviewed.docx"}));
    let bytes = std::fs::read(d.path("reviewed.docx")).unwrap();
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let mut xml = String::new();
    std::io::Read::read_to_string(&mut z.by_name("word/document.xml").unwrap(), &mut xml).unwrap();
    assert!(xml.contains("w:author=\"Legal Reviewer\"") && xml.contains("<w:ins ") && xml.contains("<w:del "), "tracked in the file");
    // Comparing with the original lists the differences.
    let cmp = m.ok("compare_documents", json!({"original": "contract.docx", "revised": "reviewed.docx", "author": "Comparer"}));
    assert!(!cmp["changes"].as_array().unwrap().is_empty(), "{cmp}");
}

/// Unzipped parts of a saved package, by name.
fn parts(path: &std::path::Path) -> std::collections::BTreeMap<String, String> {
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(std::fs::read(path).unwrap())).unwrap();
    let mut out = std::collections::BTreeMap::new();
    for i in 0..z.len() {
        let mut f = z.by_index(i).unwrap();
        let mut b = Vec::new();
        std::io::Read::read_to_end(&mut f, &mut b).unwrap();
        out.insert(f.name().to_string(), String::from_utf8_lossy(&b).into_owned());
    }
    out
}

// Markdown import of the platform's document shapes, with a theme.
#[test]
fn markdown_imports_the_platforms_document_shapes_with_a_theme() {
    let d = Dir::new("markdown");
    std::fs::create_dir_all(d.path("figures")).unwrap();
    std::fs::write(d.path("figures/chart.png"), corpus::PNG).unwrap();
    std::fs::write(d.path("logo.png"), corpus::PNG).unwrap();
    let md = "---\ntitle: Site Survey\nsubtitle: Building C\ndate: 17 October 2026\nauthor: Field Team\nheader: \"{title}\"\n---\n\n[TOC]\n\n# Findings\n\nThe roof is sound.\n\n## Measurements\n\n| Room | Area |\n|---|---|\n| Hall | 40 m² |\n| Office | 12 m² |\n\n![Floor plan](figures/chart.png)\n\n> [!WARNING]\n> The east stair needs a handrail.\n\n1. First step\n2. Second step\n\n- Bullet\n  - Nested bullet\n\n```\nreading = 42\n```\n\n\\pagebreak\n\n### Appendix\n\n#### Detail\n\n:::tip Remember\nKeep the photos.\n:::\n";
    std::fs::write(d.path("survey.md"), md).unwrap();
    let mut m = Mcp::start(&d.0, &[]);
    let r = m.ok("import_markdown", json!({"path": "survey.md", "theme": {"fonts": {"body": "Source Sans 3", "heading": "Source Serif 4"}, "colors": {"primary": "1F3864", "accent": "2E75B6"}, "logo": "logo.png"}}));
    assert_eq!((r["images"].as_u64(), r["callouts"].as_u64(), r["pageBreaks"].as_u64()), (Some(1), Some(2), Some(1)), "{r}");
    assert_eq!(r["cover"], true);
    assert_eq!(r["toc"], true);
    assert!(r["theme"].as_array().unwrap().iter().any(|x| x == "logo"), "{r}");
    let doc = m.ok("inspect_document", json!({}));
    let styles: Vec<&str> = doc["blocks"].as_array().unwrap().iter().filter_map(|b| b["style"].as_str()).collect();
    for s in ["Title", "Subtitle", "TOC1", "Heading1", "Heading2", "Heading3", "Heading4", "ListParagraph", "Code"] {
        assert!(styles.contains(&s), "{s} in {styles:?}");
    }
    m.ok("save_document", json!({"path": "survey.docx"}));
    let p = parts(&d.path("survey.docx"));
    let body = &p["word/document.xml"];
    assert!(body.contains("<w:drawing>") || body.contains("<w:drawing "), "the picture is embedded");
    assert!(body.contains("TOC \\o"), "a TOC field");
    assert!(body.contains("<w:tblHeader") && body.contains("w:fill=\"1F3864\""), "header row repeated and shaded in the theme colour");
    assert!(body.contains("w:pageBreakBefore") || body.contains("w:type=\"page\""), "page breaks");
    assert!(body.contains("<w:titlePg"), "the cover page has no header or footer");
    let footer = p.iter().find(|(k, _)| k.starts_with("word/footer")).map(|(_, v)| v.clone()).unwrap();
    assert!(footer.contains("PAGE") && footer.contains("NUMPAGES"), "page numbers in the footer: {footer}");
    let header = p.iter().find(|(k, _)| k.starts_with("word/header")).map(|(_, v)| v.clone()).unwrap();
    assert!(header.contains("Site Survey") && header.contains("<w:drawing"), "title and logo in the header");
    assert!(p["word/styles.xml"].contains("Source Serif 4"), "the theme's heading font is in the styles");
    assert!(p.keys().any(|k| k.starts_with("word/media/")));
    // It renders, cover and content.
    let img = m.call("render_page", json!({"pages": "1-2", "dpi": 50}));
    assert_eq!(img["content"].as_array().unwrap().iter().filter(|c| c["type"] == "image").count(), 2);
    // The theme alone, on another document.
    m.ok("new_document", json!({}));
    m.ok("type_text", json!({"text": "Hello"}));
    let t = m.ok("apply_theme", json!({"theme": {"font": "Inter", "primary": "#880000"}}));
    assert!(t["applied"].as_array().unwrap().iter().any(|x| x == "body font"), "{t}");
    m.err("import_markdown", json!({}), "invalid_argument");
}

// Literal import: `blocks` carry user text into the saved .docx exactly as typed (no Markdown read).
#[test]
fn blocks_import_user_text_verbatim() {
    let d = Dir::new("blocks");
    let mut m = Mcp::start(&d.0, &[]);
    let note = "   indented *not bold*\nsecond line `x` \\pagebreak [TOC]\n\tTab & <tag> # 1. - + &amp; \\*";
    let cell = "  a\nb | c";
    let r = m.ok(
        "import_markdown",
        json!({"title": "Site diary", "blocks": [
            {"type": "heading", "level": 1, "text": "Notes [x](http://e.com)"},
            {"text": note},
            {"type": "page_break"},
            {"type": "table", "rows": [["date", "note"], ["2026-10-20", cell]]},
            {"runs": [{"text": "  Signed: ", "bold": true}, "_A. Name_"]},
        ]}),
    );
    assert_eq!((r["tables"].as_u64(), r["pageBreaks"].as_u64(), r["cover"].as_bool()), (Some(1), Some(1), Some(true)), "{r}");
    m.ok("save_document", json!({"path": "diary.docx"}));
    let body = &parts(&d.path("diary.docx"))["word/document.xml"];
    assert!(body.contains(">   indented *not bold*<"), "leading spaces kept in the file: {body}");
    assert!(body.contains("<w:br"), "a line break is a break");
    assert!(body.contains("Tab &amp; &lt;tag&gt; # 1. - + &amp;amp; \\*"), "every character as typed");
    assert!(body.contains("<w:tblHeader"), "the first row is a header row");
    drop(m);
    let mut m = Mcp::start(&d.0, &[]);
    m.ok("open_document", json!({"path": "diary.docx"}));
    let doc = m.ok("inspect_document", json!({}));
    let all = doc.to_string();
    let texts: Vec<&str> = doc["blocks"].as_array().unwrap().iter().filter_map(|b| b["text"].as_str()).collect();
    for want in ["Notes [x](http://e.com)", note, "  Signed: _A. Name_"] {
        assert!(texts.contains(&want), "{want:?} in {texts:?}");
    }
    assert!(all.contains(&serde_json::to_string(cell).unwrap()), "the cell as typed: {all}");
    m.err("import_markdown", json!({"blocks": [{"type": "chart"}]}), "invalid_argument");
}

// A hard kill: the documents set aside beside the active one are in checkpoints too.
#[test]
fn a_hard_kill_keeps_a_checkpoint_of_every_open_document() {
    let d = Dir::new("checkpoint-all");
    let mut m = Mcp::start(&d.0, &["--checkpoint-edits", "1", "--checkpoint-secs", "0"]);
    m.ok("save_document", json!({"path": "first.docx"}));
    m.ok("type_text", json!({"text": "first draft"}));
    // An open sets the unsaved first document aside (it stays open) and edits a second one.
    m.ok("import_markdown", json!({"markdown": "Second document."}));
    m.ok("save_document", json!({"path": "second.docx"}));
    m.ok("type_text", json!({"text": "More. "}));
    let find = |prefix: &str| d.names().into_iter().find(|n| n.starts_with(prefix) && n.ends_with(".docx"));
    let first = wait_for("the first document's checkpoint", || find("first.recovered-"));
    let second = wait_for("the second document's checkpoint", || find("second.recovered-"));
    m.child.kill().unwrap();
    client::wait_exit(&mut m.child);
    assert_eq!(file_text(&d.path(&first)), "first draft");
    assert!(file_text(&d.path(&second)).contains("Second document."));
}

// Document handles: an export started in the process the user's document is open in never touches it.
#[test]
fn document_handles_keep_an_export_apart_from_the_open_document() {
    let d = Dir::new("handles");
    let mut m = Mcp::start(&d.0, &[]);
    let user = m.ok("new_document", json!({}))["document"].as_str().unwrap().to_string();
    m.ok("type_text", json!({"text": "The user's unsaved work."}));
    let export = m.ok("import_markdown", json!({"markdown": "# Export\n\nBody.", "footer": "none"}))["document"].as_str().unwrap().to_string();
    assert_ne!(export, user);
    m.ok("save_document", json!({"path": "export.docx", "document": export}));
    assert!(d.path("export.docx").is_file());
    // The user's document is as it was, unsaved; a call naming it acts on it, the active one comes back.
    assert_eq!(m.ok("get_text", json!({"document": user}))["text"], "The user's unsaved work.");
    assert!(text_of(&mut m).contains("Export"), "the export is still the active document");
    let st = m.ok("server_status", json!({}));
    let docs = st["documents"].as_array().unwrap();
    assert_eq!(docs.len(), 2, "{st}");
    assert_eq!((docs[0]["document"].as_str(), docs[0]["active"].as_bool()), (Some(export.as_str()), Some(true)), "{st}");
    assert!(docs.iter().any(|x| x["document"] == user.as_str() && x["dirty"] == true), "{st}");
    // close_document: refused with unsaved changes; the saved export closes and the user's document is active.
    m.err("close_document", json!({"document": user}), "failed");
    let r = m.ok("close_document", json!({"document": export}));
    assert_eq!(r["active_document"], user.as_str(), "{r}");
    assert_eq!(text_of(&mut m), "The user's unsaved work.");
    // An unknown handle names the open ones; a failed open leaves everything as it was.
    let e = m.err("get_text", json!({"document": "doc-99"}), "not_found");
    assert_eq!(e["open_documents"], json!([user]), "{e}");
    m.err("open_document", json!({"path": "missing.docx"}), "not_found");
    assert_eq!(m.ok("server_status", json!({}))["documents"].as_array().unwrap().len(), 1);
    assert_eq!(text_of(&mut m), "The user's unsaved work.");
    // Every document with unsaved work is recovered when the client goes away, each to its own file.
    m.ok("import_markdown", json!({"markdown": "Second export, not saved."}));
    m.close();
    let recovered: Vec<String> = d.names().into_iter().filter(|n| n.contains(".recovered-") && n.ends_with(".docx")).collect();
    assert_eq!(recovered.len(), 2, "{:?}", d.names());
    let mut texts = Vec::new();
    for f in &recovered {
        let mut m = Mcp::start(&d.0, &[]);
        m.ok("open_document", json!({"path": f}));
        texts.push(text_of(&mut m));
    }
    assert!(texts.iter().any(|t| t == "The user's unsaved work.") && texts.iter().any(|t| t.contains("Second export")), "{texts:?}");
    // `evict` saves and closes every open document too.
    let mut m = Mcp::start(&d.0, &[]);
    let a = m.ok("new_document", json!({}))["document"].as_str().unwrap().to_string();
    m.ok("type_text", json!({"text": "one"}));
    let b = m.ok("new_document", json!({}))["document"].as_str().unwrap().to_string();
    m.ok("type_text", json!({"text": "two"}));
    let r = m.ok("evict", json!({}));
    let ev = r["evicted"].as_array().unwrap();
    assert_eq!(ev.len(), 2, "{r}");
    assert!(ev.iter().all(|e| e["recovered_path"].is_string()) && ev[0]["document"] == b.as_str() && ev[1]["document"] == a.as_str(), "{r}");
}

/// Pages of a PDF (`/Type /Page` objects, not `/Pages`).
fn pdf_pages(bytes: &[u8]) -> usize {
    // Objects may sit in compressed object streams: inflate every stream first.
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    let mut at = 0;
    while let Some(i) = find(bytes, b"stream", at) {
        let start = i + 6 + if bytes.get(i + 6) == Some(&b'\r') { 2 } else { 1 };
        let Some(end) = find(bytes, b"endstream", start) else { break };
        if let Ok(out) = miniz_oxide::inflate::decompress_to_vec_zlib(&bytes[start..end]) {
            text.push_str(&String::from_utf8_lossy(&out));
        }
        at = end + 9;
    }
    let t = text.replace("/Type/", "/Type /");
    t.matches("/Type /Page").count() - t.matches("/Type /Pages").count()
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    hay.get(from..)?.windows(needle.len()).position(|w| w == needle).map(|i| i + from)
}

fn png_size(b64: &str) -> (u32, u32) {
    // The PNG header (IHDR) lies within the first bytes: decode just enough of the base64.
    let head: String = b64.chars().take(44).collect();
    let mut bytes = Vec::new();
    let val = |c: char| "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/".find(c).unwrap() as u32;
    for chunk in head.as_bytes().chunks(4) {
        let n = chunk.iter().fold(0u32, |acc, c| (acc << 6) | val(*c as char));
        bytes.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8, n as u8]);
    }
    (u32::from_be_bytes(bytes[16..20].try_into().unwrap()), u32::from_be_bytes(bytes[20..24].try_into().unwrap()))
}

// Render: PNG at a chosen DPI and PDF, with and without markup, for a page range.
#[test]
fn pages_render_to_png_and_pdf_by_range_with_and_without_markup() {
    let d = Dir::new("render");
    let mut m = Mcp::start(&d.0, &[]);
    m.ok("import_markdown", json!({"markdown": "# One\n\nFirst page text.\n\n\\pagebreak\n\n# Two\n\nSecond page text.\n\n\\pagebreak\n\n# Three\n\nThird page text.", "footer": "none"}));
    m.ok("review_edit", json!({"caller": "Reviewer", "ops": [{"op": "replace", "anchor": "Second page", "with": "Middle page"}, {"op": "comment", "anchor": "Third page", "text": "A remark for the margin."}]}));
    // A range at a DPI: one image per page, sized by the DPI.
    let r = m.call("render_page", json!({"pages": "1-3", "dpi": 72}));
    let imgs: Vec<String> =
        r["content"].as_array().unwrap().iter().filter(|c| c["type"] == "image").map(|c| c["data"].as_str().unwrap().to_string()).collect();
    assert_eq!(imgs.len(), 3);
    assert_eq!(png_size(&imgs[0]), (612, 792), "Letter at 72 dpi");
    let hi = m.call("render_page", json!({"page": 2, "dpi": 144}));
    assert_eq!(png_size(hi["content"][0]["data"].as_str().unwrap()), (1224, 1584));
    let some = m.call("render_page", json!({"pages": "2-", "dpi": 36}));
    assert_eq!(some["structuredContent"]["pages"].as_array().unwrap().iter().map(|p| p["page"].as_u64().unwrap()).collect::<Vec<_>>(), [2, 3]);
    // With and without markup: the page with the change and comment differs.
    let plain = m.call("render_page", json!({"page": 3, "dpi": 50, "markup": false}));
    let marked = m.call("render_page", json!({"page": 3, "dpi": 50, "markup": true}));
    assert_ne!(plain["content"][0]["data"], marked["content"][0]["data"], "markup shows the comment");
    assert_eq!(marked["structuredContent"]["markup"], true);
    m.err("render_page", json!({"pages": "4-9"}), "out_of_range");
    m.err("render_page", json!({"page": 1, "dpi": 5000}), "invalid_argument");
    // PDF: all pages, a range, with and without markup.
    m.ok("export_pdf", json!({"path": "all.pdf"}));
    m.ok("export_pdf", json!({"path": "two.pdf", "pages": "2-3"}));
    m.ok("export_pdf", json!({"path": "marked.pdf", "pages": "2-3", "markup": true}));
    let (all, two, marked) =
        (std::fs::read(d.path("all.pdf")).unwrap(), std::fs::read(d.path("two.pdf")).unwrap(), std::fs::read(d.path("marked.pdf")).unwrap());
    assert_eq!((pdf_pages(&all), pdf_pages(&two), pdf_pages(&marked)), (3, 2, 2));
    assert_ne!(two, marked, "markup changes the PDF");
    m.err("export_pdf", json!({"path": "bad.pdf", "pages": "7"}), "out_of_range");
    // The command line does the same.
    m.ok("save_document", json!({"path": "doc.docx"}));
    let cli = |args: &[&str]| std::process::Command::new(client::binary()).args(args).current_dir(&d.0).output().unwrap();
    let o = cli(&["render", "doc.docx", "page.png", "--pages", "2-3", "--dpi", "36", "--markup"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(d.path("page-2.png").is_file() && d.path("page-3.png").is_file() && !d.path("page-1.png").exists());
    let o = cli(&["convert", "doc.docx", "cli.pdf", "--pages", "1-2"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(pdf_pages(&std::fs::read(d.path("cli.pdf")).unwrap()), 2);
}

// §3 Idle eviction (`--idle-evict-secs`) and `evict` (for a supervisor): the work is auto-saved in
// the write root, a call that needs the document answers `evicted` with the file until one is
// opened, and server_status lists the eviction.
#[test]
fn idle_eviction_and_evict_auto_save_and_name_the_file() {
    let d = Dir::new("idle");
    let mut m = Mcp::start(&d.0, &["--idle-evict-secs", "1"]);
    m.ok("type_text", json!({"text": "Idle work"}));
    std::thread::sleep(Duration::from_millis(2600));
    let e = m.err("get_text", json!({}), "evicted");
    let p = e["recovered_path"].as_str().unwrap().to_string();
    assert!(p.starts_with(&d.0.to_string_lossy().into_owned()) && p.contains(".recovered-"), "{e}");
    let st = m.ok("server_status", json!({}));
    assert_eq!(st["evicted"][0]["recovered_path"], p.as_str(), "{st}");
    m.ok("open_document", json!({"path": p}));
    assert_eq!(text_of(&mut m), "Idle work");
    drop(m);
    let mut m = Mcp::start(&d.0, &[]);
    m.ok("type_text", json!({"text": "Supervised"}));
    let r = m.ok("evict", json!({"reason": "the supervisor needs the memory"}));
    let p = r["evicted"][0]["recovered_path"].as_str().unwrap().to_string();
    let e = m.err("type_text", json!({"text": "x"}), "evicted");
    assert_eq!(e["recovered_path"], p.as_str());
    m.ok("open_document", json!({"path": p}));
    assert_eq!(text_of(&mut m), "Supervised");
}

// WHIQ (2026-10-30): the Preview pane is drawn by the engine (render_preview), and agents read
// Word files through it as Markdown (export_markdown), in parts for any size.
#[test]
fn render_preview_and_export_markdown_draw_the_document_in_parts() {
    let d = Dir::new("preview");
    std::fs::write(d.path("chart.png"), corpus::PNG).unwrap();
    let mut m = Mcp::start(&d.0, &[]);
    let md = "# Findings\n\nThe roof is **sound**.\n\n| Room | Area |\n|---|---|\n| Hall | 40 m² |\n\n![Floor plan](chart.png)\n\n1. First step\n2. Second step\n\nمرحبا بالعالم\n";
    std::fs::write(d.path("survey.md"), md).unwrap();
    m.ok("import_markdown", json!({"path": "survey.md", "cover": false, "footer": "none"}));
    let full = m.call("render_preview", json!({}));
    assert_eq!(full["isError"], false, "{full}");
    let sc = &full["structuredContent"];
    let html = sc["html"].as_str().unwrap();
    assert!(html.starts_with("<!DOCTYPE html>") && html.contains("<div class=\"file-head\">") && html.contains("<div class=\"page\""));
    assert!(html.contains(">Findings</h1>") && html.contains("font-weight:700") && html.contains("<table"));
    assert!(html.contains("data:image/png;base64,"), "the picture is embedded");
    assert!(html.contains(">1.</span>") && html.contains("<p dir=\"rtl\""), "list labels and RTL");
    assert_eq!((sc["part"].as_u64(), sc["parts"].as_u64()), (Some(1), Some(1)));
    assert_eq!(sc["stats"]["pictures"], 1);
    // The page is sent once: the text block carries the rest of the answer.
    let text = full["content"][0]["text"].as_str().unwrap();
    assert!(!text.contains("<!DOCTYPE") && text.contains("\"html_in\""), "{text}");
    // Markdown for reading.
    let r = m.ok("export_markdown", json!({}));
    let md = r["markdown"].as_str().unwrap();
    assert!(md.starts_with("# Findings\n\nThe roof is **sound**."), "{md}");
    assert!(
        md.contains("| **Room** | **Area** |\n| --- | --- |\n| Hall | 40 m² |")
            && md.contains("1. First step\n2. Second step")
            && md.contains("![Floor plan](picture:"),
        "{md}"
    );
    // A long document: parts of the caller's size, every one reachable, nothing cut.
    m.ok("new_document", json!({}));
    let lines: Vec<String> = (0..1500).map(|i| format!("Paragraph {i} {}", "with words ".repeat(10))).collect();
    m.ok("type_text", json!({"text": lines.join("\n"), "paragraphs": true}));
    let first = m.ok("export_markdown", json!({"part_size": 20000}));
    let parts = first["parts"].as_u64().unwrap();
    assert!(parts >= 8, "{first}");
    assert_eq!(first["next_part"], 2);
    let mut joined = String::new();
    for n in 1..=parts {
        let r = m.ok("export_markdown", json!({"part_size": 20000, "part": n}));
        assert!(r.get("truncated").is_none(), "parts are never trimmed");
        joined.push_str(r["markdown"].as_str().unwrap());
        joined.push_str("\n\n");
    }
    assert_eq!(joined.matches("Paragraph ").count(), 1500);
    let big = m.ok("export_markdown", json!({"part_size": 100_000_000u64}));
    assert_eq!(big["parts"], 1, "no cap on the part size");
    assert!(big["markdown"].as_str().unwrap().len() > 100_000, "past --max-result-bytes, whole");
    let last = m.ok("render_preview", json!({"part_size": 20000, "part": parts}));
    assert_eq!(last["next_part"], Value::Null);
    let e = m.err("render_preview", json!({"part_size": 20000, "part": parts + 5}), "out_of_range");
    assert!(e["reason"].as_str().unwrap().contains(&format!("{parts} parts")), "{e}");
    let w = m.ok("render_preview", json!({"pages": "2"}));
    assert!(w["pages"]["from"] == 2 && w["blocks"]["from"].as_u64().unwrap() > 0, "{}", w["blocks"]);
    // To a file, the whole document.
    let f = m.ok("export_markdown", json!({"out": "long.md"}));
    assert_eq!(f["parts"], 1);
    assert_eq!(std::fs::read_to_string(d.path("long.md")).unwrap().matches("Paragraph ").count(), 1500);
    // A file drawn without opening it: the open document is untouched.
    m.ok("save_document", json!({"path": "long.docx"}));
    m.ok("new_document", json!({}));
    m.ok("type_text", json!({"text": "Open one"}));
    let pf = m.ok("render_preview", json!({"path": "long.docx", "part_size": 20000}));
    assert!(pf["html"].as_str().unwrap().contains("<div class=\"file-head\">long.docx · part 1 of"));
    assert_eq!(m.ok("get_text", json!({}))["text"], "Open one");
    m.err("render_preview", json!({"path": "missing.docx"}), "not_found");
}

/// Many edits in one call: 300 review operations (replace all, paragraphs, comments, table rows
/// and cells) and a batch of 300 commands.
#[test]
fn hundreds_of_edits_go_in_one_call() {
    let d = Dir::new("bulk");
    let mut m = Mcp::start(&d.0, &[]);
    let mut md = String::from("# Register\n\n| Item | Owner |\n|---|---|\n| first | nobody |\n\n");
    for i in 0..200 {
        md.push_str(&format!("Clause {i}: the Contractor shall deliver item {i}.\n\n"));
    }
    m.ok("import_markdown", json!({"markdown": md, "cover": false, "footer": "none"}));
    let mut ops = vec![json!({"op": "replace", "anchor": "Contractor", "with": "Supplier", "occurrence": "all"})];
    for i in 0..100 {
        ops.push(json!({"op": "insert_paragraph", "anchor": format!("Clause {i}:"), "text": format!("Note {i} added.")}));
    }
    for i in 100..200 {
        ops.push(json!({"op": "comment", "anchor": format!("item {i}."), "text": format!("Check {i}")}));
    }
    for i in 0..98 {
        ops.push(json!({"op": "insert_table_row", "table": 1, "cells": [format!("row {i}"), "owner"]}));
    }
    ops.push(json!({"op": "set_cell", "table": 1, "row": 2, "col": 2, "text": "Alice"}));
    assert_eq!(ops.len(), 300);
    let t0 = Instant::now();
    let r = m.ok("review_edit", json!({"ops": ops}));
    let took = t0.elapsed();
    assert_eq!((r["applied"].as_u64(), r["failed"].as_u64()), (Some(300), Some(0)), "{}", &r.to_string()[..500.min(r.to_string().len())]);
    eprintln!("review_edit, 300 operations: {took:?}");
    let text = m.ok("export_markdown", json!({"part_size": 1_000_000}))["markdown"].as_str().unwrap().to_string();
    assert_eq!(text.matches("{++Supplier++}{--Contractor--}").count(), 200, "replace all, tracked: {}", &text[..1500.min(text.len())]);
    assert_eq!(text.matches("Note ").count(), 100);
    assert_eq!(text.matches("{>>C").count(), 100);
    assert!(text.contains("| {++row 97++} | {++owner++} |") && text.contains("| first | {++Alice++}{--nobody--} |"), "{}", &text[..600]);
    let changes = m.ok("list_changes", json!({}));
    assert!(changes["changes"].as_array().unwrap().len() >= 300);
    // 300 commands in one batch.
    let mut commands = vec![json!({"command": "caret.docEnd"})];
    for i in 0..299 {
        commands.push(if i % 3 == 0 {
            json!({"command": "text.newParagraph"})
        } else {
            json!({"command": "text.insert", "params": {"text": format!("typed {i} ")}})
        });
    }
    let t0 = Instant::now();
    let b = m.ok("batch", json!({"commands": commands}));
    eprintln!("batch, 300 commands: {:?}", t0.elapsed());
    assert_eq!(b["ran"], 300);
    assert!(m.ok("get_text", json!({"max_bytes": 4_000_000}))["text"].as_str().unwrap().contains("typed 298"));
    // Not tracked: a plain replace-all of 200 places in one operation.
    let r = m
        .ok("review_edit", json!({"ops": [{"op": "replace", "anchor": "shall deliver", "with": "delivers", "occurrence": "all"}], "tracked": false}));
    assert_eq!(r["applied"], 1);
    let after = m.ok("export_markdown", json!({"part_size": 1_000_000}))["markdown"].as_str().unwrap().to_string();
    assert_eq!(after.matches("delivers ").count(), 200);
    assert!(!after.contains("shall deliver"));
    assert!(!after.contains("{++delivers"));
}

/// The owner's checklist (2026-10-30): an agent works a Word document fully, as a person in Word
/// does — read, create, edit, delete, search — on a real .docx saved and opened again.
#[test]
fn an_agent_works_a_word_document_like_a_person_in_word() {
    let d = Dir::new("person");
    std::fs::write(d.path("plan.png"), corpus::PNG).unwrap();
    std::fs::write(d.path("photo.png"), corpus::PNG).unwrap();
    let mut m = Mcp::start(&d.0, &[]);
    let md = "# Site report\n\nThe roof is sound.\n\n## Rooms\n\n| Room | Area |\n|---|---|\n| Hall | 40 m² |\n| Office | 12 m² |\n| Store | 6 m² |\n\n![Floor plan](plan.png)\n\n1. Inspect the stair\n2. Fix the gutter\n\nObsolete paragraph to remove.\n";
    m.ok("import_markdown", json!({"markdown": md, "cover": false, "header": "Acme Survey", "footer": "Page {page} of {pages}"}));
    // CREATE — a caption under the picture, a footnote, a comment, a heading, a section, a TOC.
    m.ok("select_text", json!({"text": "The roof is sound."}));
    m.ok("execute", json!({"command": "caret.end"}));
    m.ok("execute", json!({"command": "references.footnote", "params": {"text": "Checked on 30 October."}}));
    let created = m.ok(
        "review_edit",
        json!({"tracked": false, "ops": [
            {"op": "insert_paragraph", "anchor": "Inspect the stair", "position": "before", "text": "Actions", "style": "Heading 2"},
            {"op": "comment", "anchor": "Fix the gutter", "text": "Before winter"},
            {"op": "insert_paragraph", "anchor": "Obsolete paragraph", "text": "Appendix", "style": "Heading 1"},
        ]}),
    );
    assert_eq!(created["applied"], 3, "{created}");
    m.ok("select_text", json!({"text": "Appendix"}));
    m.ok("execute", json!({"command": "caret.home"}));
    m.ok("execute", json!({"command": "layout.break", "params": {"kind": "nextPage"}}));
    m.ok("execute", json!({"command": "caret.docEnd"}));
    m.ok("execute", json!({"command": "text.newParagraph"}));
    m.ok("execute", json!({"command": "insert.picture", "params": {"path": "photo.png", "alt": "Site photo"}}));
    m.ok("execute", json!({"command": "text.newParagraph"}));
    m.ok("execute", json!({"command": "insert.table", "params": {"rows": 2, "cols": 2}}));
    m.ok(
        "review_edit",
        json!({"tracked": false, "ops": [
            {"op": "set_cell", "table": 2, "row": 1, "col": 1, "text": "Key"},
            {"op": "set_cell", "table": 2, "row": 1, "col": 2, "text": "Value"},
        ]}),
    );
    m.ok("execute", json!({"command": "caret.docStart"}));
    m.ok("execute", json!({"command": "references.toc", "params": {}}));
    // A real Word file: saved, closed, opened again.
    m.ok("save_document", json!({"path": "report.docx"}));
    m.ok("close_document", json!({}));
    m.ok("open_document", json!({"path": "report.docx"}));

    // READ — text in order with headings, lists, tables, footnotes, headers/footers, comments,
    // pictures, fields; styles; a picture of a page.
    let md = m.ok("export_markdown", json!({}))["markdown"].as_str().unwrap().to_string();
    let at = |s: &str| md.find(s).unwrap_or_else(|| panic!("`{s}` not in:\n{md}"));
    assert!(at("# Site report") < at("## Rooms") && at("## Rooms") < at("| Hall | 40 m² |"), "in order");
    assert!(at("1. Inspect the stair\n2. {==Fix the gutter") > 0, "list labels, a tight list");
    assert!(md.contains("> **Header:** Acme Survey") && md.contains("> **Footer:** Page"), "header and footer");
    assert!(md.contains("[^1]") && md.contains("[^1]: Checked on 30 October."), "footnote");
    assert!(md.contains("{>>C1") && md.contains("Before winter<<}"), "comment");
    assert!(md.contains("![Floor plan](picture:") && md.contains("![Site photo](picture:"), "pictures with alt text");
    assert!(md.contains("<!-- pagebreak -->"), "the section starts a new page");
    assert!(md.contains("Site report") && md.to_lowercase().contains("contents") || md.contains("Rooms\t"), "the TOC field's text");
    let styles: Vec<String> =
        m.ok("inspect_document", json!({}))["blocks"].as_array().unwrap().iter().filter_map(|b| b["style"].as_str().map(str::to_string)).collect();
    assert!(styles.iter().any(|s| s == "Heading1") && styles.iter().any(|s| s == "Heading2"), "{styles:?}");
    let page = m.call("render_page", json!({"page": 1, "dpi": 40}));
    assert_eq!(page["content"][0]["type"], "image");
    let prev = m.ok("render_preview", json!({}));
    assert!(
        prev["html"].as_str().unwrap().contains("Acme Survey") && prev["stats"]["footnotes"] == 1 && prev["stats"]["comments"] == 1,
        "{}",
        prev["stats"]
    );

    // SEARCH — find text with its locations; find and replace, tracked and not.
    let found = m.ok("find_anchor", json!({"anchor": "Office"}));
    assert!(found["matches"].as_array().is_some_and(|v| !v.is_empty() && v[0]["path"].is_array()), "{found}");
    let r = m.ok("review_edit", json!({"ops": [{"op": "replace", "anchor": "m²", "with": "sq m", "occurrence": "all"}]}));
    assert_eq!(r["changes"][0]["matches"].as_array().map(Vec::len), Some(3), "tracked replace of every match");
    m.ok("review_edit", json!({"tracked": false, "ops": [{"op": "replace", "anchor": "gutter", "with": "downpipe", "occurrence": "all"}]}));

    // EDIT — text, styles, tables, accept / reject, in one batch where they can be.
    let e = m.ok(
        "review_edit",
        json!({"ops": [
            {"op": "insert", "anchor": "The roof is sound", "text": " and dry"},
            {"op": "set_style", "anchor": "Appendix", "style": "Heading 2"},
            {"op": "insert_table_row", "table": 1, "cells": ["Lab", "9 sq m"]},
            {"op": "set_cell", "anchor": "Store", "col": 2, "text": "7 sq m"},
        ]}),
    );
    assert_eq!(e["applied"], 4, "{e}");
    let ch = m.ok("list_changes", json!({}));
    let id_of = |want: &str| -> Vec<String> {
        ch["changes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["text"].as_str().is_some_and(|t| t.contains(want)))
            .map(|c| c["id"].as_str().unwrap().to_string())
            .collect()
    };
    let dry = id_of("and dry");
    assert_eq!(dry.len(), 1, "{ch}");
    m.ok("reject_changes", json!({"ids": dry}));
    let sq = id_of("sq m");
    assert!(!sq.is_empty());
    let acc = m.ok("accept_changes", json!({"ids": sq}));
    assert!(acc["resolved"].as_array().is_some_and(|v| !v.is_empty()), "{acc}");
    let b = m.ok("batch", json!({"commands": [{"command": "caret.docStart"}, {"command": "para.heading1"}]}));
    assert_eq!(b["ran"], 2);

    // DELETE — a paragraph, a table row, a picture, a comment, a section.
    m.ok("accept_changes", json!({"all": true}));
    let del = m.ok(
        "review_edit",
        json!({"tracked": false, "ops": [
            {"op": "delete_paragraph", "anchor": "Obsolete paragraph"},
            {"op": "delete_table_row", "anchor": "Office"},
            {"op": "delete_picture", "alt": "Site photo"},
            {"op": "delete_comment", "anchor": "downpipe"},
            {"op": "delete_section_break", "section": 1},
        ]}),
    );
    assert_eq!(del["applied"], 5, "{del}");
    m.ok("save_document", json!({"path": "report-2.docx"}));
    m.ok("close_document", json!({}));
    m.ok("open_document", json!({"path": "report-2.docx"}));
    let md2 = m.ok("export_markdown", json!({}))["markdown"].as_str().unwrap().to_string();
    for gone in ["Obsolete paragraph", "| Office", "Site photo", "{>>", "{++", "{--", "<!-- section break -->"] {
        assert!(!md2.contains(gone), "`{gone}` is gone:\n{md2}");
    }
    assert!(!md2.contains("and dry"), "the rejected insertion is gone");
    for kept in ["The roof is sound.", "| Store | 7 sq m |", "| Lab | 9 sq m |", "Fix the downpipe", "![Floor plan](picture:"] {
        assert!(md2.contains(kept), "`{kept}` is kept:\n{md2}");
    }
}

/// v0.7.4: `export_markdown {path}` READS the file (as render_preview does) and never writes it;
/// `out` is where the Markdown goes, and it may not name a document or the file being read.
#[test]
fn export_markdown_reads_path_and_never_writes_over_it() {
    let d = Dir::new("md-read");
    let mut m = Mcp::start(&d.0, &[]);
    m.ok("import_markdown", json!({"markdown": "# Contract\n\nThe Supplier shall deliver.\n", "cover": false, "footer": "none"}));
    m.ok("save_document", json!({"path": "contract.docx"}));
    m.ok("new_document", json!({}));
    m.ok("type_text", json!({"text": "Open one"}));
    let before = std::fs::read(d.path("contract.docx")).unwrap();
    let r = m.ok("export_markdown", json!({"path": "contract.docx"}));
    assert!(r["markdown"].as_str().unwrap().starts_with("# Contract\n\nThe Supplier shall deliver."), "{r}");
    assert_eq!(r["read"], "contract.docx");
    assert_eq!(std::fs::read(d.path("contract.docx")).unwrap(), before, "the file is byte for byte as it was");
    assert!(!d.names().iter().any(|n| n.starts_with("OLD_")), "no OLD_ copy: nothing was written over: {:?}", d.names());
    assert_eq!(m.ok("get_text", json!({}))["text"], "Open one", "the open document is untouched");
    // `out` writes the Markdown; `output` means `out` here.
    let w = m.ok("export_markdown", json!({"path": "contract.docx", "out": "contract.md"}));
    assert_eq!(w["out"], "contract.md");
    assert!(std::fs::read_to_string(d.path("contract.md")).unwrap().starts_with("# Contract"));
    m.ok("export_markdown", json!({"path": "contract.docx", "output": "copy.md"}));
    assert!(d.path("copy.md").exists());
    // Never over a document, or over the file being read.
    for out in ["contract.docx", "other.docx", "t.dotx", "m.docm"] {
        let e = m.err("export_markdown", json!({"path": "contract.docx", "out": out}), "invalid_argument");
        assert!(e["reason"].as_str().unwrap().contains("out"), "{e}");
    }
    assert_eq!(std::fs::read(d.path("contract.docx")).unwrap(), before);
    m.err("export_markdown", json!({"path": "missing.docx"}), "not_found");
}
