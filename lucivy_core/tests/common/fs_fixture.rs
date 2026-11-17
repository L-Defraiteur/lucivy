//! Single-capture admission for the correctness fixture. Refusal and its durable
//! receipt precede the consumer; only immutable captured bytes reach the oracle.
//! This module is the entry used by v3_ground_truth_contains, not a parallel loader.

use std::{cell::RefCell, io::{self, Read, Write}, path::{Path, PathBuf}, rc::Rc};
use serde::Serialize;
use sha2::{Digest, Sha256};

pub const POLICY: &str = "v3-contains/fs-effective-order-v2";
pub const CAP: usize = 5000;
pub const EXCLUSIONS: &[&str] = &["target", "node_modules", ".git", "build", "__pycache__", "playground", ".vault", ".env*"];
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
    // Presence matters, including empty values and "0".
    pub flags: Vec<(String, String)>,
}
impl CapturePlan {
    pub fn initial(root: PathBuf) -> Self {
        Self { root: absolute(&root).expect("resolve capture root identity"), policy: POLICY.into(), cap: CAP,
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
        let m = std::fs::symlink_metadata(path)?;
        Ok(Metadata { directory: m.is_dir(), regular: m.is_file(), size: m.len() })
    }
    fn read(&mut self, path: &Path) -> io::Result<Vec<u8>> {
        self.trace.record(Event::Read(path.into()));
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let mut file = options.open(path)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "selected file changed type"));
        }
        let mut bytes = Vec::new();
        // Bound a file that grows after metadata as well as the initial size.
        Read::by_ref(&mut file).take(100_001).read_to_end(&mut bytes)?;
        if bytes.len() > 100_000 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "selected file grew beyond policy"));
        }
        Ok(bytes)
    }
}

#[derive(Debug, Serialize)]
pub enum Refusal {
    Empty,
    Plan { reason: String },
    Path { path: String },
    Manifest { path: PathBuf, kind: String },
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
    /// All offsets refer to the admitted bytes, never to a reopened path.
    pub fn literal_spans(&self, needle: &str, strict: bool) -> Vec<(usize, usize, usize)> {
        self.files.iter().enumerate().flat_map(|(ordinal, (_, content))| {
            super::corpus_oracle::literal_spans(content.as_bytes(), needle, strict)
                .expect("snapshot contains validated UTF-8").into_iter()
                .map(move |(from, to)| (ordinal, from, to))
        }).collect()
    }
}
pub fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Exclusive admission receipt, distinct from any index report. Reserve before
/// capture: reusing an invocation path cannot trigger a second selection.
/// Construction has no I/O. Parent must already exist, outside the input root.
pub struct ManifestSink {
    pub path: PathBuf,
    // All consumer outputs are declared before capture. None may alias the
    // input, another output, or an existing file.
    pub report_paths: Vec<PathBuf>,
    pub trace: Trace,
}
impl ManifestSink {
    fn reserve(&self) -> io::Result<std::fs::File> {
        let out = std::fs::OpenOptions::new().write(true).create_new(true).open(&self.path)?;
        self.trace.record(Event::Manifest(self.path.clone()));
        Ok(out)
    }
    fn publish(&self, mut out: std::fs::File, plan: &CapturePlan, paths: &InvocationPaths,
               snapshot: &Snapshot, refusal: Option<&Refusal>) -> io::Result<String> {
        let body = serde_json::to_vec(&serde_json::json!({
            "schema": 1, "plan": plan, "paths": paths, "documents": snapshot.pins(), "panel": PANEL,
            "verdict": if refusal.is_some() { "REFUSED" } else { "ADMITTED" },
            "refusal": refusal,
        }))?;
        let digest = sha256(&body);
        let envelope = serde_json::to_vec(&serde_json::json!({"sha256": digest, "body":
            serde_json::from_slice::<serde_json::Value>(&body)? }))?;
        out.write_all(&envelope)?;
        out.sync_all()?;
        std::fs::File::open(self.path.parent().ok_or_else(||
            io::Error::new(io::ErrorKind::InvalidInput, "manifest without parent"))?)?.sync_all()?;
        Ok(digest)
    }
}

/// Validated namespace shared by admission and the real consumer. Paths are
/// resolved once, including aliases in parents, before any output is reserved.
/// Private fields prevent the callback from substituting an undeclared output.
#[derive(Debug, Serialize)]
pub struct InvocationPaths {
    input_root: PathBuf,
    manifest: PathBuf,
    reports: Vec<PathBuf>,
}
impl InvocationPaths {
    pub fn manifest_path(&self) -> &Path { &self.manifest }
    pub fn report_path(&self, index: usize) -> Option<&Path> {
        self.reports.get(index).map(PathBuf::as_path)
    }

