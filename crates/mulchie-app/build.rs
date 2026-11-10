use std::{env, fs, path::Path};

fn main() {
    // `bundle.externalBin` requires `binaries/mulchie-<target triple>` to exist at build time.
    // On a clean checkout create an empty placeholder so `cargo check` works; release builds
    // replace it with the real CLI binary.
    let target = env::var("TARGET").expect("cargo sets TARGET for build scripts");
    let ext = if target.contains("windows") {
        ".exe"
    } else {
        ""
    };
    let dir = Path::new("binaries");
    let sidecar = dir.join(format!("mulchie-{target}{ext}"));
    if !sidecar.exists() {
        fs::create_dir_all(dir).expect("create binaries dir");
        fs::write(&sidecar, b"").expect("create placeholder sidecar");
        println!(
            "cargo:warning=sidecar {} is an empty placeholder; build the mulchie CLI and copy it there before bundling",
            sidecar.display()
        );
    }
    println!("cargo:rerun-if-changed=binaries");
    tauri_build::build()
}
