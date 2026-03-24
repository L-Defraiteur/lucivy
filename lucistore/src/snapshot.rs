//! LUCE — Lucivy Unified Compact Export.
//!
//! Generic binary snapshot format for exporting/importing file bundles.
//! All integers are little-endian. No compression (segment files are already lz4).
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

use crate::binary::{read_u32, read_string, write_string};

const MAGIC: &[u8; 4] = b"LUCE";
const VERSION: u32 = 1;

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

/// Serialize one or more indexes into a LUCE snapshot blob.
pub fn export_snapshot(indexes: &[SnapshotIndex<'_>]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(&VERSION.to_le_bytes());
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

/// Deserialize a LUCE snapshot blob into a list of indexes with their files.
pub fn import_snapshot(data: &[u8]) -> Result<Vec<ImportedIndex>, String> {
    let mut pos = 0;

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
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].path, "/my/index");
        assert_eq!(result[0].files.len(), 2);
    }

    #[test]
    fn test_bad_magic() {
        let err = import_snapshot(b"BADx\x01\x00\x00\x00\x00\x00\x00\x00").unwrap_err();
        assert!(err.contains("bad magic"));
    }
}
