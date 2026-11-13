//! Build step: compile the libavcodec shim against the installed FFmpeg headers.
//!
//! The shim is deliberately C: libavcodec's context structs are private to the
//! installed version, so mirroring them in Rust would break on every FFmpeg
//! upgrade. The bridge exposes a dozen stable functions instead.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn run(cmd: &mut Command) -> String {
    let out = cmd.output().unwrap_or_else(|e| panic!("cannot run {cmd:?}: {e}"));
    if !out.status.success() {
        panic!(
            "{cmd:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn main() {
    println!("cargo:rerun-if-changed=src/dec_shim.c");
    println!("cargo:rerun-if-changed=build.rs");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let obj = out_dir.join("dec_shim.o");
    let lib = out_dir.join("liblgx_dec_shim.a");

    let mut cflags = vec![
        "-O2".to_string(),
        "-fPIC".to_string(),
        "-std=c11".to_string(),
        "-Wall".to_string(),
        "-Wextra".to_string(),
    ];
    if let Ok(pc) = Command::new("pkg-config")
        .args(["--cflags", "libavcodec", "libavutil"])
        .output()
    {
        if pc.status.success() {
            cflags.extend(
                String::from_utf8_lossy(&pc.stdout)
                    .split_whitespace()
                    .map(str::to_string),
            );
        }
    }

    let cc = env::var("CC").unwrap_or_else(|_| "cc".to_string());
    run(Command::new(cc)
        .args(&cflags)
        .arg("-c")
        .arg("src/dec_shim.c")
        .arg("-o")
        .arg(&obj));

    let ar = env::var("AR").unwrap_or_else(|_| "ar".to_string());
    run(Command::new(ar).arg("rcs").arg(&lib).arg(&obj));

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=lgx_dec_shim");
    println!("cargo:rustc-link-lib=avcodec");
    println!("cargo:rustc-link-lib=avutil");
}
