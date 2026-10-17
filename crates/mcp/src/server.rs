//! JSON-RPC 2.0 framing and the MCP lifecycle / tools / resources methods.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

use crate::backend::Backend;

/// The MCP revision we implement.
pub const PROTOCOL_VERSION: &str = "2025-06-18";
const SUPPORTED_VERSIONS: &[&str] = &[PROTOCOL_VERSION, "2025-03-26", "2024-11-05"];

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;
const RESOURCE_NOT_FOUND: i64 = -32002;

const INSTRUCTIONS: &str = "WordCraft is a word processor (a Microsoft Word clone). Every action is a command: find ids and \
parameters with list_commands and run them with execute (or several with batch). Positions are \
{story:\"body\", path:[block,(row,cell,block)...], off:byteOffset}. Typical flow: new_document → type_text \
\"Hello\" → execute text.newParagraph → execute para.style {style:\"Heading 1\"} → select_text {text:\"Hello\"} → \
execute format.bold → inspect_document to verify → render_page to look at the result → save {path:\"out.docx\"}.";

/// Resource URIs.
pub const DOC_URI: &str = "wordcraft://document";
pub const COMMANDS_URI: &str = "wordcraft://commands";

/// The `initialize` result (WHIQ: also answered by the runtime's main thread, which never waits
/// for the document).
pub fn initialize_result(params: &Value, describe: &str) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL_VERSION);
    let version = if SUPPORTED_VERSIONS.contains(&asked) { asked } else { PROTOCOL_VERSION };
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {}, "resources": {}},
        "serverInfo": {"name": "wordcraft", "title": "WordCraft", "version": env!("CARGO_PKG_VERSION"), "websiteUrl": "https://getartcraft.com/apps/wordcraft"},
        "instructions": format!(
            "{INSTRUCTIONS} {WHIQ_INSTRUCTIONS} Backend: {describe}. Community: https://discord.gg/artcraft · https://getartcraft.com/apps/wordcraft · https://github.com/storytold/wordcraft",
        ),
    })
}

/// WHIQ: how the contract shows to agents.
const WHIQ_INSTRUCTIONS: &str = "Errors carry code, reason and next_step. A busy server answers code busy with retry_after_ms; a long call answers with a job_id (follow it with job_status; the result is kept). Big results come trimmed with truncated and continue_from (pass it back). server_status shows memory, the open document, jobs and recovered files. Saves never replace a file silently: the old one is kept as OLD_<name> unless overwrite: true. Pass caller to sign tracked changes and comments.";

/// The `resources/list` result.
pub fn resources_list() -> Value {
    json!({"resources": [
        {"uri": DOC_URI, "name": "document", "title": "Active document", "description": "Blocks, text, formatting, sections, parts and selection of the document (document.inspect)", "mimeType": "application/json"},
        {"uri": COMMANDS_URI, "name": "commands", "title": "Commands", "description": "Every command: id, label, ribbon location, shortcut, params, enabled (engine.commands)", "mimeType": "application/json"},
    ]})
}

/// An MCP server bound to one backend.
pub struct Server {
    backend: Box<dyn Backend>,
    initialized: bool,
    /// WHIQ: limits of the contract (result size, images, memory, test hooks).
    limits: crate::whiq::Limits,
    /// WHIQ: what the runtime's main thread reads (the open document, recovered files, roots).
    shared: crate::runtime::SharedState,
    /// WHIQ: recovered work the agent hasn't been told about (eviction, or a previous process's
    /// auto-save): the next call that needs the document answers `evicted` with it.
    notice: Option<Value>,
    started: std::time::Instant,
}

fn response(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.into()}})
}

impl Server {
    pub fn new(backend: Box<dyn Backend>) -> Self {
        let shared = crate::runtime::SharedState::default();
        if let Ok(mut g) = shared.lock() {
            g.backend = backend.describe();
        }
        Self { backend, initialized: false, limits: Default::default(), shared, notice: None, started: std::time::Instant::now() }
    }

    /// WHIQ: the contract's limits.
    pub fn with_limits(mut self, limits: crate::whiq::Limits) -> Self {
        self.limits = limits;
        self
    }

    /// WHIQ: share state with the runtime (and report documents an earlier process recovered in
    /// the write root: the first call that needs the document answers `evicted` with them).
    pub fn with_shared(mut self, shared: crate::runtime::SharedState) -> Self {
        let dir = if let Ok(mut g) = shared.lock() {
            g.backend = self.backend.describe();
            g.author = self.backend.author();
            g.write_root.clone()
        } else {
            None
        };
        self.shared = shared;
        let dir = crate::recover::recovery_dir(dir.as_deref(), None);
        let found = crate::recover::read_marker(&dir);
        if !found.is_empty() {
            if let Ok(mut g) = self.shared.lock() {
                for f in &found {
                    if !g.recovered.contains(f) {
                        g.recovered.push(f.clone());
                    }
                }
            }
            self.notice = Some(json!({"reason": "an earlier server auto-saved the work", "recovered": found}));
        }
        self.publish(false);
        self
    }

