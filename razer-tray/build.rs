//! Pre-bake the tray icons into raw RGBA at build time.
//!
//! The tray needs `tray_icon::Icon::from_rgba(bytes, w, h)`. Previously the six PNGs were
//! embedded whole and decoded at runtime with the `image` crate, which meant shipping a
//! PNG/JPEG/GIF/WebP decoder inside a binary whose only imaging job is six fixed icons.
//! That is dead weight in an artifact we're about to sign and ask strangers to trust, and
//! `image` has a history of soundness advisories (RUSTSEC-2019-0014, RUSTSEC-2020-0073).
//!
//! Decoding here instead moves `image` to a *build*-dependency: it never links into the
//! shipped executable.
//!
//! The sources are 480x480. Embedding those as raw RGBA would be 921,600 bytes each --
//! 5.5 MB for six, far worse than the ~213 KB of compressed PNG. So they're downscaled to
//! ICON_SIZE first, which is both smaller than the PNGs *and* removes the runtime decode.
//! Windows draws tray icons at 16x16 (100% DPI) or 32x32 (200%), so 64x64 keeps headroom
//! for a high-DPI display while costing 16 KB per icon.

use std::path::Path;

/// Edge length of the baked icons. Comfortably above the 32x32 Windows uses at 200% DPI.
const ICON_SIZE: u32 = 64;

/// Icon stems, in the order `program.rs` indexes them by perf mode.
const ICONS: &[&str] = &["blue", "yellow", "green", "red", "violet", "brown"];

fn main() {
    let out_dir = std::env::var("OUT_DIR").expect("cargo sets OUT_DIR");
    let icons_dir = Path::new("icons");

    for stem in ICONS {
        let src = icons_dir.join(format!("razer-{stem}.png"));
        // Re-run only when an icon actually changes.
        println!("cargo:rerun-if-changed={}", src.display());

        let decoded = image::open(&src)
            .unwrap_or_else(|e| panic!("failed to decode {}: {e}", src.display()))
            // Lanczos3: these are detailed logos going down 7.5x, where a cheaper filter
            // visibly aliases the thin strokes.
            .resize_exact(ICON_SIZE, ICON_SIZE, image::imageops::FilterType::Lanczos3)
            .into_rgba8();

        let dest = Path::new(&out_dir).join(format!("icon-{stem}.rgba"));
        std::fs::write(&dest, decoded.as_raw())
            .unwrap_or_else(|e| panic!("failed to write {}: {e}", dest.display()));
    }

    // Embed a manifest asking for Common Controls 6, so the rules window's list, combo
    // boxes and buttons have the current Windows look instead of the Windows 95 one. Only
    // the MSVC linker (link.exe, lld-link) takes these flags.
    let manifest = Path::new("res").join("razer-tray.manifest");
    println!("cargo:rerun-if-changed={}", manifest.display());
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os == "windows" && target_env == "msvc" {
        // Not canonicalize: on Windows it returns a \\?\ path, which link.exe rejects.
        let dir = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
        let full = Path::new(&dir).join(&manifest);
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
            full.display()
        );
    }

    println!("cargo:rerun-if-changed=build.rs");
}
