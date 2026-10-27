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
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
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

/// The bytes of one picture in a file of the spill folder. The file is removed on drop.
#[derive(Debug)]
pub struct SpillFile {
    path: PathBuf,
    len: u64,
}

static NEXT: AtomicU64 = AtomicU64::new(0);

/// The spill folder (created on first use).
pub fn spill_dir() -> PathBuf {
    match std::env::var_os("WORDCRAFT_SPILL_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => std::env::temp_dir().join("wordcraft-spill"),
    }
}

impl SpillFile {
    /// Copy `src` into a new spill file. At most `max` bytes: a source that holds more is an
    /// error (`InvalidData`), never a silently shortened copy.
    pub fn from_reader(src: &mut dyn Read, max: u64) -> std::io::Result<SpillFile> {
        let dir = spill_dir();
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}-{}.part", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&path)?;
        // Removed again on any failure below (the drop).
        let mut sf = SpillFile { path, len: 0 };
        let mut buf = vec![0u8; 1 << 20];
        let mut limited = src.take(max.saturating_add(1));
        loop {
            let n = limited.read(&mut buf)?;
            if n == 0 {
                break;
            }
            sf.len = sf.len.saturating_add(n as u64);
            if sf.len > max {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("more than the {max} bytes it declared")));
            }
            f.write_all(buf.get(..n).unwrap_or_default())?;
        }
        f.flush()?;
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

    /// The whole file in memory (a caller that has to decode it to draw it).
    pub fn read_all(&self) -> std::io::Result<Vec<u8>> {
        std::fs::read(&self.path)
    }

    /// Stream the bytes into `w`; returns the bytes copied.
    pub fn copy_to(&self, w: &mut dyn Write) -> std::io::Result<u64> {
        let mut f = std::fs::File::open(&self.path)?;
        let n = std::io::copy(&mut f, w)?;
        if n != self.len {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, format!("{} holds {n} of {} bytes", self.path.display(), self.len)));
        }
        Ok(n)
    }

    /// The first `n` bytes (format sniffing).
    pub fn head(&self, n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(n);
        if let Ok(f) = std::fs::File::open(&self.path) {
            let _ = f.take(n as u64).read_to_end(&mut out);
        }
        out
    }
}

impl Drop for SpillFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The process id a spill file's name starts with (`<pid>-<n>.part`).
fn owner(path: &Path) -> Option<u32> {
    path.file_name()?.to_str()?.split('-').next()?.parse().ok()
}

/// Remove spill files left by processes that are gone: older than `age`, or (where the system
/// tells it, Linux) whose process no longer runs. Returns how many.
pub fn sweep_stale(age: std::time::Duration) -> usize {
    let Ok(rd) = std::fs::read_dir(spill_dir()) else { return 0 };
    let me = std::process::id();
    let mut n = 0;
    for e in rd.flatten() {
        let path = e.path();
        if path.extension().is_none_or(|x| x != "part") {
            continue;
        }
        let old = e.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| t.elapsed().ok()).is_some_and(|d| d > age);
        let gone = owner(&path).is_some_and(|pid| pid != me && Path::new("/proc/self").exists() && !Path::new(&format!("/proc/{pid}")).exists());
        if (old || gone) && std::fs::remove_file(&path).is_ok() {
            n += 1;
        }
    }
    n
}

/// Remove this process's spill files (before an exit that skips destructors: a signal).
pub fn remove_own() {
    let Ok(rd) = std::fs::read_dir(spill_dir()) else { return };
    let me = std::process::id();
    for e in rd.flatten() {
        if owner(&e.path()) == Some(me) {
            let _ = std::fs::remove_file(e.path());
        }
    }
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

    /// The bytes in memory (read back from disk for a spilled picture; empty when the spill file
    /// cannot be read, which is logged).
    fn load(&self) -> Arc<Vec<u8>> {
        match self {
            Blob::Mem(b) => b.clone(),
            Blob::Disk(f) => match f.read_all() {
                Ok(v) => Arc::new(v),
                Err(e) => {
                    log::error!("a picture kept on disk could not be read back ({}): {e}", f.path().display());
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
            Blob::Disk(f) => Ok(Box::new(std::io::BufReader::new(std::fs::File::open(f.path())?))),
        }
    }

    /// The whole picture in memory (read back from disk when it is kept there).
    pub fn bytes(&self) -> Arc<Vec<u8>> {
        self.blob.load()
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
