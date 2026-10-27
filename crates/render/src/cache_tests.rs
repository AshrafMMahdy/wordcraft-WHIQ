//! Decoded-media cache: hits need the same allocation, and the byte budget bounds what is kept.

use super::*;

/// The cache's handle on a picture held in memory as this allocation.
fn pin(a: &Arc<Vec<u8>>) -> wordcraft_doc::media::Pin {
    let mut m = wordcraft_doc::media::MediaMap::new();
    m.insert("k".into(), a.clone());
    m.pin("k").unwrap()
}

#[test]
fn media_cache_only_hits_the_same_allocation() {
    let mut c: MediaCache<u32> = MediaCache { map: HashMap::new(), bytes: 0 };
    let a = Arc::new(vec![1u8, 2, 3]);
    let b = Arc::new(vec![1u8, 2, 3]);
    c.insert("pic", &pin(&a), 7, 3);
    assert_eq!(c.get("pic", &pin(&a)), Some(7));
    assert_eq!(c.get("pic", &pin(&b)), None, "equal bytes in another allocation are another picture");
}

#[test]
fn media_cache_is_bounded_by_bytes() {
    let mut c: MediaCache<u32> = MediaCache { map: HashMap::new(), bytes: 0 };
    let src = Arc::new(vec![0u8]);
    let big = CACHE_BYTES / 2 + 1;
    c.insert("a", &pin(&src), 1, big);
    c.insert("b", &pin(&src), 2, big);
    assert_eq!(c.get("a", &pin(&src)), None, "the first entry is evicted to make room");
    assert_eq!(c.get("b", &pin(&src)), Some(2));
    assert!(c.bytes <= CACHE_BYTES);
    c.insert("huge", &pin(&src), 3, CACHE_BYTES + 1);
    assert_eq!(c.get("huge", &pin(&src)), None, "a value over the whole budget is not kept");
}

#[test]
fn media_cache_counts_the_source_bytes_it_holds() {
    let mut c: MediaCache<Option<u32>> = MediaCache { map: HashMap::new(), bytes: 0 };
    let src = Arc::new(vec![0u8; 1000]);
    c.insert("broken", &pin(&src), None, 0);
    assert_eq!(c.bytes, 1000, "an entry that decoded to nothing still holds its source");
    c.insert("pic", &pin(&src), Some(1), 24);
    assert_eq!(c.bytes, 2024);
    // A source over the whole budget is not kept, whatever it decoded to (calloc'd, so cheap).
    let huge = Arc::new(vec![0u8; CACHE_BYTES + 1]);
    c.insert("huge", &pin(&huge), None, 0);
    assert!(!c.map.contains_key("huge"));
    assert_eq!(CACHE_BYTES * 2, 256 * 1024 * 1024, "the image and vector caches share about 256 MB");
}

/// A wave stroke starting around 1e20 never advanced (`x + step == x`), pushing curves until memory ran out.
#[test]
fn wave_stroke_at_a_huge_coordinate_ends() {
    assert!(wave_path(1e20, 2e20, 0.0, 1.0).elements().len() < 3);
    assert!(wave_path(0.0, 1e20, 0.0, 1.0).elements().len() < 20_100);
    assert!(wave_path(0.0, 100.0, 0.0, 1.0).elements().len() > 10);
}
