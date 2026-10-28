//! WHIQ: a document's embedded pictures, kept in memory up to a share of the memory budget and
//! on disk beyond it.
//!
//! The owner's rule: no tool is limited by file size. A report with 600 MB of photographs does
//! not have to hold them in memory: a picture that would take the document past its share of the
//! budget is inflated straight from the zip into a [`SpillFile`] in the spill folder, read back
//! only while it is drawn or exported, and streamed from there into every file saved. The
//! document model sees the same `get` / `insert` / `iter` of a map of shared byte buffers.
//!
//! The spill folder is `WORDCRAFT_SPILL_DIR` when set, else `wordcraft-spill` in the system's
//! temporary folder. A spill file is removed when the last document holding it is dropped; files a
//! killed process left behind are removed by [`sweep_stale`] (the MCP server runs it at start).

use std::collections::BTreeMap;
use std::fmt;
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// Pictures under this size always stay in memory (they cost less than the file handling does).
pub const SPILL_MIN: u64 = 256 << 10;

static SHARE: AtomicU64 = AtomicU64::new(0);

/// The share of the memory budget a document's pictures may take in memory (an eighth), at least
/// 32 MiB, unless [`set_memory_share`] fixed it; what comes after goes to disk.
pub fn memory_share() -> u64 {
    match SHARE.load(Ordering::Relaxed) {
        0 => crate::memory::share(32 << 20, 8),
        s => s,
    }
}

/// Fix the bytes of pictures a document may hold in memory (0 goes back to the share of the
/// budget). Tests use it to reach the disk route with small pictures.
pub fn set_memory_share(bytes: u64) {
    SHARE.store(bytes, Ordering::Relaxed);
}

/// The bytes of one picture in a file of the spill folder. The file is removed on drop. It stays
/// OPEN while the picture is cold: every read goes through that handle, so a cleaner that deletes
/// the file does not take the picture; a read that fails is remembered (`failure`) and the saves and
/// exports refuse (never a picture read as empty).
#[derive(Debug)]
pub struct SpillFile {
    path: PathBuf,
    file: Mutex<std::fs::File>,
    len: u64,
    failure: Mutex<Option<String>>,
}

static NEXT: AtomicU64 = AtomicU64::new(0);

/// The spill folder of THIS process: a folder of its own (`pid-<pid>`) under the spill base, which
/// is `WORDCRAFT_SPILL_DIR`, else `wordcraft/spill` in the user's data folder (not the temporary
/// folder, which cleaners empty), else the temporary folder. A lock file in it is held open (and
/// locked) for as long as the process lives: that is what tells another process it is in use. When
/// another process holds that lock (a sweeper, for a moment) the claim waits, then takes
/// `pid-<pid>-<random>`.
pub fn spill_dir() -> PathBuf {
    let base = spill_base();
    let pid = std::process::id();
    match CLAIM.get_or_init(|| {
        let waits: Vec<_> = CLAIM_WAITS_MS.iter().map(|ms| std::time::Duration::from_millis(*ms)).collect();
        claim_folder(&base, pid, &waits)
    }) {
        Some((dir, _)) => dir.clone(),
        None => base.join(format!("pid-{pid}")),
    }
}

static BASE: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Fix the folder all processes' spill folders are in (before the first picture is kept on disk).
pub fn set_spill_base(base: Option<PathBuf>) {
    if let Ok(mut b) = BASE.lock() {
        *b = base;
    }
}

/// The folder all processes' spill folders are in.
pub fn spill_base() -> PathBuf {
    if let Some(b) = BASE.lock().ok().and_then(|b| b.clone()) {
        return b;
    }
    match std::env::var_os("WORDCRAFT_SPILL_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => {
            let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
            let data = var("LOCALAPPDATA").or_else(|| var("XDG_DATA_HOME")).or_else(|| var("HOME").map(|h| h.join(".local").join("share")));
            data.unwrap_or_else(std::env::temp_dir).join("wordcraft").join("spill")
        }
    }
}

const LOCK_NAME: &str = "lock";

/// The folder this process claimed and the lock on it, held until it exits.
static CLAIM: std::sync::OnceLock<Option<(PathBuf, std::fs::File)>> = std::sync::OnceLock::new();

/// Waits between tries to take the lock of `pid-<pid>` when someone else holds it for a moment (a
/// sweeper looking at the folder of a dead process that had our PID).
const CLAIM_WAITS_MS: [u64; 7] = [10, 20, 40, 80, 160, 320, 640];

