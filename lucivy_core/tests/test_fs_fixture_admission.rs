//! RED witnesses on the real correctness-entry collection seam. No corpus,
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
fn run(root: &Path, fault: Option<(Fault, PathBuf)>) -> (Result<Option<usize>, Refusal>, Vec<Event>, PathBuf) {
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
fn refused(result: &Result<Option<usize>, Refusal>, events: &[Event], manifest: &Path,
           reason: impl FnOnce(&Refusal) -> bool) {
    // Print the actual callback/effect history before the first failing assertion.
    eprintln!("admission={result:?}; events={events:?}");
    assert!(matches!(result, Err(r) if reason(r)), "missing precise REFUSED before builder: {result:?}");
    assert!(!events.iter().any(|e| matches!(e, Event::Builder | Event::Report |
        Event::Reuse(_) | Event::Delete(_) | Event::Persist(_))), "effect before admission: {events:?}");
    let receipt: serde_json::Value = serde_json::from_slice(&std::fs::read(manifest).unwrap()).unwrap();
    assert_eq!(receipt["body"]["verdict"], "REFUSED");
    // This location is outside the candidate root, never fed back into capture.
    std::fs::remove_file(manifest).unwrap();
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
    let dir = fixture();
    let path = dir.path().join("covered.txt");
    std::fs::write(&path, COVERED).unwrap();
    let plan = CapturePlan::initial(dir.path().into());
    let trace = Trace::default();
    let mut fs = SpyFs { inner: StdFs { trace: trace.clone() }, fault: None };
    let sink = ManifestSink { path: dir.path().join("unused-red-manifest.json"), trace: trace.clone() };
    let result = with_v3_contains(&plan, &mut fs, &sink, &trace, |snapshot| {
        assert_eq!(snapshot.files().len(), 1);
        let bytes = snapshot.files()[0].1.as_bytes();
        assert_eq!(bytes, COVERED);
        assert_eq!(snapshot.pins()[0].sha256, sha256(bytes));
        for &(needle, strict) in PANEL {
            assert!(!corpus_oracle::literal_spans(bytes, needle, strict).unwrap().is_empty(),
                "fixture does not cover {needle:?}, strict={strict}");
        }
    });
    assert!(matches!(result, Ok(Some(()))));
    let events = trace.events();
    assert_eq!(events.iter().filter(|e| matches!(e, Event::Builder)).count(), 1);
    assert_eq!(events.iter().filter(|e| matches!(e, Event::Read(p) if p == &path)).count(), 1);
    // Baseline positive proves no blanket refusal. It does NOT qualify native
    // oracle read-once, receipt publication or a real index/search comparison.
}