    pub fn backend(&mut self) -> &mut dyn Backend {
        self.backend.as_mut()
    }

    /// Whether the client has sent `notifications/initialized`.
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// Serve newline-delimited JSON-RPC until `input` closes. Logs go to stderr only (stdout is
    /// the protocol stream).
    pub fn serve(&mut self, input: impl BufRead, mut output: impl Write) -> std::io::Result<()> {
        for line in input.lines() {
            let line = line?;
            if let Some(reply) = self.handle_line(&line) {
                output.write_all(reply.as_bytes())?;
                output.write_all(b"\n")?;
                output.flush()?;
            }
        }
        Ok(())
    }

    /// Handle one line; returns the reply line (None for notifications and blank lines).
    pub fn handle_line(&mut self, line: &str) -> Option<String> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        let reply = match serde_json::from_str::<Value>(line) {
            Ok(Value::Array(batch)) => {
                // Batches were removed in 2025-06-18; still answer older clients sensibly.
                if batch.is_empty() {
                    Some(error(Value::Null, INVALID_REQUEST, "empty batch"))
                } else {
                    let replies: Vec<Value> = batch.into_iter().filter_map(|m| self.handle(m)).collect();
                    (!replies.is_empty()).then_some(Value::Array(replies))
                }
            }
            Ok(msg) => self.handle(msg),
            Err(e) => Some(error(Value::Null, PARSE_ERROR, format!("parse error: {e}"))),
        };
        reply.map(|r| r.to_string())
    }

    /// Handle one JSON-RPC message; `None` for notifications and responses.
    pub fn handle(&mut self, msg: Value) -> Option<Value> {
        let Value::Object(o) = &msg else { return Some(error(Value::Null, INVALID_REQUEST, "message must be an object")) };
        let id = o.get("id").cloned();
        let Some(method) = o.get("method").and_then(Value::as_str) else {
            // A response to a server→client request (we send none): ignore. Anything else is invalid.
            if o.contains_key("result") || o.contains_key("error") {
                return None;
            }
            return Some(error(id.unwrap_or(Value::Null), INVALID_REQUEST, "missing `method`"));
        };
        let params = o.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = id else {
            self.notification(method);
            return None;
        };
        if !(id.is_string() || id.is_number()) {
            return Some(error(Value::Null, INVALID_REQUEST, "`id` must be a string or number"));
        }
        Some(match self.request(method, &params) {
            Ok(r) => response(id, r),
            Err((code, m)) => error(id, code, m),
        })
    }

    fn notification(&mut self, method: &str) {
        match method {
            "notifications/initialized" => self.initialized = true,
            "notifications/cancelled" | "notifications/progress" | "notifications/roots/list_changed" => {}
            other => log::debug!("ignoring notification {other}"),
        }
    }

    fn request(&mut self, method: &str, params: &Value) -> Result<Value, (i64, String)> {
        match method {
            "initialize" => Ok(initialize_result(params, &self.backend.describe())),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": crate::whiq::tool_definitions()})),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).ok_or((INVALID_PARAMS, "missing tool `name`".to_string()))?;
                let args = params.get("arguments").cloned().unwrap_or(Value::Null);
                Ok(self.call(name, &args))
            }
            "resources/list" => Ok(resources_list()),
            "resources/templates/list" => Ok(json!({"resourceTemplates": []})),
            "resources/read" => {
                let uri = params.get("uri").and_then(Value::as_str).ok_or((INVALID_PARAMS, "missing `uri`".to_string()))?;
                let v = match uri {
                    DOC_URI => self.backend.call("document.inspect", json!({})),
                    COMMANDS_URI => self.backend.call("engine.commands", json!({})),
                    _ => return Err((RESOURCE_NOT_FOUND, format!("resource not found: {uri}"))),
                }
                .map_err(|e| (INTERNAL_ERROR, e))?;
                Ok(json!({"contents": [{"uri": uri, "mimeType": "application/json", "text": serde_json::to_string_pretty(&v).unwrap_or_default()}]}))
            }
            other => Err((METHOD_NOT_FOUND, format!("method not found: {other}"))),
        }
    }
}

/// WHIQ: tools that may run while recovered work waits to be reopened (they don't need it).
const WITHOUT_DOCUMENT: &[&str] =
    &["open_document", "new_document", "import_markdown", "close_document", "server_status", "job_status", "list_commands", "parity"];

