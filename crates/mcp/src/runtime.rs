//! WHIQ: nothing hangs. `wordcraft-cli mcp` serves through this runtime:
//!
//! - a reader thread reads requests, a worker thread runs them on the document, and the main
//!   thread answers: it never waits on the worker;
//! - a call that needs the document while another runs waits at most `busy_after` (default
//!   300 ms, for pipelined requests), then answers `busy` with `retry_after_ms`;
//! - a call still running after `job_after` (or sent with `async: true`, or opening a file of
//!   `open_job_bytes` or more) becomes a job: its answer is a `job_id` at once, `job_status`
//!   reports progress and keeps the result (it survives the client timing out); a request the
//!   client cancels (`notifications/cancelled`, which clients also send on their own timeouts)
//!   becomes a job too;
//! - `server_status`, `job_status`, `list_jobs` and `cancel` are answered by the main thread at
//!   any time;
//! - every running call reports progress (0..1, its stage, the steps of the stage and the
//!   stages passed) in `job_status` and `server_status`, and as `notifications/progress` when the
//!   request carries a `progressToken`; `cancel` stops it at its next safe point
//!   (`wordcraft_engine::whiq_progress`);
//! - on SIGTERM / SIGINT / SIGHUP (or Ctrl+C, closing the console on Windows), on the client
//!   going away with unsaved changes, and on a panic, the open document is auto-saved to a
//!   recovered file ([`crate::recover`]); after `idle_evict` without calls the document is
//!   evicted (auto-saved and closed);
//! - while there is unsaved work, it is checkpointed to that recovered file every
//!   `checkpoint_edits` edits or `checkpoint_every` (in the background), so a kill that can't be
//!   caught (SIGKILL, the OOM killer, `TerminateProcess` on Windows) loses at most that much.
//!
//! The shapes (`job_status`, `server_status`, `cancel`, recovered files, error codes) are the
//! same in WordCraft, GridCraft and DeckCraft: CONTRACT-PARITY.md.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::contract::{ToolError, code, unique_match};
use crate::recover::{self, Snapshot};
use crate::server::Server;

/// State the worker publishes and the main thread reads.
#[derive(Default)]
pub struct Shared {
    /// The open document after the last call.
    pub snapshot: Option<Snapshot>,
    /// Recovered files made or found in this session (`{path, reason, original, at}`).
    pub recovered: Vec<Value>,
    /// Recovered files not yet named to the agent: the next tool answer names them, once.
    pub unannounced: Vec<Value>,
    /// Documents the server closed (eviction): `{name, reason, recovered_path, original_path}`.
    pub evicted: Vec<Value>,
    pub read_root: Option<PathBuf>,
    pub write_root: Option<PathBuf>,
    pub author: Option<String>,
    pub backend: String,
    /// The document state last auto-saved (not saved twice).
    pub autosaved: Option<(u64, u64)>,
    /// The last checkpoint: its file, the document state it holds, when.
    pub checkpoint: Option<Value>,
    /// Checkpoints written in this session.
    pub checkpoint_writes: u64,
    /// Edits not in a checkpoint yet.
    pub pending_edits: u32,
    /// Tool calls served, and `busy` answers given.
    pub calls: u64,
    pub busy_replies: u64,
}

impl Shared {
    /// A recovered file the agent must hear about: kept for `server_status` and named once on
    /// the next answer.
    pub fn add_recovered(&mut self, entry: Value) {
        let path = entry["path"].clone();
        self.recovered.retain(|e| e["path"] != path);
        self.unannounced.retain(|e| e["path"] != path);
        self.recovered.push(entry.clone());
        self.unannounced.push(entry);
        let n = self.recovered.len();
        if n > 500 {
            self.recovered.drain(..n - 500);
        }
    }
}

pub type SharedState = Arc<Mutex<Shared>>;

/// The progress fields every craft app reports for a call or job: `progress` (0..1, never
/// going back, 1 only once done), `stage`, `steps` `{done, total}` of the stage and the `stages`
/// passed so far.
fn progress_fields(p: Option<&wordcraft_engine::whiq_progress::Progress>, tool: &str, done: bool) -> Value {
    let p = p.cloned().unwrap_or_default();
    let stage = if p.phase.is_empty() { tool.to_string() } else { p.phase.clone() };
    let mut stages = p.stages.clone();
    if stages.is_empty() {
        stages.push(stage.clone());
    }
    let progress = if done { 1.0 } else { (p.overall.min(0.99) * 1000.0).round() / 1000.0 };
    json!({
        "progress": progress,
        "stage": stage,
        "steps": {"done": if done && p.total > 0 { p.total } else { p.done }, "total": p.total},
        "stages": stages,
    })
}

