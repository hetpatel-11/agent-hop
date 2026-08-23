//! Fetches, builds, and links `libghostty-vt` -- Ghostty's own embeddable
//! terminal-state engine -- pinned to the exact commit herdr (a real,
//! shipping AI-agent terminal multiplexer solving this same problem) vendors
//! as of this writing. See src/vt.rs's module doc for why this replaced
//! alacritty_terminal: alacritty_terminal doesn't implement OSC 133
//! (semantic prompt marking), which is what a real Codex session uses to
//! mark its input line -- and that's a fundamental "the model doesn't
//! understand this sequence" gap, not something patchable in our own code.
//! libghostty-vt is Ghostty's own engine, so anything Ghostty itself
//! understands, this understands too.
//!
//! Not vendored into git: the full Ghostty source tree is ~300MB, most of
//! which (fonts, the GUI app, several unrelated Zig packages) isn't needed
//! just to build the `-Demit-lib-vt` library target. Downloaded and cached
//! under `target/` instead -- reproducible (pinned to an exact commit) and
//! ordinary for crates with a heavy non-Rust dependency, without bloating
//! every clone of this repo. Requires network access and a Zig toolchain at
//! build time; end users installing a prebuilt release binary never do
//! either.

use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Exact commit herdr's own vendor/libghostty-vt.vendor.json pins as of the
/// version whose CHANGELOG documents the vt100 -> Ghostty migration this
/// mirrors.
const GHOSTTY_COMMIT: &str = "c5a21edfcbc2d5b46540ad91b7980aca31f5f1f3";

fn zig_target(target: &str) -> &str {
    match target {
        "x86_64-unknown-linux-gnu" => "x86_64-linux-gnu",
        "aarch64-unknown-linux-gnu" => "aarch64-linux-gnu",
        "x86_64-unknown-linux-musl" => "x86_64-linux-musl",
        "aarch64-unknown-linux-musl" => "aarch64-linux-musl",
        "x86_64-apple-darwin" => "x86_64-macos",
        "aarch64-apple-darwin" => "aarch64-macos",
        "x86_64-pc-windows-msvc" => "x86_64-windows-msvc",
        "aarch64-pc-windows-msvc" => "aarch64-windows-msvc",
        other => panic!("unsupported target for libghostty-vt build: {other}"),
    }
}

fn download_and_extract(cache_dir: &Path) {
    let url = format!("https://codeload.github.com/ghostty-org/ghostty/tar.gz/{GHOSTTY_COMMIT}");
    println!("cargo:warning=downloading Ghostty source ({GHOSTTY_COMMIT}) for libghostty-vt -- one-time, cached under target/");
    let resp = ureq::get(&url).call().unwrap_or_else(|e| panic!("failed to download Ghostty source: {e}"));
    let mut bytes = Vec::new();
    resp.into_reader().read_to_end(&mut bytes).expect("failed to read Ghostty source archive");

    let tmp_extract = cache_dir.with_extension("tmp");
    let _ = fs::remove_dir_all(&tmp_extract);
    fs::create_dir_all(&tmp_extract).expect("failed to create extraction dir");
    let tar = flate2::read::GzDecoder::new(&bytes[..]);
    let mut archive = tar::Archive::new(tar);
    archive.unpack(&tmp_extract).expect("failed to extract Ghostty source archive");

    // GitHub archives extract into a single top-level `ghostty-<commit>/` dir.
    let extracted = fs::read_dir(&tmp_extract)
        .expect("failed to read extracted archive")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.is_dir())
        .expect("extracted archive had no top-level directory");

    let _ = fs::remove_dir_all(cache_dir);
    fs::rename(&extracted, cache_dir).expect("failed to move extracted Ghostty source into place");
    let _ = fs::remove_dir_all(&tmp_extract);
}

