fn main() {
    #[cfg(windows)]
    sideload_conpty();
    tauri_build::build();
}

/// Place a newer ConPTY (conpty.dll + OpenConsole.exe) next to the built exe.
///
/// portable-pty prefers a co-located `conpty.dll` over the OS one. The Windows
/// 10 inbox ConPTY silently strips SGR attributes such as dim/faint before they
/// reach the terminal; this bundled build (the same one Windows Terminal ships)
/// preserves them. This copy covers the cargo output dir for `tauri dev` and
/// `cargo run`; the installer ships the same two files via tauri.conf.json's
/// `bundle > resources`, which land next to the installed exe.
#[cfg(windows)]
fn sideload_conpty() {
    use std::path::Path;
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let out_dir = std::env::var("OUT_DIR").unwrap();
    // OUT_DIR = target/<profile>/build/<crate>-<hash>/out — the exe is 3 up.
    let Some(profile_dir) = Path::new(&out_dir).ancestors().nth(3) else {
        return;
    };
    for name in ["conpty.dll", "OpenConsole.exe"] {
        let src = Path::new(&manifest).join("conpty").join(name);
        println!("cargo:rerun-if-changed={}", src.display());
        if let Err(e) = std::fs::copy(&src, profile_dir.join(name)) {
            println!("cargo:warning=sideload_conpty: copy {} failed: {e}", src.display());
        }
    }
}