/// A `notifications/progress` for the running call when it carries a progress token and there is
/// news (a step done, or a second gone by): its progress only ever grows.
fn progress_notification(f: &mut Inflight) -> Option<Value> {
    let token = f.token.clone()?;
    let p = wordcraft_engine::whiq_progress::current().unwrap_or_default();
    let now = Instant::now();
    let counted = p.total > 0 && (p.done as f64) > f.sent;
    let beat = f.sent_at.is_none_or(|t| now.duration_since(t) >= Duration::from_secs(1));
    if !counted && !beat {
        return None;
    }
    let value = if counted { p.done as f64 } else { f.sent + 0.001 };
    f.sent = value;
    f.sent_at = Some(now);
    let phase = if p.phase.is_empty() { f.tool.clone() } else { p.phase };
    let elapsed = f.started.elapsed().as_secs();
    let mut params = json!({"progressToken": token, "progress": value, "message": if p.total > 0 {
        format!("{phase}: {} of {} ({elapsed} s)", p.done, p.total)
    } else {
        format!("{phase} ({elapsed} s)")
    }});
    if p.total > 0 {
        params["total"] = json!(p.total);
    }
    Some(json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": params}))
}

/// Runtime settings (`wordcraft-cli mcp` flags; the same names in every craft app).
#[derive(Clone, Debug)]
pub struct Options {
    /// `--job-after-ms`: a call still running after this long becomes a job.
    pub job_after: Duration,
    /// `--busy-after-ms`: a call arriving while another runs waits this long for it, then
    /// answers `busy`.
    pub busy_after: Duration,
    /// `--open-job-mb`: opening a file at least this big runs as a job from the start.
    pub open_job_bytes: u64,
    /// `--idle-evict-secs`: evict (auto-save and close) the document after this long without calls.
    pub idle_evict: Option<Duration>,
    /// How long to let a running call finish when the client goes away.
    pub drain: Duration,
    /// `--checkpoint-edits`: checkpoint unsaved work after this many edits (0: not by edits).
    pub checkpoint_edits: u32,
    /// `--checkpoint-secs`: checkpoint unsaved work this long after its first edit since the last
    /// checkpoint (`None`: not by time).
    pub checkpoint_every: Option<Duration>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            job_after: Duration::from_millis(15_000),
            busy_after: Duration::from_millis(300),
            open_job_bytes: 20 * 1024 * 1024,
            idle_evict: None,
            drain: Duration::from_secs(30),
            checkpoint_edits: 20,
            checkpoint_every: Some(Duration::from_secs(60)),
        }
    }
}

/// The checkpoint bookkeeping of the main thread.
#[derive(Default)]
struct Checkpoints {
    /// The document state last seen.
    seen: Option<(u64, u64)>,
    /// Edits since the last checkpoint.
    edits: u32,
    /// The first unsaved edit since the last checkpoint.
    since: Option<Instant>,
    /// The checkpoint being written.
    running: Option<std::thread::JoinHandle<()>>,
}

impl Checkpoints {
    /// Let a checkpoint being written finish (before an auto-save on the way out).
    fn finish(&mut self) {
        if let Some(h) = self.running.take() {
            let _ = h.join();
        }
    }

    /// After every event: count edits, and write a checkpoint in the background when it is due;
    /// drop the checkpoint once the document is saved.
    fn tick(&mut self, shared: &SharedState, options: &Options) {
        if self.running.as_ref().is_some_and(std::thread::JoinHandle::is_finished) {
            self.finish();
        }
        let Some((rev, dirty, root)) = shared.lock().ok().and_then(|g| g.snapshot.as_ref().map(|s| (s.rev, s.dirty, g.write_root.clone()))) else {
            return;
        };
        if self.seen != Some(rev) {
            match self.seen {
                Some((doc, _)) if doc == rev.0 && dirty => self.edits = self.edits.saturating_add(1),
                Some((doc, _)) if doc != rev.0 => self.edits = u32::from(dirty),
                _ => {}
            }
            if dirty && self.since.is_none() {
                self.since = Some(Instant::now());
            }
            self.seen = Some(rev);
        }
        if let Ok(mut g) = shared.lock() {
            g.pending_edits = self.edits;
        }
        if !dirty {
            self.edits = 0;
            self.since = None;
            if self.running.is_none() {
                let dir = recover::recovery_dir(root.as_deref(), None);
                recover::discard_checkpoint(rev.0, &dir);
                if let Ok(mut g) = shared.lock()
                    && g.checkpoint.as_ref().is_some_and(|c| c["document"] == json!(rev.0))
                {
                    g.checkpoint = None;
                }
            }
            return;
        }
        if self.running.is_some() {
            return;
        }
        let by_edits = options.checkpoint_edits > 0 && self.edits >= options.checkpoint_edits;
        let by_time = options.checkpoint_every.is_some_and(|d| self.since.is_some_and(|t| t.elapsed() >= d));
        if !(by_edits || by_time) {
            return;
        }
        let Some(snap) = shared.lock().ok().and_then(|g| g.snapshot.clone()) else { return };
        let (edits, shared) = (self.edits, shared.clone());
        self.edits = 0;
        self.since = None;
        let spawned = std::thread::Builder::new().name("mcp-checkpoint".into()).spawn(move || {
            let dir = recover::recovery_dir(root.as_deref(), Some(&snap));
            match recover::checkpoint(&snap, &dir) {
                Ok(Some(p)) => {
                    if let Ok(mut g) = shared.lock() {
                        g.checkpoint = Some(json!({
                            "path": p.to_string_lossy(),
                            "document": snap.rev.0,
                            "revision": snap.rev.1,
                            "edits": edits,
                            "at": wordcraft_engine::cmd::now_iso(),
                        }));
                        g.checkpoint_writes += 1;
                        g.pending_edits = 0;
                    }
                }
                Ok(None) => {}
                Err(e) => eprintln!("wordcraft-cli: could not write a checkpoint: {e}"),
            }
        });
        self.running = spawned.ok();
    }
}

enum Event {
    Line(String),
    Eof,
    /// The worker answered a call (`notify`: a notification, which no call waits for).
    Done {
        reply: Option<Value>,
        notify: bool,
    },
    /// SIGTERM / SIGINT / SIGHUP (native builds).
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    Signal,
}

