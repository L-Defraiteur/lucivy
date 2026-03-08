//! LUCE — Lucivy Unified Compact Export.
//!
//! Binary snapshot format for exporting/importing one or more lucivy indexes.
//! All integers are little-endian. No compression (tantivy segments are already lz4).
//!
//! Format:
//!   [4 bytes] magic: "LUCE"
//!   [4 bytes] version: u32 LE (currently 1)
//!   [4 bytes] num_indexes: u32 LE
//!   For each index:
//!     [4 bytes] path_len: u32 LE
//!     [path_len bytes] path: UTF-8
//!     [4 bytes] num_files: u32 LE
//!     For each file:
//!       [4 bytes] name_len: u32 LE
//!       [name_len bytes] name: UTF-8
//!       [4 bytes] data_len: u32 LE
//!       [data_len bytes] data: raw bytes

use std::path::Path;

use crate::handle::LucivyHandle;

const MAGIC: &[u8; 4] = b"LUCE";
const VERSION: u32 = 1;

/// An index entry ready for snapshot export.
pub struct SnapshotIndex<'a> {
    pub path: &'a str,
    pub files: Vec<(String, Vec<u8>)>,
}

/// Serialize one or more indexes into a LUCE snapshot blob.
///
/// Each index is represented by its path and a list of (filename, data) pairs.
/// The caller is responsible for collecting files from the appropriate Directory
/// (StdFsDirectory on native, MemoryDirectory on wasm/emscripten).
pub fn export_snapshot(indexes: &[SnapshotIndex<'_>]) -> Vec<u8> {
    let mut buf = Vec::new();

    // Header
    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(&VERSION.to_le_bytes());
    buf.extend_from_slice(&(indexes.len() as u32).to_le_bytes());

    // Each index
    for index in indexes {
        let path_bytes = index.path.as_bytes();
        buf.extend_from_slice(&(path_bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(path_bytes);

        buf.extend_from_slice(&(index.files.len() as u32).to_le_bytes());
        for (name, data) in &index.files {
            let name_bytes = name.as_bytes();
            buf.extend_from_slice(&(name_bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(name_bytes);
            buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
            buf.extend_from_slice(data);
        }
    }

    buf
}

/// A deserialized index from a LUCE snapshot.
#[derive(Debug)]
pub struct ImportedIndex {
    pub path: String,
    pub files: Vec<(String, Vec<u8>)>,
}

/// Deserialize a LUCE snapshot blob into a list of indexes with their files.
pub fn import_snapshot(data: &[u8]) -> Result<Vec<ImportedIndex>, String> {
    let mut pos = 0;

    // Header
    if data.len() < 12 {
        return Err("snapshot too small: missing header".into());
    }
    if &data[pos..pos + 4] != MAGIC {
        return Err("invalid snapshot: bad magic (expected LUCE)".into());
    }
    pos += 4;

    let version = read_u32(data, &mut pos)?;
    if version != VERSION {
        return Err(format!("unsupported snapshot version: {version} (expected {VERSION})"));
    }

    let num_indexes = read_u32(data, &mut pos)?;
    let mut indexes = Vec::with_capacity(num_indexes as usize);

    for _ in 0..num_indexes {
        let path = read_string(data, &mut pos)?;
        let num_files = read_u32(data, &mut pos)?;

        let mut files = Vec::with_capacity(num_files as usize);
        for _ in 0..num_files {
            let name = read_string(data, &mut pos)?;
            let data_len = read_u32(data, &mut pos)? as usize;
            if pos + data_len > data.len() {
                return Err(format!(
                    "snapshot truncated: expected {data_len} bytes for file '{name}' in index '{path}'"
                ));
            }
            files.push((name, data[pos..pos + data_len].to_vec()));
            pos += data_len;
        }

        indexes.push(ImportedIndex { path, files });
    }

    Ok(indexes)
}

/// Validate that a LucivyHandle has no uncommitted changes before export.
pub fn check_committed(handle: &LucivyHandle, path: &str) -> Result<(), String> {
    if handle.has_uncommitted() {
        return Err(format!(
            "index '{path}' has uncommitted changes — call commit() before export"
        ));
    }
    Ok(())
}

/// Files to exclude from snapshots (tantivy lock files).
const EXCLUDED_FILES: &[&str] = &[".lock", ".tantivy-writer.lock", ".lucivy-writer.lock", ".managed.json"];

/// Read all files from a filesystem directory (for StdFsDirectory-based indexes).
/// Excludes lock files that should not be part of a snapshot.
pub fn read_directory_files(path: &Path) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut files = Vec::new();
    let entries = std::fs::read_dir(path)
        .map_err(|e| format!("cannot read directory '{}': {e}", path.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("directory entry error: {e}"))?;
        let ft = entry
            .file_type()
            .map_err(|e| format!("file type error: {e}"))?;
        if ft.is_file() {
            let name = entry.file_name().to_string_lossy().to_string();
            if EXCLUDED_FILES.contains(&name.as_str()) {
                continue;
            }
            let data = std::fs::read(entry.path())
                .map_err(|e| format!("cannot read file '{}': {e}", entry.path().display()))?;
            files.push((name, data));
        }
    }
    Ok(files)
}

// ── High-level API (native / filesystem) ────────────────────────────────────

/// Export a single index from disk as a LUCE snapshot blob.
/// Checks for uncommitted changes, reads all files, serializes.
pub fn export_index(handle: &LucivyHandle, path: &Path) -> Result<Vec<u8>, String> {
    let path_str = path.to_str()
        .ok_or_else(|| "index path is not valid UTF-8".to_string())?;
    check_committed(handle, path_str)?;
    let files = read_directory_files(path)?;
    let snap = SnapshotIndex { path: path_str, files };
    Ok(export_snapshot(&[snap]))
}

/// Import a LUCE snapshot blob to a destination directory on disk.
/// Writes all files, then opens and returns a LucivyHandle.
pub fn import_index(data: &[u8], dest_path: &Path) -> Result<LucivyHandle, String> {
    let mut indexes = import_snapshot(data)?;
    if indexes.is_empty() {
        return Err("snapshot contains no indexes".into());
    }
    let imported = indexes.remove(0);
    write_imported_files(dest_path, &imported.files)?;

    let dir = crate::directory::StdFsDirectory::open(dest_path)
        .map_err(|e| format!("cannot open directory '{}': {e}", dest_path.display()))?;
    LucivyHandle::open(dir)
}

/// Write imported files to a destination directory.
fn write_imported_files(dest: &Path, files: &[(String, Vec<u8>)]) -> Result<(), String> {
    std::fs::create_dir_all(dest)
        .map_err(|e| format!("cannot create directory '{}': {e}", dest.display()))?;
    for (name, data) in files {
        let file_path = dest.join(name);
        std::fs::write(&file_path, data)
            .map_err(|e| format!("cannot write file '{}': {e}", file_path.display()))?;
    }
    Ok(())
}

// ── Internal helpers ────────────────────────────────────────────────────────

fn read_u32(data: &[u8], pos: &mut usize) -> Result<u32, String> {
    if *pos + 4 > data.len() {
        return Err(format!("snapshot truncated at offset {pos}"));
    }
    let bytes: [u8; 4] = data[*pos..*pos + 4]
        .try_into()
        .map_err(|_| "read_u32 slice error")?;
    *pos += 4;
    Ok(u32::from_le_bytes(bytes))
}

fn read_string(data: &[u8], pos: &mut usize) -> Result<String, String> {
    let len = read_u32(data, pos)? as usize;
    if *pos + len > data.len() {
        return Err(format!("snapshot truncated: expected {len} bytes string at offset {}", *pos));
    }
    let s = std::str::from_utf8(&data[*pos..*pos + len])
        .map_err(|e| format!("invalid UTF-8 in snapshot: {e}"))?;
    *pos += len;
    Ok(s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip_empty() {
        let blob = export_snapshot(&[]);
        let result = import_snapshot(&blob).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_roundtrip_single_index() {
        let files = vec![
            ("meta.json".to_string(), b"{}".to_vec()),
            ("_config.json".to_string(), b"{\"fields\":[]}".to_vec()),
            ("segment_data".to_string(), vec![0u8, 1, 2, 3, 255]),
        ];
        let snapshot = SnapshotIndex {
            path: "/my/index",
            files: files.clone(),
        };
        let blob = export_snapshot(&[snapshot]);
        let result = import_snapshot(&blob).unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].path, "/my/index");
        assert_eq!(result[0].files.len(), 3);
        for (i, (name, data)) in result[0].files.iter().enumerate() {
            assert_eq!(name, &files[i].0);
            assert_eq!(data, &files[i].1);
        }
    }

    #[test]
    fn test_roundtrip_multi_index() {
        let idx1 = SnapshotIndex {
            path: "/index/a",
            files: vec![("f1".into(), vec![10, 20])],
        };
        let idx2 = SnapshotIndex {
            path: "/index/b",
            files: vec![
                ("f2".into(), vec![30]),
                ("f3".into(), vec![40, 50, 60]),
            ],
        };
        let blob = export_snapshot(&[idx1, idx2]);
        let result = import_snapshot(&blob).unwrap();

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].path, "/index/a");
        assert_eq!(result[0].files.len(), 1);
        assert_eq!(result[1].path, "/index/b");
        assert_eq!(result[1].files.len(), 2);
    }

    #[test]
    fn test_bad_magic() {
        let err = import_snapshot(b"BADx\x01\x00\x00\x00\x00\x00\x00\x00").unwrap_err();
        assert!(err.contains("bad magic"));
    }

    #[test]
    fn test_bad_version() {
        let mut blob = Vec::new();
        blob.extend_from_slice(b"LUCE");
        blob.extend_from_slice(&99u32.to_le_bytes());
        blob.extend_from_slice(&0u32.to_le_bytes());
        let err = import_snapshot(&blob).unwrap_err();
        assert!(err.contains("unsupported snapshot version"));
    }

    #[test]
    fn test_truncated_header() {
        let err = import_snapshot(b"LUC").unwrap_err();
        assert!(err.contains("too small"));
    }

    #[test]
    fn test_truncated_after_magic() {
        // Magic OK but missing version + num_indexes (only 6 bytes, need 12)
        let err = import_snapshot(b"LUCE\x01\x00").unwrap_err();
        assert!(err.contains("too small"), "got: {err}");
    }

    #[test]
    fn test_empty_files() {
        let snapshot = SnapshotIndex {
            path: "/empty",
            files: vec![("empty.bin".into(), vec![])],
        };
        let blob = export_snapshot(&[snapshot]);
        let result = import_snapshot(&blob).unwrap();
        assert_eq!(result[0].files[0].1.len(), 0);
    }
}
