use std::path::PathBuf;
use std::process::Command;

fn main() {
    build_hook();
    tauri_build::build()
}

/// Compiles `crates/keel-hook`, the program Claude Code runs around a builder's Bash commands, for
/// the app to carry inside itself (see `keel_harness::hook`). It has no dependencies, so plain
/// `rustc` builds it, for the same target as the app. Panics unwind so the hook can catch them and
/// stay quiet: an abort would leave a crash report.
fn build_hook() {
    let source = PathBuf::from("../crates/keel-hook/src");
    println!("cargo:rerun-if-changed={}", source.display());
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let target = std::env::var("TARGET").expect("cargo sets TARGET");
    let status = Command::new(rustc)
        .args(["--edition", "2024", "--crate-name", "keel_hook"])
        .args(["--crate-type", "bin", "--target", &target])
        .args([
            "-C",
            "opt-level=2",
            "-C",
            "panic=unwind",
            "-C",
            "debuginfo=0",
        ])
        .arg("-o")
        .arg(out.join("keel-hook"))
        .arg(source.join("main.rs"))
        .status()
        .expect("rustc runs");
    assert!(status.success(), "the hook didn't compile");
}
