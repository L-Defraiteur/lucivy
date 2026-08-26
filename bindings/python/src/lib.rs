//! lucivy — Python bindings for ld-lucivy BM25 full-text search.
//!
//! Unified on ShardedHandle (even single-shard uses ShardedHandle with shards=1).

//!
//! Threading rule: every call into the engine that may block on lucivy's own
//! scheduler threads (commits, merges, searches, and every blob-store call
//! made on the engine's behalf) runs inside `py.allow_threads`. A Python
//! blob store is called back from those threads under `Python::with_gil`;
//! if the calling thread still held the GIL while waiting for them, nothing
//! would ever finish.

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::Arc;

use ld_lucivy::query::HighlightSink;
use ld_lucivy::schema::{FieldType, Value as LucivyValue};
use ld_lucivy::LucivyDocument;

use pyo3::exceptions::{PyFileNotFoundError, PyKeyError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyByteArray, PyBytes, PyDict, PyList, PyString};

use lucistore::blob_store::BlobStore;
use lucivy_core::blob_directory::BlobLoadMode;
use lucivy_core::handle::NODE_ID_FIELD;
use lucivy_core::query;
use lucivy_core::snapshot;
use lucivy_core::sharded_handle::{BlobShardStorage, ShardedHandle, ShardedSearchResult};

// ─── PyBlobStore ───────────────────────────────────────────────────────────

/// A [`BlobStore`] that forwards every call to a Python object.
///
/// The object provides ``load``, ``save``, ``delete``, ``exists`` and
/// ``list``; ``blob_len`` and ``load_range`` are optional and only used when
/// present (lazy loading). Calls come from lucivy's scheduler threads, each
/// one taking the GIL for its duration.
struct PyBlobStore {
    obj: Py<PyAny>,
    has_blob_len: bool,
    has_load_range: bool,
}

const REQUIRED_STORE_METHODS: [&str; 5] = ["load", "save", "delete", "exists", "list"];

impl PyBlobStore {
    fn new(obj: &Bound<'_, PyAny>) -> PyResult<Self> {
        for name in REQUIRED_STORE_METHODS {
            if !obj.hasattr(name)? {
                return Err(PyTypeError::new_err(format!(
                    "blob store object has no '{name}' method (required: load, save, delete, exists, list)"
                )));
            }
        }
        Ok(Self {
            obj: obj.clone().unbind(),
            has_blob_len: obj.hasattr("blob_len")?,
            has_load_range: obj.hasattr("load_range")?,
        })
    }

    /// Run `f` on the store object under the GIL, mapping a Python exception
    /// to an `io::Error`: ``FileNotFoundError`` / ``KeyError`` become
    /// `NotFound` (the trait's "absent blob" signal), anything else carries
    /// the exception text.
    fn with<T>(&self, f: impl FnOnce(Python<'_>, &Bound<'_, PyAny>) -> PyResult<T>) -> io::Result<T> {
        Python::with_gil(|py| f(py, self.obj.bind(py)).map_err(|e| py_err_to_io(py, e)))
    }
}

fn py_err_to_io(py: Python<'_>, e: PyErr) -> io::Error {
    let kind = if e.is_instance_of::<PyFileNotFoundError>(py) || e.is_instance_of::<PyKeyError>(py) {
        io::ErrorKind::NotFound
    } else {
        io::ErrorKind::Other
    };
    io::Error::new(kind, format!("blob store raised {e}"))
}

/// Bytes out of whatever the store returned: ``bytes``, ``bytearray``, or
/// anything ``bytes()`` accepts (``memoryview``, a buffer).
fn bytes_of(value: &Bound<'_, PyAny>) -> PyResult<Vec<u8>> {
    if let Ok(b) = value.downcast::<PyBytes>() {
        return Ok(b.as_bytes().to_vec());
    }
    if let Ok(b) = value.downcast::<PyByteArray>() {
        return Ok(b.to_vec());
    }
    if value.is_none() || value.is_instance_of::<PyString>() || value.extract::<i64>().is_ok() {
        return Err(PyTypeError::new_err(format!(
            "blob store must return bytes-like data, got {}", value.get_type().name()?
        )));
    }
    let converted = value.py().get_type::<PyBytes>().call1((value,))?;
    Ok(converted.downcast_into::<PyBytes>()?.as_bytes().to_vec())
}

impl BlobStore for PyBlobStore {
    fn load(&self, index_name: &str, file_name: &str) -> io::Result<Vec<u8>> {
        self.with(|_, obj| bytes_of(&obj.call_method1("load", (index_name, file_name))?))
    }

    fn save(&self, index_name: &str, file_name: &str, data: &[u8]) -> io::Result<()> {
        self.with(|py, obj| {
            obj.call_method1("save", (index_name, file_name, PyBytes::new(py, data)))?;
            Ok(())
        })
    }

    fn delete(&self, index_name: &str, file_name: &str) -> io::Result<()> {
        self.with(|_, obj| {
            obj.call_method1("delete", (index_name, file_name))?;
            Ok(())
        })
    }

    fn exists(&self, index_name: &str, file_name: &str) -> io::Result<bool> {
        self.with(|_, obj| obj.call_method1("exists", (index_name, file_name))?.is_truthy())
    }

    fn list(&self, index_name: &str) -> io::Result<Vec<String>> {
        self.with(|_, obj| obj.call_method1("list", (index_name,))?.extract())
    }

    fn blob_len(&self, index_name: &str, file_name: &str) -> io::Result<Option<u64>> {
        if !self.has_blob_len {
            return Ok(None);
        }
        self.with(|_, obj| {
            let value = obj.call_method1("blob_len", (index_name, file_name))?;
            if value.is_none() { Ok(None) } else { value.extract().map(Some) }
        })
    }

    fn load_range(
        &self,
        index_name: &str,
        file_name: &str,
        range: std::ops::Range<u64>,
    ) -> io::Result<Option<Vec<u8>>> {
        if !self.has_load_range {
            return Ok(None);
        }
        self.with(|_, obj| {
            let value = obj.call_method1(
                "load_range",
                (index_name, file_name, range.start, range.end - range.start),
            )?;
            if value.is_none() { Ok(None) } else { bytes_of(&value).map(Some) }
        })
    }
}

/// Default mmap cache root for blob-backed indexes. `BlobShardStorage` adds
/// `<pid>/<Lucivy_name>_<n>/` under it, so every open gets a fresh leaf, and
/// removes that leaf when the index is released.
fn default_cache_dir() -> std::path::PathBuf {
    std::env::temp_dir().join("lucivy-blob-cache")
}

/// The storage backend of a Python-driven blob-backed index.
fn blob_storage(
    store: &Bound<'_, PyAny>,
    index_name: &str,
    cache_dir: Option<&str>,
    lazy: bool,
) -> PyResult<Box<BlobShardStorage<PyBlobStore>>> {
    let store = Arc::new(PyBlobStore::new(store)?);
    // Lazy mode with a file of unknown size downloads it at first open —
    // and the engine's lazy directory takes its `pending` lock twice on
    // that path (`get_file_handle` holds it across `materialize`), so a
    // store without `blob_len` would hang on the first segment open, not
    // degrade. Refuse up front until the core handles unknown sizes.
    if lazy && !store.has_blob_len {
        return Err(PyValueError::new_err(
            "lazy=True needs a blob_len(index_name, file_name) method on the store \
             (the size of a blob without loading it); add it, or open without lazy",
        ));
    }
    let cache = cache_dir.map(std::path::PathBuf::from).unwrap_or_else(default_cache_dir);
    let mode = if lazy { BlobLoadMode::Lazy } else { BlobLoadMode::Eager };
    Ok(Box::new(BlobShardStorage::new(store, index_name, cache).with_load_mode(mode)))
}

