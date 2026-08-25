//! Test-only bridge: the fault-injecting, call-counting backend of
//! src/test_backends.{h,cc}. A separate bridge so the public header
//! (lib.rs.h) does not include test_backends.h; this module only exists in
//! test builds, the C++ side is compiled regardless (its symbols are simply
//! unused in the library).

#[cxx::bridge(namespace = "lucivy")]
pub(crate) mod ffi {
    unsafe extern "C++" {
        include!("test_backends.h");

        type BlobBackend = crate::ffi::BlobBackend;
        type MemBlobMap = crate::ffi::MemBlobMap;

        type BackendProbe;
        fn new_backend_probe() -> SharedPtr<BackendProbe>;
        fn new_probed_backend(
            map: SharedPtr<MemBlobMap>,
            probe: SharedPtr<BackendProbe>,
        ) -> UniquePtr<BlobBackend>;
        fn probe_set_fail_saves(probe: &BackendProbe, fail: bool);
        fn probe_set_lazy(probe: &BackendProbe, lazy: bool);
        // Whole loads of "index/file" since the last reset.
        fn probe_loads(probe: &BackendProbe, key: &str) -> u64;
        // Every "index/file" loaded whole since the last reset.
        fn probe_loaded_keys(probe: &BackendProbe) -> Vec<String>;
        // Range reads over every file since the last reset.
        fn probe_range_loads(probe: &BackendProbe) -> u64;
        fn probe_reset(probe: &BackendProbe);
    }
}
