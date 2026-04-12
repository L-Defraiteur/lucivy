//! LUCE — Lucivy Unified Compact Export.
//!
//! Binary snapshot format for exporting/importing file bundles.
//! All integers are little-endian. No compression (segment files are already lz4).
//!
//! ## Version 1 (original)
//!
//!   [4 bytes] magic: "LUCE"
//!   [4 bytes] version: 1
//!   [4 bytes] num_indexes: u32 LE
//!   For each index:
//!     [string] path
//!     [4 bytes] num_files: u32 LE
//!     For each file: [string] name + [u32 LE] data_len + data
//!
//! ## Version 2 (sharded support)
//!
//!   [4 bytes] magic: "LUCE"
//!   [4 bytes] version: 2
//!   [1 byte]  is_sharded: 0 or 1
//!   If is_sharded:
//!     [4 bytes] num_root_files: u32 LE
//!     For each root file: [string] name + [u32 LE] data_len + data
//!   [4 bytes] num_indexes: u32 LE
//!   For each index: (same as v1)

use crate::binary::{read_u32, read_string, write_string};

const MAGIC: &[u8; 4] = b"LUCE";
const CURRENT_VERSION: u32 = 2;

/// An index entry ready for snapshot export.
pub struct SnapshotIndex<'a> {
    pub path: &'a str,
    pub files: Vec<(String, Vec<u8>)>,
}

/// A deserialized index from a LUCE snapshot.
#[derive(Debug)]
pub struct ImportedIndex {
    pub path: String,
    pub files: Vec<(String, Vec<u8>)>,
}

/// Result of importing a LUCE snapshot.
#[derive(Debug)]
pub struct ImportedSnapshot {
    /// Root-level files (shard config, stats, etc.). Empty for non-sharded.
    pub root_files: Vec<(String, Vec<u8>)>,
    /// Per-index file bundles.
    pub indexes: Vec<ImportedIndex>,
    /// Whether this snapshot is sharded.
    pub is_sharded: bool,
}