/// Claim a spill folder for process `pid` under `base`: `pid-<pid>` when its lock can be taken
/// (retrying after each of `waits`, the holder being a sweeper that lets go in a moment), else a
/// fresh `pid-<pid>-<random>`. The folder is emptied once locked (what is in it is a crashed
/// process's). A process never runs without the lock of its own folder.
fn claim_folder(base: &Path, pid: u32, waits: &[std::time::Duration]) -> Option<(PathBuf, std::fs::File)> {
    let lock_in = |dir: &Path| -> Option<std::fs::File> {
        std::fs::create_dir_all(dir).ok()?;
        let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(dir.join(LOCK_NAME)).ok()?;
        f.try_lock().ok()?;
        empty_folder(dir);
        Some(f)
    };
    let own = base.join(format!("pid-{pid}"));
    for wait in std::iter::once(&std::time::Duration::ZERO).chain(waits) {
        std::thread::sleep(*wait);
        if let Some(f) = lock_in(&own) {
            return Some((own, f));
        }
    }
    static SALT: AtomicU64 = AtomicU64::new(0);
    for _ in 0..16 {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
        let r = nanos ^ SALT.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed).rotate_left(17);
        let dir = base.join(format!("pid-{pid}-{:08x}", r as u32));
        if let Some(f) = lock_in(&dir) {
            return Some((dir, f));
        }
    }
    None
}

/// Remove everything in `dir` but the lock file: what a crashed process with the same PID left.
fn empty_folder(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        if e.file_name() == LOCK_NAME {
            continue;
        }
        let p = e.path();
        let _ = if e.file_type().is_ok_and(|t| t.is_dir()) { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
    }
}

impl SpillFile {
    /// Copy `src` into a new spill file. At most `max` bytes: a source that holds more is an
    /// error (`InvalidData`), never a silently shortened copy.
    pub fn from_reader(src: &mut dyn Read, max: u64) -> std::io::Result<SpillFile> {
        let dir = spill_dir();
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.part", NEXT.fetch_add(1, Ordering::Relaxed)));
        let f = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&path)?;
        // Removed again on any failure below (the drop).
        let mut sf = SpillFile { path, file: Mutex::new(f), len: 0, failure: Mutex::new(None) };
        let mut buf = vec![0u8; 1 << 20];
        let mut limited = src.take(max.saturating_add(1));
        {
            let f = sf.file.get_mut().map_err(|_| std::io::Error::other("lock poisoned"))?;
            let mut len = 0u64;
            loop {
                let n = limited.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                len = len.saturating_add(n as u64);
                if len > max {
                    return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("more than the {max} bytes it declared")));
                }
                f.write_all(buf.get(..n).unwrap_or_default())?;
            }
            f.flush()?;
            sf.len = len;
        }
        Ok(sf)
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Why a read of this picture failed, if one did.
    pub fn failure(&self) -> Option<String> {
        self.failure.lock().ok().and_then(|f| f.clone())
    }

    fn fail(&self, e: &std::io::Error) {
        if let Ok(mut f) = self.failure.lock() {
            f.get_or_insert(format!("a picture kept on disk could not be read back ({}): {e}", self.path.display()));
        }
    }

    /// The file at its start, under the lock.
    fn rewound(&self) -> std::io::Result<std::sync::MutexGuard<'_, std::fs::File>> {
        let mut g = self.file.lock().map_err(|_| std::io::Error::other("lock poisoned"))?;
        g.seek(std::io::SeekFrom::Start(0))?;
        Ok(g)
    }

    /// The whole file in memory (a caller that has to decode it to draw it).
    pub fn read_all(&self) -> std::io::Result<Vec<u8>> {
        let mut g = self.rewound()?;
        let mut v = Vec::with_capacity(self.len.min(1 << 30) as usize);
        (&mut *g).take(self.len.saturating_add(1)).read_to_end(&mut v)?;
        if v.len() as u64 != self.len {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("{} holds {} of {} bytes", self.path.display(), v.len(), self.len),
            ));
        }
        Ok(v)
    }

    /// Stream the bytes into `w`; returns the bytes copied.
    pub fn copy_to(&self, w: &mut dyn Write) -> std::io::Result<u64> {
        let mut g = self.rewound()?;
        let n = std::io::copy(&mut (&mut *g).take(self.len), w)?;
        if n != self.len {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, format!("{} holds {n} of {} bytes", self.path.display(), self.len)));
        }
        Ok(n)
    }

    /// A reader on the bytes, from the start.
    fn reader(&self) -> std::io::Result<SpillReader<'_>> {
        let g = self.rewound()?;
        Ok(SpillReader { g })
    }

    /// The first `n` bytes (format sniffing). Short when the file cannot be read (see `failure`).
    pub fn head(&self, n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(n);
        match self.rewound() {
            Ok(mut g) => {
                if let Err(e) = (&mut *g).take(n as u64).read_to_end(&mut out) {
                    self.fail(&e);
                }
            }
            Err(e) => self.fail(&e),
        }
        out
    }
}

