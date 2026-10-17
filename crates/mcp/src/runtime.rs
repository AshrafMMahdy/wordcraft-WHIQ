//! WHIQ: nothing hangs. `wordcraft-cli mcp` serves through this runtime:
//!
//! - a reader thread reads requests, a worker thread runs them on the document, and the main
//!   thread answers: it never waits on the worker;
//! - a call that needs the document while another runs answers `busy` with `retry_after_ms`;
//! - a call still running after `job_after_ms` (or sent with `background: true`) becomes a job:
//!   its answer is a `job_id` at once, `job_status` reports progress and keeps the result (it
//!   survives the client timing out);
//! - `server_status` and `job_status` are answered by the main thread at any time;
//! - on SIGTERM / SIGINT / SIGHUP (or Ctrl+C, closing the console on Windows), on the client
//!   going away with unsaved changes, and on a panic, the open document is auto-saved to a
//!   recovered file ([`crate::recover`]).

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::contract::{ToolError, unique_match};
use crate::recover::{self, Snapshot};
use crate::server::Server;

/// State the worker publishes and the main thread reads.
#[derive(Default)]
pub struct Shared {
    /// The open document after the last call.
    pub snapshot: Option<Snapshot>,
    /// Recovered files made or found in this session.
    pub recovered: Vec<Value>,
    pub read_root: Option<PathBuf>,
    pub write_root: Option<PathBuf>,
    pub author: Option<String>,
    pub backend: String,
    /// The document state last auto-saved (not saved twice).
    pub autosaved: Option<(u64, u64)>,
}

pub type SharedState = Arc<Mutex<Shared>>;

