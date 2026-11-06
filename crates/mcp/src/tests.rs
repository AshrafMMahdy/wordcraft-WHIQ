//! An MCP client doing realistic tasks through the protocol only, checking results via MCP.

use serde_json::{Value, json};

use crate::{Headless, Server};

fn call(s: &mut Server, id: u64, method: &str, params: Value) -> Value {
    let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
    let r = s.handle_line(&line).expect("reply");
    serde_json::from_str(&r).expect("json")
}

fn tool(s: &mut Server, name: &str, args: Value) -> Value {
    let r = call(s, 9, "tools/call", json!({"name": name, "arguments": args}));
    let res = &r["result"];
    assert_eq!(res["isError"], false, "{name}: {res}");
    let text = res["content"][0]["text"].as_str().unwrap_or("null");
    serde_json::from_str(text).unwrap_or(Value::String(text.to_string()))
}

#[test]
fn lifecycle_and_tools() {
    let mut s = Server::new(Box::new(Headless::default()));
    let init = call(&mut s, 1, "initialize", json!({"protocolVersion": "2025-06-18"}));
    assert_eq!(init["result"]["serverInfo"]["name"], "wordcraft");
    assert!(s.handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none());
    let tools = call(&mut s, 2, "tools/list", json!({}));
    assert!(tools["result"]["tools"].as_array().unwrap().len() >= 12);
    let bad = call(&mut s, 3, "nope", json!({}));
    assert!(bad["error"].is_object());
    assert!(s.handle_line("{not json").unwrap().contains("parse error"));
}

#[test]
fn agent_writes_a_formatted_document() {
    let mut s = Server::new(Box::new(Headless::default()));
    tool(&mut s, "new_document", json!({}));
    tool(&mut s, "type_text", json!({"text": "Quarterly Report"}));
    tool(&mut s, "execute", json!({"command": "para.style", "params": {"style": "Heading 1"}}));
    tool(&mut s, "execute", json!({"command": "text.newParagraph"}));
    tool(&mut s, "type_text", json!({"text": "Sales grew strongly this quarter.\nCosts fell.", "paragraphs": true}));
    tool(&mut s, "select_text", json!({"text": "strongly"}));
    tool(&mut s, "execute", json!({"command": "format.bold"}));
    tool(&mut s, "batch", json!({"commands": [{"command": "caret.docEnd"}, {"command": "insert.table", "params": {"rows": 2, "cols": 2}}]}));
    let doc = tool(&mut s, "inspect_document", json!({}));
    let blocks = doc["blocks"].as_array().unwrap();
    assert_eq!(blocks[0]["style"], "Heading1");
    assert_eq!(blocks[0]["text"], "Quarterly Report");
    let runs = blocks[1]["runs"].as_array().unwrap();
    assert!(runs.iter().any(|r| r["props"]["bold"] == true));
    assert!(blocks.iter().any(|b| b["type"] == "table"));
    let text = tool(&mut s, "get_text", json!({}));
    assert!(text["text"].as_str().unwrap().contains("Costs fell."));
    let r = call(&mut s, 10, "tools/call", json!({"name": "render_page", "arguments": {"page": 1, "scale": 0.5}}));
    assert_eq!(r["result"]["content"][0]["type"], "image");
    let r = call(&mut s, 11, "tools/call", json!({"name": "screenshot", "arguments": {}}));
    assert_eq!(r["result"]["isError"], true);
    let p = tool(&mut s, "parity", json!({}));
    assert!(p["percent"].as_f64().unwrap() > 50.0);
}

#[test]
fn resources() {
    let mut s = Server::new(Box::new(Headless::default()));
    let l = call(&mut s, 1, "resources/list", json!({}));
    assert_eq!(l["result"]["resources"].as_array().unwrap().len(), 2);
    let r = call(&mut s, 2, "resources/read", json!({"uri": "wordcraft://document"}));
    assert!(r["result"]["contents"][0]["text"].as_str().unwrap().contains("blocks"));
}

/// A backend standing in for a running app: it answers `ui.render` and `ui.screenshot` with a
/// PNG in the reply and records what it was asked.
struct FakeApp {
    calls: std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>>,
}