/// A reader on a spill file under its lock.
struct SpillReader<'a> {
    g: std::sync::MutexGuard<'a, std::fs::File>,
}

impl Read for SpillReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.g.read(buf)
    }
}

impl std::io::Seek for SpillReader<'_> {
    fn seek(&mut self, to: std::io::SeekFrom) -> std::io::Result<u64> {
        self.g.seek(to)
    }
}

impl Drop for SpillFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Remove the spill folders of processes that are gone: a folder whose lock file nobody holds. A
/// folder of a live process (its lock is held) is never touched, whatever its age; one with no lock
/// file yet is left for a minute (it is being made). Files of the old layout (`<pid>-<n>.part` in the
/// base folder) go when the system tells their process is gone (Linux). Returns how many files.
pub fn sweep_stale(_age: std::time::Duration) -> usize {
    let base = spill_base();
    let Ok(rd) = std::fs::read_dir(&base) else { return 0 };
    let mine = spill_dir();
    let mut n = 0;
    for e in rd.flatten() {
        let path = e.path();
        if path == mine {
            continue;
        }
        if path.is_dir() {
            // `held` keeps the lock while the folder goes; `free` says the folder is nobody's.
            let mut held = None;
            let free = match std::fs::OpenOptions::new().read(true).write(true).open(path.join(LOCK_NAME)) {
                // Free when we can take the lock.
                Ok(f) => {
                    let got = f.try_lock().is_ok();
                    held = got.then_some(f);
                    got
                }
                // No lock file (a folder of an older build, or one still being made): free once it
                // is not young. (A directory cannot be opened as a file on Windows, so no handle.)
                Err(_) => !e
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.elapsed().ok())
                    .is_none_or(|d| d < std::time::Duration::from_secs(60)),
            };
            if free {
                n += std::fs::read_dir(&path).map(|r| r.flatten().filter(|x| x.file_name() != LOCK_NAME).count()).unwrap_or(0);
                drop(held);
                let _ = std::fs::remove_dir_all(&path);
            }
        } else if path.extension().is_some_and(|x| x == "part") {
            let pid: Option<u32> = path.file_name().and_then(|f| f.to_str()).and_then(|f| f.split('-').next()).and_then(|p| p.parse().ok());
            let gone = pid.is_some_and(|pid| Path::new("/proc/self").exists() && !Path::new(&format!("/proc/{pid}")).exists());
            if gone && std::fs::remove_file(&path).is_ok() {
                n += 1;
            }
        }
    }
    n
}

/// Remove this process's spill folder (before an exit that skips destructors: a signal).
pub fn remove_own() {
    let _ = std::fs::remove_dir_all(spill_dir());
}

/// One picture: in memory or on disk.
#[derive(Clone)]
enum Blob {
    Mem(Arc<Vec<u8>>),
    Disk(Arc<SpillFile>),
}

impl Blob {
    fn len(&self) -> u64 {
        match self {
            Blob::Mem(b) => b.len() as u64,
            Blob::Disk(f) => f.len(),
        }
    }

    /// The bytes in memory (read back from disk for a spilled picture). A spill file that cannot be
    /// read gives NO bytes and the failure is kept on the file ([`SpillFile::failure`]): the saves
    /// and exports ask [`MediaMap::load_failure`] and refuse, so a picture is never lost in silence.
    fn load(&self) -> Arc<Vec<u8>> {
        match self {
            Blob::Mem(b) => b.clone(),
            Blob::Disk(f) => match f.read_all() {
                Ok(v) => Arc::new(v),
                Err(e) => {
                    log::error!("a picture kept on disk could not be read back ({}): {e}", f.path().display());
                    f.fail(&e);
                    Arc::new(Vec::new())
                }
            },
        }
    }
}