struct Inflight {
    id: Value,
    tool: String,
    started: Instant,
    deadline: Option<Instant>,
    job: Option<String>,
    /// The request's `progressToken`, and the progress last notified (and when).
    token: Option<Value>,
    sent: f64,
    sent_at: Option<Instant>,
    /// A cancel was asked for.
    cancelling: bool,
    /// The runtime's own request (an idle eviction): its answer goes nowhere.
    internal: bool,
}

impl Inflight {
    fn new(id: Value, tool: String, deadline: Option<Instant>, token: Option<Value>) -> Inflight {
        Inflight { id, tool, started: Instant::now(), deadline, job: None, token, sent: 0.0, sent_at: None, cancelling: false, internal: false }
    }
}

/// A tool call waiting (at most `busy_after`) for the running one to finish.
struct Waiting {
    msg: Value,
    id: Value,
    /// The tool it calls.
    name: String,
    until: Instant,
}

struct Job {
    tool: String,
    started: Instant,
    finished: Option<Instant>,
    /// The tool's result (`{content, isError, structuredContent}`) once done.
    result: Option<Value>,
    /// The progress the call reached.
    progress: Option<wordcraft_engine::whiq_progress::Progress>,
}

impl Job {
    /// `running`, `done`, `failed` or `cancelled`.
    fn status(&self) -> &'static str {
        match &self.result {
            None => "running",
            Some(r) if r["isError"] == true => {
                if r.pointer("/structuredContent/error/code") == Some(&json!(code::CANCELLED)) {
                    "cancelled"
                } else {
                    "failed"
                }
            }
            Some(_) => "done",
        }
    }
}

fn job_elapsed(j: &Job) -> u64 {
    j.finished.unwrap_or_else(Instant::now).duration_since(j.started).as_millis() as u64
}

fn retry_after(elapsed_ms: u64) -> u64 {
    (elapsed_ms / 4).clamp(250, 5000)
}

/// One job in the shape every craft app reports (`job_status`, `list_jobs`, `server_status`):
/// `{job_id, tool, status, elapsed_ms, progress, stage, steps, stages}`, with `retry_after_ms`
/// while it runs and, with `full`, `result` or `error` once it ended.
fn job_value(id: &str, j: &Job, inflight: &Option<Inflight>, full: bool) -> Value {
    let status = j.status();
    let running = inflight.as_ref().filter(|f| f.job.as_deref() == Some(id));
    let live = running.and_then(|_| wordcraft_engine::whiq_progress::current());
    let p = if status == "running" { live.as_ref() } else { j.progress.as_ref() };
    let mut v = json!({"job_id": id, "tool": j.tool, "status": status, "elapsed_ms": job_elapsed(j)});
    if let (Some(o), Value::Object(f)) = (v.as_object_mut(), progress_fields(p, &j.tool, status == "done")) {
        o.extend(f);
    }
    match (status, &j.result) {
        ("running", _) => {
            let retry = retry_after(job_elapsed(j));
            v["retry_after_ms"] = json!(retry);
            v["cancelling"] = json!(running.is_some_and(|f| f.cancelling));
            v["next_step"] = json!(format!("call job_status {{\"job_id\": \"{id}\"}} again in about {retry} ms"));
        }
        (_, Some(r)) if full => {
            if r["isError"] == true {
                v["error"] = r.pointer("/structuredContent/error").cloned().unwrap_or(Value::Null);
                v["next_step"] = json!(if status == "cancelled" {
                    "the job was cancelled at a safe point; the document is as it was before it"
                } else {
                    "the job failed; see error.next_step"
                });
            } else {
                v["result"] = r.get("structuredContent").cloned().unwrap_or(Value::Null);
                v["next_step"] = json!("the job is finished; its result is in `result`");
            }
        }
        _ => {}
    }
    v
}

