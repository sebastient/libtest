fn main() {
    // The static backend's IOSurface storage needs the platform frameworks;
    // the link requirement propagates to whoever links crate a statically.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos")
        && std::env::var("CARGO_FEATURE_STATIC").is_ok()
    {
        println!("cargo:rustc-link-lib=framework=IOSurface");
        println!("cargo:rustc-link-lib=framework=CoreFoundation");
    }
}
