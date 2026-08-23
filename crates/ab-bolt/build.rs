fn main() {
    // Inert in static (Apple) builds. The dynamic backend links the real
    // shared libraries; the search path covers host builds (target/debug)
    // and cross builds (target/<triple>/debug).
    if std::env::var("CARGO_FEATURE_DYNAMIC").is_err() {
        return;
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
    let triple = std::env::var("TARGET").unwrap_or_default();
    let host = std::env::var("HOST").unwrap_or_default();
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".into());
    let dir = if triple == host {
        root.join(&profile)
    } else {
        root.join(&triple).join(&profile)
    };
    let dir = dir
        .canonicalize()
        .expect("build liba/libb for this target first");
    println!("cargo:rustc-link-search=native={}", dir.display());
    // Only liba: crate b's Rust logic links statically into this module
    // (it is the first-class Rust API); only crate a's dynamic backend needs
    // the shared library. libb.so remains for C-ABI consumers.
    println!("cargo:rustc-link-lib=dylib=a");
    if matches!(std::env::var("CARGO_CFG_TARGET_OS").as_deref(), Ok("linux") | Ok("android")) {
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
    }
}