/// Reads Ghostty's declared version out of its `build.zig.zon` (the
/// `.version = "X.Y.Z..."` line). Used to pass `-Dversion-string` so the
/// build never falls into git-based version detection. Falls back to a
/// valid placeholder semver if the field can't be read -- any valid
/// semantic version works, since it's only cosmetic for an embedded
/// libghostty-vt.
fn read_ghostty_version(src_dir: &Path) -> String {
    let zon = src_dir.join("build.zig.zon");
    if let Ok(content) = fs::read_to_string(&zon) {
        for line in content.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix(".version") {
                // rest looks like: = "1.3.2-dev",
                if let Some(open) = rest.find('"') {
                    if let Some(close) = rest[open + 1..].find('"') {
                        return rest[open + 1..open + 1 + close].to_string();
                    }
                }
            }
        }
    }
    "0.0.0".to_string()
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=ZIG");
    println!("cargo:rerun-if-env-changed=AH_GHOSTTY_VT_ZIG_SYSTEM_DIR");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let cache_root = manifest_dir.join("target").join("ghostty-vt-src");
    let src_dir = cache_root.join(GHOSTTY_COMMIT);

    if !src_dir.join("build.zig").exists() {
        fs::create_dir_all(&cache_root).expect("failed to create ghostty-vt-src cache dir");
        download_and_extract(&src_dir);
    }

    let target = env::var("TARGET").expect("TARGET");
    let target_zig = zig_target(&target);
    let optimize = env::var("PROFILE").map(|p| if p == "release" { "ReleaseFast" } else { "Debug" }).unwrap_or_else(|_| "ReleaseFast".into());

    let lib_out = src_dir.join("zig-out-agent-hop");
    let zig = env::var("ZIG").unwrap_or_else(|_| "zig".into());

    // Pin the version explicitly so Ghostty's build.zig skips its git-based
    // version detection entirely. Without this, that detection runs `git`
    // with the extracted source dir as cwd -- and since we cache that dir
    // under our own `target/`, git walks *up* into agent-hop's own repo. If
    // agent-hop's HEAD happens to sit on a tag (exactly what a release build
    // does), Ghostty sees a tag that isn't its own `vX.Y.Z` and hard-panics
    // ("tagged releases must be in vX.Y.Z format"). Passing -Dversion-string
    // takes the git path out of the picture. The value is cosmetic for an
    // embedded libghostty-vt; we read Ghostty's own declared version from
    // build.zig.zon so it stays truthful if GHOSTTY_COMMIT is ever bumped.
    let ghostty_version = read_ghostty_version(&src_dir);

    // Keep Zig's caches on the same drive as the source we're building.
    // Zig 0.15.2's build runner (std.Build.Step.Run.convertPathArg) asserts
    // that a Run step's path args can be made relative to the child's cwd,
    // and hits `unreachable` when they can't -- which on Windows happens
    // whenever two paths live on different drive letters (Zig 0.16 later
    // rewrote this to handle it gracefully). On GitHub's Windows runners the
    // workspace is on D:\ but Zig's default global cache is under the user
    // profile on C:\, so Ghostty's Windows-only `combine_archives` Run step
    // straddles two drives and crashes the whole build. Pointing both caches
    // at dirs beside the source (same drive) keeps every path on one drive
    // and sidesteps the assertion. Harmless on macOS/Linux (single-rooted
    // filesystems), so unconditional.
    let local_cache = cache_root.join("zig-local-cache");
    let global_cache = cache_root.join("zig-global-cache");
    let _ = fs::create_dir_all(&local_cache);
    let _ = fs::create_dir_all(&global_cache);

    let mut cmd = Command::new(&zig);
    cmd.arg("build")
        .arg("-Demit-lib-vt")
        .arg(format!("-Doptimize={optimize}"))
        .arg(format!("-Dtarget={target_zig}"))
        .arg(format!("-Dversion-string={ghostty_version}"))
        .arg("-Demit-xcframework=false")
        .arg("--cache-dir")
        .arg(&local_cache)
        .arg("--global-cache-dir")
        .arg(&global_cache)
        .arg("--prefix")
        .arg(&lib_out)
        .current_dir(&src_dir);
    if let Ok(system_dir) = env::var("AH_GHOSTTY_VT_ZIG_SYSTEM_DIR") {
        cmd.arg("--system").arg(system_dir);
    }
    let status = cmd.status().unwrap_or_else(|e| panic!("failed to run `{zig} build` for libghostty-vt: {e}"));
    assert!(status.success(), "zig build for libghostty-vt failed (target {target_zig})");

    let lib_dir = lib_out.join("lib");
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    if target.contains("apple-darwin") {
        // Name-based `-l ghostty-vt` resolves to the `.dylib` zig also
        // emits into this same directory even when told `static=` --
        // confirmed live (the built binary failed at runtime looking for
        // `@rpath/libghostty-vt.dylib`, meaning it linked dynamically
        // despite the hint). Passing the archive's exact path as a raw
        // linker argument, same as herdr's own build.rs does, leaves the
        // linker no name to re-resolve.
        let static_lib = lib_dir.join("libghostty-vt.a");
        println!("cargo:rustc-link-arg={}", static_lib.display());
        println!("cargo:rustc-link-lib=c++");
    } else if target.contains("windows-msvc") {
        println!("cargo:rustc-link-lib=static=ghostty-vt-static");
    } else {
        println!("cargo:rustc-link-lib=static=ghostty-vt");
    }

    let include_dir = lib_out.join("include");
    let header = include_dir.join("ghostty").join("vt.h");
    let bindings = bindgen::Builder::default()
        .header(header.to_string_lossy())
        .clang_arg(format!("-I{}", include_dir.display()))
        .allowlist_item("[Gg]hostty.*")
        .default_enum_style(bindgen::EnumVariation::Rust { non_exhaustive: false })
        .derive_default(true)
        .generate()
        .expect("failed to generate libghostty-vt bindings");

    // bindgen 0.70 still emits pre-edition-2024 `extern "C" { ... }` blocks;
    // this crate is edition 2024, which requires `unsafe extern "C" { ... }`.
    // Simple text substitution rather than a newer bindgen release, since
    // no bindgen option controls this yet.
    let generated = bindings.to_string().replace("extern \"C\" {", "unsafe extern \"C\" {");
    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap()).join("ghostty_vt_bindings.rs");
    fs::write(&out_path, generated).expect("failed to write libghostty-vt bindings");
}