static PROGRESS: Mutex<Option<(u64, u64, &'static str)>> = Mutex::new(None);

/// Report progress of the running call (`done` of `total` steps of `phase`), for `job_status`.
pub fn progress(done: u64, total: u64, phase: &'static str) {
    if let Ok(mut p) = PROGRESS.lock() {
        *p = Some((done, total, phase));
    }
}

fn current_progress() -> Option<Value> {
    PROGRESS.lock().ok().and_then(|p| p.map(|(d, t, ph)| json!({"done": d, "total": t, "phase": ph})))
}

/// Runtime settings (`wordcraft-cli mcp` flags).
#[derive(Clone, Debug)]
pub struct Options {
    /// A call still running after this long becomes a job.
    pub job_after: Duration,
    /// How long to let a running call finish when the client goes away.
    pub drain: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Options { job_after: Duration::from_millis(15_000), drain: Duration::from_secs(30) }
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
    Signal,
}

struct Inflight {
    id: Value,
    tool: String,
    started: Instant,
    deadline: Option<Instant>,
    job: Option<String>,
}

struct Job {
    tool: String,
    started: Instant,
    finished: Option<Instant>,
    result: Option<Value>,
}

/// The status `server_status` answers.
pub fn status_json(shared: &Shared, jobs: Vec<Value>, busy: Value, started: Instant, limits: &crate::whiq::Limits) -> Value {
    let doc = shared.snapshot.as_ref().map(Snapshot::info);
    let doc_bytes = shared.snapshot.as_ref().map(|s| recover::doc_bytes(&s.doc));
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "pid": std::process::id(),
        "uptime_ms": started.elapsed().as_millis() as u64,
        "backend": shared.backend,
        "memory": {"rss_bytes": recover::rss_bytes(), "document_bytes": doc_bytes, "limit_bytes": limits.max_memory_bytes},
        "document": doc,
        "busy": busy,
        "jobs": jobs,
        "recovered": shared.recovered,
        "roots": {
            "read": shared.read_root.as_ref().map(|p| p.to_string_lossy().into_owned()),
            "write": shared.write_root.as_ref().map(|p| p.to_string_lossy().into_owned()),
        },
        "author": shared.author,
        "limits": {"max_result_bytes": limits.max_result_bytes, "max_image_bytes": limits.max_image_bytes},
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
                g.recovered.push(json!({"path": p.to_string_lossy(), "reason": reason}));
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

/// The name a tool call asks for, if it is one the main thread answers.
fn main_tool(name: &str) -> Option<&'static str> {
    match crate::contract::norm(name).as_str() {
        "status" => Some("server_status"),
        "job" => Some("job_status"),
        _ => unique_match(name, ["server_status", "job_status"]),
    }
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
            let mut server = make();
            for msg in work_rx {
                let reply = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| server.handle(msg.clone())));
                let reply = match reply {
                    Ok(r) => r,
                    Err(p) => {
                        let what =
                            p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_default();
                        let saved = autosave_shared(&shared, "an internal error");
                        // The answer names the file: no marker needed for a later server.
                        if let Some(dir) = saved.as_ref().and_then(|p| p.parent()) {
                            recover::clear_marker(dir);
                        }
                        let mut e = ToolError::new(
                            "internal",
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
                            e = e.with("recovered", json!(p.to_string_lossy()));
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
    // Signals: auto-save, then end.
    {
        let tx = tx.clone();
        let _ = ctrlc::set_handler(move || {
            let _ = tx.send(Event::Signal);
        });
    }
    let mut inflight: Option<Inflight> = None;
    let mut jobs: BTreeMap<String, Job> = BTreeMap::new();
    let mut next_job = 1u64;
    let mut eof = false;
    let mut eof_at = None;
    let busy_json = |inflight: &Option<Inflight>| -> Value {
        match inflight {
            Some(f) => json!({"tool": f.tool, "elapsed_ms": f.started.elapsed().as_millis() as u64, "job_id": f.job}),
            None => Value::Null,
        }
    };
    let jobs_json = |jobs: &BTreeMap<String, Job>| -> Vec<Value> {
        jobs.iter()
            .map(|(id, j)| {
                json!({"job_id": id, "tool": j.tool, "status": if j.finished.is_some() { "done" } else { "running" },
                    "elapsed_ms": j.finished.unwrap_or_else(Instant::now).duration_since(j.started).as_millis() as u64})
            })
            .collect()
    };
    loop {
        let wait =
            inflight.as_ref().and_then(|f| f.deadline).map(|d| d.saturating_duration_since(Instant::now())).unwrap_or(Duration::from_millis(500));
        let ev = match rx.recv_timeout(wait.max(Duration::from_millis(5))) {
            Ok(ev) => Some(ev),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        };
        match ev {
            Some(Event::Line(line)) => {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                let msg: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(e) => {
                        let err = ToolError::new("parse_error", format!("parse error: {e}"), "send one JSON-RPC 2.0 message per line");
                        write_line(&mut output, &rpc_error(Value::Null, -32700, &err))?;
                        continue;
                    }
                };
                let method = msg.get("method").and_then(Value::as_str).unwrap_or("").to_string();
                let id = msg.get("id").cloned();
                // Notifications and replies: cancellations make the running call a job (its result kept).
                if id.is_none() {
                    if method == "notifications/cancelled"
                        && let Some(f) = inflight.as_mut()
                        && f.job.is_none()
                    {
                        let jid = format!("job-{next_job}");
                        next_job += 1;
                        jobs.insert(jid.clone(), Job { tool: f.tool.clone(), started: f.started, finished: None, result: None });
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
                        let describe = shared.lock().map(|g| g.backend.clone()).unwrap_or_default();
                        Some(crate::server::initialize_result(msg.get("params").unwrap_or(&Value::Null), &describe))
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
                            "server_status" => {
                                let g = shared.lock().map_err(|_| std::io::Error::other("state poisoned"))?;
                                crate::whiq::json_result(status_json(&g, jobs_json(&jobs), busy_json(&inflight), started, &limits), &[])
                            }
                            _ => job_status(&args, &mut jobs, &inflight),
                        };
                        write_line(&mut output, &rpc_result(id, result))?;
                        continue;
                    }
                    if let Some(f) = &inflight {
                        let retry = match f.deadline {
                            Some(d) => (d.saturating_duration_since(Instant::now()).as_millis() as u64 / 2).clamp(200, 2000),
                            None => 1000,
                        };
                        let e =
                            ToolError::busy(retry, &format!("`{}` (running for {} ms)", f.tool, f.started.elapsed().as_millis()), f.job.as_deref());
                        write_line(&mut output, &rpc_result(id, crate::whiq::error_result(&e, &[])))?;
                        continue;
                    }
                    let background = args.get("background").and_then(Value::as_bool).unwrap_or(false);
                    let now = Instant::now();
                    let mut f = Inflight { id: id.clone(), tool: name.clone(), started: now, deadline: Some(now + options.job_after), job: None };
                    if let Ok(mut p) = PROGRESS.lock() {
                        *p = None;
                    }
                    if background {
                        let jid = format!("job-{next_job}");
                        next_job += 1;
                        jobs.insert(jid.clone(), Job { tool: name.clone(), started: now, finished: None, result: None });
                        write_line(&mut output, &rpc_result(id, job_started(&jid, &name)))?;
                        f.job = Some(jid);
                        f.deadline = None;
                    }
                    let _ = work_tx.send(msg);
                    inflight = Some(f);
                    continue;
                }
                if inflight.is_some() {
                    let e = ToolError::busy(1000, "another call", inflight.as_ref().and_then(|f| f.job.as_deref()));
                    write_line(&mut output, &rpc_error(id, -32000, &e))?;
                    continue;
                }
                let _ = work_tx.send(msg);
                inflight = Some(Inflight { id, tool: method, started: Instant::now(), deadline: None, job: None });
            }
            Some(Event::Done { notify: true, .. }) => {}
            Some(Event::Done { reply, notify: false }) => {
                if let Some(f) = inflight.take() {
                    match (&f.job, reply) {
                        (Some(jid), Some(r)) => {
                            if let Some(j) = jobs.get_mut(jid) {
                                j.finished = Some(Instant::now());
                                j.result = r.get("result").cloned().or_else(|| r.get("error").map(|e| json!({"error": e})));
                            }
                        }
                        (None, Some(r)) => write_line(&mut output, &r)?,
                        _ => {}
                    }
                }
                // Old finished jobs are let go (the newest 64 are kept).
                while jobs.len() > 64 {
                    let Some(oldest) = jobs.iter().find(|(_, j)| j.finished.is_some()).map(|(k, _)| k.clone()) else { break };
                    jobs.remove(&oldest);
                }
                if eof && inflight.is_none() {
                    autosave_shared(&shared, "the client disconnected");
                    return Ok(());
                }
            }
            Some(Event::Eof) => {
                eof = true;
                eof_at = Some(Instant::now());
                if inflight.is_none() {
                    autosave_shared(&shared, "the client disconnected");
                    return Ok(());
                }
            }
            Some(Event::Signal) => {
                autosave_shared(&shared, "the server was stopped");
                let _ = output.flush();
                std::process::exit(0);
            }
            None => {
                // A call past its time becomes a job.
                if let Some(f) = inflight.as_mut()
                    && f.job.is_none()
                    && f.deadline.is_some_and(|d| Instant::now() >= d)
                {
                    let jid = format!("job-{next_job}");
                    next_job += 1;
                    jobs.insert(jid.clone(), Job { tool: f.tool.clone(), started: f.started, finished: None, result: None });
                    write_line(&mut output, &rpc_result(f.id.clone(), job_started(&jid, &f.tool)))?;
                    f.job = Some(jid);
                    f.deadline = None;
                }
                if eof && eof_at.is_some_and(|t| t.elapsed() > options.drain) {
                    autosave_shared(&shared, "the client disconnected during a call");
                    return Ok(());
                }
            }
        }
    }
}

fn job_started(jid: &str, tool: &str) -> Value {
    crate::whiq::json_result(
        json!({
            "job_id": jid,
            "status": "running",
            "tool": tool,
            "next_step": format!("call job_status {{\"job_id\": \"{jid}\"}} until status is \"done\"; the result is kept for you"),
            "retry_after_ms": 1000,
        }),
        &[],
    )
}

fn job_status(args: &Value, jobs: &mut BTreeMap<String, Job>, inflight: &Option<Inflight>) -> Value {
    let want = args.get("job_id").map(|v| match v {
        Value::String(s) => s.trim().to_ascii_lowercase(),
        other => other.to_string(),
    });
    let Some(want) = want else {
        let e = ToolError::new("bad_params", "missing `job_id`", "pass the job_id a call returned, e.g. {\"job_id\": \"job-1\"}");
        return crate::whiq::error_result(&e, &[]);
    };
    let key = if want.starts_with("job-") { want.clone() } else { format!("job-{want}") };
    let Some(j) = jobs.get(&key) else {
        let known: Vec<&String> = jobs.keys().collect();
        let e = ToolError::new(
            "job_not_found",
            format!("no job `{want}`"),
            if known.is_empty() {
                "no jobs ran yet: call the tool again".to_string()
            } else {
                format!("known jobs: {}", known.iter().map(|k| k.as_str()).collect::<Vec<_>>().join(", "))
            },
        )
        .with("jobs", json!(known));
        return crate::whiq::error_result(&e, &[]);
    };
    let elapsed = j.finished.unwrap_or_else(Instant::now).duration_since(j.started).as_millis() as u64;
    match &j.result {
        Some(result) => {
            // The tool's own result, with the job's state added.
            let mut r = result.clone();
            let job = json!({"job_id": key, "status": "done", "tool": j.tool, "elapsed_ms": elapsed});
            if let Some(sc) = r.get_mut("structuredContent").and_then(Value::as_object_mut) {
                sc.insert("job".into(), job.clone());
            } else if r.get("error").is_some() {
                let e = ToolError::new("failed", "the job's call failed", "see `error`").with("error", r["error"].clone());
                return crate::whiq::error_result(&e, &[]);
            }
            if let Some(content) = r.get_mut("content").and_then(Value::as_array_mut) {
                content.push(json!({"type": "text", "text": json!({"job": job}).to_string()}));
            }
            r
        }
        None => {
            let running = inflight.as_ref().and_then(|f| f.job.as_deref()) == Some(key.as_str());
            crate::whiq::json_result(
                json!({
                    "job_id": key, "status": "running", "tool": j.tool, "elapsed_ms": elapsed,
                    "progress": if running { current_progress().unwrap_or(json!({"phase": "running"})) } else { Value::Null },
                    "retry_after_ms": 1000,
                    "next_step": "call job_status again after retry_after_ms",
                }),
                &[],
            )
        }
    }
}
