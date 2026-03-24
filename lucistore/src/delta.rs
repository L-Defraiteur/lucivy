//! LUCID — Lucivy Incremental Delta.
//!
//! Binary delta format for incremental sync. Engine-agnostic.
//! A delta is: bundles added + bundle IDs removed + new manifest.
//!
//! Format:
//!   [5 bytes] magic: "LUCID"
//!   [4 bytes] version: u32 LE (currently 1)
//!   [string] from_version
//!   [string] to_version
//!   [4 bytes] num_added
//!   For each added bundle:
//!     [string] bundle_id
//!     [4 bytes] num_files
//!     For each file: [string] name + [u32 LE] data_len + [data_len bytes] data
//!   [4 bytes] num_removed
//!   For each removed: [string] bundle_id
//!   [4 bytes] meta_len + [meta_len bytes] meta
//!   [1 byte] has_config (0/1)
//!   If has_config: [4 bytes] config_len + [config_len bytes] config

use std::collections::HashSet;

use crate::binary::{read_u32, read_string, write_string};

const MAGIC: &[u8; 5] = b"LUCID";
const VERSION: u32 = 1;

/// Files belonging to a single bundle (segment, sparse chunk, etc.).
#[derive(Debug, Clone)]
pub struct SegmentBundle {
    pub segment_id: String,
    pub files: Vec<(String, Vec<u8>)>,
}

/// An incremental delta between two versions.
#[derive(Debug, Clone)]
pub struct IndexDelta {
    /// Version the client currently has.
    pub from_version: String,
    /// Version the client will have after applying this delta.
    pub to_version: String,
    /// New bundles to add.
    pub added_segments: Vec<SegmentBundle>,
    /// Bundle IDs to remove.
    pub removed_segment_ids: Vec<String>,
    /// New manifest content (e.g. meta.json).
    pub meta: Vec<u8>,
    /// Optional config content (e.g. _config.json).
    pub config: Option<Vec<u8>>,
}

/// Serialize an IndexDelta to the LUCID binary format.
pub fn serialize_delta(delta: &IndexDelta) -> Vec<u8> {
    let mut buf = Vec::new();

    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(&VERSION.to_le_bytes());

    write_string(&mut buf, &delta.from_version);
    write_string(&mut buf, &delta.to_version);

    buf.extend_from_slice(&(delta.added_segments.len() as u32).to_le_bytes());
    for bundle in &delta.added_segments {
        write_string(&mut buf, &bundle.segment_id);
        buf.extend_from_slice(&(bundle.files.len() as u32).to_le_bytes());
        for (name, data) in &bundle.files {
            write_string(&mut buf, name);
            buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
            buf.extend_from_slice(data);
        }
    }

    buf.extend_from_slice(&(delta.removed_segment_ids.len() as u32).to_le_bytes());
    for id in &delta.removed_segment_ids {
        write_string(&mut buf, id);
    }

    buf.extend_from_slice(&(delta.meta.len() as u32).to_le_bytes());
    buf.extend_from_slice(&delta.meta);

    match &delta.config {
        Some(config) => {
            buf.push(1);
            buf.extend_from_slice(&(config.len() as u32).to_le_bytes());
            buf.extend_from_slice(config);
        }
        None => buf.push(0),
    }

    buf
}