impl crate::backend::Backend for FakeApp {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.calls.lock().unwrap().push((method.to_string(), params.clone()));
        match method {
            "ui.render" | "ui.screenshot" => Ok(json!({"png": wordcraft_engine::cmd::insert::base64_encode(b"\x89PNG"), "width": 1, "height": 1})),
            other => Err(format!("unexpected {other}")),
        }
    }
    fn has_ui(&self) -> bool {
        true
    }
    fn describe(&self) -> String {
        "fake app".into()
    }
}

/// With an app attached, pictures travel inside the reply: the bridge never writes a file with a
/// predictable name into the shared temporary folder (another user could plant a link there).
#[test]
fn render_and_screenshot_through_the_app_use_no_temp_file() {
    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut s = Server::new(Box::new(FakeApp { calls: calls.clone() }));
    for (tool, method) in [("render_page", "ui.render"), ("screenshot", "ui.screenshot")] {
        let r = call(&mut s, 10, "tools/call", json!({"name": tool, "arguments": {"page": 1}}));
        let content = &r["result"]["content"];
        assert_eq!(r["result"]["isError"], false, "{tool}: {r}");
        assert_eq!(content[0]["type"], "image", "{tool}");
        assert_eq!(content[0]["data"], wordcraft_engine::cmd::insert::base64_encode(b"\x89PNG"), "{tool}");
        let (m, params) = calls.lock().unwrap().pop().expect("one call");
        assert_eq!(m, method);
        assert!(params.get("path").is_none(), "{tool} asked for a file: {params}");
    }
}

/// #40: a headless session never reads or writes the person's custom dictionary file: an agent's
/// Add to Dictionary stays in memory (only the desktop app saves the dictionary).
#[test]
fn headless_sessions_keep_the_dictionary_in_memory() {
    let mut h = Headless::default();
    assert!(h.session.user_dict_path.is_none());
    let mut s = Server::new(Box::new(Headless::default()));
    tool(&mut s, "execute", json!({"command": "review.addToDictionary", "params": {"word": "zxqmcpword"}}));
    h.session.run("review.addToDictionary", &json!({"word": "zxqmcpword2"})).expect("added");
    assert!(h.session.user_dict_path.is_none());
}

/// A backend whose paragraph break is refused while text insertion would succeed.
struct RefusesParagraph {
    calls: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl crate::backend::Backend for RefusesParagraph {
    fn call(&mut self, _method: &str, params: Value) -> Result<Value, String> {
        let command = params["command"].as_str().unwrap_or("").to_string();
        self.calls.lock().unwrap().push(command.clone());
        match command.as_str() {
            "text.newParagraph" => Err("paragraph break refused".into()),
            _ => Ok(json!({"ok": true})),
        }
    }
    fn has_ui(&self) -> bool {
        false
    }
    fn describe(&self) -> String {
        "refuses paragraphs".into()
    }
}

/// #1356: a refused paragraph break is reported and the next line is not typed after it.
#[test]
fn type_text_stops_at_a_refused_paragraph_break() {
    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut s = Server::new(Box::new(RefusesParagraph { calls: calls.clone() }));
    let r = call(&mut s, 11, "tools/call", json!({"name": "type_text", "arguments": {"text": "one\ntwo", "paragraphs": true}}));
    assert_eq!(r["result"]["isError"], true, "{r}");
    assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("paragraph break refused"), "{r}");
    assert_eq!(*calls.lock().unwrap(), vec!["text.insert", "text.newParagraph"]);
}

/// A headless session whose `nth` run of `command` fails (or, with `panics`, runs and then
/// panics): a call that breaks half way, for "Undo the call".
struct Faulty {
    inner: Headless,
    command: &'static str,
    nth: usize,
    seen: usize,
    panics: bool,
}

impl Faulty {
    fn server(command: &'static str, nth: usize, panics: bool) -> Server {
        Server::new(Box::new(Faulty { inner: Headless::default(), command, nth, seen: 0, panics }))
    }
}