impl Server {
    /// A `tools/call` through the contract: recovered work reported first, the call, then the
    /// open document published (for auto-saves) and memory checked.
    fn call(&mut self, name: &str, args: &Value) -> Value {
        let canonical = crate::whiq::resolve_tool(name, &crate::whiq::tool_definitions()).map(|(n, _)| n).unwrap_or_else(|_| name.to_string());
        // Without the runtime (in-process use) the status tools are answered here.
        match canonical.as_str() {
            "server_status" => {
                let jobs = Vec::new();
                let st = match self.shared.lock() {
                    Ok(g) => crate::runtime::status_json(&g, jobs, Value::Null, self.started, &self.limits),
                    Err(_) => json!({}),
                };
                return crate::whiq::json_result(st, &[]);
            }
            "job_status" => {
                let e = crate::contract::ToolError::new(
                    "job_not_found",
                    "no jobs in this session",
                    "call the tool again; jobs exist only when the server runs on stdio (wordcraft-cli mcp)",
                );
                return crate::whiq::error_result(&e, &[]);
            }
            _ => {}
        }
        let mut notice_warning = None;
        if let Some(n) = self.notice.take() {
            let dir = crate::recover::recovery_dir(self.shared.lock().ok().and_then(|g| g.write_root.clone()).as_deref(), None);
            let first =
                n.pointer("/recovered/0/path").and_then(Value::as_str).or_else(|| n.get("recovered").and_then(Value::as_str)).map(str::to_string);
            let source = n.get("source").and_then(Value::as_str).map(str::to_string);
            if !WITHOUT_DOCUMENT.contains(&canonical.as_str()) {
                crate::recover::clear_marker(&dir);
                let next = match (&first, &source) {
                    (Some(p), _) => format!("your work is auto-saved at {p}: call open_document {{\"path\": \"{p}\"}}, then carry on"),
                    (None, Some(src)) => format!("the document was saved; reopen it with open_document {{\"path\": \"{src}\"}}"),
                    (None, None) => "start again with new_document or open_document".to_string(),
                };
                let reason = n.get("reason").and_then(Value::as_str).unwrap_or("the document was closed").to_string();
                let mut e = crate::contract::ToolError::new("evicted", format!("{reason}; the open document was closed"), next);
                for (k, v) in n.as_object().into_iter().flatten() {
                    e = e.with(k, v.clone());
                }
                return crate::whiq::error_result(&e, &[]);
            }
            crate::recover::clear_marker(&dir);
            notice_warning = Some(match first {
                Some(p) => format!("note: earlier work was auto-saved at {p} (open_document on it to continue it)"),
                None => "note: the previous document was closed".to_string(),
            });
        }
        let mut r = crate::whiq::call_tool(self.backend.as_mut(), name, args, &self.limits);
        if let Some(w) = notice_warning {
            add_warning(&mut r, &w);
        }
        if let Some(w) = self.publish(true) {
            add_warning(&mut r, &w);
        }
        r
    }

    /// Publish the open document for auto-saves and `server_status`; evict it when memory is past
    /// the limit (auto-saved first). Returns a note for the call's result when it was evicted.
    fn publish(&mut self, may_evict: bool) -> Option<String> {
        let snap = self.backend.snapshot()?;
        let used = crate::recover::rss_bytes().unwrap_or_else(|| crate::recover::doc_bytes(&snap.doc));
        // Only a document that holds something is evicted (a blank, untouched one frees nothing).
        let holds = snap.dirty || snap.path.is_some();
        let over = self.limits.max_memory_bytes.filter(|limit| may_evict && holds && used > *limit);
        let author = self.backend.author();
        let Some(limit) = over else {
            if let Ok(mut g) = self.shared.lock() {
                g.snapshot = Some(snap);
                g.author = author;
            }
            return None;
        };
        let reason = format!("memory: {used} bytes in use, over the {limit}-byte limit");
        let root = self.shared.lock().ok().and_then(|g| g.write_root.clone());
        let recovered = if snap.dirty {
            let dir = crate::recover::recovery_dir(root.as_deref(), Some(&snap));
            crate::recover::autosave(&snap, &dir, &reason).ok()
        } else {
            None
        };
        let source = snap.path.as_ref().map(|p| p.to_string_lossy().into_owned());
        let _ = self.backend.call("engine.execute", json!({"command": "file.new", "params": {"template": "blank"}}));
        let mut notice = json!({"reason": format!("evicted to free memory ({reason})"), "source": source});
        if let Some(p) = &recovered {
            notice["recovered"] = json!(p.to_string_lossy());
        }
        if let Ok(mut g) = self.shared.lock() {
            if let Some(p) = &recovered {
                g.recovered.push(json!({"path": p.to_string_lossy(), "reason": reason}));
            }
            g.snapshot = self.backend.snapshot();
        }
        self.notice = Some(notice);
        Some(match &recovered {
            Some(p) => format!("this call finished, then the document was evicted to free memory: it is auto-saved at {}", p.display()),
            None => "this call finished, then the document (saved, no unsaved changes) was closed to free memory".to_string(),
        })
    }
}

/// Add `w` to a tool result's warnings (structured content and its text).
fn add_warning(r: &mut Value, w: &str) {
    let Some(sc) = r.get_mut("structuredContent") else { return };
    let target = if let Some(e) = sc.get_mut("error") { e } else { sc };
    if let Some(o) = target.as_object_mut() {
        let list = o.entry("warnings").or_insert_with(|| json!([]));
        if let Some(a) = list.as_array_mut() {
            a.push(json!(w));
        }
    }
    let text = serde_json::to_string_pretty(&r["structuredContent"]).unwrap_or_default();
    if let Some(c) = r.get_mut("content").and_then(Value::as_array_mut).and_then(|c| c.iter_mut().rev().find(|c| c["type"] == "text")) {
        c["text"] = json!(text);
    }
}
