//! WHIQ: work is never lost. On eviction (memory pressure, the client going away), a panic or
//! SIGTERM, the open document is saved to `<name>.recovered-<stamp>.docx` inside the write root
//! (the document's folder, or the working directory, without one), and a marker
//! (`.wordcraft-recovered.json`) names it so the next call (in this process or the next one
//! started on the same root) answers `evicted` with the path and the step to take.
//!
//! A hard kill can't be caught (SIGKILL, the OOM killer, `TerminateProcess` on Windows, which is
//! how a supervisor or Task Manager ends a process there). For those, the runtime writes
//! **checkpoints**: every few edits or seconds of unsaved work ([`crate::runtime::Options`]), the
//! open document is saved to its recovered file in the background and listed in the marker, so a
//! hard kill loses at most the edits since the last checkpoint. One file serves the whole unsaved
//! stretch of a document (each checkpoint replaces it atomically; a later auto-save writes its final
//! state there too); once the document is saved, its checkpoint (never reported to the agent) is
//! removed.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{Value, json};
use wordcraft_doc::Document;

/// The marker file listing recovered documents not yet reported to the agent.
pub const MARKER: &str = ".wordcraft-recovered.json";

/// The open document, as it stood after the last call.
#[derive(Clone)]
pub struct Snapshot {
    pub doc: Document,
    pub path: Option<PathBuf>,
    pub file_name: Option<String>,
    pub dirty: bool,
    pub paragraphs: usize,
    pub words: usize,
    /// Changes whenever the document does (to skip republishing an unchanged snapshot).
    pub rev: (u64, u64),
}

impl Snapshot {
    /// The document's name for people and for the recovered file: its file stem, else "untitled".
    pub fn stem(&self) -> String {
        let from_path = self.path.as_ref().and_then(|p| p.file_stem()).map(|s| s.to_string_lossy().into_owned());
        let raw = from_path.or_else(|| self.file_name.clone()).unwrap_or_else(|| "untitled".into());
        let clean: String = raw.chars().map(|c| if c.is_alphanumeric() || matches!(c, '-' | '_' | ' ' | '.') { c } else { '_' }).take(80).collect();
        if clean.trim().is_empty() { "untitled".into() } else { clean.trim().to_string() }
    }

    pub fn info(&self) -> Value {
        json!({
            "name": self.stem(),
            "path": self.path.as_ref().map(|p| p.to_string_lossy().into_owned()),
            "dirty": self.dirty,
            "paragraphs": self.paragraphs,
            "words": self.words,
        })
    }
}

/// `2026-10-17T19:30:12Z` → `20261017T193012Z`.
pub fn stamp() -> String {
    wordcraft_engine::cmd::now_iso().chars().filter(|c| !matches!(c, '-' | ':')).collect()
}

