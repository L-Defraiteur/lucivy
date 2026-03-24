//! LUCIDS — Lucivy Incremental Delta Sharded.
//!
//! Multi-shard delta format. Wraps N single-shard LUCID deltas.
//!
//! Format:
//!   [6 bytes] magic: "LUCIDS"
//!   [4 bytes] version: u32 LE (currently 1)
//!   [4 bytes] num_shards: u32 LE
//!   [1 byte] has_config (0/1)
//!   If has_config: [u32 LE] config_len + [config_len bytes] config
//!   [4 bytes] num_shard_deltas: u32 LE
//!   For each shard delta:
//!     [4 bytes] shard_id: u32 LE
//!     [4 bytes] blob_len: u32 LE
//!     [blob_len bytes] LUCID blob

use std::collections::HashSet;
use std::path::Path;

use crate::binary::{read_u32, read_string};
use crate::delta::{IndexDelta, serialize_delta, deserialize_delta};
use crate::version::compute_version_from_bytes;

/// Per-shard version info sent by the client.
#[derive(Debug, Clone)]
pub struct ShardVersion {
    pub shard_id: usize,
    pub version: String,
    pub segment_ids: HashSet<String>,
}

/// A delta spanning multiple shards.
#[derive(Debug, Clone)]
pub struct ShardedDelta {
    /// Per-shard deltas. Only contains shards that actually changed.
    pub shard_deltas: Vec<(usize, IndexDelta)>,
    /// Shard config (_shard_config.json), included if changed or on first sync.
    pub shard_config: Option<Vec<u8>>,
    /// Total number of shards (so the client knows even if some are skipped).
    pub num_shards: usize,
}

/// Serialize a ShardedDelta to binary.
pub fn serialize_sharded_delta(delta: &ShardedDelta) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"LUCIDS");
    buf.extend_from_slice(&1u32.to_le_bytes());
    buf.extend_from_slice(&(delta.num_shards as u32).to_le_bytes());

    match &delta.shard_config {
        Some(config) => {
            buf.push(1);
            buf.extend_from_slice(&(config.len() as u32).to_le_bytes());
            buf.extend_from_slice(config);
        }
        None => buf.push(0),
    }

    buf.extend_from_slice(&(delta.shard_deltas.len() as u32).to_le_bytes());
    for (shard_id, shard_delta) in &delta.shard_deltas {
        buf.extend_from_slice(&(*shard_id as u32).to_le_bytes());
        let shard_blob = serialize_delta(shard_delta);
        buf.extend_from_slice(&(shard_blob.len() as u32).to_le_bytes());
        buf.extend_from_slice(&shard_blob);
    }

    buf
}

/// Deserialize a ShardedDelta from binary.
pub fn deserialize_sharded_delta(data: &[u8]) -> Result<ShardedDelta, String> {
    let mut pos = 0;

    if data.len() < 14 {
        return Err("sharded delta too small".into());
    }
    if &data[pos..pos + 6] != b"LUCIDS" {
        return Err("invalid sharded delta: bad magic (expected LUCIDS)".into());
    }
    pos += 6;

    let version = read_u32(data, &mut pos)?;
    if version != 1 {
        return Err(format!("unsupported sharded delta version: {version}"));
    }

    let num_shards = read_u32(data, &mut pos)? as usize;

    if pos >= data.len() {
        return Err("sharded delta truncated: missing has_config".into());
    }
    let has_config = data[pos];
    pos += 1;
    let shard_config = if has_config == 1 {
        let config_len = read_u32(data, &mut pos)? as usize;
        if pos + config_len > data.len() {
            return Err("sharded delta truncated: config data".into());
        }
        let c = data[pos..pos + config_len].to_vec();
        pos += config_len;
        Some(c)
    } else {
        None
    };

    let num_deltas = read_u32(data, &mut pos)? as usize;
    let mut shard_deltas = Vec::with_capacity(num_deltas);
    for _ in 0..num_deltas {
        let shard_id = read_u32(data, &mut pos)? as usize;
        let blob_len = read_u32(data, &mut pos)? as usize;
        if pos + blob_len > data.len() {
            return Err(format!("sharded delta truncated: shard {shard_id} data"));
        }
        let shard_delta = deserialize_delta(&data[pos..pos + blob_len])?;
        pos += blob_len;
        shard_deltas.push((shard_id, shard_delta));
    }

    Ok(ShardedDelta {
        shard_deltas,
        shard_config,
        num_shards,
    })
}

/// Compute per-shard versions from a base directory.
/// Reads each shard's meta.json to get version + segment IDs.
pub fn compute_shard_versions(base_path: &Path, num_shards: usize) -> Result<Vec<ShardVersion>, String> {
    let mut versions = Vec::with_capacity(num_shards);
    for shard_id in 0..num_shards {
        let shard_dir = base_path.join(format!("shard_{shard_id}"));
        let meta_path = shard_dir.join("meta.json");
        if !meta_path.exists() {
            continue;
        }
        let meta_bytes = std::fs::read(&meta_path)
            .map_err(|e| format!("cannot read shard_{shard_id}/meta.json: {e}"))?;
        let version = compute_version_from_bytes(&meta_bytes);
        let segment_ids = crate::delta::segment_ids_from_meta(&meta_bytes)?;
        versions.push(ShardVersion {
            shard_id,
            version,
            segment_ids,
        });
    }
    Ok(versions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delta::SegmentBundle;

    #[test]
    fn test_roundtrip() {
        let sd = ShardedDelta {
            num_shards: 4,
            shard_config: Some(b"{\"shards\":4}".to_vec()),
            shard_deltas: vec![
                (0, IndexDelta {
                    from_version: "a".into(),
                    to_version: "b".into(),
                    added_segments: vec![],
                    removed_segment_ids: vec![],
                    meta: b"{}".to_vec(),
                    config: None,
                }),
                (2, IndexDelta {
                    from_version: "c".into(),
                    to_version: "d".into(),
                    added_segments: vec![SegmentBundle {
                        segment_id: "seg1".into(),
                        files: vec![("seg1.term".into(), vec![1, 2])],
                    }],
                    removed_segment_ids: vec!["old".into()],
                    meta: b"{\"s\":1}".to_vec(),
                    config: None,
                }),
            ],
        };
        let blob = serialize_sharded_delta(&sd);
        let rt = deserialize_sharded_delta(&blob).unwrap();
        assert_eq!(rt.num_shards, 4);
        assert!(rt.shard_config.is_some());
        assert_eq!(rt.shard_deltas.len(), 2);
        assert_eq!(rt.shard_deltas[1].1.removed_segment_ids, vec!["old"]);
    }

    #[test]
    fn test_bad_magic() {
        let err = deserialize_sharded_delta(b"BADxxx\x01\x00\x00\x00\x00\x00\x00\x00").unwrap_err();
        assert!(err.contains("bad magic"));
    }
}