// ─── SearchResult ──────────────────────────────────────────────────────────

#[pyclass]
#[derive(Clone)]
struct SearchResult {
    #[pyo3(get)]
    doc_id: u64,
    #[pyo3(get)]
    score: f32,
    #[pyo3(get)]
    highlights: Option<HashMap<String, Vec<(u32, u32)>>>,
    #[pyo3(get)]
    fields: Option<HashMap<String, String>>,
}

#[pymethods]
impl SearchResult {
    fn __repr__(&self) -> String {
        let mut parts = vec![
            format!("doc_id={}", self.doc_id),
            format!("score={:.4}", self.score),
        ];
        if let Some(ref h) = self.highlights {
            parts.push(format!("highlights={:?}", h));
        }
        if let Some(ref f) = self.fields {
            parts.push(format!("fields={:?}", f));
        }
        format!("SearchResult({})", parts.join(", "))
    }
}

// ─── Index ─────────────────────────────────────────────────────────────────

/// Where an index's files live.
enum Backing {
    /// A directory on disk (`create`, `open`, `import_snapshot`).
    Dir(String),
    /// A LUCE snapshot served from memory: read-only, no files of its own.
    Snapshot,
    /// A Python blob store (`create_with_blob_store`, `open_with_blob_store`):
    /// writable, no directory. The store object is held here for the
    /// index's lifetime, on top of the reference the storage backend keeps
    /// (which `drop_index` consumes with the handle).
    Blob { index_name: String, _store: Py<PyAny> },
}

#[pyclass]
struct Index {
    /// `None` once `drop_index()` has consumed the handle (or `close()` released
    /// a served snapshot): every method then raises instead of touching it.
    handle: Option<ShardedHandle>,
    backing: Backing,
    user_fields: Vec<(String, String)>,
    text_fields: Vec<String>,
}

impl Index {
    fn from_handle(handle: ShardedHandle, backing: Backing) -> Self {
        let (user_fields, text_fields) = extract_user_fields(&handle.config);
        Self { handle: Some(handle), backing, user_fields, text_fields }
    }

    /// The live handle, or a clear error once the index has been dropped.
    fn h(&self) -> PyResult<&ShardedHandle> {
        self.handle.as_ref().ok_or_else(|| PyValueError::new_err(
            "index has been dropped (drop_index) or released: open it again with Index.open()",
        ))
    }

    /// The live handle for a mutation. An index served from a snapshot has a
    /// writer that buffers adds and deletes in memory, then fails at commit
    /// when the directory refuses the write — refuse the mutation itself
    /// instead, with the reason.
    fn writable(&self) -> PyResult<&ShardedHandle> {
        let handle = self.h()?;
        if matches!(self.backing, Backing::Snapshot) {
            return Err(PyValueError::new_err(
                "a snapshot is read-only: use Index.import_snapshot() to get an editable copy",
            ));
        }
        Ok(handle)
    }

    /// The index directory, or a clear error when the index has none.
    fn dir(&self) -> PyResult<&str> {
        match &self.backing {
            Backing::Dir(path) => Ok(path),
            Backing::Snapshot => Err(PyValueError::new_err(
                "this index is served from a snapshot in memory and has no directory: \
                 export from the source index instead",
            )),
            Backing::Blob { .. } => Err(PyValueError::new_err(
                "this index lives in a blob store and has no directory: snapshot and \
                 delta export are not available on it",
            )),
        }
    }
}

/// Releasing the handle commits and closes every shard, which may wait on
/// lucivy's threads — and, for a blob-backed index, on the Python store they
/// call. Dealloc runs with the GIL held: give it up for the duration.
impl Drop for Index {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            Python::with_gil(|py| py.allow_threads(move || drop(handle)));
        }
    }
}

/// `SchemaConfig` out of the Python field list, as `Index.create` takes it.
fn schema_config(fields: &Bound<'_, PyList>, shards: Option<usize>) -> PyResult<query::SchemaConfig> {
    let mut field_defs = Vec::new();
    for item in fields.iter() {
        let dict: &Bound<'_, PyDict> = item.downcast()?;
        let name: String = dict.get_item("name")?
            .ok_or_else(|| PyValueError::new_err("field missing 'name'"))?
            .extract()?;
        let field_type: String = dict.get_item("type")?
            .ok_or_else(|| PyValueError::new_err("field missing 'type'"))?
            .extract()?;
        let stored: Option<bool> = dict.get_item("stored")?.and_then(|v| v.extract().ok());
        let indexed: Option<bool> = dict.get_item("indexed")?.and_then(|v| v.extract().ok());
        let fast: Option<bool> = dict.get_item("fast")?.and_then(|v| v.extract().ok());
        field_defs.push(query::FieldDef {
            name,
            field_type,
            stored,
            indexed,
            fast,
        });
    }

    Ok(query::SchemaConfig {
        fields: field_defs,
        tokenizer: None,
        sfx: None,
        shards,
        ..Default::default()
    })
}

#[pymethods]
impl Index {
    /// Create a new index at the given path.
    ///
    /// Args:
    ///     path: Directory path for the index files.
    ///     fields: List of field definitions.
    ///     shards: Number of shards (default 1). More shards = faster search on large datasets.
    ///
    /// Field types: ``"text"`` (full-text, tokenized), ``"u64"``, ``"i64"``, ``"f64"``, ``"bool"``, ``"date"``.
    ///
    /// Example::
    ///
    ///     index = Index.create("/tmp/my_index", [
    ///         {"name": "title", "type": "text", "stored": True},
    ///         {"name": "body", "type": "text", "stored": True},
    ///         {"name": "score", "type": "f64", "fast": True},
    ///     ], shards=4)
    #[staticmethod]
    #[pyo3(signature = (path, fields, shards=None))]
    fn create(py: Python<'_>, path: &str, fields: &Bound<'_, PyList>, shards: Option<usize>) -> PyResult<Self> {
        let config = schema_config(fields, shards)?;
        let handle = py.allow_threads(|| ShardedHandle::create(path, &config))
            .map_err(|e| PyValueError::new_err(e))?;

        Ok(Self::from_handle(handle, Backing::Dir(path.to_string())))
    }

    /// Open an existing index at the given path.
    ///
    /// Reads the persisted schema and segment metadata from disk.
    /// The index must have been previously created with ``Index.create()``.
    ///
    /// Args:
    ///     path: Directory path of the existing index (same path used in ``create()``).
    ///
    /// Returns:
    ///     An ``Index`` ready for search, add, delete, etc.
    ///
    /// Example::
    ///
    ///     index = Index.open("/tmp/my_index")
    ///     results = index.search("hello")
    #[staticmethod]
    fn open(py: Python<'_>, path: &str) -> PyResult<Self> {
        let handle = py.allow_threads(|| ShardedHandle::open(path))
            .map_err(|e| PyValueError::new_err(e))?;

        Ok(Self::from_handle(handle, Backing::Dir(path.to_string())))
    }

