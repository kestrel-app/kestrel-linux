//! Point the linker at the ffmpeg built by vendor/build-ffmpeg.sh.
//!
//! Two rpaths are emitted: `$ORIGIN/lib` so a released bundle finds the shared
//! libraries sitting beside the binary (that is how the LGPL "replaceable
//! library" requirement is satisfied), and the absolute vendor prefix so the
//! binary also runs straight out of the build tree during development.

use std::path::PathBuf;

fn main() {
    // Read at *run* time, not with env!. env! bakes the value in when this
    // script is compiled, so a cached build script kept pointing at the old
    // location after the checkout was moved — the vendored libraries then
    // looked absent, no rpath was emitted, and the release quietly linked
    // against whatever ffmpeg the build machine happened to have installed.
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets this");
    let vendor: PathBuf = PathBuf::from(manifest).join("vendor/prefix");
    let lib = vendor.join("lib");
    println!("cargo:rerun-if-changed={}", lib.display());

    let profile = std::env::var("PROFILE").unwrap_or_default();
    if !lib.exists() && profile == "release" {
        // A release that silently falls back to the system's libraries is worse
        // than no release: it works here and fails on the user's machine.
        panic!(
            "vendored ffmpeg missing at {} — run vendor/build-ffmpeg.sh first",
            lib.display()
        );
    }

    if lib.exists() {
        println!("cargo:rustc-link-search=native={}", lib.display());
        // $ORIGIN is resolved by the dynamic linker, not the shell.
        // Two layouts are supported: the tarball puts lib/ beside the binary,
        // while an AppImage/FHS tree has usr/bin/kestrel with usr/lib.
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/lib");
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../lib");

        // The absolute vendor path is a development convenience so the binary
        // runs straight out of target/. A release must NOT carry it: it would
        // resolve against this machine's build tree, hiding the fact that the
        // shipped lib/ directory is what a user actually depends on.
        if profile != "release" {
            println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib.display());
        }
    }
    // Stamp the build time into the binary so a packaged artifact can be
    // checked against the source it came from.
    let stamp = std::process::Command::new("date")
        .args(["-u", "+%Y-%m-%d %H:%M UTC"])
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=KESTREL_BUILD_DATE={stamp}");

    println!("cargo:rerun-if-changed=build.rs");
    // Re-stamp whenever any source or asset changes.
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=assets");
}