    fn prepare(plan: &CapturePlan, sink: &ManifestSink) -> Result<Self, Refusal> {
        let invalid = |reason: String| Refusal::Plan { reason };
        let root = absolute(&plan.root).map_err(|e| io_refusal("resolve", &plan.root, e))?;
        // Resolve the longest existing input prefix. An absent input remains an
        // absent input for CaptureFs, while its parents still have an identity.
        let mut prefix = root.clone();
        let mut missing = Vec::new();
        let mut input_root = loop {
            match std::fs::canonicalize(&prefix) {
                Ok(path) => break path,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    missing.push(prefix.file_name().ok_or_else(|| invalid("unresolvable input root".into()))?.to_os_string());
                    if !prefix.pop() { return Err(invalid("unresolvable input root".into())); }
                }
                Err(e) => return Err(io_refusal("resolve", &root, e)),
            }
        };
        for component in missing.into_iter().rev() { input_root.push(component); }
        if input_root.to_str().is_none() {
            return Err(invalid("resolved input root must have a lossless UTF-8 identity".into()));
        }
        let mut outputs = Vec::new();
        for declared in std::iter::once(&sink.path).chain(sink.report_paths.iter()) {
            let declared = absolute(declared).map_err(|e| io_refusal("resolve-output", declared, e))?;
            if declared.starts_with(&root) {
                return Err(invalid("all outputs must be outside the candidate input root".into()));
            }
            let parent = declared.parent().ok_or_else(|| invalid("output without parent".into()))?;
            let parent = std::fs::canonicalize(parent)
                .map_err(|e| io_refusal("resolve-output-parent", parent, e))?;
            let parent_metadata = std::fs::metadata(&parent)
                .map_err(|e| io_refusal("metadata-output-parent", &parent, e))?;
            if !parent_metadata.is_dir() {
                return Err(invalid("output parent must already be a directory".into()));
            }
            let name = declared.file_name().ok_or_else(|| invalid("output without filename".into()))?;
            let resolved = parent.join(name);
            if resolved.to_str().is_none() {
                return Err(invalid("resolved output must have a lossless UTF-8 identity".into()));
            }
            if resolved.starts_with(&input_root) {
                return Err(invalid("resolved output aliases the candidate input root".into()));
            }
            if outputs.contains(&resolved) {
                return Err(invalid("manifest and reports must name distinct outputs".into()));
            }
            match std::fs::symlink_metadata(&resolved) {
                Ok(_) if outputs.is_empty() => return Err(Refusal::Manifest {
                    path: resolved, kind: "AlreadyExists".into(),
                }),
                Ok(_) => return Err(invalid("report output already exists".into())),
                Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                Err(e) => return Err(io_refusal("metadata-output", &resolved, e)),
            }
            outputs.push(resolved);
        }
        let manifest = outputs.remove(0);
        Ok(Self { input_root, manifest, reports: outputs })
    }
}

/// The real consumer can create only its predeclared, resolved report. Opening
/// is exclusive and precedes the engine callback: no truncation or late build
/// on a report-open failure. No parent directory is created implicitly.
pub fn with_v3_report<H, R>(paths: &InvocationPaths, index: usize, trace: &Trace,
    build: impl FnOnce() -> H,
    panel: impl FnOnce(H, &mut std::fs::File) -> io::Result<R>) -> io::Result<R>
{
    let path = paths.report_path(index).ok_or_else(||
        io::Error::new(io::ErrorKind::InvalidInput, "undeclared report output"))?;
    trace.record(Event::Report);
    let mut report = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
    let handle = build();
    panel(handle, &mut report)
}

#[derive(Debug, PartialEq, Eq)]
pub enum OrdinalError { Missing, OutsideView { ordinal: u64, documents: usize } }

pub fn document_ordinal(value: Option<u64>, documents: usize) -> Result<usize, OrdinalError> {
    let value = value.ok_or(OrdinalError::Missing)?;
    let invalid = || OrdinalError::OutsideView { ordinal: value, documents };
    let ordinal = usize::try_from(value).map_err(|_| invalid())?;
    if ordinal >= documents { return Err(invalid()); }
    Ok(ordinal)
}

fn io_refusal(operation: &str, path: &Path, error: io::Error) -> Refusal {
    Refusal::Io { operation: operation.into(), path: path.into(), kind: format!("{:?}", error.kind()) }
}

// Lexical identity only; this does not open a corpus or resolve its links.
fn absolute(path: &Path) -> io::Result<PathBuf> {
    use std::path::Component;
    let joined = if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir()?.join(path) };
    let mut out = PathBuf::new();
    for part in joined.components() {
        match part {
            Component::CurDir => (),
            Component::ParentDir => { out.pop(); },
            part => out.push(part.as_os_str()),
        }
    }
    Ok(out)
}

fn validate(plan: &CapturePlan) -> Result<(), Refusal> {
    let names: Vec<String> = plan.flags.iter().filter(|(name, _)| name != "V3_CORPUS" && name != "V3_ADMISSION_MANIFEST" && name != "V3_CORRECTNESS_REPORT")
        .map(|(name, _)| name.clone()).collect();
    if !names.is_empty() { return Err(Refusal::Flags { names }); }
    if !plan.root.is_absolute() || plan.policy != POLICY || plan.cap != CAP ||
        plan.exclusions != EXCLUSIONS.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>() {
        return Err(Refusal::Plan { reason: "capture policy, cap and exclusions are fixed".into() });
    }
    Ok(())
}