    /// Create an index whose files live in a Python blob store.
    ///
    /// The store is any object with ``load``, ``save``, ``delete``,
    /// ``exists`` and ``list`` methods (``blob_len`` and ``load_range`` are
    /// optional, see ``lazy``). Blobs are the truth; the mmap cache under
    /// ``cache_dir`` is disposable and rebuilt on every open.
    ///
    /// The store's methods run on lucivy's own threads, not the caller's:
    /// they must be thread-safe and must not call back into the index.
    ///
    /// Args:
    ///     store: The blob store object.
    ///     index_name: Name of the index inside the store. Shard files are
    ///         saved under ``"Lucivy_<index_name>/shard_<i>"``, root files
    ///         under ``index_name`` itself.
    ///     fields: List of field definitions, as for ``create()``.
    ///     shards: Number of shards (default 1).
    ///     cache_dir: Root of the local mmap cache. Defaults to
    ///         ``lucivy-blob-cache`` under the system temp dir; each open
    ///         gets a fresh subdirectory, removed when the index is released.
    ///     lazy: Pull files from the store on first read instead of all at
    ///         open. Needs ``blob_len`` on the store to be effective;
    ///         ``load_range`` lets small probes skip the download entirely.
    ///
    /// Example::
    ///
    ///     index = Index.create_with_blob_store(store, "products", [
    ///         {"name": "title", "type": "text", "stored": True},
    ///     ])
    #[staticmethod]
    #[pyo3(signature = (store, index_name, fields, shards=1, cache_dir=None, lazy=false))]
    fn create_with_blob_store(
        py: Python<'_>,
        store: &Bound<'_, PyAny>,
        index_name: &str,
        fields: &Bound<'_, PyList>,
        shards: usize,
        cache_dir: Option<&str>,
        lazy: bool,
    ) -> PyResult<Self> {
        let config = schema_config(fields, Some(shards))?;
        let storage = blob_storage(store, index_name, cache_dir, lazy)?;
        let handle = py.allow_threads(|| ShardedHandle::create_with_storage(storage, &config))
            .map_err(|e| PyValueError::new_err(e))?;
        Ok(Self::from_handle(handle, Backing::Blob {
            index_name: index_name.to_string(),
            _store: store.clone().unbind(),
        }))
    }

    /// Open an index previously created with ``create_with_blob_store()``.
    ///
    /// Reads the schema and every shard back from the store. Same ``store``
    /// protocol, ``cache_dir`` and ``lazy`` as ``create_with_blob_store()``.
    ///
    /// Example::
    ///
    ///     index = Index.open_with_blob_store(store, "products", lazy=True)
    #[staticmethod]
    #[pyo3(signature = (store, index_name, cache_dir=None, lazy=false))]
    fn open_with_blob_store(
        py: Python<'_>,
        store: &Bound<'_, PyAny>,
        index_name: &str,
        cache_dir: Option<&str>,
        lazy: bool,
    ) -> PyResult<Self> {
        let storage = blob_storage(store, index_name, cache_dir, lazy)?;
        let handle = py.allow_threads(|| ShardedHandle::open_with_storage(storage))
            .map_err(|e| PyValueError::new_err(e))?;
        Ok(Self::from_handle(handle, Backing::Blob {
            index_name: index_name.to_string(),
            _store: store.clone().unbind(),
        }))
    }

    /// Add a document. Fields are passed as keyword arguments.
    ///
    /// Example::
    ///
    ///     index.add(1, title="Hello", body="World", score=3.14)
    ///
    /// Args:
    ///     doc_id: Unique document ID (_node_id).
    ///     **kwargs: Field values matching the schema (title=, body=, ...).
    #[pyo3(signature = (doc_id, **kwargs))]
    fn add(&self, py: Python<'_>, doc_id: u64, kwargs: Option<&Bound<'_, PyDict>>) -> PyResult<()> {
        let kwargs = kwargs.ok_or_else(|| PyValueError::new_err("at least one field is required"))?;
        let mut doc = LucivyDocument::new();

        let handle = self.writable()?;
        let nid_field = handle.field(NODE_ID_FIELD)
            .ok_or_else(|| PyValueError::new_err("no _node_id field in schema"))?;
        doc.add_u64(nid_field, doc_id);

        add_fields_from_dict(handle, &mut doc, kwargs)?;

        py.allow_threads(|| handle.add_document(doc, doc_id))
            .map_err(|e| PyValueError::new_err(e))
    }

    /// Add multiple documents at once.
    ///
    /// Example::
    ///
    ///     index.add_many([
    ///         {"doc_id": 1, "title": "Hello", "body": "World"},
    ///         {"doc_id": 2, "title": "Foo", "body": "Bar"},
    ///     ])
    ///
    /// Each dict must have a ``doc_id`` key. Other keys are field values.
    fn add_many(&self, py: Python<'_>, docs: &Bound<'_, PyList>) -> PyResult<()> {
        let handle = self.writable()?;
        let nid_field = handle.field(NODE_ID_FIELD)
            .ok_or_else(|| PyValueError::new_err("no _node_id field in schema"))?;

        for item in docs.iter() {
            let dict: &Bound<'_, PyDict> = item.downcast()?;
            let doc_id: u64 = dict.get_item("doc_id")?
                .ok_or_else(|| PyValueError::new_err("each doc must have a 'doc_id' key"))?
                .extract()?;

            let mut doc = LucivyDocument::new();
            doc.add_u64(nid_field, doc_id);

            for (key, value) in dict.iter() {
                let field_name: String = key.extract()?;
                if field_name == "doc_id" { continue; }
                add_field_value(handle, &mut doc, &field_name, &value)?;
            }

            py.allow_threads(|| handle.add_document(doc, doc_id))
                .map_err(|e| PyValueError::new_err(e))?;
        }
        Ok(())
    }

    /// Delete a document by its ``_node_id``.
    ///
    /// The deletion is staged in memory. Call ``commit()`` or run a search
    /// (which auto-commits via lazy commit) to make the deletion visible.
    ///
    /// Args:
    ///     doc_id: The ``_node_id`` of the document to delete.
    ///
    /// Example::
    ///
    ///     index.delete(42)
    ///     index.commit()
    fn delete(&self, py: Python<'_>, doc_id: u64) -> PyResult<()> {
        let handle = self.writable()?;
        py.allow_threads(|| handle.delete_by_node_id(doc_id))
            .map_err(|e| PyValueError::new_err(e))
    }

    /// Update a document (delete + re-add). Same kwargs syntax as ``add()``.
    ///
    /// Example::
    ///
    ///     index.update(1, title="New title", body="New body")
    #[pyo3(signature = (doc_id, **kwargs))]
    fn update(&self, py: Python<'_>, doc_id: u64, kwargs: Option<&Bound<'_, PyDict>>) -> PyResult<()> {
        self.delete(py, doc_id)?;
        self.add(py, doc_id, kwargs)
    }

