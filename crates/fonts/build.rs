//! Optional craft-fonts build input (https://github.com/storytold/craft-fonts, recipe from its
//! `docs/integration.md`). With `CRAFT_FONTS_DIR=<checkout>` the fonts in its
//! `fonts/manifest.txt` (WHIQ: and its `fonts/latin-manifest.txt`, the Latin presentation fonts
//! GridCraft and DeckCraft embed too, less the faces already bundled) are embedded as
//! `CRAFT_FONTS`; unset, `CRAFT_FONTS` is empty. Web
//! (wasm32) builds embed only the UI face, BIZ UDPGothic Regular, to stay within the web size
//! budget. With `CRAFT_FONTS_REQUIRED` (releases), a native build also fails when the checkout
//! has no font for a CJK interface language (Japanese, Simplified Chinese), as an outdated pin
//! once did (#241). It only reads the local checkout: no network.
use std::fmt::Write as _;
use std::path::PathBuf;

fn main() {
    println!("cargo::rerun-if-env-changed=CRAFT_FONTS_DIR");
    println!("cargo::rerun-if-env-changed=CRAFT_FONTS_REQUIRED");
    let mut src = String::from("pub static CRAFT_FONTS: &[CraftFont] = &[\n");
    if let Some(dir) = std::env::var_os("CRAFT_FONTS_DIR").map(PathBuf::from) {
        match craft_fonts(&dir) {
            Ok((entries, covered)) => {
                src.push_str(&entries);
                // Each CJK interface language needs its own face: without the Chinese one, Chinese
                // menus fall back to the Japanese face and show boxes for simplified hanzi (#241).
                let wasm = std::env::var("CARGO_CFG_TARGET_ARCH").is_ok_and(|a| a == "wasm32");
                if std::env::var_os("CRAFT_FONTS_REQUIRED").is_some() && !wasm {
                    for s in REQUIRED_SCRIPTS.iter().filter(|s| !covered.iter().any(|c| c == *s)) {
                        println!("cargo::error=CRAFT_FONTS_DIR={}: no {s} font in fonts/manifest.txt (craft-fonts too old?)", dir.display());
                    }
                    // WHIQ: the stand-ins for the document fonts nobody may bundle (Aptos and Calibri,
                    // Arial, Times New Roman), so text never falls to whatever the host has: on
                    // Windows the default font fell to the system Arial, which drew the Arabic too.
                    for f in REQUIRED_FAMILIES.iter().filter(|f| !entries.contains(&format!("family: {f:?},"))) {
                        println!(
                            "cargo::error=CRAFT_FONTS_DIR={}: no {f} font in fonts/latin-manifest.txt (craft-fonts older than 43913056?)",
                            dir.display()
                        );
                    }
                }
            }
            Err(e) if std::env::var_os("CRAFT_FONTS_REQUIRED").is_some() => {
                println!("cargo::error=CRAFT_FONTS_DIR={}: {e}", dir.display());
            }
            Err(e) => println!("cargo::warning=building without craft-fonts: CRAFT_FONTS_DIR={}: {e}", dir.display()),
        }
    }
    src.push_str("];\n");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or_default()).join("craft_fonts.rs");
    if let Err(e) = std::fs::write(&out, src) {
        println!("cargo::error=writing {}: {e}", out.display());
    }
}

/// Scripts a release build (`CRAFT_FONTS_REQUIRED`, native) must embed a font for: the CJK
/// interface languages' (Japanese, Simplified Chinese), and (WHIQ) Arabic, so Arabic text is
/// always shaped with a real face even on a machine without one.
const REQUIRED_SCRIPTS: &[&str] = &["Jpan", "Hans", "Arab"];

/// WHIQ: Latin families a release (`CRAFT_FONTS_REQUIRED`, native) must embed: the metric-compatible
/// stand-ins for Calibri / Aptos (Carlito), Cambria (Caladea), Arial, Times New Roman and Courier
/// New (Liberation).
const REQUIRED_FAMILIES: &[&str] = &["Carlito", "Caladea", "Liberation Sans", "Liberation Serif", "Liberation Mono"];

/// WHIQ: faces of `fonts/latin-manifest.txt` the crate already bundles (`assets/fonts`, the
/// `BUNDLED` list in `fontdb.rs`): not embedded twice.
const ALREADY_BUNDLED: &[(&str, &str)] = &[
    ("Inter", "Regular"),
    ("Inter", "SemiBold"),
    ("Source Sans 3", "Regular"),
    ("Source Sans 3", "Semibold"),
    ("Source Sans 3", "Bold"),
    ("Source Sans 3", "Italic"),
    ("Source Serif 4", "Regular"),
    ("Source Serif 4", "Italic"),
    ("Source Serif 4", "Semibold"),
    ("Source Serif 4", "Bold"),
];

/// One `CraftFont { .. }` initialiser per manifest line (`fonts/manifest.txt`, then WHIQ
/// `fonts/latin-manifest.txt` when the checkout has it), and the scripts the embedded fonts cover.
fn craft_fonts(dir: &std::path::Path) -> Result<(String, Vec<String>), String> {
    let mut out = String::new();
    let mut covered = Vec::new();
    manifest(dir, &dir.join("fonts/manifest.txt"), false, &mut out, &mut covered)?;
    // The Latin presentation fonts are optional within craft-fonts (older checkouts lack them).
    let latin = dir.join("fonts/latin-manifest.txt");
    println!("cargo::rerun-if-changed={}", latin.display());
    if latin.exists() {
        manifest(dir, &latin, true, &mut out, &mut covered)?;
    }
    Ok((out, covered))
}

fn manifest(dir: &std::path::Path, manifest: &std::path::Path, latin: bool, out: &mut String, covered: &mut Vec<String>) -> Result<(), String> {
    println!("cargo::rerun-if-changed={}", manifest.display());
    let text = std::fs::read_to_string(manifest).map_err(|e| format!("{}: {e}", manifest.display()))?;
    let wasm = std::env::var("CARGO_CFG_TARGET_ARCH").is_ok_and(|a| a == "wasm32");
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let f: Vec<&str> = line.split(" | ").map(str::trim).collect();
        let [family, style, file, scripts, ..] = f.as_slice() else {
            return Err(format!("malformed manifest line: {line}"));
        };
        if wasm && !(*family == "BIZ UDPGothic" && *style == "Regular") {
            continue;
        }
        if latin && ALREADY_BUNDLED.contains(&(*family, *style)) {
            continue;
        }
        let path = dir.join(file).canonicalize().map_err(|e| format!("{file}: {e}"))?;
        println!("cargo::rerun-if-changed={}", path.display());
        covered.extend(scripts.split(',').map(|s| s.trim().to_string()));
        let scripts: Vec<String> = scripts.split(',').map(|s| format!("{:?}", s.trim())).collect();
        let _ = writeln!(
            out,
            "    CraftFont {{ family: {family:?}, style: {style:?}, scripts: &[{}], bytes: include_bytes!({:?}) }},",
            scripts.join(", "),
            path.display().to_string(),
        );
    }
    Ok(())
}
