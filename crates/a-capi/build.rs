fn main() {
    // ELF: without a SONAME, consumers record whatever path the linker was
    // given (e.g. target/.../liba.so), which breaks deployment.
    //
    // Release builds go through cargo-c (see package.sh), which sets the
    // VERSIONED soname itself and enables the `capi` feature while doing
    // so. Setting one here as well would pass -soname twice and leave the
    // winner to link order, so this unversioned fallback applies only to a
    // plain `cargo build`, where nothing else would set one at all.
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("linux" | "android") => {
            if std::env::var_os("CARGO_FEATURE_CAPI").is_none() {
                println!("cargo:rustc-link-arg=-Wl,-soname,liba.so");
            }
            // Export trimming is a CORRECTNESS requirement, not hygiene:
            // without it the cdylib exports the Rust runtime, and a second
            // Rust dylib in the process can interpose on it (the ELF twin
            // of the Windows import-library collision — see win-import.sh).
            println!("cargo:rustc-link-arg=-Wl,--version-script={manifest}/exports.map");
            println!("cargo:rerun-if-changed={manifest}/exports.map");
        }
        Ok("macos") => {
            println!("cargo:rustc-link-arg=-Wl,-exported_symbols_list,{manifest}/exports.exp");
            println!("cargo:rerun-if-changed={manifest}/exports.exp");
        }
        _ => {}
    }
}
