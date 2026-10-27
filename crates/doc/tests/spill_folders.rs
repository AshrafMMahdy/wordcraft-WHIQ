//! WHIQ: spill folders belong to a process. A second process starting up never deletes the pictures
//! of one that is alive, a cleaner deleting a file does not take the picture, and a picture that
//! cannot be read back is said, never read as empty. This test sets the spill folder, so it has the
//! binary to itself.
#![cfg(not(target_arch = "wasm32"))]
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use wordcraft_doc::media::{MediaMap, SpillFile, set_memory_share, set_spill_base, spill_base, spill_dir, sweep_stale};

#[test]
fn a_live_process_keeps_its_spill_files_and_a_dead_ones_go() {
    let base = std::env::temp_dir().join(format!("wc-spill-folders-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    set_spill_base(Some(base.clone()));
    assert_eq!(spill_base(), base);

    // Another process, alive: a folder with its lock file held, and a file in it, old.
    let live = base.join("pid-4000001");
    std::fs::create_dir_all(&live).unwrap();
    let live_lock = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(live.join("lock")).unwrap();
    live_lock.try_lock().unwrap();
    std::fs::write(live.join("0.part"), b"a live picture").unwrap();
    // Another process, gone: a folder whose lock nobody holds.
    let dead = base.join("pid-4000002");
    std::fs::create_dir_all(&dead).unwrap();
    std::fs::write(dead.join("lock"), b"").unwrap();
    std::fs::write(dead.join("0.part"), b"a dead one's picture").unwrap();

    // Our own folder, with a picture.
    set_memory_share(300_000);
    let mut m = MediaMap::new();
    let pic: Vec<u8> = (0..400_000u32).map(|i| i as u8).collect();
    m.insert("a.png".into(), Arc::new(pic.clone()));
    assert!(m.is_spilled("a.png"));
    assert!(spill_dir().starts_with(&base) && spill_dir().ends_with(format!("pid-{}", std::process::id())));

    let n = sweep_stale(Duration::from_secs(0));
    assert_eq!(n, 1, "only the dead process's file");
    assert!(live.join("0.part").exists(), "a live process's picture is never swept, whatever its age");
    assert!(!dead.exists());
    assert_eq!(m.get("a.png").unwrap().as_slice(), pic.as_slice(), "ours is untouched");

    // A cleaner deletes our file behind our back: the handle we hold still reads it.
    #[cfg(unix)]
    {
        for e in std::fs::read_dir(spill_dir()).unwrap().flatten() {
            if e.path().extension().is_some_and(|x| x == "part") {
                std::fs::remove_file(e.path()).unwrap();
            }
        }
        assert_eq!(m.get("a.png").unwrap().as_slice(), pic.as_slice());
        assert!(m.load_failure().is_none());
    }

    // A file cut short is said, and refuses the saves; it is not an empty picture.
    let mut m2 = MediaMap::new();
    m2.insert("b.png".into(), Arc::new(pic.clone()));
    let path = spill_dir().read_dir().unwrap().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "part")).max().unwrap();
    std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(10).unwrap();
    assert!(m2.get("b.png").unwrap().is_empty());
    assert!(m2.load_failure().is_some_and(|w| w.contains("b.png")), "{:?}", m2.load_failure());
    let mut sink = Vec::new();
    assert!(m2.copy_to("b.png", &mut sink).is_err());
    let _ = SpillFile::from_reader(&mut &pic[..], 10).unwrap_err();
    drop(live_lock);
    set_memory_share(0);
    std::fs::remove_dir_all(&base).unwrap();
}