fn capture(plan: &CapturePlan, fs: &mut impl CaptureFs, out: &mut Snapshot) -> Result<(), Refusal> {
    fn walk(dir: &Path, plan: &CapturePlan, fs: &mut impl CaptureFs, out: &mut Snapshot)
        -> Result<(), Refusal>
    {
        if out.files.len() >= plan.cap { return Ok(()); }
        let entries = fs.enumerate(dir).map_err(|e| io_refusal("enumerate", dir, e))?;
        for entry in entries {
            if out.files.len() >= plan.cap { break; }
            let path = entry.map_err(|e| io_refusal("entry", dir, e))?;
            let relative = path.strip_prefix(&plan.root).map_err(|_| Refusal::Path { path: format!("{path:?}") })?;
            if relative.components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
                return Err(Refusal::Path { path: format!("{path:?}") });
            }
            let name = path.file_name().and_then(|n| n.to_str())
                .ok_or_else(|| Refusal::Path { path: format!("{path:?}") })?;
            // Private metadata/content never becomes a candidate. This versioned
            // policy also excludes symlinks through symlink_metadata below.
            if name.starts_with(".env") || plan.exclusions.iter().any(|x| x == name) { continue; }
            let meta = fs.metadata(&path).map_err(|e| io_refusal("metadata", &path, e))?;
            if meta.directory {
                walk(&path, plan, fs, out)?;
            } else if meta.regular {
                if meta.size == 0 || meta.size > 100_000 { continue; }
                let bytes = fs.read(&path).map_err(|e| io_refusal("read", &path, e))?;
                // Binary exclusion is declared; malformed text is a refusal.
                if bytes.contains(&0) { continue; }
                let digest = sha256(&bytes);
                let content = String::from_utf8(bytes).map_err(|e| Refusal::Utf8 {
                    path: path.clone(), valid_up_to: e.utf8_error().valid_up_to(),
                })?;
                if content.trim().is_empty() { continue; }
                let relative = relative.to_str().ok_or_else(|| Refusal::Path { path: format!("{path:?}") })?.to_owned();
                out.pins.push(DocumentPin { ordinal: out.files.len(), path: relative.clone(),
                    bytes: content.len(), sha256: digest });
                out.files.push((relative, content));
            }
        }
        Ok(())
    }
    let root_meta = fs.metadata(&plan.root).map_err(|e| io_refusal("metadata", &plan.root, e))?;
    if !root_meta.directory {
        return Err(Refusal::Plan { reason: "capture root must be a directory, not a symlink or file".into() });
    }
    walk(&plan.root, plan, fs, out)
}

/// Validate configuration without effects, admit the complete namespace, reserve
/// once, capture once, check every fixed panel row, then fsync the receipt before
/// granting the consumer the snapshot and validated output slots.
/// There is no empty-success variant: every success consumed an admitted view.
pub fn with_v3_contains<R>(plan: &CapturePlan, fs: &mut impl CaptureFs,
    sink: &ManifestSink, trace: &Trace, consumer: impl FnOnce(&Snapshot, &InvocationPaths) -> R)
    -> Result<R, Refusal>
{
    // Invalid configuration has no receipt: the destination itself is not yet
    // admitted. In particular no metadata or output effect precedes Flags.
    validate(plan)?;
    if plan.root.to_str().is_none() {
        return Err(Refusal::Plan { reason: "capture root must have a lossless UTF-8 identity".into() });
    }
    let paths = InvocationPaths::prepare(plan, sink)?;
    let sink = ManifestSink { path: paths.manifest.clone(), report_paths: paths.reports.clone(),
        trace: sink.trace.clone() };
    let manifest_error = |e: io::Error| Refusal::Manifest { path: sink.path.clone(), kind: format!("{:?}", e.kind()) };
    let out = sink.reserve().map_err(&manifest_error)?;
    let mut snapshot = Snapshot { files: vec![], pins: vec![] };
    let admission = capture(plan, fs, &mut snapshot).and_then(|()| {
        if snapshot.files.is_empty() { return Err(Refusal::Empty); }
        let queries: Vec<String> = PANEL.iter().filter(|&&(needle, strict)|
            snapshot.literal_spans(needle, strict).is_empty())
            .map(|&(needle, strict)| format!("{needle}:{}", if strict { "strict" } else { "relax" })).collect();
        if queries.is_empty() { Ok(()) } else { Err(Refusal::Uncovered { queries }) }
    });
    sink.publish(out, plan, &paths, &snapshot, admission.as_ref().err()).map_err(manifest_error)?;
    admission?;
    trace.record(Event::Builder);
    Ok(consumer(&snapshot, &paths))
}
