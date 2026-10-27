//! WHIQ: the tool layer agents call. It wraps the upstream tools (`tools.rs`) with the "no dead
//! stops" contract (`contract.rs`):
//!
//! - every result carries `structuredContent`; every error `{code, reason, next_step}`;
//! - tool names, command ids and arguments are matched forgivingly, with warnings;
//! - oversized results are trimmed with `truncated` / `continue_from`;
//! - saves never silently replace a file (`OLD_<name>` unless `overwrite: true`);
//! - `caller` names who tracked changes and comments are by, for one call;
//! - annotations say what each tool does (`readOnlyHint` only on reads, `destructiveHint` on
//!   tools that can delete or overwrite, `idempotentHint` where true);
//! - new tools: `server_status`, `job_status` (answered by the runtime while a call runs),
//!   `render_page` with a DPI, a page range and markup, `export_pdf`, `close_document`.

use serde_json::{Map, Value, json};
use wordcraft_engine::file_access::{Overwrite, with_overwrite};

use crate::backend::Backend;
use crate::contract::{self, ToolError, closest, code, normalise_args, trim, unique_match};
use crate::tools::{self, ToolResult};

/// Limits a call works within (set from `wordcraft-cli mcp` flags).
#[derive(Clone, Debug)]
pub struct Limits {
    /// Largest JSON result before it is trimmed.
    pub max_result_bytes: usize,
    /// Largest total of PNG data (base64) one `render_page` call returns.
    pub max_image_bytes: usize,
    /// Hidden test tools (`__sleep`, `__panic`) are available (`WORDCRAFT_MCP_TEST_HOOKS=1`).
    pub test_hooks: bool,
    /// Above this much memory (resident, or the document's own estimate where the platform
    /// doesn't tell), the open document is auto-saved and closed after the call (evicted).
    pub max_memory_bytes: Option<u64>,
    /// A save over an existing file keeps it as `OLD_<name>` unless the call passes `overwrite:
    /// true`. Off (`--automation-no-old-copies`): saves replace files, a supervisor keeps backups.
    pub keep_old: bool,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_result_bytes: 100_000, max_image_bytes: 16 * 1024 * 1024, test_hooks: false, max_memory_bytes: None, keep_old: true }
    }
}

/// What a tool does, for its annotations.
struct Ann {
    read_only: bool,
    destructive: bool,
    idempotent: bool,
}

const READ: Ann = Ann { read_only: true, destructive: false, idempotent: true };
const STATE: Ann = Ann { read_only: false, destructive: false, idempotent: true };
const CHANGES: Ann = Ann { read_only: false, destructive: true, idempotent: false };
const REPLACES: Ann = Ann { read_only: false, destructive: true, idempotent: true };

fn ann(name: &str) -> Ann {
    match name {
        "list_commands" | "get_text" | "inspect_document" | "render_page" | "parity" | "screenshot" | "ui_inspect" | "server_status"
        | "job_status" | "list_jobs" | "list_changes" | "find_anchor" | "roundtrip_check" => READ,
        // Stopping a call leaves the document as it was before it: asking twice is asking once.
        // Evicting auto-saves unsaved work first (nothing is lost) and leaves a blank document.
        "select_text" | "cancel" | "evict" => STATE,
        // Opening or starting a document replaces the open one (auto-saved first when it has
        // unsaved changes): the same arguments give the same result.
        "new_document" | "open_document" | "import_markdown" | "close_document" => REPLACES,
        // Writing files: never silently over another one, but `overwrite: true` can.
        "save_document" | "export_pdf" => CHANGES,
        // Review edits are tracked (reversible), but a batch can delete text and paragraphs;
        // accepting or rejecting a change is final; a comparison rewrites the open document.
        "review_edit" | "accept_changes" | "reject_changes" | "compare_documents" => CHANGES,
        // A theme restyles the document: the same theme twice is the same document.
        "apply_theme" => REPLACES,
        // Anything else edits the document and can delete or replace content.
        _ => CHANGES,
    }
}

/// The files a tool reads, for a supervisor that stages them (`_meta["whiq/files"]`): `read`
/// lists the arguments that name existing files the call reads; `opens` says the call opens or
/// starts the document the server works on (replacing the open one), which is the file of the
/// first `read` argument when one is given. Tools that read no file have no entry.
fn files_meta(name: &str) -> Option<Value> {
    let (read, opens): (&[&str], bool) = match name {
        "open_document" | "import_markdown" => (&["path"], true),
        "new_document" => (&[], true),
        "roundtrip_check" => (&["path"], false),
        "compare_documents" => (&["revised", "original"], false),
        _ => return None,
    };
    Some(json!({"read": read, "opens": opens}))
}

fn s(desc: &str) -> Value {
    json!({"type": "string", "description": desc})
}
fn n(desc: &str) -> Value {
    json!({"type": "number", "description": desc})
}
fn b(desc: &str) -> Value {
    json!({"type": "boolean", "description": desc})
}

/// WHIQ: tools that open or start a document: the open one stays open beside it (unless blank and
/// untouched), the new one becomes active, and the answer names its `document` handle.
pub const OPENS_DOCUMENT: &[&str] = &["open_document", "new_document", "import_markdown"];

/// WHIQ: tools that act on no document (no `document` argument).
const NO_DOCUMENT: &[&str] = &[
    "server_status",
    "job_status",
    "list_jobs",
    "cancel",
    "evict",
    "list_commands",
    "parity",
    "screenshot",
    "click",
    "key",
    "ui_inspect",
    "roundtrip_check",
];