/// Deserialize a LUCID binary blob into an IndexDelta.
pub fn deserialize_delta(data: &[u8]) -> Result<IndexDelta, String> {
    let mut pos = 0;

    if data.len() < 9 {
        return Err("delta too small: missing header".into());
    }
    if &data[pos..pos + 5] != MAGIC {
        return Err("invalid delta: bad magic (expected LUCID)".into());
    }
    pos += 5;

    let version = read_u32(data, &mut pos)?;
    if version != VERSION {
        return Err(format!("unsupported delta version: {version} (expected {VERSION})"));
    }

    let from_version = read_string(data, &mut pos)?;
    let to_version = read_string(data, &mut pos)?;

    let num_added = read_u32(data, &mut pos)? as usize;
    let mut added_segments = Vec::with_capacity(num_added);
    for _ in 0..num_added {
        let segment_id = read_string(data, &mut pos)?;
        let num_files = read_u32(data, &mut pos)? as usize;
        let mut files = Vec::with_capacity(num_files);
        for _ in 0..num_files {
            let name = read_string(data, &mut pos)?;
            let data_len = read_u32(data, &mut pos)? as usize;
            if pos + data_len > data.len() {
                return Err(format!(
                    "delta truncated: expected {data_len} bytes for file '{name}' in segment '{segment_id}'"
                ));
            }
            files.push((name, data[pos..pos + data_len].to_vec()));
            pos += data_len;
        }
        added_segments.push(SegmentBundle { segment_id, files });
    }

    let num_removed = read_u32(data, &mut pos)? as usize;
    let mut removed_segment_ids = Vec::with_capacity(num_removed);
    for _ in 0..num_removed {
        removed_segment_ids.push(read_string(data, &mut pos)?);
    }

    let meta_len = read_u32(data, &mut pos)? as usize;
    if pos + meta_len > data.len() {
        return Err("delta truncated: meta data".into());
    }
    let meta = data[pos..pos + meta_len].to_vec();
    pos += meta_len;

    if pos >= data.len() {
        return Err("delta truncated: missing has_config byte".into());
    }
    let has_config = data[pos];
    pos += 1;
    let config = if has_config == 1 {
        let config_len = read_u32(data, &mut pos)? as usize;
        if pos + config_len > data.len() {
            return Err("delta truncated: config data".into());
        }
        let c = data[pos..pos + config_len].to_vec();
        // pos += config_len;
        Some(c)
    } else {
        None
    };

    Ok(IndexDelta {
        from_version,
        to_version,
        added_segments,
        removed_segment_ids,
        meta,
        config,
    })
}

/// Extract bundle IDs from a manifest blob (generic JSON with "segments" array).
///
/// Expects JSON with `{"segments": [{"segment_id": "..."}, ...]}`.
/// Works for lucivy meta.json. Other engines can provide their own extractor.
pub fn segment_ids_from_meta(meta_bytes: &[u8]) -> Result<HashSet<String>, String> {
    let v: serde_json::Value = serde_json::from_slice(meta_bytes)
        .map_err(|e| format!("cannot parse meta.json: {e}"))?;
    let segments = v.get("segments")
        .and_then(|s| s.as_array())
        .ok_or("meta.json has no segments array")?;
    let mut ids = HashSet::new();
    for seg in segments {
        if let Some(id) = seg.get("segment_id").and_then(|s| s.as_str()) {
            ids.insert(id.to_string());
        }
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip_empty() {
        let delta = IndexDelta {
            from_version: "a".into(),
            to_version: "b".into(),
            added_segments: vec![],
            removed_segment_ids: vec![],
            meta: b"{}".to_vec(),
            config: None,
        };
        let blob = serialize_delta(&delta);
        let rt = deserialize_delta(&blob).unwrap();
        assert_eq!(rt.from_version, "a");
        assert_eq!(rt.to_version, "b");
        assert!(rt.added_segments.is_empty());
        assert!(rt.config.is_none());
    }

    #[test]
    fn test_roundtrip_with_segments() {
        let delta = IndexDelta {
            from_version: "v1".into(),
            to_version: "v2".into(),
            added_segments: vec![SegmentBundle {
                segment_id: "seg-abc".into(),
                files: vec![("seg-abc.term".into(), vec![1, 2, 3])],
            }],
            removed_segment_ids: vec!["seg-old".into()],
            meta: b"{}".to_vec(),
            config: Some(b"{\"fields\":[]}".to_vec()),
        };
        let blob = serialize_delta(&delta);
        let rt = deserialize_delta(&blob).unwrap();
        assert_eq!(rt.added_segments.len(), 1);
        assert_eq!(rt.removed_segment_ids, vec!["seg-old"]);
        assert!(rt.config.is_some());
    }

    #[test]
    fn test_bad_magic() {
        let err = deserialize_delta(b"BADxx\x01\x00\x00\x00").unwrap_err();
        assert!(err.contains("bad magic"));
    }

    #[test]
    fn test_segment_ids_from_meta() {
        let meta = r#"{"segments":[{"segment_id":"abc"},{"segment_id":"def"}]}"#;
        let ids = segment_ids_from_meta(meta.as_bytes()).unwrap();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains("abc"));
        assert!(ids.contains("def"));
    }
}
