fn main() {
    // no_std implies -nodefaultlibs, and a Mach-O dylib must still link
    // libSystem (ld refuses otherwise). It is also where `abort` comes
    // from. On ELF the equivalent is libc.
    match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("macos" | "ios") => println!("cargo:rustc-link-lib=dylib=System"),
        _ => println!("cargo:rustc-link-lib=dylib=c"),
    }
}
