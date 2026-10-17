//! Builds the page — the cce-ui app in `src/web`, compiled for the browser —
//! into `$OUT_DIR/site`, where `src/main.rs` embeds it. So the server binary
//! carries its page, `ccebuild install cce-remote` needs no separate step,
//! and the page and the server can never be from different builds.
//!
//! The steps are cce-vault-web's `web/build.sh`, done here:
//!
//! 1. A NESTED cargo builds this package's library for
//!    `wasm32-unknown-unknown`, always in release (a debug page is ~5x larger
//!    and slow on a phone). It gets a target dir of its own, `<target>/web`:
//!    the outer build holds its target dir's lock for its whole life, so a
//!    nested build there would wait forever. Its environment is cleaned of
//!    the outer build's flags, and given the one the browser half needs —
//!    web-sys's WebGPU bindings are behind `--cfg=web_sys_unstable_apis`.
//! 2. The `wasm-bindgen` CLI binds it (`--target web`). It must be the same
//!    version as the wasm-bindgen crate in the lock; it says so if not.
//! 3. The wasm is gzipped beside itself; the server sends that to any
//!    browser that takes gzip (all do).
//!
//! This script runs for the nested build too; on wasm32 it does nothing.
//! `CCE_REMOTE_SKIP_WEB=1` skips the page entirely (empty placeholders), for
//! a machine without the wasm toolchain: the server then has no page (its
//! sockets and debug endpoints still work).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    if std::env::var("TARGET").is_ok_and(|t| t.starts_with("wasm32")) {
        return;
    }
    for path in ["src/lib.rs", "src/web", "web"] {
        println!("cargo:rerun-if-changed={path}");
    }
    println!("cargo:rerun-if-env-changed=CCE_REMOTE_SKIP_WEB");
    println!("cargo:rerun-if-env-changed=WASM_BINDGEN");

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let site = out.join("site");
    std::fs::create_dir_all(&site).unwrap();
    if std::env::var("CCE_REMOTE_SKIP_WEB").is_ok_and(|v| v == "1") {
        println!("cargo:warning=CCE_REMOTE_SKIP_WEB=1: building without the cce-ui page; `/` will say so");
        for f in ["cce_remote.js", "cce_remote_bg.wasm", "cce_remote_bg.wasm.gz"] {
            std::fs::write(site.join(f), b"").unwrap();
        }
        return;
    }

    let crate_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    // OUT_DIR is <target>/<profile>/build/<pkg>-<hash>/out.
    let target = out.ancestors().nth(4).expect("OUT_DIR under a target dir").join("web");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut cmd = Command::new(cargo);
    cmd.current_dir(&crate_dir)
        .args(["build", "--lib", "--release", "--target", "wasm32-unknown-unknown", "--target-dir"])
        .arg(&target);
    for var in ["RUSTFLAGS", "CARGO_BUILD_RUSTFLAGS", "CARGO_TARGET_DIR", "CARGO_BUILD_TARGET", "RUSTC_WORKSPACE_WRAPPER"] {
        cmd.env_remove(var);
    }
    cmd.env("CARGO_ENCODED_RUSTFLAGS", "--cfg=web_sys_unstable_apis");
    run(&mut cmd, "the wasm32 build of the page");

    let wasm = target.join("wasm32-unknown-unknown/release/cce_remote.wasm");
    let mut bindgen = Command::new(wasm_bindgen());
    bindgen.args(["--target", "web", "--no-typescript", "--out-dir"]).arg(&site).arg(&wasm);
    run(&mut bindgen, "wasm-bindgen");

    let bytes = std::fs::read(site.join("cce_remote_bg.wasm")).unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    gz.write_all(&bytes).unwrap();
    std::fs::write(site.join("cce_remote_bg.wasm.gz"), gz.finish().unwrap()).unwrap();
}

fn run(cmd: &mut Command, what: &str) {
    match cmd.status() {
        Ok(s) if s.success() => {}
        Ok(s) => panic!("{what} failed ({s}); set CCE_REMOTE_SKIP_WEB=1 to build the server without its page"),
        Err(e) => panic!("{what} could not start: {e}; set CCE_REMOTE_SKIP_WEB=1 to build the server without its page"),
    }
}

/// `$WASM_BINDGEN`, else `wasm-bindgen` on PATH, else cargo's bin dir — where
/// `cargo install wasm-bindgen-cli` puts it, and which a service's or an
/// IDE's PATH often lacks.
fn wasm_bindgen() -> PathBuf {
    if let Ok(p) = std::env::var("WASM_BINDGEN") {
        return p.into();
    }
    let on_path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|d| d.join("wasm-bindgen"))
        .find(|p| p.is_file());
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".cargo")));
    on_path
        .or_else(|| cargo_home.map(|h| h.join("bin/wasm-bindgen")).filter(|p| p.is_file()))
        .unwrap_or_else(|| "wasm-bindgen".into())
}