    /// Commit pending changes to disk, making them visible to subsequent searches.
    ///
    /// Lucivy uses lazy commit: if you search without calling ``commit()``,
    /// uncommitted changes are auto-flushed before the search executes.
    /// Call ``commit()`` explicitly when you need to control the commit point
    /// (e.g., after a batch of adds/deletes).
    ///
    /// Example::
    ///
    ///     index.add(1, title="Hello")
    ///     index.add(2, title="World")
    ///     index.commit()  # both docs now searchable
    fn commit(&self, py: Python<'_>) -> PyResult<()> {
        let handle = self.writable()?;
        py.allow_threads(|| handle.commit())
            .map_err(|e| PyValueError::new_err(e))
    }

    /// Flush any pending writes and release the writer lock.
    ///
    /// After ``close()``, the index data remains on disk and can be re-opened
    /// with ``Index.open()``. No further mutations are allowed on this instance.
    /// On an index served from a snapshot (``Index.open_snapshot()``) there is
    /// nothing to flush: ``close()`` releases the blob and the instance is done.
    ///
    /// Example::
    ///
    ///     index.close()
    ///     # later...
    ///     index = Index.open("/tmp/my_index")
    fn close(&mut self, py: Python<'_>) -> PyResult<()> {
        if matches!(self.backing, Backing::Snapshot) {
            let handle = self.handle.take().ok_or_else(|| PyValueError::new_err(
                "index has been dropped (drop_index) or released: open it again with Index.open()",
            ))?;
            py.allow_threads(move || drop(handle));
            return Ok(());
        }
        let handle = self.h()?;
        py.allow_threads(|| handle.close())
            .map_err(|e| PyValueError::new_err(e))
    }

    /// Search the index.
    ///
    /// Args:
    ///     query: Either a plain string or a query dict.
    ///     limit: Maximum number of results (default 10).
    ///     highlights: If True, return highlight byte offsets per field.
    ///     allowed_ids: Optional list of _node_id values to filter on.
    ///     fields: If True, return stored field values with each result.
    ///
    /// Query formats:
    ///     String: ``"hello world"`` — auto contains_split across all text fields.
    ///
    ///     Dict with ``type`` key (all substring queries are cross-token):
    ///
    ///     ``{"type": "contains", "field": "body", "value": "lock"}``
    ///         Substring match. Finds "lock" inside "unlock", "locking", etc.
    ///
    ///     ``{"type": "contains", "field": "body", "value": "lock", "distance": 1}``
    ///         Fuzzy substring (Levenshtein). Finds "lock", "look", "lack", etc.
    ///
    ///     ``{"type": "contains", "field": "body", "value": "lock.*init", "regex": true}``
    ///         Regex substring. Cross-token regex matching.
    ///
    ///     ``{"type": "startsWith", "field": "body", "value": "lock"}``
    ///         Token prefix. Finds tokens starting with "lock" (lock, locks, locking...).
    ///
    ///     ``{"type": "contains_split", "field": "body", "value": "struct device"}``
    ///         Split on whitespace, each word as contains, combined with boolean OR.
    ///
    ///     ``{"type": "term", "field": "body", "value": "lock"}``
    ///         Exact whole-token match (anchor_start + exact_match).
    ///
    ///     ``{"type": "fuzzy", "field": "body", "value": "schdule", "distance": 1}``
    ///         Compat alias for contains + distance.
    ///
    ///     ``{"type": "phrase", "field": "body", "value": "mutex lock"}``
    ///         Adjacent tokens in order.
    ///
    ///     ``{"type": "regex", "field": "body", "pattern": "sched[a-z]+"}``
    ///         Standard regex on individual tokens.
    ///
    ///     ``{"type": "boolean", "must": [...], "should": [...], "must_not": [...]}``
    ///         Boolean combination of sub-queries.
    ///
    ///     ``{"type": "disjunction_max", "queries": [...], "tie_breaker": 0.1}``
    ///         Best-score from sub-queries with tie-breaker.
    ///
    ///     ``{"type": "more_like_this", "field": "body", "value": "sample text",``
    ///     ``"min_doc_frequency": 1, "min_term_frequency": 1, "min_word_length": 3}``
    ///         TF-IDF similarity search.
    ///
    /// Filtering:
    ///     ``allowed_ids``: Pre-filter by _node_id (fast, bitmap-based)::
    ///
    ///         index.search({"type": "contains", ...}, allowed_ids=[1, 2, 3])
    ///
    ///     ``filters`` key in query dict: Filter on non-text fields (combined with AND)::
    ///
    ///         {"type": "contains", "field": "body", "value": "lock",
    ///          "filters": [
    ///              {"field": "category", "op": "eq", "value": "kernel"},
    ///              {"field": "score", "op": "gte", "value": 0.5},
    ///              {"field": "status", "op": "in", "value": ["active", "review"]},
    ///          ]}
    ///
    ///     Filter ops: ``eq``, ``ne``, ``lt``, ``lte``, ``gt``, ``gte``,
    ///     ``in``, ``not_in``, ``between``, ``starts_with``, ``contains``.
    ///     Composite: ``must``, ``should``, ``must_not`` with nested ``clauses``.
    /// Honest warnings for a query, without running it.
    ///
    /// Returns a list of plain-text warnings describing what the engine will
    /// actually search and where it falls back to brute force: separators
    /// ignored in relaxed mode, fuzzy distance too loose for the query
    /// length, regex without a usable literal (full scan), segments written
    /// by the legacy indexer. Empty when nothing applies.
    ///
    /// Example::
    ///
    ///     for w in index.query_warnings({"type": "regex", "value": "[0-9]{8}"}):
    ///         print("warning:", w)
    fn query_warnings(&self, py: Python<'_>, query: &Bound<'_, PyAny>) -> PyResult<Vec<String>> {
        let query_config = self.parse_query(query)?;
        let handle = self.h()?;
        Ok(py.allow_threads(|| handle.query_warnings(&query_config)))
    }

    #[pyo3(signature = (query, limit=10, highlights=false, allowed_ids=None, fields=false))]
    fn search(
        &self,
        py: Python<'_>,
        query: &Bound<'_, PyAny>,
        limit: u32,
        highlights: bool,
        allowed_ids: Option<Vec<u64>>,
        fields: bool,
    ) -> PyResult<Vec<SearchResult>> {
        let query_config = self.parse_query(query)?;
        let handle = self.h()?;

        let highlight_sink = if highlights {
            Some(Arc::new(HighlightSink::new()))
        } else {
            None
        };

        py.allow_threads(|| {
            let results = match allowed_ids {
                Some(ids) => {
                    let id_set: HashSet<u64> = ids.into_iter().collect();
                    handle.search_filtered(&query_config, limit as usize, highlight_sink.clone(), id_set)
                        .map_err(|e| PyValueError::new_err(e))?
                }
                None => handle.search(&query_config, limit as usize, highlight_sink.clone())
                    .map_err(|e| PyValueError::new_err(e))?,
            };
            collect_sharded_results(handle, &results, highlight_sink.as_deref(), fields)
        })
    }

    /// Number of documents in the index (property, no parentheses).
    ///
    /// Example::
    ///
    ///     count = index.num_docs  # not index.num_docs()
    #[getter]
    fn num_docs(&self, py: Python<'_>) -> PyResult<u64> {
        let handle = self.h()?;
        Ok(py.allow_threads(|| handle.num_docs()))
    }

