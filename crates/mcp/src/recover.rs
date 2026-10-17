//! WHIQ: work is never lost. On eviction (memory pressure, the client going away), a panic or
//! SIGTERM, the open document is saved to `<name>.recovered-<stamp>.docx` inside the write root
//! (the document's folder, or the working directory, without one), and a marker
//! (`.wordcraft-recovered.json`) names it so the next call (in this process or the next one
//! started on the same root) answers `evicted` with the path and the step to take.

use std::path::{Path, PathBuf};

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

/// Save `snap` as `<stem>.recovered-<stamp>.docx` in `dir` (never over another file) and record
/// it in the marker. Returns the recovered file's path.
pub fn autosave(snap: &Snapshot, dir: &Path, reason: &str) -> Result<PathBuf, String> {
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
    // A recovery copy never replaces anything and keeps nothing as OLD_.
    let (saved, _) = wordcraft_engine::file_access::with_overwrite(wordcraft_engine::file_access::Overwrite::Replace, || {
        wordcraft_engine::io::save_path(&path, &snap.doc)
    });
    saved?;
    let entry = json!({
        "path": path.to_string_lossy(),
        "reason": reason,
        "source": snap.path.as_ref().map(|p| p.to_string_lossy().into_owned()),
        "at": wordcraft_engine::cmd::now_iso(),
    });
    let marker = dir.join(MARKER);
    let mut list = read_marker(dir);
    list.push(entry);
    let _ = std::fs::write(&marker, serde_json::to_vec_pretty(&Value::Array(list)).unwrap_or_default());
    eprintln!("wordcraft-cli: work auto-saved ({reason}) at {}", path.display());
    Ok(path)
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

/// Resident memory of this process in bytes, where the platform tells it cheaply.
pub fn rss_bytes() -> Option<u64> {
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

/// A portable estimate of the memory the open document holds: its media, kept parts and text.
pub fn doc_bytes(doc: &Document) -> u64 {
    let media: u64 = doc.media.values().map(|m| m.len() as u64).sum();
    let kept: u64 = doc.passthrough.values().map(|b| b.len() as u64).sum();
    let text: u64 = doc.body.iter().filter_map(|b| b.as_para()).map(|p| p.text.len() as u64 * 8).sum();
    media + kept + text
}
