fn main() {
    // src/lib.rs is the public bridge (its header, lucivy-cpp/src/lib.rs.h,
    // includes only headers from include/). src/test_bridge.rs binds the
    // test-only backends of src/test_backends.{h,cc}; their C++ side is
    // always compiled, the Rust side only under cfg(test).
    cxx_build::bridges(["src/lib.rs", "src/test_bridge.rs"])
        .include("include")
        .include("src")
        .file("src/mem_blob_backend.cc")
        .file("src/test_backends.cc")
        .std("c++17")
        .compile("lucivy_cpp");
    println!("cargo:rerun-if-changed=src/lib.rs");
    println!("cargo:rerun-if-changed=src/test_bridge.rs");
    println!("cargo:rerun-if-changed=src/mem_blob_backend.cc");
    println!("cargo:rerun-if-changed=src/test_backends.cc");
    println!("cargo:rerun-if-changed=src/test_backends.h");
    println!("cargo:rerun-if-changed=include/lucivy/blob_backend.h");
    println!("cargo:rerun-if-changed=include/lucivy/mem_blob_backend.h");
}