    /// Number of shards (property).
    #[getter]
    fn num_shards(&self) -> PyResult<usize> {
        Ok(self.h()?.num_shards())
    }

    /// Index directory path (property). ``None`` for an index served from a
    /// snapshot in memory (``Index.open_snapshot()``) or living in a blob
    /// store (``Index.create_with_blob_store()``).
    #[getter]
    fn path(&self) -> Option<&str> {
        match &self.backing {
            Backing::Dir(path) => Some(path),
            _ => None,
        }
    }

    /// Name of the index inside its blob store (property). ``None`` unless
    /// the index was created or opened with a blob store.
    #[getter]
    fn blob_index_name(&self) -> Option<&str> {
        match &self.backing {
            Backing::Blob { index_name, .. } => Some(index_name),
            _ => None,
        }
    }

    /// On-disk bytes of every searchable segment, across all shards.
    ///
    /// Counts the files the current segments actually reference (not
    /// leftovers awaiting garbage collection), so it is the size a snapshot
    /// export or a full in-memory load will cost. For an index served from a
    /// snapshot it measures the live slices of the blob.
    ///
    /// Example::
    ///
    ///     index.commit()
    ///     print(index.index_bytes() >> 20, "MB")
    fn index_bytes(&self, py: Python<'_>) -> PyResult<u64> {
        let handle = self.h()?;
        Ok(py.allow_threads(|| handle.index_bytes()))
    }

    /// True when the last search hit the per-segment match cap
    /// (`LUCIVY_MAX_MATCHES_PER_SEGMENT`, `0` disables it) on some segment:
    /// the hits are real, but some documents were never looked at — a
    /// one-letter query over a large corpus, typically.
    #[getter]
    fn last_search_truncated(&self) -> PyResult<bool> {
        Ok(self.h()?.last_search_truncated())
    }

    /// Merge every shard's segments into segments of at most ``max_docs``
    /// documents, then commit. Returns the number of merges performed.
    ///
    /// Call it once after a bulk load: search time grows with the segment
    /// count, and the background merge policy only catches up gradually.
    /// Blocks until the merges are done. Not for the browser build — a
    /// compaction sizes its arenas from its inputs.
    ///
    /// Args:
    ///     max_docs: Upper bound on documents per merged segment (default 10000).
    ///
    /// Example::
    ///
    ///     index.add_many(docs)
    ///     merges = index.compact()
    #[pyo3(signature = (max_docs=10000))]
    fn compact(&self, py: Python<'_>, max_docs: usize) -> PyResult<usize> {
        let handle = self.writable()?;
        py.allow_threads(|| handle.compact(max_docs))
            .map_err(|e| PyValueError::new_err(e))
    }

    /// Block until no background merge is running or about to start.
    ///
    /// ``commit()`` returning never meant nothing was merging: the merge
    /// policy plans its next round from the segments a commit just
    /// published. Call this before anything that will claim a lot of
    /// memory — an ``export_snapshot()`` of a large index, a full preload —
    /// so it does not race a merge for the address space.
    ///
    /// Returns:
    ///     The number of rounds that still saw merge activity (0 = it was
    ///     already quiet).
    ///
    /// Example::
    ///
    ///     index.commit()
    ///     index.wait_merges_quiet()
    ///     blob = index.export_snapshot()
    fn wait_merges_quiet(&self, py: Python<'_>) -> PyResult<usize> {
        let handle = self.h()?;
        py.allow_threads(|| handle.wait_merges_quiet())
            .map_err(|e| PyValueError::new_err(e))
    }

    /// Delete the whole index: commit and release everything, then remove
    /// the index directory (shard files and root files included). For an
    /// index in a blob store, every blob is deleted through the store —
    /// the shard namespaces ``"Lucivy_<name>/shard_<i>"`` and the root
    /// namespace ``"<name>"`` are listed and emptied one file at a time.
    ///
    /// Consumes the handle: after ``drop_index()`` every method of this
    /// instance raises ``ValueError``, like after ``close()`` but with the
    /// files gone too. Raises ``ValueError`` for an index served from a
    /// snapshot (nothing on disk to drop).
    ///
    /// Example::
    ///
    ///     index.drop_index()
    ///     assert not os.path.exists(path)
    fn drop_index(&mut self, py: Python<'_>) -> PyResult<()> {
        if matches!(self.backing, Backing::Snapshot) {
            self.dir()?;
        }
        let handle = self.handle.take().ok_or_else(|| PyValueError::new_err(
            "index has already been dropped",
        ))?;
        py.allow_threads(move || handle.drop_index())
            .map_err(|e| PyValueError::new_err(e))
    }

    /// Schema as list of ``{"name": "...", "type": "..."}`` dicts (property).
    #[getter]
    fn schema(&self) -> Vec<HashMap<String, String>> {
        self.user_fields.iter().map(|(name, ft)| {
            let mut m = HashMap::new();
            m.insert("name".to_string(), name.clone());
            m.insert("type".to_string(), ft.clone());
            m
        }).collect()
    }