/// Arguments every tool takes.
fn common_props(name: &str) -> Vec<(&'static str, Value)> {
    let mut v = vec![
        ("caller", s("Who is calling: the author of tracked changes and comments made by this call (default: --author)")),
        ("async", b("Run as a job: answer at once with a job_id; job_status gives progress and the result (also read as `background`)")),
        ("continue_from", n("Where to resume a trimmed result (its continue_from)")),
        ("max_bytes", n("Largest result to return before trimming (default --max-result-bytes)")),
    ];
    if !NO_DOCUMENT.contains(&name) && !OPENS_DOCUMENT.contains(&name) {
        v.push((
            "document",
            s("The document to act on: the `document` handle open_document / new_document / import_markdown returned (default: the active one)"),
        ));
    }
    if matches!(name, "save_document" | "export_pdf" | "execute" | "batch") || name.starts_with("review") {
        v.push(("overwrite", b("Replace an existing file at the path. Without it the existing file is kept as OLD_<name> and the response says so")));
    }
    v
}

fn def(name: &str, title: &str, desc: &str, props: Value, req: &[&str]) -> Value {
    json!({"name": name, "title": title, "description": desc, "inputSchema": {"type": "object", "properties": props, "required": req}})
}

/// The tools WHIQ adds or redefines (by name, replacing an upstream one of the same name).
fn own_tools() -> Vec<Value> {
    vec![
        def(
            "server_status",
            "Server status",
            "Memory, the open document (name, path, unsaved changes), running and finished jobs, recovered files, roots, author and version. Answered even while another call runs.",
            json!({}),
            &[],
        ),
        def(
            "job_status",
            "Job status",
            "Progress of a job (a call that took long, ran with async: true, or opened a big file) — status, progress 0..1, stage, steps and the stages passed — and, once done, its result (or error). Results stay available after the client times out. Without job_id: every job, like list_jobs.",
            json!({"job_id": s("The job_id a call returned")}),
            &[],
        ),
        def(
            "list_jobs",
            "List jobs",
            "Every job this server knows (running and finished, newest 64): job_id, tool, status, progress, stage, elapsed time.",
            json!({}),
            &[],
        ),
        def(
            "cancel",
            "Cancel",
            "Stop the running call (or job `job_id`) at its next safe point. The document is put back as it was before the call (a review edit batch, a batch of commands, a multi-paragraph type_text, a Markdown import: nothing of it stays; a PDF export writes no file); a page range being rendered answers with continue_from. The stopped call answers with code `cancelled` (job status `cancelled`, for a job). queued: true also drops the calls waiting behind it. Answered even while the call runs.",
            json!({"job_id": s("The job to stop (default: the running call)"), "queued": b("Also drop the calls waiting behind the running one (each answers `cancelled`)")}),
            &[],
        ),
        def(
            "evict",
            "Evict",
            "For a supervisor: auto-save the open document's unsaved work to a recovered file and close it (a blank document takes its place). The next call that needs it answers `evicted` with the file to reopen.",
            json!({"reason": s("Why (shown to the agent)")}),
            &[],
        ),
        def(
            "render_page",
            "Render page",
            "Render pages to PNG to look at the result: one page, or a range (\"2-4,7\"), at a DPI, with or without markup (tracked changes and comment balloons, as Print Markup prints them).",
            json!({
                "page": n("1-based page (default 1)"),
                "pages": s("Page range instead of one page, e.g. \"1-3,5\" or \"4-\""),
                "dpi": n("Resolution (default 96; 72 = one pixel per point)"),
                "scale": n("Pixels per point (overrides dpi)"),
                "markup": b("true: Print Markup (tracked changes marked up, comments in balloons beside the page); false (default): the printed page"),
            }),
            &[],
        ),
        def(
            "export_pdf",
            "Export PDF",
            "Write the document (or some pages) as PDF, with or without markup. Fonts are embedded as subsets; Arabic and other complex scripts are shaped.",
            json!({
                "path": s("Output .pdf path (relative paths are inside the write root)"),
                "pages": s("Page range, e.g. \"1-3,5\" (default all)"),
                "markup": b("Print Markup: tracked changes marked up and comments beside each page (default false)"),
            }),
            &["path"],
        ),
        def(
            "import_markdown",
            "Import Markdown",
            "Start a document from Markdown with the platform's shapes: headings 1–4, tables (header row shaded, bold, repeated), pictures by path, callouts (> [!NOTE], !!! tip, :::warning), numbered and bulleted lists, code blocks, page breaks (\\pagebreak, <!-- pagebreak -->), a table of contents ([TOC]), a cover block (front matter title, subtitle, date, author), headers and footers with page numbers ({page} of {pages}). The theme (fonts, colours, logo) is applied as one style batch. Replaces the open document. For user text that must stay exactly as typed, pass `blocks` instead of Markdown: their text is literal (no Markdown is read; leading spaces, line breaks and every character are kept).",
            json!({
                "markdown": s("The Markdown text"),
                "path": s("A .md file instead (pictures are read relative to it)"),
                "blocks": {"type": "array", "items": {"type": "object"}, "description": "Instead of Markdown, the document as blocks whose text is taken literally (\\n is a line break): {type: \"paragraph\", text, style?: name, heading?: 1-6} or {runs: [{text, bold?, italic?, underline?, strike?, color?, size?, font?}]}; {type: \"heading\", level, text}; {type: \"table\", rows: [[cell text or {text, runs}]], header?: true (the first row: shaded, bold, repeated)}; {type: \"page_break\"}; a string is a paragraph"},
                "theme": {"type": "object", "description": "{fonts: {body, heading}, colors: {text, primary, accent, tableHeader, tableHeaderText}, logo: path, size: pt}"},
                "toc": b("Add a table of contents at the top, after the title page when there is one (or where a [TOC] line is)"),
                "cover": b("Cover block from title/subtitle/date/author (default when there is a title)"),
                "title": s("Cover title (else front matter)"),
                "subtitle": s("Cover subtitle"),
                "date": s("Cover date"),
                "author": s("Cover author (also the document's author)"),
                "header": s("Header text; {page} {pages} {title} {date}"),
                "footer": s("Footer text (default \"Page {page} of {pages}\"; \"none\" for none)"),
            }),
            &[],
        ),
        def(
            "apply_theme",
            "Apply theme",
            "Apply a theme to the open document as one style batch: body and heading fonts, text, heading and accent colours, table header colours, a logo in the header.",
            json!({"theme": {"type": "object", "description": "{fonts: {body, heading}, colors: {text, primary, accent, tableHeader, tableHeaderText}, logo: path, size: pt}"}}),
            &["theme"],
        ),
        def(
            "roundtrip_check",
            "Round-trip check",
            "Open and save a Word file in memory (without touching the open document or the file) and report whether anything would be dropped: parts, relationships, elements of the body, headers, footers, notes and comments, paragraph text. Use it on a customer's file before editing it.",
            json!({"path": s("The .docx, .docm, .dotx or .dotm file")}),
            &["path"],
        ),
        def(
            "close_document",
            "Close document",
            "Close an open document (by its `document` handle, default the active one) to free its memory; another open document (or a blank one) becomes active. A document with unsaved changes stays open unless discard: true. The other open documents are untouched.",
            json!({"discard": b("Close even with unsaved changes (they are lost). Default false")}),
            &[],
        ),
    ]
}

