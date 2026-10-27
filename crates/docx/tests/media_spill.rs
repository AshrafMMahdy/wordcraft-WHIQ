//! WHIQ: pictures past the document's share of the memory budget are kept on disk: read from the
//! zip straight into a spill file, streamed from it into every save. Nothing is dropped, and the
//! package is byte for byte what the in-memory writer makes.
#![allow(clippy::unwrap_used)]

use std::io::{Cursor, Write};

use wordcraft_doc::media::set_memory_share;
use wordcraft_doc::para::Float;
use wordcraft_doc::{CharProps, Document, InlineObject, Paragraph, para_block};

/// A picture of `n` bytes that is a valid PNG (a few pixels, then padding in a text chunk).
fn png_of(n: usize, seed: u8) -> Vec<u8> {
    let img = image::RgbaImage::from_fn(4, 4, |x, y| image::Rgba([x as u8 * 60, y as u8 * 60, seed, 255]));
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    let mut png = out.into_inner();
    // A tEXt chunk of filler before IEND keeps it a valid PNG of the size asked for.
    let iend = png.len() - 12;
    let tail = png.split_off(iend);
    let filler = n.saturating_sub(png.len() + tail.len() + 12);
    let mut chunk = b"tEXt".to_vec();
    chunk.extend(b"k\0");
    chunk.extend(std::iter::repeat_n(b'x', filler.saturating_sub(2)));
    png.extend((chunk.len() as u32 - 4).to_be_bytes());
    let crc = crc32(&chunk);
    png.extend(&chunk);
    png.extend(crc.to_be_bytes());
    png.extend(tail);
    png
}

/// The memory share and budget are process-wide: these tests run one at a time.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for b in data {
        c ^= u32::from(*b);
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    !c
}

fn doc_with(pictures: &[Vec<u8>]) -> Document {
    let mut d = Document::new();
    let mut p = Paragraph::with_text("pictures: ", CharProps::default());
    for png in pictures {
        let key = d.add_media(png.clone(), "png");
        let end = p.len();
        p.insert_object(
            end,
            InlineObject::Image { media: key, w: 10.0, h: 10.0, alt: String::new(), float: Float::default(), crop: [0.0; 4], ole: None },
            &CharProps::default(),
        )
        .unwrap();
    }
    d.body = vec![para_block(p)];
    d
}

#[test]
fn big_pictures_are_read_to_disk_and_saved_whole() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // The share holds one picture; the others go to disk, as a real document past its share does.
    set_memory_share(400_000);
    let pics: Vec<Vec<u8>> = (0..4u8).map(|k| png_of(300_000, k)).collect();
    let d = doc_with(&pics);
    // Writing: the in-memory package and the streamed one are the same bytes.
    let whole = wordcraft_docx::write(&d).unwrap();
    let streamed = wordcraft_docx::write_to(&d, wordcraft_docx::Flavor::Document, Cursor::new(Vec::new())).unwrap().into_inner();
    assert_eq!(whole, streamed, "the streamed package is the in-memory one");
    assert!(whole.len() > 4 * 300_000, "all four pictures are in it");
    // Reading from a file: pictures past the share are kept on disk.
    let dir = std::env::temp_dir().join(format!("wordcraft-media-spill-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("pics.docx");
    std::fs::File::create(&file).unwrap().write_all(&whole).unwrap();
    let back = wordcraft_docx::read_reader(Box::new(std::fs::File::open(&file).unwrap())).unwrap();
    assert_eq!(back.media.len(), 4);
    let spilled = back.media.keys().filter(|k| back.media.is_spilled(k)).count();
    assert!(spilled >= 3, "{spilled} of 4 on disk");
    assert!(back.media.spilled_bytes() >= 3 * 290_000);
    for png in &pics {
        assert!(back.media.find_bytes(png).is_some(), "a picture came back unchanged");
    }
    // Saving a document whose pictures are on disk: the same package again, none held whole.
    let again = wordcraft_docx::write_to(&back, wordcraft_docx::Flavor::Document, Cursor::new(Vec::new())).unwrap().into_inner();
    let reread = wordcraft_docx::read(&again).unwrap();
    assert_eq!(reread.media.len(), 4);
    for png in &pics {
        assert!(reread.media.find_bytes(png).is_some());
    }
    // A clone (an undo snapshot) shares the files; dropping the original keeps them for the clone.
    let snapshot = back.clone();
    drop(back);
    assert!(snapshot.media.find_bytes(&pics[3]).is_some());
    set_memory_share(0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn one_picture_past_the_share_opens_from_bytes_too() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // `read` (bytes in memory) takes the same route; the part limit not applying to pictures is
    // proved in `package` (`whiq_a_picture_is_handed_to_the_media_map_past_the_part_limit`).
    set_memory_share(300_000);
    wordcraft_doc::memory::set_budget(1 << 20);
    let big = png_of(3_000_000, 9);
    let d = doc_with(std::slice::from_ref(&big));
    let bytes = wordcraft_docx::write(&d).unwrap();
    let back = wordcraft_docx::read(&bytes).unwrap();
    assert!(back.media.is_spilled(back.media.keys().next().unwrap()));
    assert_eq!(back.media.find_bytes(&big).map(String::as_str), back.media.keys().next().map(String::as_str));
    set_memory_share(0);
    wordcraft_doc::memory::set_budget(0);
}
