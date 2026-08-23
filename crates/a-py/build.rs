fn main() {
    // Python extension-module link flags (macOS: -undefined dynamic_lookup
    // for libpython symbols).
    pyo3_build_config::add_extension_module_link_args();

    // The dynamic backend links the real liba shared library (DT_NEEDED /
    // LC_LOAD_DYLIB) — symbol interposition between extension modules is NOT
    // reliable because Python dlopens them RTLD_LOCAL.
    if std::env::var("CARGO_FEATURE_DYNAMIC").is_ok() {
        let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".into());
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../target/{profile}"));
        let dir = dir
            .canonicalize()
            .expect("build liba first: run ./run.sh at the repo root");
        println!("cargo:rustc-link-search=native={}", dir.display());
        println!("cargo:rustc-link-lib=dylib=a");
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", dir.display());
    }
}
