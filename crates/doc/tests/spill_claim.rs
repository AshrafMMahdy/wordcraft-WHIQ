//! WHIQ: claiming and sweeping spill folders. A folder is ours once we hold its lock, and a reused
//! PID's leftovers are emptied then; folders of dead processes (lock free, or no lock file and not
//! young) are swept; a folder another handle has locked is never touched. This test sets the spill
//! folder, so it has the binary to itself.
#![cfg(not(target_arch = "wasm32"))]
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use wordcraft_doc::media::{set_spill_base, spill_dir, sweep_stale};

fn lock(dir: &std::path::Path) -> std::fs::File {
    let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(dir.join("lock")).unwrap();
    f.try_lock().unwrap();
    f
}

#[test]
fn claim_empties_our_folder_and_the_sweep_takes_only_dead_folders() {
    let base = std::env::temp_dir().join(format!("wc-spill-claim-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    set_spill_base(Some(base.clone()));

    // A crashed process with our PID left junk (and a lock file nobody holds) in our folder.
    let own = base.join(format!("pid-{}", std::process::id()));
    std::fs::create_dir_all(own.join("sub")).unwrap();
    std::fs::write(own.join("0.part"), b"leftover").unwrap();
    std::fs::write(own.join("7.part"), b"leftover").unwrap();
    std::fs::write(own.join("sub").join("x"), b"leftover").unwrap();
    std::fs::write(own.join("lock"), b"").unwrap();

    // Claiming (the first use of the folder) empties it, keeping only the held lock file.
    assert_eq!(spill_dir(), own);
    let left: Vec<_> = std::fs::read_dir(&own).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(left, vec!["lock".to_string()], "our folder was emptied on claim");

    // Another process, gone: its lock is free.
    let dead = base.join("pid-4000002");
    std::fs::create_dir_all(&dead).unwrap();
    std::fs::write(dead.join("lock"), b"").unwrap();
    std::fs::write(dead.join("0.part"), b"dead").unwrap();
    // Another process, alive: a second handle holds the lock.
    let live = base.join("pid-4000001");
    std::fs::create_dir_all(&live).unwrap();
    let live_lock = lock(&live);
    std::fs::write(live.join("0.part"), b"live").unwrap();
    // A folder with no lock file that was just made (being set up): left alone.
    let young = base.join("pid-4000003");
    std::fs::create_dir_all(&young).unwrap();
    std::fs::write(young.join("0.part"), b"young").unwrap();
    // A folder with no lock file from an older build, an hour old: swept (unix can age a folder).
    #[cfg(unix)]
    let legacy = {
        let legacy = base.join("pid-4000004");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("0.part"), b"legacy").unwrap();
        let hour_ago = std::time::SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::open(&legacy).unwrap().set_modified(hour_ago).unwrap();
        legacy
    };

    let n = sweep_stale(Duration::from_secs(0));
    assert!(!dead.exists(), "a folder whose lock is free is swept");
    #[cfg(unix)]
    {
        assert!(!legacy.exists(), "an old folder with no lock file is swept");
        assert_eq!(n, 2);
    }
    #[cfg(not(unix))]
    assert_eq!(n, 1);
    assert!(live.join("0.part").exists(), "a folder locked by a live handle is kept");
    assert!(young.join("0.part").exists(), "a folder still being made is kept");
    assert!(own.join("lock").exists(), "our own folder is kept");

    // Once the live handle lets go (the process died), the next sweep takes it.
    drop(live_lock);
    sweep_stale(Duration::from_secs(0));
    assert!(!live.exists());
    std::fs::remove_dir_all(&base).unwrap();
}
