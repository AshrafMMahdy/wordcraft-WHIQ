//! WordCraft's MCP server (Model Context Protocol, JSON-RPC 2.0 over stdio).
//!
//! Two backends: [`Headless`] runs an in-process editing session (no window); [`backend::Remote`]
//! talks to a running WordCraft app through its control channel (`wordcraft --control PORT`), so
//! agents can also click, type and screenshot the real UI.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod backend;
pub mod contract;
pub mod recover;
pub mod runtime;
pub mod server;
pub mod tools;
pub mod whiq;
pub mod whiq_review;

pub use backend::{Backend, Remote};
pub use server::Server;

use std::sync::Arc;

use serde_json::{Value, json};
use wordcraft_engine::Session;
use wordcraft_engine::file_access::{self, AutomationRoots};

/// Methods that need the desktop app.
pub const NEEDS_APP: &str = "needs the desktop app: start `wordcraft --control 7981` and run `wordcraft-cli mcp --connect 127.0.0.1:7981`";

/// An in-process session.
pub struct Headless {
    /// The active document's session.
    pub session: Session,
    roots: Option<Arc<AutomationRoots>>,
    /// WHIQ parity pass: the document before the running multi-command call (for a cancel).
    before_call: Option<wordcraft_engine::EditSnapshot>,
    /// WHIQ: the active document's handle number (`doc-<n>`).
    handle: u64,
    /// WHIQ: the other open documents, by handle number (the most recently active last).
    parked: Vec<(u64, Session)>,
    next_handle: u64,
    /// WHIQ: the document an open set aside ([`Backend::open_slot`]), until the open succeeds.
    set_aside: Option<u64>,
}

/// WHIQ: the handle a document is named by (`document: "doc-3"`).
pub fn handle_of(n: u64) -> String {
    format!("doc-{n}")
}

impl Headless {
    /// Confine every file the session's commands read and write to `roots`
    /// (`wordcraft-cli mcp --automation-read-root … --automation-write-root …`).
    pub fn with_automation_roots(mut self, roots: Option<Arc<AutomationRoots>>) -> Self {
        self.roots = roots;
        self
    }

    /// A new session for another document: the active one's author and options.
    fn fresh_session(&self) -> Session {
        let mut s = Session::new(wordcraft_doc::Document::new());
        s.author = self.session.author.clone();
        s.initials = self.session.initials.clone();
        s.prefs = self.session.prefs.clone();
        s
    }

    /// Make the parked document `n` the active one (the active one is parked).
    fn activate(&mut self, n: u64) -> bool {
        if n == self.handle {
            return true;
        }
        let Some(i) = self.parked.iter().position(|(h, _)| *h == n) else { return false };
        let (h, s) = self.parked.remove(i);
        let old = std::mem::replace(&mut self.session, s);
        self.parked.push((std::mem::replace(&mut self.handle, h), old));
        self.before_call = None;
        true
    }

    fn snap(s: &Session, handle: u64) -> recover::Snapshot {
        recover::Snapshot {
            doc: s.doc.clone(),
            path: s.path.clone(),
            file_name: s.file_name.clone(),
            dirty: s.dirty,
            paragraphs: s.doc.paragraph_count(),
            words: s.doc.word_count(),
            // One key per document of the process (each session counts its documents from 1).
            rev: ((handle << 40) | (s.document_id() & ((1 << 40) - 1)), s.rev()),
            handle: handle_of(handle),
            others: Vec::new(),
        }
    }

    fn answer(&mut self, method: &str, params: Value) -> Result<Value, String> {
        match method {
            "engine.execute" | "command" => {
                let id = params.get("command").and_then(Value::as_str).ok_or("missing `command`")?.to_string();
                let p = params.get("params").cloned().filter(|v| !v.is_null()).unwrap_or(json!({}));
                self.session.run(&id, &p).map_err(|e| e.to_string())
            }
            "engine.commands" => Ok(self.session.registry.describe()),
            "document.inspect" => self.session.run("document.inspect", &params).map_err(|e| e.to_string()),
            "ui.parity" => Ok(wordcraft_engine::catalog::parity(&self.session.registry)),
            "ui.render" | "render.page" => {
                let page = params.get("page").and_then(Value::as_u64).unwrap_or(1).max(1) as usize - 1;
                let scale = params.get("scale").and_then(Value::as_f64).unwrap_or(1.0).clamp(0.1, 8.0) as f32;
                // WHIQ: `markup` picks the printed page with or without Print Markup (as a PDF
                // export does); without it, the page as the editing view lays it out.
                let (l, opts) = match params.get("markup").and_then(Value::as_bool) {
                    Some(markup) => {
                        let mut o = wordcraft_render::RenderOptions::default();
                        o.display.dim_header = false;
                        (self.session.export_layout_with(markup), o)
                    }
                    None => (self.session.layout(), Default::default()),
                };
                let pg = l.pages.get(page).ok_or_else(|| format!("no page {}", page + 1))?;
                let img = wordcraft_render::render_page(&self.session.doc, pg, scale, &opts);
                let png = img.to_png();
                Ok(
                    json!({"png": wordcraft_engine::cmd::insert::base64_encode(&png), "width": img.width, "height": img.height, "pages": l.pages.len()}),
                )
            }
            m if m.starts_with("ui.") => Err(format!("`{m}` {NEEDS_APP}")),
            other => self.session.run(other, &params).map_err(|e| e.to_string()),
        }
    }
}

