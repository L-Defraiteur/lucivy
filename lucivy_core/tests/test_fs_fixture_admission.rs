//! Admission witnesses on the real correctness-entry collection seam. No corpus,
//! engine builder or persisted index; only tiny owned temporary fixtures.
mod common;
use common::{corpus_oracle, fs_fixture::*};
use std::{io, path::{Path, PathBuf}};

const COVERED: &[u8] = b"function return struct void rag3db include uint64_t std::unique_ptr ku_dynamic_cast TableFunction\n";

#[derive(Clone, Copy)]
enum Fault { Enumerate, Metadata, Read }
struct SpyFs { inner: StdFs, fault: Option<(Fault, PathBuf)> }
impl SpyFs {
    fn fails(&self, operation: Fault, path: &Path) -> bool {
        self.fault.as_ref().is_some_and(|(op, p)|
            std::mem::discriminant(op) == std::mem::discriminant(&operation) && p == path)
    }
    fn error() -> io::Error { io::Error::new(io::ErrorKind::PermissionDenied, "injected FS witness") }
}
impl CaptureFs for SpyFs {
    fn enumerate(&mut self, path: &Path) -> io::Result<Vec<io::Result<PathBuf>>> {
        if self.fails(Fault::Enumerate, path) {
            self.inner.trace.record(Event::Enumerate(path.into()));
            return Err(Self::error());
        }
        self.inner.enumerate(path)
    }
    fn metadata(&mut self, path: &Path) -> io::Result<Metadata> {
        if self.fails(Fault::Metadata, path) {
            self.inner.trace.record(Event::Metadata(path.into()));
            return Err(Self::error());
        }
        self.inner.metadata(path)
    }
    fn read(&mut self, path: &Path) -> io::Result<Vec<u8>> {
        if self.fails(Fault::Read, path) {
            self.inner.trace.record(Event::Read(path.into()));
            return Err(Self::error());
        }
        self.inner.read(path)
    }
}

fn fixture() -> tempfile::TempDir { common::scratch("fs-admission") }
fn run(root: &Path, fault: Option<(Fault, PathBuf)>) -> (Result<usize, Refusal>, Vec<Event>, PathBuf) {
    let plan = CapturePlan::initial(root.into());
    let trace = Trace::default();
    let mut fs = SpyFs { inner: StdFs { trace: trace.clone() }, fault };
    let manifest = root.parent().unwrap().join(format!("{}.admission.json",
        root.file_name().unwrap().to_string_lossy()));
    let sink = ManifestSink { path: manifest.clone(), trace: trace.clone() };
    let result = with_v3_contains(&plan, &mut fs, &sink, &trace, |snapshot| {
        trace.record(Event::Report);
        snapshot.files().len()
    });
    (result, trace.events(), manifest)
}
fn refused(result: &Result<usize, Refusal>, events: &[Event], manifest: &Path,
           reason: impl FnOnce(&Refusal) -> bool) {
    // Print the actual callback/effect history before the first failing assertion.
    eprintln!("admission={result:?}; events={events:?}");
    assert!(matches!(result, Err(r) if reason(r)), "missing precise REFUSED before builder: {result:?}");
    assert!(!events.iter().any(|e| matches!(e, Event::Builder | Event::Report |
        Event::Reuse(_) | Event::Delete(_) | Event::Persist(_))), "effect before admission: {events:?}");
    let receipt = receipt(manifest);
    assert_eq!(receipt["body"]["verdict"], "REFUSED");
    // This location is outside the candidate root, never fed back into capture.
    std::fs::remove_file(manifest).unwrap();
}

/// Preserve the complete receipt and its outer SHA in the judge's raw log
/// before removing the tiny fixture artifact. The body hash is independent.
fn receipt(path: &Path) -> serde_json::Value {
    let bytes = std::fs::read(path).unwrap();
    eprintln!("ADMISSION_RECEIPT path={path:?} sha256={} raw={}", sha256(&bytes),
        std::str::from_utf8(&bytes).unwrap());
    let receipt: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let body = serde_json::to_vec(&receipt["body"]).unwrap();
    assert_eq!(receipt["sha256"].as_str().unwrap(), sha256(&body));
    receipt
}

#[test]
fn empty_view_is_refused_before_builder() {
    let dir = fixture();
    let (r, e, m) = run(dir.path(), None);
    refused(&r, &e, &m, |x| matches!(x, Refusal::Empty));
}
fn io_case(fault: Fault, operation: &str) {
    let dir = fixture();
    std::fs::write(dir.path().join("covered.txt"), COVERED).unwrap();
    let path = match fault {
        Fault::Enumerate => dir.path().join("unreadable-directory"),
        _ => dir.path().join("selected.txt"),
    };
    match fault {
        Fault::Enumerate => std::fs::create_dir(&path).unwrap(),
        _ => std::fs::write(&path, COVERED).unwrap(),
    }
    let (r, e, m) = run(dir.path(), Some((fault, path.clone())));
    refused(&r, &e, &m, |x| matches!(x, Refusal::Io { operation: op, path: p, kind }
        if op == operation && p == &path && kind == "PermissionDenied"));
}
#[test]
fn enumeration_error_is_not_an_empty_subtree() { io_case(Fault::Enumerate, "enumerate"); }
#[test]
fn metadata_error_is_not_a_filtered_file() { io_case(Fault::Metadata, "metadata"); }
#[test]
fn read_error_is_not_a_filtered_file() { io_case(Fault::Read, "read"); }

#[test]
fn invalid_utf8_is_refused_not_dropped() {
    let dir = fixture();
    std::fs::write(dir.path().join("covered.txt"), COVERED).unwrap();
    let path = dir.path().join("invalid.txt");
    std::fs::write(&path, [0xff, b'x']).unwrap();
    let (r, e, m) = run(dir.path(), None);
    refused(&r, &e, &m, |x| matches!(x, Refusal::Utf8 { path: p, valid_up_to: 0 } if p == &path));
}