/// Where recovered documents go: the write root, else the document's folder, else the working
/// directory.
pub fn recovery_dir(write_root: Option<&Path>, snap: Option<&Snapshot>) -> PathBuf {
    if let Some(r) = write_root {
        return r.to_path_buf();
    }
    if let Some(dir) = snap.and_then(|s| s.path.as_ref()).and_then(|p| p.parent()).filter(|d| !d.as_os_str().is_empty()) {
        return dir.to_path_buf();
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// The recovered file of a document (by its id) that later saves of it reuse: the checkpoint file,
/// or the file an auto-save wrote.
static RECOVERY_FILE: Mutex<Option<(u64, PathBuf)>> = Mutex::new(None);
/// The newest document state saved to a recovered file: (document id, revision).
static SAVED: Mutex<Option<(u64, u64)>> = Mutex::new(None);
/// One recovery save at a time (a checkpoint in the background, an auto-save on the way out).
static SAVE_LOCK: Mutex<()> = Mutex::new(());

/// Save `snap` to its recovered file in `dir` and record it in the marker: the file this document
/// was checkpointed or auto-saved to already, else a new `<stem>.recovered-<stamp>.docx` (never
/// over another file). Returns the recovered file's path.
pub fn autosave(snap: &Snapshot, dir: &Path, reason: &str) -> Result<PathBuf, String> {
    save_recovery(snap, dir, reason, false)?.ok_or_else(|| "nothing to save".to_string())
}

/// A checkpoint of `snap` ([`autosave`] in the background, marked `checkpoint`). `None` when a
/// newer state of the document is saved already.
pub fn checkpoint(snap: &Snapshot, dir: &Path) -> Result<Option<PathBuf>, String> {
    save_recovery(snap, dir, "checkpoint", true)
}

fn save_recovery(snap: &Snapshot, dir: &Path, reason: &str, checkpoint: bool) -> Result<Option<PathBuf>, String> {
    let _one = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (doc_id, rev) = snap.rev;
    let saved = *SAVED.lock().unwrap_or_else(|e| e.into_inner());
    if checkpoint && saved.is_some_and(|(d, r)| d == doc_id && r >= rev) {
        return Ok(None);
    }
    let reuse = RECOVERY_FILE.lock().unwrap_or_else(|e| e.into_inner()).clone().filter(|(d, _)| *d == doc_id).map(|(_, p)| p);
    let path = match reuse {
        Some(p) => p,
        None => {
            let ext = match snap.path.as_ref().and_then(|p| p.extension()).map(|e| e.to_string_lossy().to_ascii_lowercase()).as_deref() {
                Some("docm" | "dotm") => "docm",
                _ => "docx",
            };
            let base = format!("{}.recovered-{}", snap.stem(), stamp());
            let mut path = dir.join(format!("{base}.{ext}"));
            let mut n = 2;
            while path.exists() && n < 1000 {
                path = dir.join(format!("{base}-{n}.{ext}"));
                n += 1;
            }
            path
        }
    };
    // A recovery copy never keeps anything as OLD_ (it replaces only its own earlier state).
    let (saved, _) = wordcraft_engine::file_access::with_overwrite(wordcraft_engine::file_access::Overwrite::Replace, || {
        wordcraft_engine::io::save_path(&path, &snap.doc)
    });
    saved?;
    let entry = json!({
        "path": path.to_string_lossy(),
        "reason": reason,
        "source": snap.path.as_ref().map(|p| p.to_string_lossy().into_owned()),
        "at": wordcraft_engine::cmd::now_iso(),
        "checkpoint": checkpoint,
    });
    marker_put(dir, &path, Some(entry));
    *SAVED.lock().unwrap_or_else(|e| e.into_inner()) = Some((doc_id, rev));
    *RECOVERY_FILE.lock().unwrap_or_else(|e| e.into_inner()) = Some((doc_id, path.clone()));
    if !checkpoint {
        eprintln!("wordcraft-cli: work auto-saved ({reason}) at {}", path.display());
    }
    Ok(Some(path))
}

/// Replace the marker's entry for `path` with `entry` (or remove it); the marker goes when empty.
fn marker_put(dir: &Path, path: &Path, entry: Option<Value>) {
    let p = path.to_string_lossy();
    let mut list: Vec<Value> = read_marker(dir).into_iter().filter(|e| e["path"].as_str() != Some(&p)).collect();
    list.extend(entry);
    let marker = dir.join(MARKER);
    if list.is_empty() {
        let _ = std::fs::remove_file(&marker);
    } else {
        let _ = std::fs::write(&marker, serde_json::to_vec_pretty(&Value::Array(list)).unwrap_or_default());
    }
}

/// The document `doc_id` is saved: its checkpoint file (written in the background, never reported
/// to the agent) and its marker entry go. A file an auto-save wrote (and reported) stays.
pub fn discard_checkpoint(doc_id: u64, dir: &Path) {
    let _one = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut slot = RECOVERY_FILE.lock().unwrap_or_else(|e| e.into_inner());
    let Some((d, path)) = slot.clone() else { return };
    if d != doc_id {
        return;
    }
    let p = path.to_string_lossy().into_owned();
    let entry = read_marker(dir).into_iter().find(|e| e["path"].as_str() == Some(&p));
    if entry.as_ref().is_some_and(|e| e["checkpoint"] == json!(true)) {
        let _ = std::fs::remove_file(&path);
        marker_put(dir, &path, None);
    }
    *slot = None;
}

/// The current checkpoint or recovered file of document `doc_id`, if any.
pub fn recovery_file(doc_id: u64) -> Option<PathBuf> {
    RECOVERY_FILE.lock().ok()?.clone().filter(|(d, _)| *d == doc_id).map(|(_, p)| p)
}

/// The recovered documents the marker in `dir` lists (empty without one).
pub fn read_marker(dir: &Path) -> Vec<Value> {
    std::fs::read(dir.join(MARKER))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
}

/// Forget the marker (its documents were reported); the recovered documents stay.
pub fn clear_marker(dir: &Path) {
    let _ = std::fs::remove_file(dir.join(MARKER));
}

/// Resident memory of this process in bytes, as the operating system counts it: the working set
/// on Windows (`GetProcessMemoryInfo`), the resident size on macOS (`task_info`), `VmRSS` on Linux.
/// `None` only where the platform doesn't say (the web build).
pub fn rss_bytes() -> Option<u64> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        memory_stats::memory_stats().map(|m| m.physical_mem as u64).filter(|b| *b > 0).or_else(proc_rss)
    }
    #[cfg(target_arch = "wasm32")]
    {
        proc_rss()
    }
}

/// Linux's own count, should the call above fail.
fn proc_rss() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let s = std::fs::read_to_string("/proc/self/status").ok()?;
        let kb: u64 = s.lines().find_map(|l| l.strip_prefix("VmRSS:"))?.trim().trim_end_matches("kB").trim().parse().ok()?;
        Some(kb * 1024)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Where [`rss_bytes`] comes from on this platform (for `server_status`).
pub fn rss_source() -> &'static str {
    if cfg!(target_os = "windows") {
        "GetProcessMemoryInfo working set"
    } else if cfg!(target_os = "macos") {
        "task_info resident size"
    } else if cfg!(target_os = "linux") {
        "/proc resident set size"
    } else {
        "operating system"
    }
}

/// A portable estimate of the memory the open document holds: its media, kept parts and text.
pub fn doc_bytes(doc: &Document) -> u64 {
    let media: u64 = doc.media.values().map(|m| m.len() as u64).sum();
    let kept: u64 = doc.passthrough.values().map(|b| b.len() as u64).sum();
    let text: u64 = doc.body.iter().filter_map(|b| b.as_para()).map(|p| p.text.len() as u64 * 8).sum();
    media + kept + text
}

#[cfg(test)]
mod tests {
    #[test]
    fn resident_memory_comes_from_the_operating_system() {
        // This process holds a few megabytes at least; the count isn't an estimate of documents.
        let rss = super::rss_bytes();
        if cfg!(any(target_os = "linux", target_os = "windows", target_os = "macos")) {
            assert!(rss.is_some_and(|b| b > 1024 * 1024), "{rss:?}");
        }
        let grow: Vec<u8> = vec![1; 64 * 1024 * 1024];
        let after = super::rss_bytes();
        assert!(after >= rss, "{rss:?} {after:?}");
        assert_eq!(grow.iter().map(|b| *b as u64).sum::<u64>(), 64 * 1024 * 1024);
    }
}
