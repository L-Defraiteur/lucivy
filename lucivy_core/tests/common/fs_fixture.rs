//! FS correctness fixture, RED baseline. Deliberately retains silent drops,
//! empty success and missing coverage admission until their failures are seen.
//! This module is the entry used by v3_ground_truth_contains, not a parallel loader.

use std::{cell::RefCell, io::{self, Write}, path::{Path, PathBuf}, rc::Rc};
use serde::Serialize;
use sha2::{Digest, Sha256};

pub const POLICY: &str = "v3-contains/fs-effective-order-v1";
pub const CAP: usize = 5000;
pub const EXCLUSIONS: &[&str] = &["target", "node_modules", ".git", "build", "__pycache__", "playground"];
pub const PANEL: &[(&str, bool)] = &[
    ("function", true), ("function", false), ("return", true),
    ("struct", true), ("void", true), ("rag3db", true), ("include", true),
    ("uint64_t", true), ("uint64_t", false), ("std::unique_ptr", true),
    ("std::unique_ptr", false), ("ku_dynamic_cast", true),
    ("ku_dynamic_cast", false), ("TableFunction", true), ("TableFunction", false),
];

#[derive(Clone, Debug, Serialize)]
pub struct CapturePlan {
    pub root: PathBuf,
    pub policy: String,
    pub cap: usize,
    pub exclusions: Vec<String>,
    // Presence matters, including empty values and "0". Baseline ignores these.
    pub flags: Vec<(String, String)>,
}
impl CapturePlan {
    pub fn initial(root: PathBuf) -> Self {
        Self { root, policy: POLICY.into(), cap: CAP,
            exclusions: EXCLUSIONS.iter().map(|s| (*s).into()).collect(), flags: vec![] }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event { Enumerate(PathBuf), Metadata(PathBuf), Read(PathBuf), Builder, Report,
    Reuse(PathBuf), Delete(PathBuf), Persist(PathBuf), Manifest(PathBuf) }
#[derive(Clone, Default)]
pub struct Trace(Rc<RefCell<Vec<Event>>>);
impl Trace {
    pub fn record(&self, event: Event) { self.0.borrow_mut().push(event); }
    pub fn events(&self) -> Vec<Event> { self.0.borrow().clone() }
}

pub struct Metadata { pub directory: bool, pub regular: bool, pub size: u64 }
pub trait CaptureFs {
    fn enumerate(&mut self, path: &Path) -> io::Result<Vec<io::Result<PathBuf>>>;
    fn metadata(&mut self, path: &Path) -> io::Result<Metadata>;
    fn read(&mut self, path: &Path) -> io::Result<Vec<u8>>;
}
pub struct StdFs { pub trace: Trace }
impl CaptureFs for StdFs {
    fn enumerate(&mut self, path: &Path) -> io::Result<Vec<io::Result<PathBuf>>> {
        self.trace.record(Event::Enumerate(path.into()));
        // No sorting. Capture the effective order once.
        Ok(std::fs::read_dir(path)?.map(|e| e.map(|e| e.path())).collect())
    }
    fn metadata(&mut self, path: &Path) -> io::Result<Metadata> {
        self.trace.record(Event::Metadata(path.into()));
        let m = std::fs::metadata(path)?;
        Ok(Metadata { directory: m.is_dir(), regular: m.is_file(), size: m.len() })
    }
    fn read(&mut self, path: &Path) -> io::Result<Vec<u8>> {
        self.trace.record(Event::Read(path.into()));
        std::fs::read(path)
    }
}

#[derive(Debug, Serialize)]
pub enum Refusal {
    Empty,
    Io { operation: String, path: PathBuf, kind: String },
    Utf8 { path: PathBuf, valid_up_to: usize },
    Uncovered { queries: Vec<String> },
    Flags { names: Vec<String> },
}
#[derive(Clone, Debug, Serialize)]
pub struct DocumentPin { pub ordinal: usize, pub path: String, pub bytes: usize, pub sha256: String }
pub struct Snapshot {
    files: Vec<(String, String)>,
    pins: Vec<DocumentPin>,
}
impl Snapshot {
    pub fn files(&self) -> &[(String, String)] { &self.files }
    pub fn pins(&self) -> &[DocumentPin] { &self.pins }
}
pub fn sha256(bytes: &[u8]) -> String { format!("{:x}", Sha256::digest(bytes)) }

/// Prepared transport, deliberately NOT emitted as ADMITTED by the red baseline.
/// No I/O at construction. Parent must already exist; never truncates a receipt.
pub struct ManifestSink { pub path: PathBuf, pub trace: Trace }
impl ManifestSink {
    pub fn publish(&self, plan: &CapturePlan, snapshot: &Snapshot,
                   refusal: Option<&Refusal>) -> io::Result<String> {
        let body = serde_json::to_vec(&serde_json::json!({
            "schema": 1, "plan": plan, "documents": snapshot.pins(), "panel": PANEL,
            "verdict": if refusal.is_some() { "REFUSED" } else { "ADMITTED" },
            "refusal": refusal,
        }))?;
        let digest = sha256(&body);
        let envelope = serde_json::to_vec(&serde_json::json!({"sha256": digest, "body":
            serde_json::from_slice::<serde_json::Value>(&body)? }))?;
        self.trace.record(Event::Manifest(self.path.clone()));
        let mut out = std::fs::OpenOptions::new().write(true).create_new(true).open(&self.path)?;
        out.write_all(&envelope)?;
        out.sync_all()?;
        std::fs::File::open(self.path.parent().ok_or_else(||
            io::Error::new(io::ErrorKind::InvalidInput, "manifest without parent"))?)?.sync_all()?;
        Ok(digest)
    }
}

fn capture_baseline(plan: &CapturePlan, fs: &mut impl CaptureFs) -> Snapshot {
    fn walk(dir: &Path, plan: &CapturePlan, fs: &mut impl CaptureFs, out: &mut Snapshot) {
        if out.files.len() >= plan.cap { return; }
        let entries = match fs.enumerate(dir) { Ok(e) => e, Err(_) => return };
        for path in entries.into_iter().flatten() {
            if out.files.len() >= plan.cap { return; }
            let meta = match fs.metadata(&path) { Ok(m) => m, Err(_) => continue };
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if meta.directory {
                if !plan.exclusions.iter().any(|x| x == name.as_ref()) { walk(&path, plan, fs, out); }
            } else if meta.regular {
                if meta.size == 0 || meta.size > 100_000 { continue; }
                let bytes = match fs.read(&path) { Ok(b) => b, Err(_) => continue };
                if bytes.contains(&0) { continue; }
                let digest = sha256(&bytes);
                let content = match String::from_utf8(bytes) { Ok(s) => s, Err(_) => continue };
                if content.trim().is_empty() { continue; }
                let relative = path.strip_prefix(&plan.root).unwrap_or(&path).to_string_lossy().into_owned();
                out.pins.push(DocumentPin { ordinal: out.files.len(), path: relative.clone(),
                    bytes: content.len(), sha256: digest });
                out.files.push((relative, content));
            }
        }
    }
    let mut out = Snapshot { files: vec![], pins: vec![] };
    walk(&plan.root, plan, fs, &mut out);
    out
}

/// Real consumer seam. Current faults are intentional: no refusal/receipt,
/// no panel coverage check and empty success. A later fix must precede builder.
pub fn with_v3_contains<R>(plan: &CapturePlan, fs: &mut impl CaptureFs,
    _sink: &ManifestSink, trace: &Trace, consumer: impl FnOnce(&Snapshot) -> R)
    -> Result<Option<R>, Refusal>
{
    let snapshot = capture_baseline(plan, fs);
    if snapshot.files.is_empty() { return Ok(None); }
    trace.record(Event::Builder);
    Ok(Some(consumer(&snapshot)))
}