    /// Export this index as a LUCE snapshot (bytes).
    ///
    /// Returns the full index content as a binary blob that can be stored,
    /// transferred, or later restored with ``Index.import_snapshot()``.
    /// Includes all shards, schema, and segment data.
    ///
    /// Returns:
    ///     ``bytes`` containing the LUCE snapshot.
    ///
    /// Example::
    ///
    ///     blob = index.export_snapshot()
    ///     with open("backup.luce", "wb") as f:
    ///         f.write(blob)
    fn export_snapshot<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        let handle = self.h()?;
        let dir = self.dir()?;
        let blob = py.allow_threads(|| {
            snapshot::export_to_snapshot(handle, std::path::Path::new(dir))
        }).map_err(|e| PyValueError::new_err(e))?;
        Ok(PyBytes::new(py, &blob))
    }

    /// Export this index as a LUCE snapshot directly to a file.
    ///
    /// Writes the LUCE binary blob to the given path. Equivalent to
    /// ``export_snapshot()`` followed by a file write, but avoids
    /// returning the bytes to Python.
    ///
    /// Args:
    ///     path: Destination file path (typically ending in ``.luce``).
    ///
    /// Example::
    ///
    ///     index.export_snapshot_to("/backups/my_index.luce")
    fn export_snapshot_to(&self, py: Python<'_>, path: &str) -> PyResult<()> {
        let handle = self.h()?;
        let dir = self.dir()?;
        py.allow_threads(|| {
            let blob = snapshot::export_to_snapshot(handle, std::path::Path::new(dir))
                .map_err(|e| PyValueError::new_err(e))?;
            std::fs::write(path, &blob)
                .map_err(|e| PyValueError::new_err(format!("cannot write snapshot: {e}")))
        })
    }

    /// Import an index from a LUCE snapshot (bytes).
    ///
    /// Restores a full index from a binary blob previously created by
    /// ``export_snapshot()``. The index files are written to ``dest_path``.
    ///
    /// Args:
    ///     data: Raw LUCE snapshot bytes.
    ///     dest_path: Directory to write the restored index into.
    ///         Defaults to ``"/tmp/lucivy_import"``.
    ///
    /// Returns:
    ///     A new ``Index`` instance ready for search.
    ///
    /// Example::
    ///
    ///     with open("backup.luce", "rb") as f:
    ///         blob = f.read()
    ///     index = Index.import_snapshot(blob, "/data/restored_index")
    #[staticmethod]
    #[pyo3(signature = (data, dest_path=None))]
    fn import_snapshot(py: Python<'_>, data: &[u8], dest_path: Option<&str>) -> PyResult<Self> {
        let dest = dest_path.unwrap_or("/tmp/lucivy_import");
        let dest_path = std::path::Path::new(dest);
        let handle = py.allow_threads(|| snapshot::import_from_snapshot(data, dest_path))
            .map_err(|e| PyValueError::new_err(e))?;
        Ok(Self::from_handle(handle, Backing::Dir(dest.to_string())))
    }

    /// Import an index from a LUCE snapshot file (.luce).
    ///
    /// Reads the snapshot from a file and restores it. Convenience wrapper
    /// around ``import_snapshot()`` that handles the file read.
    ///
    /// Args:
    ///     path: Path to the ``.luce`` snapshot file.
    ///     dest_path: Directory to write the restored index into.
    ///         Defaults to ``"/tmp/lucivy_import"``.
    ///
    /// Returns:
    ///     A new ``Index`` instance ready for search.
    ///
    /// Example::
    ///
    ///     index = Index.import_snapshot_from("/backups/my_index.luce", "/data/restored")
    #[staticmethod]
    #[pyo3(signature = (path, dest_path=None))]
    fn import_snapshot_from(py: Python<'_>, path: &str, dest_path: Option<&str>) -> PyResult<Self> {
        let data = std::fs::read(path)
            .map_err(|e| PyValueError::new_err(format!("cannot read snapshot: {e}")))?;
        Self::import_snapshot(py, &data, dest_path)
    }

    /// Serve a LUCE snapshot straight from memory, without extracting it.
    ///
    /// ``import_snapshot()`` writes every file out, so the blob and the files
    /// exist at once. Here the blob *is* the index: readers get slices of it,
    /// nothing is written to disk, and the memory cost is the blob's own
    /// length. Search, highlights, ``index_bytes()`` and the distributed
    /// helpers all work.
    ///
    /// Read-only by construction: ``add()``, ``delete()``, ``update()``,
    /// ``commit()`` and ``compact()`` raise ``ValueError``, ``path`` is
    /// ``None``, and the export / delta methods raise because there is no
    /// directory to export from. To edit, ``import_snapshot()`` it instead.
    ///
    /// Args:
    ///     data: Raw LUCE snapshot bytes (from ``export_snapshot()``).
    ///
    /// Returns:
    ///     A read-only ``Index`` backed by the bytes.
    ///
    /// Example::
    ///
    ///     blob = source.export_snapshot()
    ///     served = Index.open_snapshot(blob)
    ///     served.search("hello")  # same answers as source.search("hello")
    #[staticmethod]
    fn open_snapshot(py: Python<'_>, data: &[u8]) -> PyResult<Self> {
        let bytes = ld_lucivy::directory::OwnedBytes::new(data.to_vec());
        let handle = py.allow_threads(|| ShardedHandle::open_snapshot(bytes))
            .map_err(|e| PyValueError::new_err(e))?;
        Ok(Self::from_handle(handle, Backing::Snapshot))
    }

    /// Serve a LUCE snapshot file (.luce) straight from memory.
    ///
    /// Reads the file once and hands it to ``open_snapshot()``: the index is
    /// read-only and nothing is extracted next to the file.
    ///
    /// Args:
    ///     path: Path to the ``.luce`` snapshot file.
    ///
    /// Returns:
    ///     A read-only ``Index`` backed by the file's bytes.
    ///
    /// Example::
    ///
    ///     served = Index.open_snapshot_from("/backups/my_index.luce")
    #[staticmethod]
    fn open_snapshot_from(py: Python<'_>, path: &str) -> PyResult<Self> {
        let data = std::fs::read(path)
            .map_err(|e| PyValueError::new_err(format!("cannot read snapshot: {e}")))?;
        Self::open_snapshot(py, &data)
    }

    // ── Delta sync ──────────────────────────────────────────────────────

    /// Per-shard version info for delta sync (property, no parentheses).
    ///
    /// Returns the current version and segment IDs for each shard.
    /// Pass this to a remote server's ``export_sharded_delta()`` to
    /// receive only the segments that changed since your last sync.
    ///
    /// Returns:
    ///     ``list[dict]`` — each dict has keys:
    ///     ``{"shard_id": int, "version": str, "segment_ids": [str, ...]}``.
    ///
    /// Example::
    ///
    ///     versions = index.shard_versions  # not shard_versions()
    ///     # [{"shard_id": 0, "version": "abc", "segment_ids": ["x", "y"]}, ...]
    #[getter]
    fn shard_versions(&self, py: Python<'_>) -> PyResult<Vec<HashMap<String, Py<PyAny>>>> {
        let handle = self.h()?;
        let versions = py.allow_threads(|| handle.shard_versions())
            .map_err(|e| PyValueError::new_err(e))?;
        Ok(versions.iter().map(|sv| {
            let mut m = HashMap::new();
            m.insert("shard_id".into(), sv.shard_id.into_pyobject(py).unwrap().into_any().unbind());
            m.insert("version".into(), sv.version.clone().into_pyobject(py).unwrap().into_any().unbind());
            let ids: Vec<String> = sv.segment_ids.iter().cloned().collect();
            m.insert("segment_ids".into(), ids.into_pyobject(py).unwrap().into_any().unbind());
            m
        }).collect())
    }

    /// Export a sharded delta (LUCIDS blob) containing only segments that
    /// changed since the client's known versions.
    ///
    /// Used for incremental sync: the client sends its ``shard_versions``,
    /// the server computes and returns only the diff.
    ///
    /// Args:
    ///     client_versions: List of dicts, each with keys
    ///         ``{"shard_id": int, "version": str, "segment_ids": [str]}``.
    ///         Typically obtained from the client's ``shard_versions`` property.
    ///
    /// Returns:
    ///     ``bytes`` — the LUCIDS binary delta blob.
    ///
    /// Example::
    ///
    ///     delta = server_index.export_sharded_delta(client_index.shard_versions)
    ///     client_index.apply_sharded_delta(delta)
    fn export_sharded_delta<'py>(
        &self,
        py: Python<'py>,
        client_versions: Vec<HashMap<String, Py<PyAny>>>,
    ) -> PyResult<Bound<'py, PyBytes>> {
        let versions: Vec<lucistore::delta_sharded::ShardVersion> = client_versions.iter()
            .map(|m| {
                let get = |key: &str| m.get(key).ok_or_else(|| {
                    PyValueError::new_err(format!("shard version missing '{key}'"))
                });
                let shard_id: usize = get("shard_id")?.extract(py)?;
                let version: String = get("version")?.extract(py)?;
                let ids: Vec<String> = get("segment_ids")?.extract(py)?;
                Ok(lucistore::delta_sharded::ShardVersion {
                    shard_id,
                    version,
                    segment_ids: ids.into_iter().collect(),
                })
            })
            .collect::<PyResult<_>>()?;

        let handle = self.h()?;
        let dir = self.dir()?;
        let blob = py.allow_threads(|| handle.export_sharded_delta(dir, &versions))
            .map_err(|e| PyValueError::new_err(e))?;
        Ok(PyBytes::new(py, &blob))
    }

    /// Apply a sharded delta (LUCIDS blob) to this index.
    ///
    /// Merges the delta's segments into the local index, bringing it
    /// up to date with the server. Only modified shards are touched.
    ///
    /// Args:
    ///     data: LUCIDS binary blob from ``export_sharded_delta()``.
    ///
    /// Example::
    ///
    ///     delta = server_index.export_sharded_delta(client_index.shard_versions)
    ///     client_index.apply_sharded_delta(delta)
    fn apply_sharded_delta(&self, py: Python<'_>, data: &[u8]) -> PyResult<()> {
        let handle = self.h()?;
        let dir = self.dir()?;
        py.allow_threads(|| handle.apply_sharded_delta(dir, data))
            .map_err(|e| PyValueError::new_err(e))
    }

    // ── Distributed search ──────────────────────────────────────────────

    /// Export BM25 statistics for a query (for distributed search).
    ///
    /// In a distributed setup, each node exports its local BM25 stats.
    /// A coordinator merges them into global stats and sends them back
    /// for scoring with ``search_with_global_stats()``.
    ///
    /// Args:
    ///     query: Query string or dict (same format as ``search()``).
    ///
    /// Returns:
    ///     JSON string of ``ExportableStats`` (document frequencies, doc counts).
    ///
    /// Example::
    ///
    ///     stats_a = node_a.export_stats("mutex lock")
    ///     stats_b = node_b.export_stats("mutex lock")
    ///     # merge stats_a + stats_b on coordinator, then distribute back
    fn export_stats(&self, py: Python<'_>, query: &Bound<'_, PyAny>) -> PyResult<String> {
        let query_config = self.parse_query(query)?;
        let handle = self.h()?;
        let stats = py.allow_threads(|| handle.export_stats(&query_config))
            .map_err(|e| PyValueError::new_err(e))?;
        serde_json::to_string(&stats)
            .map_err(|e| PyValueError::new_err(format!("serialize stats: {e}")))
    }

    /// Search using externally-provided global BM25 stats (distributed mode).
    ///
    /// Scores are computed using the merged global stats instead of local-only
    /// stats, ensuring consistent ranking across nodes.
    ///
    /// Args:
    ///     query: Query string or dict (same format as ``search()``).
    ///     global_stats_json: JSON string of merged ``ExportableStats``
    ///         from all nodes (obtained by merging ``export_stats()`` outputs).
    ///     limit: Maximum number of results (default 10).
    ///     highlights: If True, return highlight byte offsets per field.
    ///
    /// Returns:
    ///     ``list[SearchResult]`` scored with global BM25 statistics.
    ///
    /// Example::
    ///
    ///     results = node_a.search_with_global_stats(
    ///         "mutex lock", merged_stats_json, limit=5
    ///     )
    ///
    /// ``allowed_ids`` restricts the search to those ``_node_id`` values —
    /// a real pre-filter, under the federation's statistics: the ids decide
    /// which documents are visited, the statistics how they score::
    ///
    ///     node_a.search_with_global_stats(
    ///         "mutex lock", merged_stats_json, allowed_ids=[3, 7, 11]
    ///     )
    #[pyo3(signature = (query, global_stats_json, limit=10, highlights=false, allowed_ids=None))]
    fn search_with_global_stats(
        &self,
        py: Python<'_>,
        query: &Bound<'_, PyAny>,
        global_stats_json: &str,
        limit: u32,
        highlights: bool,
        allowed_ids: Option<Vec<u64>>,
    ) -> PyResult<Vec<SearchResult>> {
        let query_config = self.parse_query(query)?;
        let global_stats: lucivy_core::bm25_global::ExportableStats =
            serde_json::from_str(global_stats_json)
                .map_err(|e| PyValueError::new_err(format!("invalid stats JSON: {e}")))?;
        let handle = self.h()?;

        let highlight_sink = if highlights {
            Some(Arc::new(HighlightSink::new()))
        } else {
            None
        };

        py.allow_threads(|| {
            let results = match allowed_ids {
                Some(ids) => handle.search_filtered_with_global_stats(
                    &query_config, limit as usize, &global_stats, highlight_sink.clone(),
                    ids.into_iter().collect::<HashSet<u64>>(),
                ),
                None => handle.search_with_global_stats(
                    &query_config, limit as usize, &global_stats, highlight_sink.clone(),
                ),
            }.map_err(|e| PyValueError::new_err(e))?;
            collect_sharded_results(handle, &results, highlight_sink.as_deref(), false)
        })
    }

    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    fn __exit__(
        &self,
        _exc_type: Option<&Bound<'_, PyAny>>,
        _exc_val: Option<&Bound<'_, PyAny>>,
        _exc_tb: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<bool> {
        Ok(false)
    }

    fn __repr__(&self) -> String {
        let Some(handle) = self.handle.as_ref() else {
            return "Index(dropped)".to_string();
        };
        format!("Index(path={}, num_docs={}, shards={})",
            match &self.backing {
                Backing::Dir(p) => format!("'{p}'"),
                Backing::Snapshot => "<snapshot>".to_string(),
                Backing::Blob { index_name, .. } => format!("<blob:{index_name}>"),
            },
            handle.num_docs(), handle.num_shards())
    }
}