impl Default for Headless {
    fn default() -> Self {
        Headless {
            session: Session::new(wordcraft_doc::Document::new()),
            roots: None,
            before_call: None,
            handle: 1,
            parked: Vec::new(),
            next_handle: 2,
            set_aside: None,
        }
    }
}

impl Backend for Headless {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let roots = self.roots.clone();
        file_access::confine(roots.as_ref(), || self.answer(method, params))
    }
    fn has_ui(&self) -> bool {
        false
    }
    fn describe(&self) -> String {
        "headless session (no window)".into()
    }
    fn snapshot(&mut self) -> Option<recover::Snapshot> {
        let mut snap = Self::snap(&self.session, self.handle);
        snap.others = self.parked.iter().rev().map(|(h, s)| Self::snap(s, *h)).collect();
        Some(snap)
    }
    fn has_handles(&self) -> bool {
        true
    }
    fn active_document(&self) -> Option<String> {
        Some(handle_of(self.handle))
    }
    fn select_document(&mut self, handle: &str) -> Result<(), Vec<String>> {
        let want = handle.trim().to_ascii_lowercase();
        let n = want.trim_start_matches("doc").trim_start_matches(['-', '_', ' ']).parse::<u64>().ok();
        if n.is_some_and(|n| self.activate(n)) {
            return Ok(());
        }
        let mut open: Vec<u64> = self.parked.iter().map(|(h, _)| *h).chain([self.handle]).collect();
        open.sort_unstable();
        Err(open.into_iter().map(handle_of).collect())
    }
    fn open_slot(&mut self) {
        // A blank, untouched document is used as it is; anything else stays open beside the new one.
        let s = &self.session;
        if s.path.is_none() && !s.dirty && s.doc.plain_text(wordcraft_doc::StoryRef::Body).trim().is_empty() {
            self.set_aside = None;
            return;
        }
        let fresh = self.fresh_session();
        let old = std::mem::replace(&mut self.session, fresh);
        let n = std::mem::replace(&mut self.handle, self.next_handle);
        self.next_handle += 1;
        self.parked.push((n, old));
        self.set_aside = Some(n);
        self.before_call = None;
    }
    fn end_open(&mut self, opened: bool) {
        if let Some(n) = self.set_aside.take()
            && !opened
            && let Some(i) = self.parked.iter().position(|(h, _)| *h == n)
        {
            // The open failed: its new document goes, the one set aside comes back.
            self.session = self.parked.remove(i).1;
            self.handle = n;
            self.next_handle -= 1;
        }
    }
    fn close_active(&mut self, discard: bool) -> Result<(String, String), String> {
        if self.session.dirty && !discard {
            return Err("the document has unsaved changes: save_document it first, or pass discard: true to close it anyway".into());
        }
        let closed = handle_of(self.handle);
        match self.parked.pop() {
            Some((h, s)) => {
                self.session = s;
                self.handle = h;
            }
            None => {
                self.session = self.fresh_session();
                self.handle = self.next_handle;
                self.next_handle += 1;
            }
        }
        self.before_call = None;
        Ok((closed, handle_of(self.handle)))
    }
    fn reset(&mut self) {
        self.parked.clear();
        self.session = self.fresh_session();
        self.handle = self.next_handle;
        self.next_handle += 1;
        self.before_call = None;
    }
    fn author(&self) -> Option<String> {
        Some(self.session.author.clone())
    }
    fn begin_call(&mut self) {
        self.before_call = Some(self.session.edit_snapshot());
    }
    fn rollback_call(&mut self) -> bool {
        match self.before_call.take() {
            Some(snap) => {
                self.session.restore(snap);
                true
            }
            None => false,
        }
    }
    fn end_call(&mut self) {
        self.before_call = None;
    }
}

#[cfg(test)]
mod tests;