/// What a reader of a picture must be: bytes it can read and seek in.
pub trait ReadSeek: Read + std::io::Seek {}
impl<T: Read + std::io::Seek> ReadSeek for T {}

/// One picture of a document, wherever it is kept: its bytes are not read until they are asked for,
/// and a picture on disk can be copied out ([`Picture::copy_to`]) or read from ([`Picture::open`]) a
/// piece at a time, so an export writes any number of pictures holding none whole.
#[derive(Clone)]
pub struct Picture {
    blob: Blob,
}

impl Picture {
    pub fn len(&self) -> u64 {
        self.blob.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Is it kept on disk?
    pub fn is_spilled(&self) -> bool {
        matches!(self.blob, Blob::Disk(_))
    }

    /// The first `n` bytes (format sniffing).
    pub fn head(&self, n: usize) -> Vec<u8> {
        match &self.blob {
            Blob::Mem(b) => b.get(..n.min(b.len())).unwrap_or_default().to_vec(),
            Blob::Disk(f) => f.head(n),
        }
    }

    /// Stream the bytes into `w`; returns the bytes copied.
    pub fn copy_to(&self, w: &mut dyn Write) -> std::io::Result<u64> {
        match &self.blob {
            Blob::Mem(b) => {
                w.write_all(b)?;
                Ok(b.len() as u64)
            }
            Blob::Disk(f) => f.copy_to(w),
        }
    }

    /// A reader on the bytes (a file on disk is read as it goes, not into memory).
    pub fn open(&self) -> std::io::Result<Box<dyn ReadSeek + '_>> {
        match &self.blob {
            Blob::Mem(b) => Ok(Box::new(std::io::Cursor::new(b.as_slice()))),
            Blob::Disk(f) => Ok(Box::new(std::io::BufReader::new(f.reader()?))),
        }
    }

    /// The whole picture in memory (read back from disk when it is kept there). A picture that
    /// cannot be read gives no bytes: see [`Picture::failure`].
    pub fn bytes(&self) -> Arc<Vec<u8>> {
        self.blob.load()
    }

    /// Note that reading this picture failed (an exporter that read it a piece at a time found it
    /// short), so [`MediaMap::load_failure`] says so and the save is refused.
    pub fn report_failure(&self, e: &std::io::Error) {
        if let Blob::Disk(f) = &self.blob {
            f.fail(e);
        }
    }

    /// Why the picture could not be read back from disk, if it could not.
    pub fn failure(&self) -> Option<String> {
        match &self.blob {
            Blob::Disk(f) => f.failure(),
            Blob::Mem(_) => None,
        }
    }
}

impl fmt::Debug for Picture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Picture({} bytes{})", self.len(), if self.is_spilled() { ", on disk" } else { "" })
    }
}

impl PartialEq for Picture {
    fn eq(&self, other: &Self) -> bool {
        match (&self.blob, &other.blob) {
            (Blob::Mem(x), Blob::Mem(y)) => Arc::ptr_eq(x, y) || x == y,
            (Blob::Disk(x), Blob::Disk(y)) if Arc::ptr_eq(x, y) => true,
            (a, b) => a.len() == b.len() && a.load() == b.load(),
        }
    }
}

/// A handle on one picture that tells whether it is still the same one: caches of decoded
/// pictures key on it. It keeps the picture's allocation (or spill file) alive, so its identity
/// cannot be reused while a cache holds it.
#[derive(Clone)]
pub struct Pin {
    ident: usize,
    hold: Blob,
}

impl Pin {
    /// Bytes this handle keeps in memory (none for a picture on disk).
    pub fn memory_len(&self) -> usize {
        match &self.hold {
            Blob::Mem(b) => b.len(),
            Blob::Disk(_) => 0,
        }
    }
}

impl PartialEq for Pin {
    fn eq(&self, other: &Self) -> bool {
        self.ident == other.ident
    }
}

/// The pictures of a document by key.
#[derive(Clone, Default)]
pub struct MediaMap {
    items: BTreeMap<String, Blob>,
    /// Bytes of the pictures held in memory (not those on disk).
    mem: u64,
}