impl crate::backend::Backend for Faulty {
    #[allow(clippy::panic)]
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let hit = params["command"] == self.command && {
            self.seen += 1;
            self.seen == self.nth
        };
        if hit && !self.panics {
            return Err(format!("{} refused", self.command));
        }
        let r = self.inner.call(method, params);
        if hit {
            std::panic::panic_any("half way");
        }
        r
    }
    fn has_ui(&self) -> bool {
        false
    }
    fn describe(&self) -> String {
        "faulty headless session".into()
    }
    fn snapshot(&mut self) -> Option<crate::recover::Snapshot> {
        self.inner.snapshot()
    }
    fn author(&self) -> Option<String> {
        self.inner.author()
    }
    fn begin_call(&mut self) {
        self.inner.begin_call()
    }
    fn rollback_call(&mut self) -> bool {
        self.inner.rollback_call()
    }
    fn end_call(&mut self) {
        self.inner.end_call()
    }
    fn has_handles(&self) -> bool {
        self.inner.has_handles()
    }
    fn active_document(&self) -> Option<String> {
        self.inner.active_document()
    }
    fn select_document(&mut self, handle: &str) -> Result<(), Vec<String>> {
        self.inner.select_document(handle)
    }
    fn open_slot(&mut self) {
        self.inner.open_slot()
    }
    fn end_open(&mut self, opened: bool) {
        self.inner.end_open(opened)
    }
}

fn tool_err(s: &mut Server, name: &str, args: Value) -> Value {
    let r = call(s, 12, "tools/call", json!({"name": name, "arguments": args}));
    assert_eq!(r["result"]["isError"], true, "{name} should fail: {r}");
    r["result"]["structuredContent"]["error"].clone()
}

/// The call panics half way (in MCP code, not in an engine command): what comes back to the
/// runtime, which answers it.
fn panicked(s: &mut Server, name: &str, args: Value) -> crate::whiq::Panicked {
    let line = json!({"jsonrpc": "2.0", "id": 13, "method": "tools/call", "params": {"name": name, "arguments": args}}).to_string();
    let p = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.handle_line(&line))).expect_err("the call panics");
    *p.downcast::<crate::whiq::Panicked>().expect("the call's panic, passed on")
}

fn text(s: &mut Server) -> String {
    tool(s, "get_text", json!({}))["text"].as_str().unwrap_or("").trim_end().to_string()
}

/// "Undo the call": type_text {paragraphs: true} that fails on its third line types nothing; the
/// selection and the undo history are as before it.
#[test]
fn type_text_that_fails_half_way_types_nothing() {
    // text.insert runs: 1 "Before", 2 "one", 3 "two", 4 "three" (refused).
    let mut s = Faulty::server("text.insert", 4, false);
    tool(&mut s, "type_text", json!({"text": "Before"}));
    let e = tool_err(&mut s, "type_text", json!({"text": "one\ntwo\nthree", "paragraphs": true}));
    assert_eq!((e["rolled_back"].as_bool(), e["typed_paragraphs"].as_u64()), (Some(true), Some(0)), "{e}");
    assert!(e["next_step"].as_str().unwrap().contains("nothing of this call was applied"), "{e}");
    assert!(e["reason"].as_str().unwrap().contains("paragraph 3 of 3"), "{e}");
    assert_eq!(text(&mut s), "Before");
    // The call left no undo step: one undo takes back the typing before it, redo brings it back.
    tool(&mut s, "execute", json!({"command": "edit.undo"}));
    assert_eq!(text(&mut s), "");
    tool(&mut s, "execute", json!({"command": "edit.redo"}));
    assert_eq!(text(&mut s), "Before");
}

/// "Undo the call": a panic half way through a call leaves the live document as it was before
/// the call (and the caller's name is put back), and says so to the runtime.
#[test]
fn a_panic_half_way_leaves_the_document_as_it_was() {
    // text.insert runs: 1 "Before", 2 "one", 3 "two" (runs, then panics).
    let mut s = Faulty::server("text.insert", 3, true);
    tool(&mut s, "type_text", json!({"text": "Before"}));
    let author = s.backend().author();
    let p = panicked(&mut s, "type_text", json!({"text": "one\ntwo", "paragraphs": true, "caller": "Reviewer"}));
    assert!(p.undone);
    assert_eq!(p.message, "half way");
    assert_eq!(s.backend().author(), author);
    assert_eq!(text(&mut s), "Before");
    tool(&mut s, "type_text", json!({"text": "!"}));
    assert_eq!(text(&mut s), "Before!");
}

