fn main() {
    #[cfg(feature = "cxx-bridge")]
    {
        let mut build = cxx_build::bridge("src/bridge.rs");
        build.flag_if_supported("-std=c++17");

        // Emscripten disables exceptions by default, but cxx bridge needs throw.
        if std::env::var("TARGET").unwrap_or_default().contains("emscripten") {
            build.flag("-fexceptions");
            build.flag("-sDISABLE_EXCEPTION_CATCHING=0");
        }

        build.compile("lucivy_fts_cxx");
    }
}