impl MediaMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.items.contains_key(key)
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.items.keys()
    }

    /// Bytes of pictures held in memory (the rest are on disk).
    pub fn memory_bytes(&self) -> u64 {
        self.mem
    }

    /// Bytes of pictures kept on disk.
    pub fn spilled_bytes(&self) -> u64 {
        self.items.values().map(|b| if let Blob::Disk(f) = b { f.len() } else { 0 }).sum()
    }

    /// The picture's bytes, read back from disk when it is kept there (one picture at a time:
    /// the caller drops them after drawing or encoding it).
    pub fn get(&self, key: &str) -> Option<Arc<Vec<u8>>> {
        self.items.get(key).map(Blob::load)
    }

    /// The picture itself, for reading it a piece at a time (see [`Picture`]), without reading it.
    pub fn picture(&self, key: &str) -> Option<Picture> {
        self.items.get(key).map(|b| Picture { blob: b.clone() })
    }

    /// A handle that identifies this picture as it is now (see [`Pin`]), without reading it.
    pub fn pin(&self, key: &str) -> Option<Pin> {
        let b = self.items.get(key)?;
        let ident = match b {
            Blob::Mem(m) => Arc::as_ptr(m) as usize,
            Blob::Disk(f) => Arc::as_ptr(f) as usize,
        };
        Some(Pin { ident, hold: b.clone() })
    }

    /// Why a picture kept on disk could not be read back, if one could not (its key and the
    /// reason). A save or export of the document checks this and refuses: a picture is never
    /// written as empty.
    pub fn load_failure(&self) -> Option<String> {
        self.items.iter().find_map(|(k, b)| match b {
            Blob::Disk(f) => f.failure().map(|why| format!("picture {k}: {why}")),
            Blob::Mem(_) => None,
        })
    }

    /// The size of a picture, without reading it.
    pub fn byte_len(&self, key: &str) -> Option<u64> {
        self.items.get(key).map(Blob::len)
    }

    /// Is the picture kept on disk?
    pub fn is_spilled(&self, key: &str) -> bool {
        matches!(self.items.get(key), Some(Blob::Disk(_)))
    }

    /// The first `n` bytes of a picture (format sniffing), without reading a spilled one whole.
    pub fn head(&self, key: &str, n: usize) -> Option<Vec<u8>> {
        match self.items.get(key)? {
            Blob::Mem(b) => Some(b.get(..n.min(b.len())).unwrap_or_default().to_vec()),
            Blob::Disk(f) => Some(f.head(n)),
        }
    }

    /// Stream a picture into `w` (a spilled one is never held whole); returns the bytes copied.
    pub fn copy_to(&self, key: &str, w: &mut dyn Write) -> std::io::Result<u64> {
        match self.items.get(key) {
            Some(Blob::Mem(b)) => {
                w.write_all(b)?;
                Ok(b.len() as u64)
            }
            Some(Blob::Disk(f)) => f.copy_to(w),
            None => Err(std::io::Error::new(std::io::ErrorKind::NotFound, format!("no picture {key}"))),
        }
    }

    fn remove_blob(&mut self, key: &str) {
        if let Some(Blob::Mem(b)) = self.items.remove(key) {
            self.mem = self.mem.saturating_sub(b.len() as u64);
        }
    }

    /// Add a picture held in memory. One that would take the pictures in memory past
    /// [`memory_share`] is kept on disk instead (the document reads the same).
    pub fn insert(&mut self, key: String, bytes: Arc<Vec<u8>>) {
        self.remove_blob(&key);
        let len = bytes.len() as u64;
        if len >= SPILL_MIN && self.mem.saturating_add(len) > memory_share() {
            let mut r: &[u8] = &bytes;
            match SpillFile::from_reader(&mut r, len) {
                Ok(f) => {
                    self.items.insert(key, Blob::Disk(Arc::new(f)));
                    return;
                }
                // No disk to spill to: keep it in memory (the memory check reports the pressure).
                Err(e) => log::warn!("a picture could not be kept on disk, held in memory: {e}"),
            }
        }
        self.mem = self.mem.saturating_add(len);
        self.items.insert(key, Blob::Mem(bytes));
    }

    /// Add a picture of `size` bytes (declared; at most `max` are read) streamed from `src`:
    /// straight to disk when it would take the pictures in memory past [`memory_share`], so a
    /// picture of any size is never held whole.
    pub fn insert_reader(&mut self, key: String, src: &mut dyn Read, size: u64, max: u64) -> std::io::Result<()> {
        if size >= SPILL_MIN && self.mem.saturating_add(size) > memory_share() {
            let f = SpillFile::from_reader(src, max)?;
            self.remove_blob(&key);
            self.items.insert(key, Blob::Disk(Arc::new(f)));
            return Ok(());
        }
        let mut buf = Vec::with_capacity(size.min(16 << 20) as usize);
        let n = src.take(max.saturating_add(1)).read_to_end(&mut buf)?;
        if n as u64 > max {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("more than the {max} bytes it declared")));
        }
        self.insert(key, Arc::new(buf));
        Ok(())
    }

    /// The key of a picture with exactly these bytes (a spilled one is compared as a stream).
    pub fn find_bytes(&self, bytes: &[u8]) -> Option<&String> {
        self.items.iter().find_map(|(k, b)| {
            if b.len() != bytes.len() as u64 {
                return None;
            }
            match b {
                Blob::Mem(m) => (m.as_slice() == bytes).then_some(k),
                Blob::Disk(f) => f.read_all().ok().filter(|v| v.as_slice() == bytes).map(|_| k),
            }
        })
    }

    pub fn iter(&self) -> Iter<'_> {
        Iter { inner: self.items.iter() }
    }

    pub fn values(&self) -> impl Iterator<Item = Arc<Vec<u8>>> + '_ {
        self.items.values().map(Blob::load)
    }

    /// The sizes of the pictures by key, without reading them.
    pub fn sizes(&self) -> impl Iterator<Item = (&String, u64)> + '_ {
        self.items.iter().map(|(k, b)| (k, b.len()))
    }
}