/// Every tool with its schema and annotations: the upstream tools (with WHIQ's fixes) and WHIQ's.
pub fn tool_definitions() -> Value {
    let own = own_tools();
    let mut out: Vec<Value> = Vec::new();
    let upstream = match tools::tool_definitions() {
        Value::Array(v) => v,
        _ => Vec::new(),
    };
    let own_names: Vec<String> = own.iter().filter_map(|t| t["name"].as_str().map(str::to_string)).collect();
    for t in upstream.into_iter().filter(|t| !own_names.iter().any(|n| Some(n.as_str()) == t["name"].as_str())) {
        out.push(t);
    }
    out.extend(own);
    out.extend(crate::whiq_review::tool_definitions());
    for t in out.iter_mut() {
        let name = t["name"].as_str().unwrap_or("").to_string();
        let title = t["title"].as_str().unwrap_or(&name).to_string();
        if let Some(props) = t.pointer_mut("/inputSchema/properties").and_then(Value::as_object_mut) {
            for (k, v) in common_props(&name) {
                props.entry(k).or_insert(v);
            }
        }
        // `save_document` writes files: say so, and how existing ones are treated.
        if name == "save_document"
            && let Some(p) = t.pointer_mut("/inputSchema/properties").and_then(Value::as_object_mut)
        {
            p.insert("format".into(), s("\"wordml2003\" or \"flatopc\" for an .xml file"));
            p.insert("markup".into(), b("Print Markup in a PDF, PNG, HTML, Markdown or text copy"));
        }
        let a = ann(&name);
        t["annotations"] = json!({"title": title, "readOnlyHint": a.read_only, "destructiveHint": a.destructive, "idempotentHint": a.idempotent, "openWorldHint": false});
        if let Some(files) = files_meta(&name) {
            t["_meta"] = json!({"whiq/files": files});
        }
    }
    Value::Array(out)
}

/// Short names agents use for tools.
const ALIASES: &[(&str, &str)] = &[
    ("open", "open_document"),
    ("save", "save_document"),
    ("new", "new_document"),
    ("close", "close_document"),
    ("render", "render_page"),
    ("status", "server_status"),
    ("text", "get_text"),
    ("inspect", "inspect_document"),
    ("type", "type_text"),
    ("select", "select_text"),
    ("export", "export_pdf"),
    ("pdf", "export_pdf"),
    ("job", "job_status"),
    ("jobs", "list_jobs"),
    ("stop", "cancel"),
    ("commands", "list_commands"),
    ("run", "execute"),
];

/// The tool `name` means: exact, or in another spelling or case, or an alias (with a warning).
pub fn resolve_tool(name: &str, defs: &Value) -> Result<(String, Option<String>), ToolError> {
    let names: Vec<&str> = defs.as_array().map(|v| v.iter().filter_map(|t| t["name"].as_str()).collect()).unwrap_or_default();
    if names.contains(&name) {
        return Ok((name.to_string(), None));
    }
    if let Some(m) = unique_match(name, names.iter().copied()) {
        return Ok((m.to_string(), Some(format!("tool `{name}` read as `{m}`"))));
    }
    if let Some((_, to)) = ALIASES.iter().find(|(a, _)| contract::norm(a) == contract::norm(name)) {
        return Ok(((*to).to_string(), Some(format!("tool `{name}` read as `{to}`"))));
    }
    let near = closest(name, names.iter().copied(), 5);
    Err(ToolError::new(
        "not_found",
        format!("no tool `{name}`"),
        if near.is_empty() { "call tools/list for the tools".to_string() } else { format!("call one of: {}", near.join(", ")) },
    )
    .with("closest", json!(near)))
}

/// The JSON a tool result's first text block carries (the result itself).
fn result_json(r: &ToolResult) -> Value {
    let text = r.content.iter().find_map(|c| c.get("text").and_then(Value::as_str)).unwrap_or("");
    serde_json::from_str(text).unwrap_or_else(|_| json!({"text": text}))
}

/// An error result in the contract's shape: the error as JSON text and as structured content.
pub fn error_result(e: &ToolError, warnings: &[String]) -> Value {
    let err = e.to_json();
    let mut body = json!({"error": err});
    let mut text = e.text();
    if !warnings.is_empty() {
        body["warnings"] = json!(warnings);
        text.push_str(&format!("\nwarnings: {}", warnings.join("; ")));
    }
    json!({"content": [{"type": "text", "text": text}], "isError": true, "structuredContent": body})
}

