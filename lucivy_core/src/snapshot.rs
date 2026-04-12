//! LUCE snapshot — lucivy-specific high-level API.
//!
//! Delegates format serialization to `lucistore::snapshot` (LUCE v1/v2).
//! This module adds:
//! - `export_index` / `import_index` for single-shard filesystem indexes
//! - `export_sharded` / `import_sharded` for multi-shard filesystem indexes
//! - Helper utilities (check_committed, read_directory_files, etc.)

use std::path::Path;

use crate::handle::LucivyHandle;
use crate::sharded_handle::ShardedHandle;

// Re-export the format types so bindings keep using `lucivy_core::snapshot::*`.
pub use lucistore::snapshot::{
    SnapshotIndex, ImportedIndex, ImportedSnapshot,
    export_snapshot, export_snapshot_sharded, import_snapshot,
};

/// Validate that a LucivyHandle has no uncommitted changes before export.
pub fn check_committed(handle: &LucivyHandle, path: &str) -> Result<(), String> {
    if handle.has_uncommitted() {
        return Err(format!(
            "index '{path}' has uncommitted changes — call commit() before export"
        ));
    }
    Ok(())
}

/// Files to exclude from snapshots (lock files, managed.json).
const EXCLUDED_FILES: &[&str] = &[".lock", ".tantivy-writer.lock", ".lucivy-writer.lock", ".managed.json"];

/// Read all files from a filesystem directory.
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

// ── Single-shard (filesystem) ──────────────────────────────────────────────

/// Export a single index from disk as a LUCE snapshot blob.
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
    let imported = import_snapshot(data)?;
    if imported.indexes.is_empty() {
        return Err("snapshot contains no indexes".into());
    }
    let first = &imported.indexes[0];
    write_imported_files(dest_path, &first.files)?;

    let dir = crate::directory::StdFsDirectory::open(dest_path)
        .map_err(|e| format!("cannot open directory '{}': {e}", dest_path.display()))?;
    LucivyHandle::open(dir)
}

// ── Sharded (filesystem) ───────────────────────────────────────────────────

/// Export a sharded index from disk as a LUCE v2 snapshot blob.
///
/// Reads `_shard_config.json`, `_shard_stats.bin` (if present), and all
/// shard directories (`shard_0/`, `shard_1/`, ...).
pub fn export_sharded(handle: &ShardedHandle, base_path: &Path) -> Result<Vec<u8>, String> {
    let num_shards = handle.num_shards();

    // Check all shards are committed.
    for i in 0..num_shards {
        let shard = handle.shard(i)
            .ok_or_else(|| format!("shard {i} not found"))?;
        check_committed(shard, &format!("shard_{i}"))?;
    }

    // Collect root files.
    let mut root_files = Vec::new();
    for name in &["_shard_config.json", "_shard_stats.bin"] {
        let file_path = base_path.join(name);
        if file_path.exists() {
            let data = std::fs::read(&file_path)
                .map_err(|e| format!("cannot read root file '{name}': {e}"))?;
            root_files.push((name.to_string(), data));
        }
    }

    if root_files.is_empty() {
        return Err("no root files found — is this a sharded index?".into());
    }

    // Collect per-shard files.
    let shard_paths: Vec<String> = (0..num_shards)
        .map(|i| format!("shard_{i}"))
        .collect();
    let mut shard_indexes = Vec::with_capacity(num_shards);
    for (i, path_str) in shard_paths.iter().enumerate() {
        let shard_dir = base_path.join(format!("shard_{i}"));
        let files = read_directory_files(&shard_dir)?;
        shard_indexes.push(SnapshotIndex {
            path: path_str,
            files,
        });
    }

    Ok(export_snapshot_sharded(&shard_indexes, &root_files))
}

/// Import a sharded LUCE v2 snapshot blob to a destination directory.
///
/// Writes root files + per-shard files, then opens a ShardedHandle.
pub fn import_sharded(data: &[u8], dest_path: &Path) -> Result<ShardedHandle, String> {
    let imported = import_snapshot(data)?;
    if !imported.is_sharded {
        return Err("snapshot is not sharded — use import_index instead".into());
    }

    // Write root files.
    std::fs::create_dir_all(dest_path)
        .map_err(|e| format!("cannot create directory '{}': {e}", dest_path.display()))?;
    for (name, data) in &imported.root_files {
        let file_path = dest_path.join(name);
        std::fs::write(&file_path, data)
            .map_err(|e| format!("cannot write root file '{name}': {e}"))?;
    }

    // Write per-shard files.
    for index in &imported.indexes {
        let shard_dir = dest_path.join(&index.path);
        write_imported_files(&shard_dir, &index.files)?;
    }

    // Open via ShardedHandle::open (reads _shard_config.json, opens each shard).
    let dest_str = dest_path.to_str()
        .ok_or_else(|| "dest path is not valid UTF-8".to_string())?;
    ShardedHandle::open(dest_str)
}

// ── Internal helpers ───────────────────────────────────────────────────────

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip_empty() {
        let blob = export_snapshot(&[]);
        let result = import_snapshot(&blob).unwrap();
        assert!(result.indexes.is_empty());
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

        assert_eq!(result.indexes.len(), 1);
        assert_eq!(result.indexes[0].path, "/my/index");
        assert_eq!(result.indexes[0].files.len(), 3);
        for (i, (name, data)) in result.indexes[0].files.iter().enumerate() {
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

        assert_eq!(result.indexes.len(), 2);
        assert_eq!(result.indexes[0].path, "/index/a");
        assert_eq!(result.indexes[0].files.len(), 1);
        assert_eq!(result.indexes[1].path, "/index/b");
        assert_eq!(result.indexes[1].files.len(), 2);
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
        assert_eq!(result.indexes[0].files[0].1.len(), 0);
    }
}