/// "Undo the call": an open that panics half way brings back the document it set aside.
#[test]
fn an_open_that_panics_brings_back_the_open_document() {
    let mut s = Faulty::server("file.new", 1, true);
    tool(&mut s, "type_text", json!({"text": "Keep"}));
    let active = s.backend().active_document();
    assert!(panicked(&mut s, "new_document", json!({})).undone);
    assert_eq!(s.backend().active_document(), active);
    assert_eq!(text(&mut s), "Keep");
}

/// "Undo the call": an open into the blank document that panics after reading the file leaves
/// the blank, untouched document (no file behind it: a save can't overwrite the file with it).
#[test]
fn an_open_into_the_blank_document_that_panics_leaves_it_blank() {
    let dir = std::env::temp_dir().join(format!("wordcraft-mcp-undo-open-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("on-disk.docx");
    let mut w = Server::new(Box::new(Headless::default()));
    tool(&mut w, "type_text", json!({"text": "On disk"}));
    tool(&mut w, "save_document", json!({"path": path.to_string_lossy(), "overwrite": true}));
    let mut s = Faulty::server("file.open", 1, true);
    let active = s.backend().active_document();
    assert!(panicked(&mut s, "open_document", json!({"path": path.to_string_lossy()})).undone);
    assert_eq!(s.backend().active_document(), active);
    assert_eq!(text(&mut s), "");
    let snap = s.backend().snapshot().unwrap();
    assert!(snap.path.is_none() && !snap.dirty, "{:?}", snap.path);
    let _ = std::fs::remove_dir_all(&dir);
}

/// "Undo the call": compare_documents whose change report fails takes the comparison back.
#[test]
fn a_comparison_without_its_report_is_taken_back() {
    let dir = std::env::temp_dir().join(format!("wordcraft-mcp-undo-compare-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let revised = dir.join("revised.docx");
    let mut w = Server::new(Box::new(Headless::default()));
    tool(&mut w, "type_text", json!({"text": "The revised text"}));
    tool(&mut w, "save_document", json!({"path": revised.to_string_lossy(), "overwrite": true}));
    let mut s = Faulty::server("review.changeReport", 1, false);
    tool(&mut s, "type_text", json!({"text": "The original text"}));
    let before = tool(&mut s, "inspect_document", json!({}));
    let e = tool_err(&mut s, "compare_documents", json!({"revised": revised.to_string_lossy()}));
    assert_eq!(e["rolled_back"], true, "{e}");
    assert!(e["next_step"].as_str().unwrap().contains("nothing of this call was applied"), "{e}");
    assert_eq!(tool(&mut s, "inspect_document", json!({})), before);
    let _ = std::fs::remove_dir_all(&dir);
}

/// "Undo the call": a save that fails after stamping the caller as the document's creator takes
/// the stamp back.
#[test]
fn a_failed_save_takes_back_the_creator_it_stamped() {
    let mut s = Faulty::server("file.save", 1, false);
    tool(&mut s, "type_text", json!({"text": "Unsaved"}));
    let e = tool_err(&mut s, "save_document", json!({"path": "never-written.docx", "caller": "Reviewer"}));
    assert_eq!(e["rolled_back"], true, "{e}");
    let p = tool(&mut s, "execute", json!({"command": "file.properties"}));
    assert!(p["creator"].as_str().is_none_or(str::is_empty), "{p}");
    assert_eq!(text(&mut s), "Unsaved");
}

/// A batch keeps its rule: a failed command keeps the commands before it.
#[test]
fn a_failed_batch_command_keeps_the_ones_before_it() {
    let mut s = Faulty::server("text.insert", 2, false);
    let e = tool_err(
        &mut s,
        "batch",
        json!({"commands": [{"command": "text.insert", "params": {"text": "kept"}}, {"command": "text.insert", "params": {"text": " lost"}}]}),
    );
    assert_eq!(e["failed_at"], 1, "{e}");
    assert!(e.get("rolled_back").is_none(), "{e}");
    assert_eq!(text(&mut s), "kept");
}