impl Index {
    fn parse_query(&self, query: &Bound<'_, PyAny>) -> PyResult<query::QueryConfig> {
        if let Ok(s) = query.extract::<String>() {
            if self.text_fields.is_empty() {
                return Err(PyValueError::new_err("no text fields in schema for string query"));
            }
            Ok(build_contains_split_multi_field(&s, &self.text_fields, None))
        } else if let Ok(dict) = query.downcast::<PyDict>() {
            let py = dict.py();
            let json_mod = py.import("json")?;
            let json_str: String = json_mod.call_method1("dumps", (dict,))?.extract()?;
            let config: query::QueryConfig = serde_json::from_str(&json_str)
                .map_err(|e| PyValueError::new_err(format!("invalid query dict: {e}")))?;
            Ok(config)
        } else {
            Err(PyValueError::new_err("query must be a string or a dict"))
        }
    }
}

// ─── Helpers ───────────────────────────────────────────────────────────────

fn build_contains_split_multi_field(value: &str, text_fields: &[String], distance: Option<u8>) -> query::QueryConfig {
    if text_fields.len() == 1 {
        return query::QueryConfig {
            query_type: "contains_split".into(),
            field: Some(text_fields[0].clone()),
            value: Some(value.to_string()),
            distance,
            ..Default::default()
        };
    }
    let words: Vec<&str> = value.split_whitespace()
        .filter(|w| w.chars().any(|c| c.is_alphanumeric()))
        .collect();
    let word_queries: Vec<query::QueryConfig> = words.iter().map(|word| {
        let field_queries: Vec<query::QueryConfig> = text_fields.iter().map(|f| {
            query::QueryConfig {
                query_type: "contains".into(),
                field: Some(f.clone()),
                value: Some(word.to_string()),
                distance,
                ..Default::default()
            }
        }).collect();
        query::QueryConfig {
            query_type: "boolean".into(),
            should: Some(field_queries),
            ..Default::default()
        }
    }).collect();
    if word_queries.len() == 1 {
        word_queries.into_iter().next().unwrap()
    } else {
        query::QueryConfig {
            query_type: "boolean".into(),
            should: Some(word_queries),
            ..Default::default()
        }
    }
}

