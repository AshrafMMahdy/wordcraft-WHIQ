//! WHIQ: progress and cancellation of the running command, for a host that runs commands on
//! one thread and answers on another (the MCP runtime). Long commands say how far they are
//! ([`report`]) and stop at their next safe point once a cancel is asked for ([`check`]): a
//! command that stops returns an error, so the session puts the document back as it was before
//! the command. Nothing here is set in the desktop app: there, every check passes.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::CmdError;

/// How far the running call is: `done` of `total` steps (0 when it can't count) of `phase`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Progress {
    pub done: u64,
    pub total: u64,
    pub phase: String,
}

static PROGRESS: Mutex<Option<Progress>> = Mutex::new(None);
static CANCEL: AtomicBool = AtomicBool::new(false);
/// The thread that runs the calls a cancel is for (the host's worker).
static WATCHED: Mutex<Option<std::thread::ThreadId>> = Mutex::new(None);

/// The calling thread runs the calls: a cancel stops what it runs (and nothing on other threads).
pub fn watch_this_thread() {
    if let Ok(mut w) = WATCHED.lock() {
        *w = Some(std::thread::current().id());
    }
}

/// The running call is at step `done` of `total` of `phase`.
pub fn report(done: u64, total: u64, phase: &str) {
    if let Ok(mut p) = PROGRESS.lock() {
        *p = Some(Progress { done, total: total.max(done), phase: phase.chars().take(200).collect() });
    }
}

/// The running call is in `phase` (no steps counted).
pub fn phase(phase: &str) {
    report(0, 0, phase);
}

/// How far the running call is.
pub fn current() -> Option<Progress> {
    PROGRESS.lock().ok().and_then(|p| p.clone())
}

/// A call starts (or ended): no progress, no cancel asked.
pub fn reset() {
    if let Ok(mut p) = PROGRESS.lock() {
        *p = None;
    }
    CANCEL.store(false, Ordering::SeqCst);
}

/// Ask the running call to stop at its next safe point.
pub fn cancel() {
    CANCEL.store(true, Ordering::SeqCst);
}

/// Whether a cancel was asked for the calls this thread runs.
pub fn cancel_requested() -> bool {
    CANCEL.load(Ordering::SeqCst) && WATCHED.lock().ok().is_some_and(|w| *w == Some(std::thread::current().id()))
}

/// A safe point: `Err` (the command stops, the session undoes it) once a cancel was asked for.
/// `what` says where it stopped.
pub fn check(what: &str) -> Result<(), CmdError> {
    if cancel_requested() { Err(CmdError::Failed(format!("cancelled: {what}"))) } else { Ok(()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The state is the process's: these tests take turns.
    static SERIAL: Mutex<()> = Mutex::new(());

    #[test]
    fn progress_and_cancel() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        watch_this_thread();
        reset();
        assert!(check("x").is_ok());
        report(2, 5, "pages");
        assert_eq!(current(), Some(Progress { done: 2, total: 5, phase: "pages".into() }));
        cancel();
        assert!(check("stopped before page 3").is_err_and(|e| e.to_string().starts_with("cancelled: stopped before page 3")));
        reset();
        assert!(check("x").is_ok() && current().is_none());
        // Another thread's commands aren't stopped.
        cancel();
        assert!(std::thread::spawn(|| check("elsewhere").is_ok()).join().unwrap());
        reset();
    }

    /// The engine's safe points: each stopped command leaves the document as it was.
    #[test]
    fn stopped_commands_change_nothing() {
        use crate::Session;
        use serde_json::json;
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        watch_this_thread();
        reset();
        let mut s = Session::new(wordcraft_doc::Document::from_text("alpha beta gamma"));
        let before = s.doc.plain_text(wordcraft_doc::StoryRef::Body);
        cancel();
        // A review edit batch.
        let e = s.run("review.edit", &json!({"ops": [{"op": "replace", "anchor": "beta", "with": "BETA"}]})).unwrap_err();
        assert!(e.to_string().starts_with("cancelled: stopped before operation 1 of 1"), "{e}");
        assert_eq!(s.doc.plain_text(wordcraft_doc::StoryRef::Body), before);
        assert!(s.doc.revisions.is_empty());
        // A Markdown import.
        let e = s.run("file.importMarkdown", &json!({"markdown": "# Title\n\nBody"})).unwrap_err();
        assert!(e.to_string().starts_with("cancelled:"), "{e}");
        assert_eq!(s.doc.plain_text(wordcraft_doc::StoryRef::Body), before);
        // A PDF export: no file.
        let dir = std::env::temp_dir().join(format!("wc-cancel-pdf-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let out = dir.join("x.pdf");
        let e = s.run("file.printPdf", &json!({"path": out.to_string_lossy()})).unwrap_err();
        assert!(e.to_string().starts_with("cancelled:"), "{e}");
        assert!(!out.exists());
        reset();
        assert!(s.run("review.edit", &json!({"ops": [{"op": "replace", "anchor": "beta", "with": "BETA"}]})).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
