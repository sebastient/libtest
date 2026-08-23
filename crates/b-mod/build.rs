fn main() {
    // Dynamic backend on every platform: liba is the one implementation
    // home shared by all modules in the process.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
    let triple = std::env::var("TARGET").unwrap_or_default();
    let host = std::env::var("HOST").unwrap_or_default();
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".into());
    let dir = if triple == host {
        root.join(&profile)
    } else {
        root.join(&triple).join(&profile)
    };
    let mut dir = dir
        .canonicalize()
        .expect("build liba for this target first");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // Filtered import library only (see win-import.sh) — the raw DLL
        // must not be visible to the linker.
        dir = dir.join("winlink");
    }
    println!("cargo:rustc-link-search=native={}", dir.display());
    println!("cargo:rustc-link-lib=dylib=a");
    match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("linux") | Ok("android") => println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN"),
        Ok("macos") => println!("cargo:rustc-link-arg=-Wl,-rpath,{}", dir.display()),
        // Windows: no rpath — the loader searches beside the module / PATH.
        _ => {}
    }
}