/// Serialize one or more indexes into a LUCE v2 snapshot blob (non-sharded).
pub fn export_snapshot(indexes: &[SnapshotIndex<'_>]) -> Vec<u8> {
    export_snapshot_sharded(indexes, &[])
}

/// Serialize a sharded snapshot: root files + per-shard index bundles.
pub fn export_snapshot_sharded(
    indexes: &[SnapshotIndex<'_>],
    root_files: &[(String, Vec<u8>)],
) -> Vec<u8> {
    let is_sharded = !root_files.is_empty();
    let mut buf = Vec::new();

    // Header
    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(&CURRENT_VERSION.to_le_bytes());

    // Sharded flag + root files
    buf.push(if is_sharded { 1 } else { 0 });
    if is_sharded {
        buf.extend_from_slice(&(root_files.len() as u32).to_le_bytes());
        for (name, data) in root_files {
            write_string(&mut buf, name);
            buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
            buf.extend_from_slice(data);
        }
    }

    // Indexes (same as v1 body)
    buf.extend_from_slice(&(indexes.len() as u32).to_le_bytes());
    for index in indexes {
        write_string(&mut buf, index.path);
        buf.extend_from_slice(&(index.files.len() as u32).to_le_bytes());
        for (name, data) in &index.files {
            write_string(&mut buf, name);
            buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
            buf.extend_from_slice(data);
        }
    }

    buf
}

/// Deserialize a LUCE snapshot blob (v1 or v2).
pub fn import_snapshot(data: &[u8]) -> Result<ImportedSnapshot, String> {
    let mut pos = 0;

    if data.len() < 12 {
        return Err("snapshot too small: missing header".into());
    }
    if &data[pos..pos + 4] != MAGIC {
        return Err("invalid snapshot: bad magic (expected LUCE)".into());
    }
    pos += 4;

    let version = read_u32(data, &mut pos)?;

    let (is_sharded, root_files) = match version {
        1 => {
            // v1: no sharded flag, straight to num_indexes
            (false, Vec::new())
        }
        2 => {
            if pos >= data.len() {
                return Err("snapshot truncated: missing is_sharded byte".into());
            }
            let flag = data[pos];
            pos += 1;
            if flag == 1 {
                let num_root = read_u32(data, &mut pos)? as usize;
                let mut files = Vec::with_capacity(num_root);
                for _ in 0..num_root {
                    let name = read_string(data, &mut pos)?;
                    let data_len = read_u32(data, &mut pos)? as usize;
                    if pos + data_len > data.len() {
                        return Err(format!(
                            "snapshot truncated: expected {data_len} bytes for root file '{name}'"
                        ));
                    }
                    files.push((name, data[pos..pos + data_len].to_vec()));
                    pos += data_len;
                }
                (true, files)
            } else {
                (false, Vec::new())
            }
        }
        _ => return Err(format!("unsupported snapshot version: {version} (expected 1 or 2)")),
    };

    // Index entries (same for v1 and v2)
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

    Ok(ImportedSnapshot {
        root_files,
        indexes,
        is_sharded,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip_empty() {
        let blob = export_snapshot(&[]);
        let result = import_snapshot(&blob).unwrap();
        assert!(!result.is_sharded);
        assert!(result.root_files.is_empty());
        assert!(result.indexes.is_empty());
    }

    #[test]
    fn test_roundtrip_single() {
        let snap = SnapshotIndex {
            path: "/my/index",
            files: vec![
                ("meta.json".into(), b"{}".to_vec()),
                ("data.bin".into(), vec![0, 1, 2, 255]),
            ],
        };
        let blob = export_snapshot(&[snap]);
        let result = import_snapshot(&blob).unwrap();
        assert!(!result.is_sharded);
        assert_eq!(result.indexes.len(), 1);
        assert_eq!(result.indexes[0].path, "/my/index");
        assert_eq!(result.indexes[0].files.len(), 2);
    }

    #[test]
    fn test_roundtrip_sharded() {
        let root_files = vec![
            ("_shard_config.json".into(), b"{\"shards\":4}".to_vec()),
            ("_shard_stats.bin".into(), vec![1, 2, 3, 4]),
        ];
        let shards = vec![
            SnapshotIndex {
                path: "shard_0",
                files: vec![("meta.json".into(), b"{}".to_vec())],
            },
            SnapshotIndex {
                path: "shard_1",
                files: vec![
                    ("meta.json".into(), b"{}".to_vec()),
                    ("seg.data".into(), vec![10, 20]),
                ],
            },
        ];
        let blob = export_snapshot_sharded(&shards, &root_files);
        let result = import_snapshot(&blob).unwrap();

        assert!(result.is_sharded);
        assert_eq!(result.root_files.len(), 2);
        assert_eq!(result.root_files[0].0, "_shard_config.json");
        assert_eq!(result.root_files[1].0, "_shard_stats.bin");
        assert_eq!(result.indexes.len(), 2);
        assert_eq!(result.indexes[0].path, "shard_0");
        assert_eq!(result.indexes[1].files.len(), 2);
    }

    #[test]
    fn test_v1_compat() {
        // Build a v1 blob manually
        let mut blob = Vec::new();
        blob.extend_from_slice(b"LUCE");
        blob.extend_from_slice(&1u32.to_le_bytes()); // version 1
        blob.extend_from_slice(&1u32.to_le_bytes()); // 1 index
        // path
        let path = b"/old/index";
        blob.extend_from_slice(&(path.len() as u32).to_le_bytes());
        blob.extend_from_slice(path);
        // 0 files
        blob.extend_from_slice(&0u32.to_le_bytes());

        let result = import_snapshot(&blob).unwrap();
        assert!(!result.is_sharded);
        assert_eq!(result.indexes.len(), 1);
        assert_eq!(result.indexes[0].path, "/old/index");
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
        blob.extend_from_slice(&[0u8; 4]); // padding to pass header size check
        let err = import_snapshot(&blob).unwrap_err();
        assert!(err.contains("unsupported snapshot version"), "got: {err}");
    }

    #[test]
    fn test_non_sharded_v2_compat() {
        // v2 with is_sharded=0 should work like v1
        let snap = SnapshotIndex {
            path: "test",
            files: vec![("f.bin".into(), vec![42])],
        };
        let blob = export_snapshot(&[snap]);
        let result = import_snapshot(&blob).unwrap();
        assert!(!result.is_sharded);
        assert_eq!(result.indexes.len(), 1);
        assert_eq!(result.indexes[0].files[0].1, vec![42]);
    }
}