/// A successful JSON result: `v` (an object; other values under `result`) as text and as
/// structured content, with warnings and notes added.
pub fn json_result(v: Value, warnings: &[String]) -> Value {
    let mut obj = match v {
        Value::Object(m) => m,
        other => {
            let mut m = Map::new();
            m.insert("result".into(), other);
            m
        }
    };
    if !warnings.is_empty() {
        obj.insert("warnings".into(), json!(warnings));
    }
    let v = Value::Object(obj);
    json!({"content": [{"type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default()}], "isError": false, "structuredContent": v})
}

/// What a tool produced.
pub enum Out {
    Json(Value),
    /// Images (base64 PNG) and the summary that goes with them.
    Images(Vec<String>, Value),
}

fn exec(b: &mut dyn Backend, id: &str, params: Value) -> Result<Value, String> {
    b.call("engine.execute", json!({"command": id, "params": params}))
}

/// Every command id (for matching a misspelt one).
fn command_ids(b: &mut dyn Backend) -> Vec<String> {
    match b.call("engine.commands", json!({})) {
        Ok(Value::Array(v)) => v.iter().filter_map(|c| c.get("id").and_then(Value::as_str).map(str::to_string)).collect(),
        _ => Vec::new(),
    }
}

/// Run command `id`, matching a misspelt id (case, `_`/`-`) to the one it means.
fn exec_forgiving(b: &mut dyn Backend, id: &str, params: Value, warnings: &mut Vec<String>) -> Result<Value, ToolError> {
    match exec(b, id, params.clone()) {
        Err(e) if e.starts_with("unknown command") => {
            let ids = command_ids(b);
            if let Some(m) = unique_match(id, ids.iter().map(String::as_str)).map(str::to_string) {
                warnings.push(format!("command `{id}` read as `{m}`"));
                return exec(b, &m, params).map_err(|e| contract::classify(&e, "execute", &json!({"command": m}), &ids));
            }
            Err(contract::classify(&e, "execute", &json!({"command": id}), &ids))
        }
        Err(e) => Err(contract::classify(&e, "execute", &json!({"command": id, "path": params.get("path")}), &[])),
        Ok(v) => Ok(v),
    }
}

/// `Some(true)` when the first letter of `text` is right-to-left, `Some(false)` left-to-right,
/// `None` without letters.
pub fn first_strong(text: &str) -> Option<bool> {
    text.chars().any(|c| c.is_alphabetic() || c == '\u{200E}' || c == '\u{200F}').then(|| wordcraft_doc::bidi::first_strong_is_rtl(text))
}

/// The pages `spec` names in a document of `total` pages (1-based, in order, no repeats).
fn page_list(spec: &str, total: usize) -> Result<Vec<usize>, ToolError> {
    wordcraft_engine::print::parse_page_range(spec, total).map(|v| v.into_iter().map(|p| p + 1).collect()).map_err(|e| {
        ToolError::new("out_of_range", format!("pages \"{spec}\": {e}"), format!("ask for pages within 1–{total}, e.g. \"1-{}\"", total.min(3)))
    })
}

/// WHIQ: the printed page count, counted holding a few pages at a time and remembered for the
/// revision (`document.layout` laid the whole document out and listed every page). A connected
/// app without the command falls back to that.
fn page_count(b: &mut dyn Backend, markup: bool) -> usize {
    if let Ok(v) = exec(b, "document.pageCount", json!({"markup": markup}))
        && let Some(n) = v.get("pages").and_then(Value::as_u64)
    {
        return (n as usize).max(1);
    }
    exec(b, "document.layout", json!({})).ok().and_then(|v| v.get("pages").and_then(Value::as_array).map(Vec::len)).unwrap_or(1).max(1)
}

/// WHIQ: trim a window the engine already started at `from` (inspect_document): the
/// continuation is absolute, and a window that fits but is not the last says where the next
/// one starts.
fn trim_window(v: Value, from: usize, max: usize) -> Value {
    let next_window = v.get("nextOffset").and_then(Value::as_u64);
    let mut t = trim(v, 0, max);
    if t["truncated"] == true {
        let rel = t["continue_from"].as_u64().unwrap_or(0) as usize;
        let n = from + rel;
        t["continue_from"] = json!(n);
        t["next_step"] = json!(format!("call the same tool again with continue_from: {n} for the rest"));
    } else if let Some(n) = next_window {
        t["truncated"] = json!(true);
        t["continue_from"] = json!(n);
        t["next_step"] = json!(format!("call the same tool again with continue_from: {n} for the rest"));
    }
    t
}

/// Run one tool (by its resolved name) on normalised arguments.
fn run(b: &mut dyn Backend, name: &str, a: &Value, limits: &Limits, warnings: &mut Vec<String>) -> Result<Out, ToolError> {
    let st = |k: &str| a.get(k).and_then(Value::as_str);
    match name {
        "render_page" => {
            let scale = match (a.get("scale").and_then(Value::as_f64), a.get("dpi").and_then(Value::as_f64)) {
                (Some(s), _) => s,
                (None, Some(d)) => d / 72.0,
                (None, None) => 96.0 / 72.0,
            };
            if !scale.is_finite() || !(0.1..=8.0).contains(&scale) {
                return Err(ToolError::new(
                    "invalid_argument",
                    format!("resolution {:.0} dpi is outside 7–576 dpi", scale * 72.0),
                    "ask for a dpi between 7 and 576 (96 is screen size)",
                ));
            }
            let total = page_count(b, a.get("markup").and_then(Value::as_bool).unwrap_or(false));
            let pages = match (st("pages"), a.get("page").and_then(Value::as_u64)) {
                (Some(spec), _) => page_list(spec, total)?,
                (None, Some(p)) => vec![p.max(1) as usize],
                (None, None) => vec![1],
            };
            let from = a.get("continue_from").and_then(Value::as_u64).map(|n| n as usize);
            // The printed page by default (no spelling marks or editing view furniture).
            let markup = Some(a.get("markup").and_then(Value::as_bool).unwrap_or(false));
            let mut images = Vec::new();
            let mut info = Vec::new();
            let mut used = 0usize;
            let mut next = None;
            let todo: Vec<usize> = pages.iter().copied().filter(|p| from.is_none_or(|f| *p >= f)).collect();
            for (k, &p) in todo.iter().enumerate() {
                // A safe point between pages: what is rendered so far stays out of the answer.
                progress(k as u64, todo.len() as u64, "rendering pages");
                if wordcraft_engine::whiq_progress::cancel_requested() {
                    return Err(ToolError::new(
                        "cancelled",
                        format!("cancelled before page {p} ({k} of {} rendered)", todo.len()),
                        format!("call render_page again with continue_from: {p} to go on"),
                    )
                    .with("continue_from", json!(p)));
                }
                if p > total {
                    return Err(ToolError::new(
                        "out_of_range",
                        format!("no page {p}: the document has {total}"),
                        format!("ask for a page within 1–{total}"),
                    )
                    .with("pages", json!(total)));
                }
                let mut params = json!({"page": p, "scale": scale});
                if let Some(m) = markup {
                    params["markup"] = json!(m);
                }
                let v = b.call("ui.render", params).map_err(|e| contract::classify(&e, "render_page", a, &[]))?;
                let png = v.get("png").and_then(Value::as_str).unwrap_or("").to_string();
                if png.is_empty() {
                    return Err(ToolError::new("failed", "the reply carried no picture", "retry, or render another page"));
                }
                if used + png.len() > limits.max_image_bytes {
                    if images.is_empty() {
                        let fit = (scale * ((limits.max_image_bytes as f64 / png.len() as f64).sqrt()) * 72.0 * 0.9).floor();
                        return Err(ToolError::new(
                            "too_large",
                            format!(
                                "page {p} at {:.0} dpi is {} bytes of PNG, over the {}-byte limit",
                                scale * 72.0,
                                png.len(),
                                limits.max_image_bytes
                            ),
                            format!("ask for dpi {fit:.0} or lower"),
                        )
                        .with("max_dpi", json!(fit)));
                    }
                    next = Some(p);
                    break;
                }
                used += png.len();
                info.push(json!({"page": p, "width": v["width"], "height": v["height"]}));
                images.push(png);
            }
            let mut summary = json!({"pages": info, "total_pages": total, "dpi": (scale * 72.0).round(), "markup": markup});
            if let (Some(first), true) = (info.first(), info.len() == 1) {
                summary["width"] = first["width"].clone();
                summary["height"] = first["height"].clone();
            }
            if let Some(nx) = next {
                summary["truncated"] = json!(true);
                summary["continue_from"] = json!(nx);
                summary["next_step"] = json!(format!("call render_page again with the same pages and continue_from: {nx}"));
            }
            Ok(Out::Images(images, summary))
        }
        "inspect_document" => {
            // WHIQ: the engine reads a window of blocks from `continue_from` (every block is
            // reachable; nothing is laid out to count the pages).
            let from = a.get("continue_from").or_else(|| a.get("offset")).and_then(Value::as_u64).unwrap_or(0);
            let mut p = json!({"offset": from, "limit": 2000});
            if let Some(t) = a.get("text").and_then(Value::as_bool) {
                p["text"] = json!(t);
            }
            b.call("document.inspect", p).map(Out::Json).map_err(|e| contract::classify(&e, "inspect_document", a, &[]))
        }
        "export_text" => {
            // WHIQ: the whole text into a file (the bulk read), any length.
            let path = st("path").unwrap_or("");
            exec(b, "document.textToFile", json!({"path": path})).map(Out::Json).map_err(|e| contract::classify(&e, "export_text", a, &[]))
        }
        "export_pdf" => {
            let path = st("path").unwrap_or("");
            let mut p = json!({"path": path, "markup": a.get("markup").and_then(Value::as_bool).unwrap_or(false)});
            if let Some(spec) = st("pages") {
                // Checked first, for an answer in the contract's terms.
                page_list(spec, page_count(b, a.get("markup").and_then(Value::as_bool).unwrap_or(false)))?;
                p["pages"] = json!(spec);
            }
            exec(b, "file.printPdf", p).map(Out::Json).map_err(|e| contract::classify(&e, "export_pdf", a, &[]))
        }
        "import_markdown" => {
            let mut p = a.as_object().cloned().unwrap_or_default();
            for k in crate::contract::COMMON_ARGS {
                p.remove(*k);
            }
            if p.get("markdown").is_none() && p.get("path").is_none() && p.get("blocks").is_none() {
                return Err(ToolError::new(
                    "invalid_argument",
                    "missing `markdown`, `path` or `blocks`",
                    "pass the Markdown text as `markdown`, a .md file as `path`, or literal text as `blocks` ([{\"text\": \"…\"}])",
                ));
            }
            exec(b, "file.importMarkdown", Value::Object(p)).map(Out::Json).map_err(|e| contract::classify(&e, "import_markdown", a, &[]))
        }
        "apply_theme" => exec(b, "styles.applyTheme", json!({"theme": a.get("theme").cloned().unwrap_or(json!({}))}))
            .map(Out::Json)
            .map_err(|e| contract::classify(&e, "apply_theme", a, &[])),
        "roundtrip_check" => {
            let path = st("path").unwrap_or("");
            progress(0, 2, "round trip: reading");
            let bytes = wordcraft_engine::io::read_file(path).map_err(|e| contract::classify(&e, "roundtrip_check", a, &[]))?;
            progress(1, 2, "round trip: saving and comparing");
            let (report, _) = wordcraft_docx::roundtrip::check(&bytes, path).map_err(|e| {
                ToolError::new(
                    "failed",
                    format!("{path}: {e}"),
                    "roundtrip_check reads Word packages (.docx, .docm, .dotx, .dotm); convert other files with open_document and save_document",
                )
            })?;
            let mut v: Value = serde_json::from_str(&report.to_json()).unwrap_or(json!({}));
            v["renamed_parts"] = json!(report.renamed_parts);
            if !report.lossless() {
                v["next_step"] =
                    json!("the parts listed under problems would change on save: tell the owner before editing, or keep the original file");
            }
            Ok(Out::Json(v))
        }
        "close_document" if b.has_handles() => {
            let discard = a.get("discard").and_then(Value::as_bool).unwrap_or(false);
            let (closed, active) = b.close_active(discard).map_err(|e| {
                ToolError::new(
                    code::FAILED,
                    e,
                    "save_document {\"document\": …} first (or pass discard: true to lose the changes); server_status lists the open documents",
                )
            })?;
            Ok(Out::Json(json!({"closed": true, "document": closed, "active_document": active})))
        }
        "close_document" => {
            let mut out = json!({"closed": true});
            if !a.get("discard").and_then(Value::as_bool).unwrap_or(false)
                && let Some(snap) = b.snapshot().filter(|s| s.dirty)
            {
                let dir = crate::recover::recovery_dir(wordcraft_engine::file_access::current().as_deref().and_then(|r| r.write_root()), Some(&snap));
                let path = crate::recover::autosave(&snap, &dir, "closed with unsaved changes").map_err(|e| {
                    ToolError::new("failed", format!("could not keep the unsaved changes: {e}"), "save_document first, or close with discard: true")
                })?;
                crate::recover::clear_marker(&dir);
                out["recovered_path"] = json!(path.to_string_lossy());
                warnings.push(format!("the unsaved changes were kept in {} (open_document on it to continue)", path.display()));
            }
            exec(b, "file.new", json!({"template": "blank"})).map_err(|e| contract::classify(&e, "close_document", a, &[]))?;
            Ok(Out::Json(out))
        }
        "execute" => {
            let Some(c) = st("command") else {
                return Err(ToolError::new("invalid_argument", "missing `command`", "pass {\"command\": \"<id>\", \"params\": {…}}"));
            };
            exec_forgiving(b, c, a.get("params").cloned().filter(|p| !p.is_null()).unwrap_or(json!({})), warnings).map(Out::Json)
        }
        "batch" => {
            let keep = a.get("keepGoing").and_then(Value::as_bool).unwrap_or(false);
            let list = a.get("commands").and_then(Value::as_array).cloned().unwrap_or_default();
            let total = list.len();
            let mut out = Vec::new();
            // WHIQ parity pass: a cancelled call leaves the document as it was before the call.
            b.begin_call();
            for (i, c) in list.into_iter().enumerate() {
                progress(i as u64, total as u64, "batch commands");
                let id = c.get("command").and_then(Value::as_str).unwrap_or("").to_string();
                // A safe point between commands: the whole batch is undone.
                if wordcraft_engine::whiq_progress::cancel_requested() {
                    let rolled = b.rollback_call();
                    return Err(ToolError::new(
                        "cancelled",
                        format!("cancelled before command {} of {total} (`{id}`)", i + 1),
                        if rolled {
                            "the document is as it was before the batch (nothing of it stays): send the batch again to do it"
                        } else {
                            "the commands before it ran (see results); send the rest as a new batch to go on"
                        },
                    )
                    .with("results", json!(out))
                    .with("rolled_back", json!(rolled))
                    .with("stopped_at", json!(i)));
                }
                match exec_forgiving(b, &id, c.get("params").cloned().filter(|p| !p.is_null()).unwrap_or(json!({})), warnings) {
                    Ok(v) => out.push(json!({"command": id, "ok": true, "result": v})),
                    Err(e) => {
                        out.push(json!({"command": id, "ok": false, "error": e.to_json()}));
                        if !keep {
                            return Err(ToolError::new(
                                e.code,
                                format!("command {} of {total} (`{id}`) failed: {}", i + 1, e.reason),
                                format!("{}; the commands before it ran (see results); pass keepGoing: true to run the rest anyway", e.next_step),
                            )
                            .with("results", json!(out))
                            .with("failed_at", json!(i)));
                        }
                    }
                }
            }
            Ok(Out::Json(json!({"results": out, "ran": total})))
        }
        "type_text" if a.get("paragraphs").and_then(Value::as_bool).unwrap_or(false) => {
            // Each line is a new paragraph whose direction comes from its first letter (an
            // Arabic line is right to left, an English one left to right), not from the line
            // before it.
            let t = st("text").unwrap_or("");
            let mut last = Value::Null;
            let lines = t.split('\n').count();
            b.begin_call();
            for (i, line) in t.split('\n').enumerate() {
                progress(i as u64, lines as u64, "typing paragraphs");
                // A safe point between paragraphs: the whole call is undone.
                if wordcraft_engine::whiq_progress::cancel_requested() {
                    let rolled = b.rollback_call();
                    return Err(ToolError::new(
                        "cancelled",
                        format!("cancelled before paragraph {} of {lines}", i + 1),
                        if rolled {
                            "the document is as it was before the call (nothing was typed): call it again to type the text"
                        } else {
                            "the paragraphs before it are typed; send the rest of the text to go on"
                        },
                    )
                    .with("rolled_back", json!(rolled))
                    .with("typed_paragraphs", json!(if rolled { 0 } else { i })));
                }
                if i > 0 {
                    last = exec(b, "text.newParagraph", json!({})).map_err(|e| contract::classify(&e, "type_text", a, &[]))?;
                    if let Some(rtl) = first_strong(line) {
                        exec(b, if rtl { "para.rtl" } else { "para.ltr" }, json!({"value": true}))
                            .map_err(|e| contract::classify(&e, "type_text", a, &[]))?;
                    }
                }
                if !line.is_empty() {
                    last = exec(b, "text.insert", json!({"text": line, "raw": true})).map_err(|e| contract::classify(&e, "type_text", a, &[]))?;
                }
            }
            Ok(Out::Json(last))
        }
        "select_text" => {
            let text = st("text").unwrap_or("");
            let occ = a.get("occurrence").and_then(Value::as_u64).unwrap_or(1).max(1) as usize;
            match exec(b, "select.text", json!({"text": text, "occurrence": occ})) {
                Ok(v) => Ok(Out::Json(v)),
                Err(e) if e.contains("not found") => {
                    let doc = exec(b, "document.text", json!({}))
                        .ok()
                        .and_then(|v| v.get("text").and_then(Value::as_str).map(str::to_string))
                        .unwrap_or_default();
                    // The same text in other case: select that, and say so.
                    if let Some(at) = contract::find_ci(&doc, text, occ) {
                        let len = doc.get(at..).map(|r| r.char_indices().nth(text.chars().count()).map_or(r.len(), |(i, _)| i)).unwrap_or(0);
                        let found = doc.get(at..at + len).unwrap_or(text).to_string();
                        let k = doc.get(..at).map_or(0, |before| before.matches(found.as_str()).count()) + 1;
                        if let Ok(v) = exec(b, "select.text", json!({"text": found, "occurrence": k})) {
                            warnings.push(format!("`{text}` matched `{found}` ignoring case"));
                            return Ok(Out::Json(v));
                        }
                    }
                    let near = contract::closest_phrases(&doc, text, 5);
                    let list: Vec<String> = near.iter().map(|(p, _)| p.clone()).collect();
                    Err(ToolError::new(
                        "not_found",
                        format!("`{text}` is not in the document"),
                        if list.is_empty() {
                            "check the text with get_text and select something that is there".to_string()
                        } else {
                            format!("select one of the closest matches: {}", list.iter().map(|p| format!("`{p}`")).collect::<Vec<_>>().join(", "))
                        },
                    )
                    .with("closest", json!(list)))
                }
                Err(e) => Err(contract::classify(&e, "select_text", a, &[])),
            }
        }
        "__sleep" if limits.test_hooks => {
            let ms = a.get("ms").and_then(Value::as_u64).unwrap_or(1000).min(120_000);
            std::thread::sleep(std::time::Duration::from_millis(ms));
            Ok(Out::Json(json!({"slept_ms": ms})))
        }
        "__panic" if limits.test_hooks => test_panic(),
        n if crate::whiq_review::handles(n) => crate::whiq_review::run(b, n, a, limits, warnings),
        _ => {
            // The upstream tool, its result mapped into the contract's shape.
            let r = tools::call_tool(b, name, a);
            if r.is_error {
                let text = r.content.iter().find_map(|c| c.get("text").and_then(Value::as_str)).unwrap_or("failed").to_string();
                let ids = if text.starts_with("unknown command") { command_ids(b) } else { Vec::new() };
                return Err(contract::classify(&text, name, a, &ids));
            }
            if let Some(img) = r.content.iter().find(|c| c["type"] == "image") {
                let info = r
                    .content
                    .iter()
                    .find_map(|c| c.get("text").and_then(Value::as_str))
                    .and_then(|t| serde_json::from_str(t).ok())
                    .unwrap_or(json!({}));
                return Ok(Out::Images(vec![img["data"].as_str().unwrap_or("").to_string()], info));
            }
            Ok(Out::Json(result_json(&r)))
        }
    }
}

/// The running call is at step `done` of `total` of `phase` (for `job_status`, `server_status`
/// and progress notifications).
fn progress(done: u64, total: u64, phase: &str) {
    wordcraft_engine::whiq_progress::report(done, total, phase);
}

/// Call tool `name` with arguments `a` on `b`: the full contract pipeline. Returns the MCP
/// `tools/call` result.
pub fn call_tool(b: &mut dyn Backend, name: &str, a: &Value, limits: &Limits) -> Value {
    let hidden = limits.test_hooks && matches!(name, "__sleep" | "__panic");
    let defs = tool_definitions();
    let (name, mut warnings) = if hidden {
        (name.to_string(), Vec::new())
    } else {
        match resolve_tool(name, &defs) {
            Ok((n, w)) => (n, w.into_iter().collect::<Vec<_>>()),
            Err(e) => return error_result(&e, &[]),
        }
    };
    let schema = defs.as_array().and_then(|v| v.iter().find(|t| t["name"] == name.as_str())).map(|t| t["inputSchema"].clone()).unwrap_or(json!({}));
    let (args, w) = if hidden {
        (a.clone(), Vec::new())
    } else {
        match normalise_args(a, &schema) {
            Ok(x) => x,
            Err(e) => return error_result(&e, &warnings),
        }
    };
    warnings.extend(w);
    // WHIQ: `document` names the document a call acts on; the active one comes back after it.
    let target = args.get("document").and_then(|v| v.as_str().map(str::to_string).or_else(|| v.as_u64().map(|n| format!("doc-{n}"))));
    let mut restore = None;
    if let Some(h) = &target {
        let before = b.active_document();
        match b.select_document(h) {
            Ok(()) => restore = before,
            Err(_) if !b.has_handles() => {
                let e = ToolError::new(
                    code::NOT_AVAILABLE,
                    "document handles exist in the headless server only",
                    "leave out `document`: the call acts on the app's open document",
                );
                return error_result(&e, &warnings);
            }
            Err(open) => {
                let e = ToolError::new(
                    code::NOT_FOUND,
                    format!("no open document has the handle `{h}` (closed, evicted or never opened)"),
                    "use a handle from server_status `documents`, or open the file again with open_document",
                )
                .with("closest", json!(closest(h, open.iter().map(String::as_str), 3)))
                .with("open_documents", json!(open));
                return error_result(&e, &warnings);
            }
        }
    }
    // Before the caller's name is set: a new document starts with the server's own author.
    let opens = OPENS_DOCUMENT.contains(&name.as_str());
    if opens {
        b.open_slot();
    }
    // Who is calling: their name on tracked changes and comments made by this call.
    let caller = args.get("caller").and_then(Value::as_str).map(str::trim).filter(|c| !c.is_empty()).map(str::to_string);
    let before = caller.as_ref().and_then(|_| b.author());
    if let Some(c) = &caller
        && let Err(e) = exec(b, "file.setAuthor", json!({"name": c}))
    {
        warnings.push(format!("caller `{c}` not used: {e}"));
    }
    // A save names its author as the document's creator when it has none (as every craft app
    // does); the last modifier is stamped by the save itself.
    let saves = name == "save_document"
        || (name == "execute" && args.get("command").and_then(Value::as_str).is_some_and(|c| c.to_ascii_lowercase().starts_with("file.save")));
    if saves
        && let Some(who) = caller.clone().or_else(|| b.author()).filter(|a| !a.trim().is_empty() && a != "WordCraft User")
        && exec(b, "file.properties", json!({})).is_ok_and(|p| p["creator"].as_str().is_none_or(str::is_empty))
    {
        let _ = exec(b, "file.properties", json!({"author": who}));
    }
    let policy =
        if !limits.keep_old || args.get("overwrite").and_then(Value::as_bool) == Some(true) { Overwrite::Replace } else { Overwrite::KeepOld };
    // Every call reports at least what it is doing (tools with steps count them).
    if wordcraft_engine::whiq_progress::current().is_none() {
        wordcraft_engine::whiq_progress::phase(&name);
    }
    let (mut out, kept) = with_overwrite(policy, || run(b, &name, &args, limits, &mut warnings));
    if opens {
        b.end_open(out.is_ok());
        if let (Ok(Out::Json(v)), Some(h)) = (&mut out, b.active_document()) {
            if !v.is_object() {
                *v = json!({"result": v.take()});
            }
            v["document"] = json!(h);
        }
    }
    b.end_call();
    if let (Some(_), Some(prev)) = (&caller, before) {
        let _ = exec(b, "file.setAuthor", json!({"name": prev}));
    }
    if let Some(prev) = restore.filter(|_| !opens && name != "close_document") {
        // The document active before comes back (when it is still open).
        let _ = b.select_document(&prev);
    }
    let kept_json: Vec<Value> = kept
        .iter()
        .map(|(dest, old)| {
            warnings.push(format!("{} existed: it was kept as {} (pass overwrite: true to replace a file)", dest.display(), old.display()));
            json!({"path": dest.to_string_lossy(), "previous": old.to_string_lossy()})
        })
        .collect();
    let notice = (!kept_json.is_empty()).then(|| {
        format!(
            "an existing file was kept: {} (pass overwrite: true to replace instead)",
            kept_json.iter().filter_map(|k| k["previous"].as_str()).collect::<Vec<_>>().join(", ")
        )
    });
    let max = args.get("max_bytes").and_then(Value::as_u64).map_or(limits.max_result_bytes, |m| (m as usize).clamp(1024, 4 << 20));
    let from = args.get("continue_from").and_then(Value::as_u64).unwrap_or(0) as usize;
    match out {
        Ok(Out::Json(v)) => {
            let mut v = match name.as_str() {
                "render_page" => v,
                "inspect_document" => trim_window(v, from, max),
                _ => trim(v, from, max),
            };
            if !kept_json.is_empty() {
                if !v.is_object() {
                    v = json!({"result": v});
                }
                v["previous_files_renamed"] = json!(kept_json);
                v["notice"] = json!(notice);
            }
            json_result(v, &warnings)
        }
        Ok(Out::Images(images, mut summary)) => {
            if !warnings.is_empty() {
                summary["warnings"] = json!(warnings);
            }
            if !kept_json.is_empty() {
                summary["previous_files_renamed"] = json!(kept_json);
                summary["notice"] = json!(notice);
            }
            let mut content: Vec<Value> = images.into_iter().map(|d| json!({"type": "image", "data": d, "mimeType": "image/png"})).collect();
            content.push(json!({"type": "text", "text": summary.to_string()}));
            json!({"content": content, "isError": false, "structuredContent": summary})
        }
        Err(mut e) => {
            if !kept_json.is_empty() {
                e = e.with("previous_files_renamed", json!(kept_json));
            }
            error_result(&e, &warnings)
        }
    }
}

/// Test hook, reachable only with `WORDCRAFT_MCP_TEST_HOOKS=1` (never in normal use): an
/// unexpected failure outside any command, so `tests/mcp-contract` can prove the work is
/// auto-saved and the server keeps answering. The one deliberate panic in shipped code.
#[allow(clippy::panic)]
fn test_panic() -> Result<Out, ToolError> {
    std::panic::panic_any("__panic test hook")
}
