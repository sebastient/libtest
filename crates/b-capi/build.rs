fn main() {
    // On macOS the linker requires all symbols resolved by default; tell it
    // to defer a_* to load time (Linux/ELF shared objects already behave
    // this way, so nothing is needed there). Harmless in the static build.
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("macos") => {
            println!("cargo:rustc-link-arg=-undefined");
            println!("cargo:rustc-link-arg=dynamic_lookup");
            println!("cargo:rustc-link-arg=-Wl,-exported_symbols_list,{manifest}/exports.exp");
            println!("cargo:rerun-if-changed={manifest}/exports.exp");
        }
        // ELF: SONAME + export trimming (see a-capi/build.rs for why the
        // trim is mandatory).
        Ok("linux" | "android") => {
            if std::env::var_os("CARGO_FEATURE_CAPI").is_none() {
                println!("cargo:rustc-link-arg=-Wl,-soname,libb.so");
            }
            println!("cargo:rustc-link-arg=-Wl,--version-script={manifest}/exports.map");
            println!("cargo:rerun-if-changed={manifest}/exports.map");
        }
        _ => {}
    }
}