/// `(key, bytes)` of each picture, a spilled one read back as it is reached.
pub struct Iter<'a> {
    inner: std::collections::btree_map::Iter<'a, String, Blob>,
}

impl<'a> Iterator for Iter<'a> {
    type Item = (&'a String, Arc<Vec<u8>>);
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|(k, b)| (k, b.load()))
    }
}

impl<'a> IntoIterator for &'a MediaMap {
    type Item = (&'a String, Arc<Vec<u8>>);
    type IntoIter = Iter<'a>;
    fn into_iter(self) -> Iter<'a> {
        self.iter()
    }
}

impl Extend<(String, Arc<Vec<u8>>)> for MediaMap {
    fn extend<T: IntoIterator<Item = (String, Arc<Vec<u8>>)>>(&mut self, iter: T) {
        for (k, v) in iter {
            self.insert(k, v);
        }
    }
}

impl FromIterator<(String, Arc<Vec<u8>>)> for MediaMap {
    fn from_iter<T: IntoIterator<Item = (String, Arc<Vec<u8>>)>>(iter: T) -> Self {
        let mut m = MediaMap::new();
        m.extend(iter);
        m
    }
}

impl PartialEq for MediaMap {
    fn eq(&self, other: &Self) -> bool {
        self.items.len() == other.items.len()
            && self.items.iter().all(|(k, a)| {
                other.items.get(k).is_some_and(|b| match (a, b) {
                    (Blob::Mem(x), Blob::Mem(y)) => Arc::ptr_eq(x, y) || x == y,
                    (Blob::Disk(x), Blob::Disk(y)) if Arc::ptr_eq(x, y) => true,
                    _ => a.len() == b.len() && a.load() == b.load(),
                })
            })
    }
}

impl fmt::Debug for MediaMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut m = f.debug_map();
        for (k, b) in &self.items {
            m.entry(k, &format_args!("{} bytes{}", b.len(), if matches!(b, Blob::Disk(_)) { " (on disk)" } else { "" }));
        }
        m.finish()
    }
}

impl serde::Serialize for MediaMap {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_map(self.iter())
    }
}