#[test]
fn uncovered_panel_is_refused_before_zero_equals_zero() {
    let dir = fixture();
    std::fs::write(dir.path().join("negative.txt"), b"unrelated vocabulary\n").unwrap();
    let (r, e, m) = run(dir.path(), None);
    refused(&r, &e, &m, |x| matches!(x, Refusal::Uncovered { queries } if queries.len() == PANEL.len()));
}

#[test]
fn covered_fixture_reaches_builder_once_on_the_captured_bytes() {
    // Independent known values also pin lowercase hex and leading zeroes.
    assert_eq!(sha256(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    assert_eq!(sha256(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    let dir = fixture();
    let path = dir.path().join("covered.txt");
    std::fs::write(&path, COVERED).unwrap();
    let plan = CapturePlan::initial(dir.path().into());
    let trace = Trace::default();
    let mut fs = SpyFs { inner: StdFs { trace: trace.clone() }, fault: None };
    let receipt_dir = fixture();
    let sink = ManifestSink { path: receipt_dir.path().join("admission.json"), trace: trace.clone() };
    let result = with_v3_contains(&plan, &mut fs, &sink, &trace, |snapshot| {
        assert_eq!(snapshot.files().len(), 1);
        // Mutation after admission cannot change the builder's or oracle's view.
        std::fs::write(&path, b"changed after capture").unwrap();
        let manifest = receipt(&sink.path);
        assert_eq!(manifest["body"]["verdict"], "ADMITTED");
        assert_eq!(manifest["body"]["documents"][0]["sha256"], sha256(COVERED));
        let bytes = snapshot.files()[0].1.as_bytes();
        assert_eq!(bytes, COVERED);
        assert_eq!(snapshot.pins()[0].sha256, sha256(bytes));
        for &(needle, strict) in PANEL {
            assert!(!snapshot.literal_spans(needle, strict).is_empty(),
                "captured oracle does not cover {needle:?}, strict={strict}");
            assert!(!corpus_oracle::literal_spans(bytes, needle, strict).unwrap().is_empty(),
                "fixture does not cover {needle:?}, strict={strict}");
        }
    });
    assert!(matches!(result, Ok(())));
    let events = trace.events();
    assert_eq!(events.iter().filter(|e| matches!(e, Event::Builder)).count(), 1);
    assert_eq!(events.iter().filter(|e| matches!(e, Event::Read(p) if p == &path)).count(), 1);
    let manifest = events.iter().position(|e| matches!(e, Event::Manifest(_))).unwrap();
    let builder = events.iter().position(|e| matches!(e, Event::Builder)).unwrap();
    assert!(manifest < builder);
    // No engine index/search comparison is run by this small positive witness.
}


#[test]
fn flag_presence_is_refused_before_any_corpus_operation() {
    for name in ["V3_INDEX_DIR", "V3_SPANS_REPORT_ONLY", "V3_DIAG", "V3_QUERIES",
                 "V3_LIMIT", "V3_MAX_DOCS", common::ABSENT_OK] {
        for value in ["", "0"] {
            let root = fixture();
            let receipts = fixture();
            let mut plan = CapturePlan::initial(root.path().into());
            plan.flags.push((name.into(), value.into()));
            let trace = Trace::default();
            let mut fs = SpyFs { inner: StdFs { trace: trace.clone() }, fault: None };
            let sink = ManifestSink { path: receipts.path().join("refused.json"), trace: trace.clone() };
            let result = with_v3_contains(&plan, &mut fs, &sink, &trace, |_| {
                trace.record(Event::Report);
            });
            eprintln!("flag={name:?} value={value:?} result={result:?} events={:?}", trace.events());
            assert!(matches!(result, Err(Refusal::Flags { names }) if names == vec![name.to_owned()]));
            assert!(trace.events().iter().all(|e| matches!(e, Event::Manifest(_))));
            assert_eq!(receipt(&sink.path)["body"]["verdict"], "REFUSED");
        }
    }
}

#[test]
fn absent_root_is_a_precise_refusal_without_skip() {
    let owned = fixture();
    let missing = owned.path().join("not-created");
    let (result, events, manifest) = run(&missing, None);
    refused(&result, &events, &manifest, |r| matches!(r,
        Refusal::Io { operation, path, kind } if operation == "metadata" && path == &missing && kind == "NotFound"));
}

#[test]
fn existing_receipt_prevents_recapture_after_refusal() {
    let root = fixture();
    let receipts = fixture();
    let plan = CapturePlan::initial(root.path().into());
    let trace = Trace::default();
    let mut fs = SpyFs { inner: StdFs { trace: trace.clone() }, fault: None };
    let sink = ManifestSink { path: receipts.path().join("once.json"), trace: trace.clone() };
    let first = with_v3_contains(&plan, &mut fs, &sink, &trace, |_| ());
    assert!(matches!(first, Err(Refusal::Empty)));
    let before = std::fs::read(&sink.path).unwrap();
    receipt(&sink.path);
    let events = trace.events();
    std::fs::write(root.path().join("now-covered.txt"), COVERED).unwrap();
    let second = with_v3_contains(&plan, &mut fs, &sink, &trace, |_| ());
    eprintln!("second={second:?} events={:?}", trace.events());
    assert!(matches!(second, Err(Refusal::Manifest { kind, .. }) if kind == "AlreadyExists"));
    assert_eq!(events, trace.events(), "a second invocation touched the candidate root");
    assert_eq!(before, std::fs::read(&sink.path).unwrap());
}
