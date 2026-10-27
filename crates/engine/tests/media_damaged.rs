//! WHIQ: a picture whose spill file was damaged is not saved as a short or empty picture, in any
//! format. This test damages the spill files of the process, so it has the binary to itself.
#![cfg(not(target_arch = "wasm32"))]
#![allow(clippy::unwrap_used)]

use wordcraft_engine::doc::media::set_memory_share;
use wordcraft_engine::doc::{CharProps, Document, InlineObject, Paragraph, para::Float, para_block};
use wordcraft_engine::io;

/// A picture whose spill file was damaged is not saved as an empty picture, in any format.
#[test]
fn a_picture_that_cannot_be_read_back_refuses_the_save() {
    set_memory_share(300_000);
    let mut d = Document::new();
    let mut p = Paragraph::with_text("pictures: ", CharProps::default());
    for k in 0..2u8 {
        let pic: Vec<u8> = (0..400_000u32).map(|i| (i as u8).wrapping_add(k)).collect();
        let key = d.add_media(pic, "png");
        let end = p.len();
        p.insert_object(
            end,
            InlineObject::Image { media: key, w: 10.0, h: 10.0, alt: String::new(), float: Float::default(), crop: [0.0; 4], ole: None },
            &CharProps::default(),
        )
        .unwrap();
    }
    d.body = vec![para_block(p)];
    let spilled: Vec<_> = d.media.keys().filter(|k| d.media.is_spilled(k)).cloned().collect();
    assert!(!spilled.is_empty());
    // Cut the spill file short, behind the document's back.
    let dir = wordcraft_engine::doc::media::spill_dir();
    for e in std::fs::read_dir(&dir).unwrap().flatten() {
        if e.path().extension().is_some_and(|x| x == "part") {
            std::fs::OpenOptions::new().write(true).open(e.path()).unwrap().set_len(5).unwrap();
        }
    }
    let out = std::env::temp_dir().join(format!("wordcraft-media-damaged-{}", std::process::id()));
    std::fs::create_dir_all(&out).unwrap();
    for name in ["a.docx", "a.odt", "a.rtf", "a.html", "a.pdf"] {
        let target = out.join(name);
        let err = io::save_path(&target, &d).err().unwrap_or_else(|| "saved".into());
        assert!(err.contains("picture") || err.contains("holds"), "{name}: {err}");
        assert!(!target.exists(), "{name}: nothing was written");
    }
    set_memory_share(0);
    let _ = std::fs::remove_dir_all(&out);
}
