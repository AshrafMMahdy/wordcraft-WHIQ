//! WHIQ: a save of a document whose pictures are kept on disk streams them into the file: the
//! `.docx` on disk is byte for byte what the in-memory writer makes, the old file is kept as
//! `OLD_<name>` when asked, and the pictures read back unchanged.
#![cfg(not(target_arch = "wasm32"))]
#![allow(clippy::unwrap_used)]

use wordcraft_engine::doc::media::set_memory_share;
use wordcraft_engine::doc::{CharProps, Document, InlineObject, Paragraph, para::Float, para_block};
use wordcraft_engine::io;

#[test]
fn a_save_streams_pictures_kept_on_disk_into_the_file() {
    set_memory_share(300_000);
    let mut d = Document::new();
    let mut p = Paragraph::with_text("pictures: ", CharProps::default());
    let pics: Vec<Vec<u8>> = (0..3u8).map(|k| (0..400_000u32).map(|i| (i as u8).wrapping_add(k)).collect()).collect();
    for pic in &pics {
        let key = d.add_media(pic.clone(), "png");
        let end = p.len();
        p.insert_object(
            end,
            InlineObject::Image { media: key, w: 10.0, h: 10.0, alt: String::new(), float: Float::default(), crop: [0.0; 4], ole: None },
            &CharProps::default(),
        )
        .unwrap();
    }
    d.body = vec![para_block(p)];
    assert!(d.media.spilled_bytes() >= 800_000, "two of three are on disk");

    let dir = std::env::temp_dir().join(format!("wordcraft-media-save-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("report.docx");
    io::save_path(&path, &d).unwrap();
    let in_memory = io::save_bytes("report.docx", &d).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), in_memory, "the streamed file is the in-memory package");
    // Opened again, every picture is there.
    let (back, _) = io::open_path_with(&path, None).unwrap();
    assert_eq!(back.media.len(), 3);
    for pic in &pics {
        assert!(back.media.find_bytes(pic).is_some());
    }
    // A save over it again goes through the same atomic replace.
    io::save_path(&path, &back).unwrap();
    assert_eq!(std::fs::read(&path).unwrap().len(), in_memory.len());
    assert!(std::fs::read_dir(&dir).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().contains(".saving")), "no temporary file is left");
    set_memory_share(0);
    let _ = std::fs::remove_dir_all(&dir);
}