impl<'de> serde::Deserialize<'de> for MediaMap {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let m = BTreeMap::<String, Arc<Vec<u8>>>::deserialize(d)?;
        Ok(m.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_claim_while_another_handle_holds_the_lock_still_gets_a_locked_folder() {
        let base = std::env::temp_dir().join(format!("wc-claim-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let pid = 4_000_010;
        let own = base.join(format!("pid-{pid}"));
        std::fs::create_dir_all(&own).unwrap();
        std::fs::write(own.join("junk.part"), b"x").unwrap();
        let hold = || {
            let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(own.join(LOCK_NAME)).unwrap();
            f.try_lock().unwrap();
            f
        };
        // A sweeper holds the lock for good: no wait helps, so the claim takes a fresh folder, locked.
        let sweeper = hold();
        let (dir, held) = claim_folder(&base, pid, &[std::time::Duration::from_millis(5)]).unwrap();
        assert_ne!(dir, own);
        assert!(dir.file_name().unwrap().to_string_lossy().starts_with("pid-4000010-"));
        let second = std::fs::OpenOptions::new().read(true).write(true).open(dir.join(LOCK_NAME)).unwrap();
        assert!(matches!(second.try_lock(), Err(std::fs::TryLockError::WouldBlock)), "the fresh folder is locked by the claimer");
        assert!(own.join("junk.part").exists(), "the folder the other handle holds is left alone");
        drop((held, second));
        // The sweeper lets go in a moment: the retry gets pid-<pid> itself, emptied.
        let t = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            drop(sweeper);
        });
        let waits = [std::time::Duration::from_millis(50); 40];
        let (dir, held) = claim_folder(&base, pid, &waits).unwrap();
        t.join().unwrap();
        assert_eq!(dir, own);
        assert!(!own.join("junk.part").exists());
        drop(held);
        std::fs::remove_dir_all(&base).unwrap();
    }

    fn bytes(n: usize, v: u8) -> Arc<Vec<u8>> {
        Arc::new(vec![v; n])
    }

    /// The share is process-wide: the tests that fix it run one at a time.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn pictures_past_the_share_go_to_disk_and_read_back_the_same() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        set_memory_share(3 << 20);
        let mut m = MediaMap::new();
        m.insert("a.png".into(), bytes(2 << 20, 1));
        assert!(!m.is_spilled("a.png"));
        // 2 + 2 MiB is past the 3 MiB share: the second one goes to disk.
        m.insert("b.png".into(), bytes(2 << 20, 2));
        assert!(m.is_spilled("b.png") && !m.is_spilled("a.png"));
        assert_eq!((m.memory_bytes(), m.spilled_bytes()), (2 << 20, 2 << 20));
        assert_eq!(m.get("b.png").unwrap().first(), Some(&2));
        assert_eq!(m.byte_len("b.png"), Some(2 << 20));
        assert_eq!(m.head("b.png", 4).unwrap(), vec![2, 2, 2, 2]);
        let mut sink = Vec::new();
        assert_eq!(m.copy_to("b.png", &mut sink).unwrap(), 2 << 20);
        assert!(sink.iter().all(|b| *b == 2));
        // Small pictures stay in memory whatever the share.
        m.insert("c.png".into(), bytes(1000, 3));
        assert!(!m.is_spilled("c.png"));
        // A clone shares the file; the file goes with the last holder.
        let Some(Blob::Disk(f)) = m.items.get("b.png") else { panic!("b.png is on disk") };
        let path = f.path().to_path_buf();
        let copy = m.clone();
        assert_eq!(copy, m);
        drop(m);
        assert!(path.exists(), "the clone still holds it");
        drop(copy);
        assert!(!path.exists(), "the last holder removes it");
        set_memory_share(0);
    }

    #[test]
    fn a_stream_is_spilled_without_being_held_and_a_lying_size_is_an_error() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        set_memory_share(3 << 20);
        let mut m = MediaMap::new();
        m.insert("a.png".into(), bytes(2 << 20, 1));
        let data = vec![9u8; 2 << 20];
        let mut r = data.as_slice();
        m.insert_reader("big.png".into(), &mut r, data.len() as u64, data.len() as u64).unwrap();
        assert!(m.is_spilled("big.png"));
        assert_eq!(m.get("big.png").unwrap().as_slice(), data.as_slice());
        assert_eq!(m.find_bytes(&data).map(String::as_str), Some("big.png"));
        let mut r = data.as_slice();
        assert!(m.insert_reader("liar.png".into(), &mut r, 10, 10).is_err(), "more than it declared");
        assert!(!m.contains_key("liar.png"));
        set_memory_share(0);
    }

    #[test]
    fn json_shape_is_a_map_of_bytes() {
        let mut m = MediaMap::new();
        m.insert("x.png".into(), Arc::new(vec![1, 2, 3]));
        let j = serde_json::to_string(&m).unwrap();
        assert_eq!(j, r#"{"x.png":[1,2,3]}"#);
        let back: MediaMap = serde_json::from_str(&j).unwrap();
        assert_eq!(back, m);
    }
}