fn extract_user_fields(config: &query::SchemaConfig) -> (Vec<(String, String)>, Vec<String>) {
    let user_fields: Vec<(String, String)> = config.fields.iter()
        .map(|f| (f.name.clone(), f.field_type.clone()))
        .collect();
    let text_fields: Vec<String> = config.fields.iter()
        .filter(|f| f.field_type == "text")
        .map(|f| f.name.clone())
        .collect();
    (user_fields, text_fields)
}

fn add_fields_from_dict(
    handle: &ShardedHandle,
    doc: &mut LucivyDocument,
    kwargs: &Bound<'_, PyDict>,
) -> PyResult<()> {
    for (key, value) in kwargs.iter() {
        let field_name: String = key.extract()?;
        add_field_value(handle, doc, &field_name, &value)?;
    }
    Ok(())
}

fn add_field_value(
    handle: &ShardedHandle,
    doc: &mut LucivyDocument,
    field_name: &str,
    value: &Bound<'_, PyAny>,
) -> PyResult<()> {
    let field = handle.field(field_name)
        .ok_or_else(|| PyValueError::new_err(format!("unknown field: {field_name}")))?;
    let field_entry = handle.schema.get_field_entry(field);

    match field_entry.field_type() {
        FieldType::Str(_) => {
            let text: String = value.extract()?;
            doc.add_text(field, &text);
        }
        FieldType::U64(_) => {
            let v: u64 = value.extract()?;
            doc.add_u64(field, v);
        }
        FieldType::I64(_) => {
            let v: i64 = value.extract()?;
            doc.add_i64(field, v);
        }
        FieldType::F64(_) => {
            let v: f64 = value.extract()?;
            doc.add_f64(field, v);
        }
        _ => return Err(PyValueError::new_err(format!("unsupported field type for {field_name}"))),
    }
    Ok(())
}

fn collect_sharded_results(
    handle: &ShardedHandle,
    results: &[ShardedSearchResult],
    highlight_sink: Option<&HighlightSink>,
    include_fields: bool,
) -> PyResult<Vec<SearchResult>> {
    let nid_field = handle.schema.get_field(NODE_ID_FIELD)
        .map_err(|_| PyValueError::new_err("no _node_id field"))?;

    let mut out = Vec::with_capacity(results.len());
    for r in results {
        let shard = handle.shard(r.shard_id)
            .ok_or_else(|| PyValueError::new_err(format!("shard {} not found", r.shard_id)))?;
        let searcher = shard.reader.searcher();
        let doc: LucivyDocument = searcher.doc(r.doc_address)
            .map_err(|e| PyValueError::new_err(e.to_string()))?;

        let doc_id = doc.get_first(nid_field)
            .and_then(|v| v.as_value().as_u64())
            .unwrap_or(0);

        let highlights = highlight_sink.and_then(|sink| {
            let seg_id = searcher.segment_reader(r.doc_address.segment_ord).segment_id();
            let by_field = sink.get(seg_id, r.doc_address.doc_id)?;
            let map: HashMap<String, Vec<(u32, u32)>> = by_field.into_iter()
                .map(|(name, offsets)| {
                    let ranges = offsets.into_iter().map(|[s, e]| (s as u32, e as u32)).collect();
                    (name, ranges)
                })
                .collect();
            if map.is_empty() { None } else { Some(map) }
        });

        let fields = if include_fields {
            let mut map = HashMap::new();
            for (field, value) in doc.field_values() {
                let name = handle.schema.get_field_name(field);
                if name == NODE_ID_FIELD { continue; }
                let rv = value.as_value();
                let val_str = if let Some(s) = rv.as_str() {
                    s.to_string()
                } else if let Some(n) = rv.as_u64() {
                    n.to_string()
                } else if let Some(n) = rv.as_i64() {
                    n.to_string()
                } else if let Some(n) = rv.as_f64() {
                    n.to_string()
                } else {
                    continue;
                };
                map.insert(name.to_string(), val_str);
            }
            if map.is_empty() { None } else { Some(map) }
        } else {
            None
        };

        out.push(SearchResult { doc_id, score: r.score, highlights, fields });
    }
    Ok(out)
}

// ─── Module ────────────────────────────────────────────────────────────────

/// Merge BM25 stats from multiple nodes into global stats (for distributed search).
///
/// Each node calls ``index.export_stats(query)`` which returns a JSON string.
/// The coordinator collects all JSON strings and merges them with this function.
/// The merged result is then passed back to each node via
/// ``index.search_with_global_stats(query, merged_json)``.
///
/// Args:
///     stats_list: List of JSON strings, one per node (from ``export_stats()``).
///
/// Returns:
///     JSON string of merged ``ExportableStats`` ready for ``search_with_global_stats()``.
///
/// Example::
///
///     stats_a = node_a.export_stats({"type": "contains", "field": "body", "value": "mutex"})
///     stats_b = node_b.export_stats({"type": "contains", "field": "body", "value": "mutex"})
///     merged = lucivy.merge_stats([stats_a, stats_b])
///     results_a = node_a.search_with_global_stats(query, merged, limit=10)
///     results_b = node_b.search_with_global_stats(query, merged, limit=10)
#[pyfunction]
fn merge_stats(stats_list: Vec<String>) -> PyResult<String> {
    let parsed: Vec<lucivy_core::bm25_global::ExportableStats> = stats_list
        .iter()
        .map(|s| serde_json::from_str(s))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| PyValueError::new_err(format!("invalid stats JSON: {e}")))?;
    let merged = lucivy_core::bm25_global::ExportableStats::merge(&parsed);
    serde_json::to_string(&merged)
        .map_err(|e| PyValueError::new_err(format!("serialize merged stats: {e}")))
}

#[pymodule]
fn lucivy(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Index>()?;
    m.add_class::<SearchResult>()?;
    m.add_function(wrap_pyfunction!(merge_stats, m)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields_one() -> Vec<String> { vec!["content".into()] }
    fn fields_two() -> Vec<String> { vec!["title".into(), "body".into()] }

    #[test]
    fn build_contains_split_propagates_distance_single_field() {
        let q = build_contains_split_multi_field("hello world", &fields_one(), Some(3));
        assert_eq!(q.query_type, "contains_split");
        assert_eq!(q.distance, Some(3));
    }

    #[test]
    fn build_contains_split_propagates_distance_multi_field() {
        let q = build_contains_split_multi_field("hello", &fields_two(), Some(2));
        assert_eq!(q.query_type, "boolean");
        for sub in q.should.as_ref().unwrap() {
            assert_eq!(sub.query_type, "contains");
            assert_eq!(sub.distance, Some(2));
        }
    }

    #[test]
    fn build_contains_split_none_distance_stays_none() {
        let q = build_contains_split_multi_field("hello world", &fields_one(), None);
        assert_eq!(q.query_type, "contains_split");
        assert_eq!(q.distance, None);
    }
}
