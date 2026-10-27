//! WHIQ: an ODT or RTF export copies pictures kept on disk from their files into the output: the
//! model keeps no copy of their bytes, and the output is the one the pictures in memory give. This
//! test sets the share of memory pictures may take, so it has the binary to itself.
#![cfg(not(target_arch = "wasm32"))]
#![allow(clippy::unwrap_used)]

use wordcraft_doc::media::set_memory_share;
use wordcraft_doc::{CharProps, Document, InlineObject, Paragraph, para::Float, para_block};

/// A PNG of noise: big enough to be kept on disk, and not compressible.
fn noise_png(side: u32, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
    let img = image::RgbImage::from_fn(side, side, |_, _| {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        image::Rgb([x as u8, (x >> 8) as u8, (x >> 16) as u8])
    });
    let mut buf = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png).unwrap();
    buf
}

fn document(pics: &[Vec<u8>]) -> Document {
    let mut d = Document::new();
    let mut p = Paragraph::with_text("pictures: ", CharProps::default());
    for pic in pics {
        let key = d.add_media(pic.clone(), "png");
        let end = p.len();
        p.insert_object(
            end,
            InlineObject::Image { media: key, w: 60.0, h: 60.0, alt: "noise".into(), float: Float::default(), crop: [0.0; 4], ole: None },
            &CharProps::default(),
        )
        .unwrap();
    }
    d.body = vec![para_block(p)];
    d
}

#[test]
fn pictures_on_disk_are_streamed_into_odt_and_rtf_and_never_read_back_into_the_model() {
    set_memory_share(300_000);
    let pics: Vec<Vec<u8>> = (1..=3).map(|k| noise_png(300, k)).collect();
    assert!(pics.iter().all(|p| p.len() > 256_000), "each picture is kept on disk");
    let d = document(&pics);
    assert!(d.media.spilled_bytes() > 0, "some of them are on disk");

    // The model holds no bytes for a picture on disk.
    let flow = wordcraft_formats::model::from_doc_tracked(&d);
    let imgs: Vec<&wordcraft_formats::model::Img> = flow
        .blocks
        .iter()
        .flat_map(|b| match b {
            wordcraft_formats::model::FBlock::Para(p) => p.inlines.iter().collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .filter_map(|i| if let wordcraft_formats::model::Inline::Image(img) = i { Some(img) } else { None })
        .collect();
    assert_eq!(imgs.len(), 3);
    let on_disk = imgs.iter().filter(|i| i.disk.is_some()).count();
    assert!(on_disk >= 2, "{on_disk} pictures are on disk");
    assert!(imgs.iter().filter(|i| i.disk.is_some()).all(|i| i.data.is_empty()), "no bytes were read back for them");

    // ODT: streamed into a file = the in-memory package, and the pictures read back unchanged.
    let in_memory = wordcraft_formats::odt::export(&d).unwrap();
    let dir = std::env::temp_dir().join(format!("wc-stream-pictures-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("p.odt");
    {
        let f = std::fs::File::create(&path).unwrap();
        wordcraft_formats::odt::export_to(&d, std::io::BufWriter::new(f)).unwrap();
    }
    assert!(std::fs::read(&path).unwrap() == in_memory, "the same package");
    let back = wordcraft_formats::odt::import(&in_memory).unwrap();
    let got: Vec<std::sync::Arc<Vec<u8>>> = back.media.values().collect();
    assert_eq!(got.len(), 3);
    for pic in &pics {
        assert!(got.iter().any(|g| g.as_slice() == pic.as_slice()), "a picture came back byte for byte");
    }

    // RTF: the text streamed = the string, with each picture's bytes as hex.
    let text = wordcraft_formats::rtf::export(&d);
    let mut streamed = Vec::new();
    wordcraft_formats::rtf::export_to(&d, &mut streamed).unwrap();
    assert!(streamed == text.as_bytes());
    assert!(!text.contains('\u{1}') && !text.contains('\u{2}'), "no marker is left");
    assert_eq!(text.matches("\\pngblip").count(), 3);
    let hex: String = text.split("\\pichgoal").nth(1).unwrap().lines().skip(1).take_while(|l| !l.starts_with('}')).collect();
    let decoded: Vec<u8> = (0..hex.len() / 2).map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap()).collect();
    assert!(pics.iter().any(|p| decoded.starts_with(&p[..64]) && decoded.len() >= p.len()), "the first picture is in the text");

    // HTML (which needs the bytes) still works.
    assert_eq!(wordcraft_formats::html::export(&d).matches("data:image/png;base64,").count(), 3);
    std::fs::remove_dir_all(&dir).unwrap();
}