/// The `job_status` answer for a known job: the job in the shared shape as structured content;
/// a finished job's own content blocks (pictures) come along.
fn job_result(id: &str, j: &Job, inflight: &Option<Inflight>) -> Value {
    let v = job_value(id, j, inflight, true);
    let mut content: Vec<Value> = j
        .result
        .as_ref()
        .and_then(|r| r["content"].as_array())
        .map(|c| c.iter().filter(|b| b["type"] != "text").cloned().collect())
        .unwrap_or_default();
    content.push(json!({"type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default()}));
    json!({"content": content, "isError": false, "structuredContent": v})
}

/// The canonical job id `job-N` for what an agent passed (`3`, `JOB-3`, `job_3`).
fn job_key(v: &Value) -> String {
    let raw = match v {
        Value::String(s) => s.trim().to_ascii_lowercase(),
        other => other.to_string(),
    };
    let digits = raw.trim_start_matches("job").trim_start_matches(['-', '_', ' ']);
    if digits.chars().all(|c| c.is_ascii_digit()) && !digits.is_empty() { format!("job-{digits}") } else { raw }
}

/// The fonts this build embeds (craft-fonts), in the shape every craft app reports.
pub fn fonts_json() -> Value {
    let all = wordcraft_fonts::CRAFT_FONTS;
    let mut families: Vec<&str> = Vec::new();
    for f in all {
        if !families.contains(&f.family) {
            families.push(f.family);
        }
    }
    let arabic = all.iter().find(|f| f.scripts.contains(&"Arab")).map(|f| f.family);
    let latin = all.iter().find(|f| f.scripts.contains(&"Latn") && !f.scripts.contains(&"Jpan") && !f.scripts.contains(&"Hans")).map(|f| f.family);
    json!({"source": "craft-fonts", "faces": all.len(), "families": families, "default": latin, "arabic": arabic})
}

/// The status `server_status` answers (the same shape in every craft app).
pub fn status_json(shared: &Shared, jobs: Vec<Value>, current: Value, started: Instant, limits: &crate::whiq::Limits) -> Value {
    status_json_with(shared, jobs, current, started, limits, &Options::default(), None)
}

/// [`status_json`] with the runtime's settings and idle time.
fn status_json_with(
    shared: &Shared,
    jobs: Vec<Value>,
    current: Value,
    started: Instant,
    limits: &crate::whiq::Limits,
    options: &Options,
    idle: Option<Duration>,
) -> Value {
    let doc = shared.snapshot.as_ref().map(|s| {
        let mut d = s.info();
        d["active"] = json!(true);
        d
    });
    let doc_bytes = shared.snapshot.as_ref().map(|s| recover::doc_bytes(&s.doc));
    let rss = recover::rss_bytes();
    let checkpoint_files: Vec<Value> = shared.checkpoint.iter().cloned().collect();
    json!({
        "server": "wordcraft",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": crate::server::PROTOCOL_VERSION,
        "pid": std::process::id(),
        "uptime_ms": started.elapsed().as_millis() as u64,
        "backend": shared.backend,
        "author": shared.author,
        "memory": {
            "rss_bytes": rss,
            "committed_bytes": recover::committed_bytes(),
            "limit_bytes": limits.max_memory_bytes,
            "measured": rss.is_some(),
            "source": recover::rss_source(),
            "document_bytes": doc_bytes,
        },
        "fonts": fonts_json(),
        "roots": {
            "read": shared.read_root.as_ref().map(|p| p.to_string_lossy().into_owned()),
            "write": shared.write_root.as_ref().map(|p| p.to_string_lossy().into_owned()),
        },
        "documents": doc.into_iter().collect::<Vec<_>>(),
        "current_call": current,
        "jobs": jobs,
        "recovered_files": shared.recovered,
        "evicted": shared.evicted,
        "checkpoints": {
            "every_edits": options.checkpoint_edits,
            "every_secs": options.checkpoint_every.map(|d| d.as_secs()).unwrap_or(0),
            "pending_edits": shared.pending_edits,
            "writes": shared.checkpoint_writes,
            "files": checkpoint_files,
        },
        "idle_ms": idle.map(|d| d.as_millis() as u64),
        "calls": shared.calls,
        "busy_replies": shared.busy_replies,
        "limits": {
            "max_result_bytes": limits.max_result_bytes,
            "max_image_bytes": limits.max_image_bytes,
            "max_memory_mb": limits.max_memory_bytes.map(|b| b / (1024 * 1024)),
            "busy_after_ms": options.busy_after.as_millis() as u64,
            "job_after_ms": options.job_after.as_millis() as u64,
            "open_job_bytes": options.open_job_bytes,
            "idle_evict_secs": options.idle_evict.map(|d| d.as_secs()),
            "old_copies": limits.keep_old,
        },
    })
}

/// Auto-save the last published document if it has unsaved changes.
fn autosave_shared(shared: &SharedState, reason: &str) -> Option<PathBuf> {
    let (snap, root, done) = {
        let g = shared.lock().ok()?;
        (g.snapshot.clone()?, g.write_root.clone(), g.autosaved)
    };
    if !snap.dirty || done == Some(snap.rev) {
        return None;
    }
    let dir = recover::recovery_dir(root.as_deref(), Some(&snap));
    match recover::autosave(&snap, &dir, reason) {
        Ok(p) => {
            if let Ok(mut g) = shared.lock() {
                g.add_recovered(recover::entry(&p, reason, snap.path.as_deref()));
                g.autosaved = Some(snap.rev);
            }
            Some(p)
        }
        Err(e) => {
            eprintln!("wordcraft-cli: could not auto-save the document: {e}");
            None
        }
    }
}

fn write_line(out: &mut impl Write, v: &Value) -> std::io::Result<()> {
    out.write_all(v.to_string().as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()
}

fn rpc_error(id: Value, code: i64, e: &ToolError) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": format!("{}: {}", e.code, e.reason), "data": e.to_json()}})
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// Recovered files not yet named to the agent go into this tool answer (`recovered_files`, once).
fn announce(reply: &mut Value, shared: &SharedState) {
    let Some(sc) = reply.pointer_mut("/result/structuredContent").and_then(Value::as_object_mut) else { return };
    let Some(news) = shared.lock().ok().map(|mut g| std::mem::take(&mut g.unannounced)).filter(|n| !n.is_empty()) else { return };
    let paths: Vec<String> = news.iter().filter_map(|e| e["path"].as_str().map(str::to_string)).collect();
    sc.insert("recovered_files".into(), Value::Array(news));
    let note = format!("work was auto-saved to recovered files: {} (open_document on one to continue that work)", paths.join(", "));
    if let Some(content) = reply.pointer_mut("/result/content").and_then(Value::as_array_mut) {
        content.push(json!({"type": "text", "text": note}));
    }
}

/// The name a tool call asks for, if it is one the main thread answers.
fn main_tool(name: &str) -> Option<&'static str> {
    match crate::contract::norm(name).as_str() {
        "status" => Some("server_status"),
        "job" => Some("job_status"),
        "jobs" => Some("list_jobs"),
        "cancel" | "stop" | "abort" | "canceljob" | "cancelcall" => Some("cancel"),
        _ => unique_match(name, ["server_status", "job_status", "list_jobs"]),
    }
}

/// Is this call an `open_document` of a file at least `bytes` big (a job from the start)?
fn is_big_open(name: &str, args: &Value, bytes: u64) -> bool {
    let tool = crate::whiq::resolve_tool(name, &crate::whiq::tool_definitions()).map(|(n, _)| n).unwrap_or_default();
    if tool != "open_document" {
        return false;
    }
    let path = ["path", "file", "filename", "file_path"].iter().find_map(|k| args.get(*k).and_then(Value::as_str)).unwrap_or("");
    !path.is_empty() && std::fs::metadata(path).is_ok_and(|m| m.len() >= bytes)
}

/// Serve newline-delimited JSON-RPC from `input` to `output` until `input` closes. `make` builds
/// the server on the worker thread.
pub fn serve(
    make: impl FnOnce() -> Server + Send + 'static,
    shared: SharedState,
    options: Options,
    limits: crate::whiq::Limits,
    input: impl BufRead + Send + 'static,
    mut output: impl Write,
) -> std::io::Result<()> {
    let started = Instant::now();
    let (tx, rx): (Sender<Event>, Receiver<Event>) = channel();
    let (work_tx, work_rx) = channel::<Value>();
    // Reader.
    {
        let tx = tx.clone();
        std::thread::Builder::new().name("mcp-reader".into()).spawn(move || {
            for line in input.lines() {
                match line {
                    Ok(l) => {
                        if tx.send(Event::Line(l)).is_err() {
                            return;
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = tx.send(Event::Eof);
        })?;
    }
    // Worker: owns the document.
    {
        let tx = tx.clone();
        let shared = shared.clone();
        std::thread::Builder::new().name("mcp-worker".into()).spawn(move || {
            // `cancel` stops what this thread runs.
            wordcraft_engine::whiq_progress::watch_this_thread();
            let mut server = make();
            for msg in work_rx {
                let reply = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| server.handle(msg.clone())));
                let reply = match reply {
                    Ok(r) => r,
                    Err(p) => {
                        let what =
                            p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_default();
                        let saved = autosave_shared(&shared, "panic");
                        // The answer names the file: no marker needed for a later server.
                        if let Some(dir) = saved.as_ref().and_then(|p| p.parent()) {
                            recover::clear_marker(dir);
                        }
                        if let Ok(mut g) = shared.lock() {
                            let named = saved.as_ref().map(|p| json!(p.to_string_lossy()));
                            g.unannounced.retain(|e| Some(&e["path"]) != named.as_ref());
                        }
                        let mut e = ToolError::new(
                            code::INTERNAL,
                            format!("internal error: {what}"),
                            match &saved {
                                Some(p) => format!(
                                    "your work up to the previous call is auto-saved at {}: call open_document on it to continue",
                                    p.display()
                                ),
                                None => "the document had no unsaved changes; carry on (the failure was logged)".to_string(),
                            },
                        );
                        if let Some(p) = &saved {
                            e = e.with("recovered_path", json!(p.to_string_lossy()));
                        }
                        let id = msg.get("id").cloned().unwrap_or(Value::Null);
                        Some(rpc_result(id, crate::whiq::error_result(&e, &[])))
                    }
                };
                let notify = msg.get("id").is_none();
                if tx.send(Event::Done { reply, notify }).is_err() {
                    return;
                }
            }
        })?;
    }
    // Signals: auto-save, then end (native builds: the web build has none).
    #[cfg(not(target_arch = "wasm32"))]
    {
        let tx = tx.clone();
        let _ = ctrlc::set_handler(move || {
            let _ = tx.send(Event::Signal);
        });
    }
    let mut inflight: Option<Inflight> = None;
    let mut waiting: VecDeque<Waiting> = VecDeque::new();
    let mut checkpoints = Checkpoints::default();
    let mut jobs: BTreeMap<String, Job> = BTreeMap::new();
    let mut next_job = 1u64;
    let mut eof = false;
    let mut eof_at = None;
    let mut last_activity = Instant::now();
    let mut idle_done = false;
    let current_json = |inflight: &Option<Inflight>| -> Value {
        match inflight.as_ref().filter(|f| !f.internal) {
            Some(f) => {
                let mut v =
                    json!({"tool": f.tool, "job_id": f.job, "elapsed_ms": f.started.elapsed().as_millis() as u64, "cancelling": f.cancelling});
                if let (Some(o), Value::Object(p)) =
                    (v.as_object_mut(), progress_fields(wordcraft_engine::whiq_progress::current().as_ref(), &f.tool, false))
                {
                    o.extend(p);
                }
                v
            }
            None => Value::Null,
        }
    };
    let jobs_json = |jobs: &BTreeMap<String, Job>, inflight: &Option<Inflight>| -> Vec<Value> {
        let mut v: Vec<(u64, Value)> =
            jobs.iter().map(|(id, j)| (id.trim_start_matches("job-").parse().unwrap_or(0), job_value(id, j, inflight, false))).collect();
        v.sort_by_key(|x| x.0);
        v.into_iter().map(|x| x.1).collect()
    };
    // A tool call goes to the worker: as a call, or as a job from the start.
    macro_rules! dispatch {
        ($msg:expr, $id:expr, $name:expr) => {{
            let msg: Value = $msg;
            let id: Value = $id;
            let name: String = $name;
            let args = msg.pointer("/params/arguments").cloned().unwrap_or(Value::Null);
            let flag = |k: &str| match args.get(k) {
                Some(Value::Bool(b)) => *b,
                Some(Value::String(s)) => matches!(s.trim().to_ascii_lowercase().as_str(), "true" | "yes" | "1"),
                _ => false,
            };
            let as_job = flag("async") || flag("background") || is_big_open(&name, &args, options.open_job_bytes);
            let now = Instant::now();
            let token = msg.pointer("/params/_meta/progressToken").cloned().filter(|t| t.is_string() || t.is_number());
            let mut f = Inflight::new(id.clone(), name.clone(), Some(now + options.job_after), token);
            wordcraft_engine::whiq_progress::reset();
            if as_job {
                let jid = format!("job-{next_job}");
                next_job += 1;
                jobs.insert(jid.clone(), Job { tool: name.clone(), started: now, finished: None, result: None, progress: None });
                write_line(&mut output, &rpc_result(id, job_started(&jid, &name)))?;
                f.job = Some(jid);
                f.deadline = None;
            }
            if let Ok(mut g) = shared.lock() {
                g.calls += 1;
            }
            last_activity = now;
            idle_done = false;
            let _ = work_tx.send(msg);
            inflight = Some(f);
        }};
    }
    loop {
        let mut wait =
            inflight.as_ref().and_then(|f| f.deadline).map(|d| d.saturating_duration_since(Instant::now())).unwrap_or(Duration::from_millis(500));
        if let Some(w) = waiting.front() {
            wait = wait.min(w.until.saturating_duration_since(Instant::now()));
        }
        // At least twice a second: progress notifications and checkpoints are due on the clock.
        let ev = match rx.recv_timeout(wait.clamp(Duration::from_millis(5), Duration::from_millis(500))) {
            Ok(ev) => Some(ev),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        };
        // Unsaved work is checkpointed every few edits or seconds (in the background).
        if !matches!(ev, Some(Event::Signal | Event::Eof)) {
            checkpoints.tick(&shared, &options);
        }
        // The running call's progress, to a client that asked for it.
        if let Some(n) = inflight.as_mut().and_then(progress_notification) {
            write_line(&mut output, &n)?;
        }
        // Calls that waited their time behind the running one: busy.
        while waiting.front().is_some_and(|w| Instant::now() >= w.until) {
            let Some(w) = waiting.pop_front() else { break };
            let (tool, elapsed, job) = match &inflight {
                Some(f) => (f.tool.clone(), f.started.elapsed().as_millis() as u64, f.job.clone()),
                None => (String::from("a previous call"), 0, None),
            };
            let e =
                ToolError::busy(retry_after(elapsed), &format!("`{tool}` (running for {elapsed} ms)"), job.as_deref()).with("running", json!(tool));
            if let Ok(mut g) = shared.lock() {
                g.busy_replies += 1;
            }
            write_line(&mut output, &rpc_result(w.id, crate::whiq::error_result(&e, &[])))?;
        }
        match ev {
            Some(Event::Line(line)) => {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                let msg: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(e) => {
                        let err = ToolError::new(code::INVALID_REQUEST, format!("parse error: {e}"), "send one JSON-RPC 2.0 message per line");
                        write_line(&mut output, &rpc_error(Value::Null, -32700, &err))?;
                        continue;
                    }
                };
                let method = msg.get("method").and_then(Value::as_str).unwrap_or("").to_string();
                let id = msg.get("id").cloned();
                // Notifications and replies: a cancelled request becomes a job (its result kept).
                if id.is_none() {
                    if method == "notifications/cancelled"
                        && let Some(f) = inflight.as_mut()
                        && f.job.is_none()
                        && !f.internal
                        && msg.pointer("/params/requestId").is_none_or(|r| *r == f.id)
                    {
                        let jid = format!("job-{next_job}");
                        next_job += 1;
                        jobs.insert(jid.clone(), Job { tool: f.tool.clone(), started: f.started, finished: None, result: None, progress: None });
                        f.job = Some(jid);
                        f.deadline = None;
                    }
                    // Queued for the server's lifecycle state; never answered.
                    let _ = work_tx.send(msg);
                    continue;
                }
                let id = id.unwrap_or(Value::Null);
                // Answered here at any time: none of these needs the document.
                let quick = match method.as_str() {
                    "ping" => Some(json!({})),
                    "initialize" => {
                        let (describe, found) = shared
                            .lock()
                            .map(|g| {
                                (g.backend.clone(), g.recovered.iter().filter_map(|e| e["path"].as_str().map(str::to_string)).collect::<Vec<_>>())
                            })
                            .unwrap_or_default();
                        let mut r = crate::server::initialize_result(msg.get("params").unwrap_or(&Value::Null), &describe);
                        if !found.is_empty()
                            && let Some(s) = r["instructions"].as_str()
                        {
                            r["instructions"] = json!(format!(
                                "{s} Recovered files are waiting: {} (open_document on one to continue that work).",
                                found.iter().take(5).cloned().collect::<Vec<_>>().join(", ")
                            ));
                        }
                        Some(r)
                    }
                    "tools/list" => Some(json!({"tools": crate::whiq::tool_definitions()})),
                    "resources/list" => Some(crate::server::resources_list()),
                    "resources/templates/list" => Some(json!({"resourceTemplates": []})),
                    _ => None,
                };
                if let Some(r) = quick {
                    write_line(&mut output, &rpc_result(id, r))?;
                    continue;
                }
                if method == "tools/call" {
                    let name = msg.pointer("/params/name").and_then(Value::as_str).unwrap_or("").to_string();
                    let args = msg.pointer("/params/arguments").cloned().unwrap_or(Value::Null);
                    if let Some(t) = main_tool(&name) {
                        let result = match t {
                            "cancel" => {
                                let mut r = cancel(&args, &mut inflight, &jobs);
                                if args.get("queued").is_some_and(|q| q == true || q == "true") {
                                    let n = waiting.len();
                                    for w in waiting.drain(..) {
                                        let e = ToolError::new(
                                            code::CANCELLED,
                                            "dropped from the queue by cancel {queued: true}",
                                            "send the call again if it is still wanted",
                                        );
                                        write_line(&mut output, &rpc_result(w.id, crate::whiq::error_result(&e, &[])))?;
                                    }
                                    r["structuredContent"]["dropped_from_queue"] = json!(n);
                                    r["content"][0]["text"] = json!(serde_json::to_string_pretty(&r["structuredContent"]).unwrap_or_default());
                                }
                                r
                            }
                            "server_status" => {
                                let g = shared.lock().map_err(|_| std::io::Error::other("state poisoned"))?;
                                let idle = inflight.is_none().then(|| last_activity.elapsed());
                                crate::whiq::json_result(
                                    status_json_with(&g, jobs_json(&jobs, &inflight), current_json(&inflight), started, &limits, &options, idle),
                                    &[],
                                )
                            }
                            "list_jobs" => crate::whiq::json_result(json!({"jobs": jobs_json(&jobs, &inflight)}), &[]),
                            _ => match args.get("job_id").or_else(|| args.get("id")).or_else(|| args.get("job")) {
                                None => crate::whiq::json_result(json!({"jobs": jobs_json(&jobs, &inflight)}), &[]),
                                Some(want) => {
                                    let key = job_key(want);
                                    match jobs.get(&key) {
                                        Some(j) => job_result(&key, j, &inflight),
                                        None => no_such_job(&key, &jobs),
                                    }
                                }
                            },
                        };
                        write_line(&mut output, &rpc_result(id, result))?;
                        continue;
                    }
                    if inflight.is_some() || !waiting.is_empty() {
                        if options.busy_after.is_zero() {
                            let f = inflight.as_ref();
                            let elapsed = f.map_or(0, |f| f.started.elapsed().as_millis() as u64);
                            let tool = f.map_or_else(|| "a queued call".to_string(), |f| f.tool.clone());
                            let e = ToolError::busy(
                                retry_after(elapsed),
                                &format!("`{tool}` (running for {elapsed} ms)"),
                                f.and_then(|f| f.job.as_deref()),
                            )
                            .with("running", json!(tool));
                            if let Ok(mut g) = shared.lock() {
                                g.busy_replies += 1;
                            }
                            write_line(&mut output, &rpc_result(id, crate::whiq::error_result(&e, &[])))?;
                        } else {
                            waiting.push_back(Waiting { msg, id, name, until: Instant::now() + options.busy_after });
                        }
                        continue;
                    }
                    dispatch!(msg, id, name);
                    continue;
                }
                if inflight.is_some() {
                    let e = ToolError::busy(1000, "another call", inflight.as_ref().and_then(|f| f.job.as_deref()));
                    write_line(&mut output, &rpc_error(id, -32000, &e))?;
                    continue;
                }
                wordcraft_engine::whiq_progress::reset();
                let _ = work_tx.send(msg);
                inflight = Some(Inflight::new(id, method, None, None));
            }
            Some(Event::Done { notify: true, .. }) => {}
            Some(Event::Done { reply, notify: false }) => {
                last_activity = Instant::now();
                if let Some(f) = inflight.take() {
                    let progress = wordcraft_engine::whiq_progress::current();
                    match (&f.job, reply) {
                        _ if f.internal => {}
                        (Some(jid), Some(r)) => {
                            if let Some(j) = jobs.get_mut(jid) {
                                j.finished = Some(Instant::now());
                                j.progress = progress;
                                j.result = Some(r.get("result").cloned().unwrap_or_else(|| {
                                    let e = ToolError::new(
                                        code::FAILED,
                                        r.pointer("/error/message").and_then(Value::as_str).unwrap_or("the request failed").to_string(),
                                        "check the request",
                                    );
                                    crate::whiq::error_result(&e, &[])
                                }));
                            }
                        }
                        (None, Some(mut r)) => {
                            announce(&mut r, &shared);
                            write_line(&mut output, &r)?;
                        }
                        _ => {}
                    }
                }
                // Old finished jobs are let go (the newest 64 are kept).
                while jobs.len() > 64 {
                    let Some(oldest) = jobs.iter().find(|(_, j)| j.finished.is_some()).map(|(k, _)| k.clone()) else { break };
                    jobs.remove(&oldest);
                }
                // The next call that waited behind this one.
                if let Some(w) = waiting.pop_front() {
                    dispatch!(w.msg, w.id, w.name);
                    continue;
                }
                if eof && inflight.is_none() {
                    checkpoints.finish();
                    autosave_shared(&shared, "disconnect");
                    return Ok(());
                }
            }
            Some(Event::Eof) => {
                eof = true;
                eof_at = Some(Instant::now());
                if inflight.is_none() && waiting.is_empty() {
                    checkpoints.finish();
                    autosave_shared(&shared, "disconnect");
                    return Ok(());
                }
            }
            Some(Event::Signal) => {
                checkpoints.finish();
                autosave_shared(&shared, "signal");
                let _ = output.flush();
                std::process::exit(0);
            }
            None => {
                // A call past its time becomes a job.
                if let Some(f) = inflight.as_mut()
                    && f.job.is_none()
                    && !f.internal
                    && f.deadline.is_some_and(|d| Instant::now() >= d)
                {
                    let jid = format!("job-{next_job}");
                    next_job += 1;
                    jobs.insert(jid.clone(), Job { tool: f.tool.clone(), started: f.started, finished: None, result: None, progress: None });
                    write_line(&mut output, &rpc_result(f.id.clone(), job_started(&jid, &f.tool)))?;
                    f.job = Some(jid);
                    f.deadline = None;
                }
                // Idle too long: the document is evicted (auto-saved and closed).
                if let Some(idle) = options.idle_evict
                    && inflight.is_none()
                    && waiting.is_empty()
                    && !idle_done
                    && !eof
                    && last_activity.elapsed() >= idle
                {
                    idle_done = true;
                    let holds = shared.lock().ok().and_then(|g| g.snapshot.as_ref().map(|s| s.dirty || s.path.is_some())).unwrap_or(false);
                    if holds {
                        let reason = format!("the document was closed after {} s without calls", idle.as_secs());
                        let mut f = Inflight::new(json!("__whiq_idle"), "evict".into(), None, None);
                        f.internal = true;
                        let _ = work_tx.send(
                            json!({"jsonrpc": "2.0", "id": "__whiq_idle", "method": "whiq/evict", "params": {"reason": reason, "kind": "idle"}}),
                        );
                        inflight = Some(f);
                    }
                }
                if eof && eof_at.is_some_and(|t| t.elapsed() > options.drain) {
                    checkpoints.finish();
                    autosave_shared(&shared, "disconnect");
                    return Ok(());
                }
            }
        }
    }
}

/// `not_found` for an unknown job, with the closest ids.
fn no_such_job(key: &str, jobs: &BTreeMap<String, Job>) -> Value {
    let known: Vec<&str> = jobs.keys().map(String::as_str).collect();
    let e = ToolError::new(
        code::NOT_FOUND,
        format!("no job `{key}`"),
        if known.is_empty() {
            "no jobs ran yet: call the tool again".to_string()
        } else {
            "use a job_id a call returned; list_jobs lists them".to_string()
        },
    )
    .with("closest", json!(crate::contract::closest(key, known.iter().copied(), 5)));
    crate::whiq::error_result(&e, &[])
}

/// `cancel`: ask the running call (or job `job_id`) to stop at its next safe point. The answer has
/// the same shape in every craft app: `{cancelled, status, job_id, tool, next_step}`.
fn cancel(args: &Value, inflight: &mut Option<Inflight>, jobs: &BTreeMap<String, Job>) -> Value {
    if let Some(want) = args.get("job_id").or_else(|| args.get("id")).or_else(|| args.get("job")) {
        let key = job_key(want);
        let Some(j) = jobs.get(&key) else { return no_such_job(&key, jobs) };
        if j.finished.is_some() {
            return crate::whiq::json_result(
                json!({"cancelled": false, "status": j.status(), "job_id": key, "tool": j.tool,
                    "next_step": format!("the job had ended; job_status {{\"job_id\": \"{key}\"}} gives its result")}),
                &[],
            );
        }
        if inflight.as_ref().and_then(|f| f.job.as_deref()) != Some(key.as_str()) {
            return crate::whiq::json_result(
                json!({"cancelled": false, "status": "not_running", "job_id": key, "tool": j.tool, "next_step": "server_status shows the running call"}),
                &[],
            );
        }
    }
    let Some(f) = inflight.as_mut().filter(|f| !f.internal) else {
        return crate::whiq::json_result(
            json!({"cancelled": false, "status": "not_running", "next_step": "no call is running: nothing to stop"}),
            &[],
        );
    };
    f.cancelling = true;
    wordcraft_engine::whiq_progress::cancel();
    let next = match &f.job {
        Some(j) => {
            format!("call job_status {{\"job_id\": \"{j}\"}}: once stopped, its status is cancelled (a call that finishes first keeps its result)")
        }
        None => "the call answers with code cancelled when it stops (a call that finishes first keeps its result)".to_string(),
    };
    crate::whiq::json_result(
        json!({"cancelled": true, "status": "cancelling", "tool": f.tool, "job_id": f.job, "elapsed_ms": f.started.elapsed().as_millis() as u64, "next_step": next, "retry_after_ms": 500}),
        &[],
    )
}

fn job_started(jid: &str, tool: &str) -> Value {
    crate::whiq::json_result(
        json!({
            "job_id": jid,
            "status": "running",
            "tool": tool,
            "next_step": format!("call job_status {{\"job_id\": \"{jid}\"}} until status is not \"running\"; the result is kept for you"),
            "retry_after_ms": 1000,
        }),
        &[],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_ids_in_any_spelling() {
        assert_eq!(job_key(&json!("job-3")), "job-3");
        assert_eq!(job_key(&json!("JOB_3")), "job-3");
        assert_eq!(job_key(&json!(3)), "job-3");
        assert_eq!(job_key(&json!("3")), "job-3");
        assert_eq!(retry_after(100), 250);
    }

    #[test]
    fn a_finished_job_reports_the_shared_shape() {
        let j = Job {
            tool: "export_pdf".into(),
            started: Instant::now(),
            finished: Some(Instant::now()),
            result: Some(crate::whiq::json_result(json!({"path": "x.pdf"}), &[])),
            progress: None,
        };
        let v = job_value("job-1", &j, &None, true);
        for k in ["job_id", "tool", "status", "elapsed_ms", "progress", "stage", "steps", "stages", "result", "next_step"] {
            assert!(v.get(k).is_some(), "{k}: {v}");
        }
        assert_eq!((v["status"].as_str(), v["progress"].as_f64()), (Some("done"), Some(1.0)));
    }
}
