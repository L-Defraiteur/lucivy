//! LUCID — Lucivy Incremental Delta.
//!
//! Binary delta format for incremental sync of lucivy indexes.
//! Segments are WORM (Write Once, Read Many) — a delta is just:
//! - Segments added (present in new meta, absent in old)
//! - Segments removed (present in old meta, absent in new)
//! - New meta.json
//!
//! All integers are little-endian.
//!
//! Format:
//!   [5 bytes] magic: "LUCID"
//!   [4 bytes] version: u32 LE (currently 1)
//!   [4 bytes] from_version_len: u32 LE
//!   [from_version_len bytes] from_version: UTF-8
//!   [4 bytes] to_version_len: u32 LE
//!   [to_version_len bytes] to_version: UTF-8
//!   [4 bytes] num_added_segments: u32 LE
//!   For each added segment:
//!     [4 bytes] segment_id_len: u32 LE
//!     [segment_id_len bytes] segment_id: UTF-8
//!     [4 bytes] num_files: u32 LE
//!     For each file:
//!       [4 bytes] name_len: u32 LE
//!       [name_len bytes] name: UTF-8
//!       [4 bytes] data_len: u32 LE
//!       [data_len bytes] data: raw bytes
//!   [4 bytes] num_removed_segment_ids: u32 LE
//!   For each removed segment:
//!     [4 bytes] id_len: u32 LE
//!     [id_len bytes] id: UTF-8
//!   [4 bytes] meta_len: u32 LE
//!   [meta_len bytes] meta: raw bytes (meta.json)
//!   [1 byte] has_config: 0 or 1
//!   If has_config == 1:
//!     [4 bytes] config_len: u32 LE
//!     [config_len bytes] config: raw bytes (_config.json)

use std::collections::HashSet;
use std::path::Path;

use ld_lucivy::directory::Directory;
use ld_lucivy::Index;

use crate::handle::LucivyHandle;

const MAGIC: &[u8; 5] = b"LUCID";
const VERSION: u32 = 1;

/// Files belonging to a single segment.
#[derive(Debug, Clone)]
pub struct SegmentBundle {
    pub segment_id: String,
    pub files: Vec<(String, Vec<u8>)>,
}

/// An incremental delta between two index versions.
#[derive(Debug, Clone)]
pub struct IndexDelta {
    /// Version the client currently has (hash of its meta.json).
    pub from_version: String,
    /// Version the client will have after applying this delta.
    pub to_version: String,
    /// New segments to add.
    pub added_segments: Vec<SegmentBundle>,
    /// Segment IDs to remove.
    pub removed_segment_ids: Vec<String>,
    /// New meta.json content.
    pub meta: Vec<u8>,
    /// New _config.json content, if changed.
    pub config: Option<Vec<u8>>,
}

// ── Version ──────────────────────────────────────────────────────────────────

/// Compute a version string from the meta.json bytes.
/// Uses a simple hash of the first 16 bytes of SHA-256 (hex-encoded).
pub fn compute_version_from_bytes(meta_bytes: &[u8]) -> String {
    // Simple FNV-1a 128-bit hash (no external dep needed).
    // Two 64-bit FNV hashes with different offsets give 128 bits.
    let h1 = fnv1a_64(meta_bytes, 0xcbf29ce484222325);
    let h2 = fnv1a_64(meta_bytes, 0x6c62272e07bb0142);
    format!("{:016x}{:016x}", h1, h2)
}

fn fnv1a_64(data: &[u8], offset_basis: u64) -> u64 {
    let mut hash = offset_basis;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Compute the version of a LucivyHandle's current committed state.
pub fn compute_version(handle: &LucivyHandle) -> Result<String, String> {
    let meta_bytes = read_meta_bytes(&handle.index)?;
    Ok(compute_version_from_bytes(&meta_bytes))
}

/// Read raw meta.json bytes from an index's directory.
fn read_meta_bytes(index: &Index) -> Result<Vec<u8>, String> {
    let dir = index.directory();
    dir.atomic_read(Path::new("meta.json"))
        .map_err(|e| format!("cannot read meta.json: {e}"))
}

// ── Export delta ──────────────────────────────────────────────────────────────

/// Compute a delta from a known set of client segment IDs to the current index state.
///
/// `client_segment_ids` is the set of segment UUID strings the client currently has.
/// `client_version` is the version string the client reports (for embedding in the delta).
pub fn export_delta(
    handle: &LucivyHandle,
    index_path: &Path,
    client_segment_ids: &HashSet<String>,
    client_version: &str,
) -> Result<IndexDelta, String> {
    if handle.has_uncommitted() {
        return Err("index has uncommitted changes — commit before export".into());
    }

    // Read current meta and compute server version.
    let meta_bytes = read_meta_bytes(&handle.index)?;
    let to_version = compute_version_from_bytes(&meta_bytes);

    // Get current segment IDs from the index meta.
    let meta = handle.index.load_metas()
        .map_err(|e| format!("cannot load index metas: {e}"))?;
    let current_ids: HashSet<String> = meta.segments.iter()
        .map(|s| s.id().uuid_string())
        .collect();

    // Added = in current but not in client.
    let added_ids: Vec<&String> = current_ids.difference(client_segment_ids).collect();
    // Removed = in client but not in current.
    let removed_ids: Vec<String> = client_segment_ids.difference(&current_ids)
        .cloned()
        .collect();

    // Build segment bundles for added segments.
    let mut added_segments = Vec::with_capacity(added_ids.len());
    for seg_id_str in &added_ids {
        // Find the SegmentMeta to get its file list.
        let seg_meta = meta.segments.iter()
            .find(|s| &s.id().uuid_string() == *seg_id_str)
            .ok_or_else(|| format!("segment {} not found in meta", seg_id_str))?;

        let files = read_segment_files(index_path, seg_meta)?;
        added_segments.push(SegmentBundle {
            segment_id: (*seg_id_str).clone(),
            files,
        });
    }

    // Optionally include _config.json.
    let config = std::fs::read(index_path.join("_config.json")).ok();

    Ok(IndexDelta {
        from_version: client_version.to_string(),
        to_version,
        added_segments,
        removed_segment_ids: removed_ids,
        meta: meta_bytes,
        config,
    })
}

/// Read all files belonging to a segment from disk.
fn read_segment_files(
    index_path: &Path,
    seg_meta: &ld_lucivy::index::SegmentMeta,
) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut files = Vec::new();
    for rel_path in seg_meta.list_files() {
        let full_path = index_path.join(&rel_path);
        if full_path.exists() {
            let name = rel_path.to_string_lossy().to_string();
            let data = std::fs::read(&full_path)
                .map_err(|e| format!("cannot read segment file '{}': {e}", full_path.display()))?;
            files.push((name, data));
        }
    }
    Ok(files)
}

// ── Apply delta ──────────────────────────────────────────────────────────────

/// Apply a delta to a directory on disk.
///
/// Writes new segment files, removes obsolete segment files, and writes the new meta.json.
/// After applying, the caller should reopen/reload the index.
pub fn apply_delta(dest_path: &Path, delta: &IndexDelta) -> Result<(), String> {
    // 1. Write added segment files.
    for bundle in &delta.added_segments {
        for (name, data) in &bundle.files {
            let file_path = dest_path.join(name);
            std::fs::write(&file_path, data)
                .map_err(|e| format!("cannot write segment file '{}': {e}", file_path.display()))?;
        }
    }

    // 2. Remove files belonging to removed segments.
    // Segment files are named "{uuid}.{ext}", so we match by UUID prefix.
    if !delta.removed_segment_ids.is_empty() {
        let entries = std::fs::read_dir(dest_path)
            .map_err(|e| format!("cannot read directory '{}': {e}", dest_path.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("dir entry error: {e}"))?;
            let name = entry.file_name().to_string_lossy().to_string();
            for removed_id in &delta.removed_segment_ids {
                if name.starts_with(removed_id.as_str()) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }

    // 3. Write new meta.json (atomic: write to temp then rename).
    let meta_path = dest_path.join("meta.json");
    let meta_tmp = dest_path.join("meta.json.tmp");
    std::fs::write(&meta_tmp, &delta.meta)
        .map_err(|e| format!("cannot write meta.json.tmp: {e}"))?;
    std::fs::rename(&meta_tmp, &meta_path)
        .map_err(|e| format!("cannot rename meta.json.tmp → meta.json: {e}"))?;

    // 4. Write _config.json if present.
    if let Some(config) = &delta.config {
        std::fs::write(dest_path.join("_config.json"), config)
            .map_err(|e| format!("cannot write _config.json: {e}"))?;
    }

    Ok(())
}

// ── Binary serialization ─────────────────────────────────────────────────────

/// Serialize an IndexDelta to the LUCID binary format.
pub fn serialize_delta(delta: &IndexDelta) -> Vec<u8> {
    let mut buf = Vec::new();

    // Header
    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(&VERSION.to_le_bytes());

    // Versions
    write_string(&mut buf, &delta.from_version);
    write_string(&mut buf, &delta.to_version);

    // Added segments
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

    // Removed segments
    buf.extend_from_slice(&(delta.removed_segment_ids.len() as u32).to_le_bytes());
    for id in &delta.removed_segment_ids {
        write_string(&mut buf, id);
    }

    // Meta
    buf.extend_from_slice(&(delta.meta.len() as u32).to_le_bytes());
    buf.extend_from_slice(&delta.meta);

    // Config
    match &delta.config {
        Some(config) => {
            buf.push(1);
            buf.extend_from_slice(&(config.len() as u32).to_le_bytes());
            buf.extend_from_slice(config);
        }
        None => {
            buf.push(0);
        }
    }

    buf
}

/// Deserialize a LUCID binary blob into an IndexDelta.
pub fn deserialize_delta(data: &[u8]) -> Result<IndexDelta, String> {
    let mut pos = 0;

    // Header
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

    // Versions
    let from_version = read_string(data, &mut pos)?;
    let to_version = read_string(data, &mut pos)?;

    // Added segments
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

    // Removed segments
    let num_removed = read_u32(data, &mut pos)? as usize;
    let mut removed_segment_ids = Vec::with_capacity(num_removed);
    for _ in 0..num_removed {
        removed_segment_ids.push(read_string(data, &mut pos)?);
    }

    // Meta
    let meta_len = read_u32(data, &mut pos)? as usize;
    if pos + meta_len > data.len() {
        return Err("delta truncated: meta.json data".into());
    }
    let meta = data[pos..pos + meta_len].to_vec();
    pos += meta_len;

    // Config
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
        pos += config_len;
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

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Extract segment ID strings from a meta.json blob (without needing an Index).
/// Useful client-side to know which segments the client has.
pub fn segment_ids_from_meta(meta_bytes: &[u8]) -> Result<HashSet<String>, String> {
    // Parse just enough of meta.json to get segment IDs.
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

fn write_string(buf: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(bytes);
}

fn read_u32(data: &[u8], pos: &mut usize) -> Result<u32, String> {
    if *pos + 4 > data.len() {
        return Err(format!("delta truncated at offset {pos}"));
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
        return Err(format!("delta truncated: expected {len} bytes string at offset {}", *pos));
    }
    let s = std::str::from_utf8(&data[*pos..*pos + len])
        .map_err(|e| format!("invalid UTF-8 in delta: {e}"))?;
    *pos += len;
    Ok(s.to_string())
}

// ── Sharded delta ────────────────────────────────────────────────────────────

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

/// Export a sharded delta.
///
/// `shard_handles` is a slice of (shard_id, &LucivyHandle, shard_directory_path).
/// `client_versions` maps shard_id → ShardVersion. Missing shards get a full export.
/// `requested_shards` is the set of shard IDs the client wants (None = all).
pub fn export_sharded_delta(
    shard_handles: &[(usize, &LucivyHandle, &Path)],
    client_versions: &[ShardVersion],
    requested_shards: Option<&HashSet<usize>>,
    shard_config: Option<Vec<u8>>,
) -> Result<ShardedDelta, String> {
    let num_shards = shard_handles.len();
    let mut shard_deltas = Vec::new();

    for (shard_id, handle, shard_path) in shard_handles {
        // Skip if client didn't request this shard.
        if let Some(requested) = requested_shards {
            if !requested.contains(shard_id) {
                continue;
            }
        }

        // Find client's version for this shard.
        let client_sv = client_versions.iter().find(|sv| sv.shard_id == *shard_id);

        let delta = match client_sv {
            Some(sv) => {
                // Check if shard actually changed.
                let current_version = compute_version(handle)?;
                if current_version == sv.version {
                    continue; // No changes for this shard.
                }
                export_delta(handle, shard_path, &sv.segment_ids, &sv.version)?
            }
            None => {
                // Client doesn't have this shard — full export (empty client segments).
                export_delta(handle, shard_path, &HashSet::new(), "")?
            }
        };

        shard_deltas.push((*shard_id, delta));
    }

    Ok(ShardedDelta {
        shard_deltas,
        shard_config,
        num_shards,
    })
}

/// Apply a sharded delta to a base directory.
///
/// Each shard's delta is applied to `base_path/shard_{id}/`.
/// Creates shard directories if they don't exist.
pub fn apply_sharded_delta(base_path: &Path, delta: &ShardedDelta) -> Result<(), String> {
    // Write shard config if present.
    if let Some(config) = &delta.shard_config {
        std::fs::create_dir_all(base_path)
            .map_err(|e| format!("cannot create base dir: {e}"))?;
        std::fs::write(base_path.join("_shard_config.json"), config)
            .map_err(|e| format!("cannot write _shard_config.json: {e}"))?;
    }

    // Apply per-shard deltas.
    for (shard_id, shard_delta) in &delta.shard_deltas {
        let shard_dir = base_path.join(format!("shard_{shard_id}"));
        std::fs::create_dir_all(&shard_dir)
            .map_err(|e| format!("cannot create shard_{shard_id} dir: {e}"))?;
        apply_delta(&shard_dir, shard_delta)?;
    }

    Ok(())
}

/// Serialize a ShardedDelta to binary.
///
/// Format: "LUCIDS" magic + version + num_shards + shard_config + N shard deltas.
pub fn serialize_sharded_delta(delta: &ShardedDelta) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"LUCIDS"); // Lucivy Incremental Delta — Sharded
    buf.extend_from_slice(&1u32.to_le_bytes()); // version
    buf.extend_from_slice(&(delta.num_shards as u32).to_le_bytes());

    // Shard config
    match &delta.shard_config {
        Some(config) => {
            buf.push(1);
            buf.extend_from_slice(&(config.len() as u32).to_le_bytes());
            buf.extend_from_slice(config);
        }
        None => buf.push(0),
    }

    // Per-shard deltas
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

    // Shard config
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

    // Per-shard deltas
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
            continue; // Shard not synced yet.
        }
        let meta_bytes = std::fs::read(&meta_path)
            .map_err(|e| format!("cannot read shard_{shard_id}/meta.json: {e}"))?;
        let version = compute_version_from_bytes(&meta_bytes);
        let segment_ids = segment_ids_from_meta(&meta_bytes)?;
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

    // ── Binary roundtrip tests ───────────────────────────────────────────

    #[test]
    fn test_serialize_deserialize_empty_delta() {
        let delta = IndexDelta {
            from_version: "aaa".into(),
            to_version: "bbb".into(),
            added_segments: vec![],
            removed_segment_ids: vec![],
            meta: b"{}".to_vec(),
            config: None,
        };
        let blob = serialize_delta(&delta);
        let rt = deserialize_delta(&blob).unwrap();
        assert_eq!(rt.from_version, "aaa");
        assert_eq!(rt.to_version, "bbb");
        assert!(rt.added_segments.is_empty());
        assert!(rt.removed_segment_ids.is_empty());
        assert_eq!(rt.meta, b"{}");
        assert!(rt.config.is_none());
    }

    #[test]
    fn test_serialize_deserialize_with_segments() {
        let delta = IndexDelta {
            from_version: "v1".into(),
            to_version: "v2".into(),
            added_segments: vec![
                SegmentBundle {
                    segment_id: "seg-abc".into(),
                    files: vec![
                        ("seg-abc.term".into(), vec![1, 2, 3]),
                        ("seg-abc.pos".into(), vec![4, 5]),
                    ],
                },
            ],
            removed_segment_ids: vec!["seg-old".into()],
            meta: b"{\"segments\":[]}".to_vec(),
            config: Some(b"{\"fields\":[]}".to_vec()),
        };
        let blob = serialize_delta(&delta);
        let rt = deserialize_delta(&blob).unwrap();
        assert_eq!(rt.added_segments.len(), 1);
        assert_eq!(rt.added_segments[0].segment_id, "seg-abc");
        assert_eq!(rt.added_segments[0].files.len(), 2);
        assert_eq!(rt.added_segments[0].files[0].1, vec![1, 2, 3]);
        assert_eq!(rt.removed_segment_ids, vec!["seg-old"]);
        assert!(rt.config.is_some());
        assert_eq!(rt.config.unwrap(), b"{\"fields\":[]}");
    }

    #[test]
    fn test_bad_magic() {
        let err = deserialize_delta(b"BADxx\x01\x00\x00\x00").unwrap_err();
        assert!(err.contains("bad magic"));
    }

    #[test]
    fn test_bad_version() {
        let mut blob = Vec::new();
        blob.extend_from_slice(b"LUCID");
        blob.extend_from_slice(&99u32.to_le_bytes());
        let err = deserialize_delta(&blob).unwrap_err();
        assert!(err.contains("unsupported delta version"));
    }

    #[test]
    fn test_truncated() {
        let err = deserialize_delta(b"LUCI").unwrap_err();
        assert!(err.contains("too small"));
    }

    // ── Version hash tests ───────────────────────────────────────────────

    #[test]
    fn test_compute_version_deterministic() {
        let v1 = compute_version_from_bytes(b"hello");
        let v2 = compute_version_from_bytes(b"hello");
        assert_eq!(v1, v2);
        assert_eq!(v1.len(), 32); // 2 × 16 hex chars
    }

    #[test]
    fn test_compute_version_different_input() {
        let v1 = compute_version_from_bytes(b"meta_v1");
        let v2 = compute_version_from_bytes(b"meta_v2");
        assert_ne!(v1, v2);
    }

    // ── segment_ids_from_meta ────────────────────────────────────────────

    #[test]
    fn test_segment_ids_from_meta() {
        let meta = r#"{"segments":[{"segment_id":"abc-123","max_doc":10},{"segment_id":"def-456","max_doc":5}]}"#;
        let ids = segment_ids_from_meta(meta.as_bytes()).unwrap();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains("abc-123"));
        assert!(ids.contains("def-456"));
    }

    #[test]
    fn test_segment_ids_from_meta_empty() {
        let meta = r#"{"segments":[]}"#;
        let ids = segment_ids_from_meta(meta.as_bytes()).unwrap();
        assert!(ids.is_empty());
    }

    // ── Integration: create index, commit, export delta, apply ───────────

    #[test]
    fn test_export_apply_delta_e2e() {
        use crate::directory::StdFsDirectory;
        use crate::handle::LucivyHandle;
        use crate::query::SchemaConfig;

        let tmp_src = std::env::temp_dir().join("lucivy_sync_test_src");
        let tmp_dst = std::env::temp_dir().join("lucivy_sync_test_dst");
        let _ = std::fs::remove_dir_all(&tmp_src);
        let _ = std::fs::remove_dir_all(&tmp_dst);
        std::fs::create_dir_all(&tmp_src).unwrap();
        std::fs::create_dir_all(&tmp_dst).unwrap();

        let config: SchemaConfig = serde_json::from_value(serde_json::json!({
            "fields": [{"name": "body", "type": "text", "stored": true}],
            "sfx": false
        })).unwrap();

        // Phase 1: Create index with initial docs.
        let dir = StdFsDirectory::open(tmp_src.to_str().unwrap()).unwrap();
        let handle = LucivyHandle::create(dir, &config).unwrap();
        let body = handle.field("body").unwrap();
        let nid = handle.field("_node_id").unwrap();
        {
            let mut g = handle.writer.lock().unwrap();
            let w = g.as_mut().unwrap();
            for i in 0u64..10 {
                let mut doc = ld_lucivy::LucivyDocument::new();
                doc.add_u64(nid, i);
                doc.add_text(body, &format!("initial document number {i}"));
                w.add_document(doc).unwrap();
            }
            w.commit().unwrap();
        }
        handle.reader.reload().unwrap();

        // Take a snapshot of current state (simulates client's initial full sync).
        let v1 = compute_version(&handle).unwrap();
        let meta_v1 = read_meta_bytes(&handle.index).unwrap();
        let client_segments = segment_ids_from_meta(&meta_v1).unwrap();

        // Copy all files to dst (simulates initial full snapshot).
        crate::snapshot::read_directory_files(&tmp_src).unwrap()
            .iter()
            .for_each(|(name, data)| {
                std::fs::write(tmp_dst.join(name), data).unwrap();
            });

        // Phase 2: Add more docs (new commit = new segments).
        {
            let mut g = handle.writer.lock().unwrap();
            let w = g.as_mut().unwrap();
            for i in 10u64..20 {
                let mut doc = ld_lucivy::LucivyDocument::new();
                doc.add_u64(nid, i);
                doc.add_text(body, &format!("second batch document {i}"));
                w.add_document(doc).unwrap();
            }
            w.commit().unwrap();
        }
        handle.reader.reload().unwrap();

        let v2 = compute_version(&handle).unwrap();
        assert_ne!(v1, v2, "version should change after new commit");

        // Phase 3: Export delta from v1 to v2.
        let delta = export_delta(&handle, &tmp_src, &client_segments, &v1).unwrap();
        assert_eq!(delta.from_version, v1);
        assert_eq!(delta.to_version, v2);
        assert!(!delta.added_segments.is_empty(), "should have added segments");

        // Test binary roundtrip.
        let blob = serialize_delta(&delta);
        let delta_rt = deserialize_delta(&blob).unwrap();
        assert_eq!(delta_rt.added_segments.len(), delta.added_segments.len());
        assert_eq!(delta_rt.removed_segment_ids.len(), delta.removed_segment_ids.len());

        // Phase 4: Apply delta to dst.
        apply_delta(&tmp_dst, &delta).unwrap();

        // Phase 5: Open dst and verify search works with all 20 docs.
        // Remove lock files first.
        for entry in std::fs::read_dir(&tmp_dst).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().to_string();
            if name.contains(".lock") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        let dir_dst = StdFsDirectory::open(tmp_dst.to_str().unwrap()).unwrap();
        let handle_dst = LucivyHandle::open(dir_dst).unwrap();
        handle_dst.reader.reload().unwrap();
        let searcher = handle_dst.reader.searcher();
        assert_eq!(searcher.num_docs(), 20,
            "dst should have all 20 docs after applying delta");

        // Search should find docs from both batches.
        let query_config: crate::query::QueryConfig = serde_json::from_str(
            r#"{"type":"term","field":"body","value":"initial"}"#
        ).unwrap();
        let query = crate::query::build_query(
            &query_config, &handle_dst.schema, &handle_dst.index, None,
        ).unwrap();
        let collector = ld_lucivy::collector::TopDocs::with_limit(20).order_by_score();
        let results = searcher.search(&*query, &collector).unwrap();
        assert_eq!(results.len(), 10, "should find 10 'initial' docs");

        let query_config2: crate::query::QueryConfig = serde_json::from_str(
            r#"{"type":"term","field":"body","value":"second"}"#
        ).unwrap();
        let query2 = crate::query::build_query(
            &query_config2, &handle_dst.schema, &handle_dst.index, None,
        ).unwrap();
        let results2 = searcher.search(&*query2, &collector).unwrap();
        assert_eq!(results2.len(), 10, "should find 10 'second' docs");

        // Cleanup.
        handle.close().unwrap();
        handle_dst.close().unwrap();
        let _ = std::fs::remove_dir_all(&tmp_src);
        let _ = std::fs::remove_dir_all(&tmp_dst);
    }

    /// Test delta after merge: some segments removed, one new merged segment.
    #[test]
    fn test_delta_after_merge() {
        use crate::directory::StdFsDirectory;
        use crate::handle::LucivyHandle;
        use crate::query::SchemaConfig;

        let tmp_src = std::env::temp_dir().join("lucivy_sync_test_merge_src");
        let tmp_dst = std::env::temp_dir().join("lucivy_sync_test_merge_dst");
        let _ = std::fs::remove_dir_all(&tmp_src);
        let _ = std::fs::remove_dir_all(&tmp_dst);
        std::fs::create_dir_all(&tmp_src).unwrap();
        std::fs::create_dir_all(&tmp_dst).unwrap();

        let config: SchemaConfig = serde_json::from_value(serde_json::json!({
            "fields": [{"name": "body", "type": "text", "stored": true}],
            "sfx": false
        })).unwrap();

        let dir = StdFsDirectory::open(tmp_src.to_str().unwrap()).unwrap();
        let handle = LucivyHandle::create(dir, &config).unwrap();
        let body = handle.field("body").unwrap();
        let nid = handle.field("_node_id").unwrap();

        // Multiple small commits to create many segments.
        for batch in 0u64..5 {
            let mut g = handle.writer.lock().unwrap();
            let w = g.as_mut().unwrap();
            for i in 0u64..10 {
                let id = batch * 10 + i;
                let mut doc = ld_lucivy::LucivyDocument::new();
                doc.add_u64(nid, id);
                doc.add_text(body, &format!("batch {batch} doc {i} text for testing merge"));
                w.add_document(doc).unwrap();
            }
            w.commit().unwrap();
        }
        handle.reader.reload().unwrap();

        let v1 = compute_version(&handle).unwrap();
        let meta_v1 = read_meta_bytes(&handle.index).unwrap();
        let client_segments = segment_ids_from_meta(&meta_v1).unwrap();
        let num_segments_v1 = client_segments.len();

        // Copy to dst (full snapshot).
        crate::snapshot::read_directory_files(&tmp_src).unwrap()
            .iter()
            .for_each(|(name, data)| {
                std::fs::write(tmp_dst.join(name), data).unwrap();
            });

        // Add one more doc, commit, then close+reopen to let merges complete.
        {
            let mut g = handle.writer.lock().unwrap();
            let w = g.as_mut().unwrap();
            let mut doc = ld_lucivy::LucivyDocument::new();
            doc.add_u64(nid, 999);
            doc.add_text(body, "post-merge document");
            w.add_document(doc).unwrap();
            w.commit().unwrap();
        }
        // Close drops the writer, which waits for pending merges to finish.
        handle.close().unwrap();
        drop(handle);

        // Reopen — merges are done, meta.json reflects merged segments.
        let dir = StdFsDirectory::open(tmp_src.to_str().unwrap()).unwrap();
        let handle = LucivyHandle::open(dir).unwrap();
        handle.reader.reload().unwrap();

        let v2 = compute_version(&handle).unwrap();
        assert_ne!(v1, v2);

        // Export delta — should have some removed segments (merged) and new ones.
        let delta = export_delta(&handle, &tmp_src, &client_segments, &v1).unwrap();
        eprintln!(
            "Merge delta: {} added, {} removed (was {} segments)",
            delta.added_segments.len(),
            delta.removed_segment_ids.len(),
            num_segments_v1,
        );

        // Apply to dst.
        apply_delta(&tmp_dst, &delta).unwrap();

        // Verify.
        for entry in std::fs::read_dir(&tmp_dst).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().to_string();
            if name.contains(".lock") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        let dir_dst = StdFsDirectory::open(tmp_dst.to_str().unwrap()).unwrap();
        let handle_dst = LucivyHandle::open(dir_dst).unwrap();
        handle_dst.reader.reload().unwrap();
        let num_docs = handle_dst.reader.searcher().num_docs();
        assert_eq!(num_docs, 51, "should have 50 + 1 post-merge doc");

        handle.close().unwrap();
        handle_dst.close().unwrap();
        let _ = std::fs::remove_dir_all(&tmp_src);
        let _ = std::fs::remove_dir_all(&tmp_dst);
    }

    // ── Sharded delta tests ──────────────────────────────────────────────

    #[test]
    fn test_sharded_delta_serialize_roundtrip() {
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
        assert_eq!(rt.shard_deltas[0].0, 0);
        assert_eq!(rt.shard_deltas[1].0, 2);
        assert_eq!(rt.shard_deltas[1].1.added_segments.len(), 1);
        assert_eq!(rt.shard_deltas[1].1.removed_segment_ids, vec!["old"]);
    }

    #[test]
    fn test_sharded_delta_bad_magic() {
        let err = deserialize_sharded_delta(b"BADxxx\x01\x00\x00\x00\x00\x00\x00\x00").unwrap_err();
        assert!(err.contains("bad magic"));
    }

    /// E2E: create 2-shard index, commit, export sharded delta, apply, verify search.
    #[test]
    fn test_sharded_delta_e2e() {
        use crate::directory::StdFsDirectory;
        use crate::handle::LucivyHandle;
        use crate::query::SchemaConfig;

        let tmp_src = std::env::temp_dir().join("lucivy_sharded_sync_src");
        let tmp_dst = std::env::temp_dir().join("lucivy_sharded_sync_dst");
        let _ = std::fs::remove_dir_all(&tmp_src);
        let _ = std::fs::remove_dir_all(&tmp_dst);

        let num_shards = 2;
        let config: SchemaConfig = serde_json::from_value(serde_json::json!({
            "fields": [{"name": "body", "type": "text", "stored": true}],
            "sfx": false
        })).unwrap();
        let config_json = serde_json::to_string(&config).unwrap();

        // Create 2 shards manually (we don't need the full ShardedHandle actor system).
        let mut handles = Vec::new();
        let mut shard_paths = Vec::new();
        for i in 0..num_shards {
            let shard_dir = tmp_src.join(format!("shard_{i}"));
            std::fs::create_dir_all(&shard_dir).unwrap();
            let dir = StdFsDirectory::open(shard_dir.to_str().unwrap()).unwrap();
            let handle = LucivyHandle::create(dir, &config).unwrap();
            shard_paths.push(shard_dir);
            handles.push(handle);
        }
        // Write shard config at root.
        std::fs::write(tmp_src.join("_shard_config.json"), &config_json).unwrap();

        // Insert docs with round-robin routing.
        let body_field = handles[0].field("body").unwrap();
        let nid_field = handles[0].field("_node_id").unwrap();
        for i in 0u64..20 {
            let shard = (i as usize) % num_shards;
            let mut g = handles[shard].writer.lock().unwrap();
            let w = g.as_mut().unwrap();
            let mut doc = ld_lucivy::LucivyDocument::new();
            doc.add_u64(nid_field, i);
            doc.add_text(body_field, &format!("document number {i} for sharded sync"));
            w.add_document(doc).unwrap();
        }
        for h in &handles {
            let mut g = h.writer.lock().unwrap();
            g.as_mut().unwrap().commit().unwrap();
            h.reader.reload().unwrap();
        }

        // --- First sync: full snapshot (copy everything) ---
        std::fs::create_dir_all(&tmp_dst).unwrap();
        std::fs::write(
            tmp_dst.join("_shard_config.json"),
            &config_json,
        ).unwrap();
        for i in 0..num_shards {
            let src_shard = tmp_src.join(format!("shard_{i}"));
            let dst_shard = tmp_dst.join(format!("shard_{i}"));
            std::fs::create_dir_all(&dst_shard).unwrap();
            crate::snapshot::read_directory_files(&src_shard).unwrap()
                .iter()
                .for_each(|(name, data)| {
                    std::fs::write(dst_shard.join(name), data).unwrap();
                });
        }

        // Compute client versions from dst.
        let client_versions = compute_shard_versions(&tmp_dst, num_shards).unwrap();
        assert_eq!(client_versions.len(), num_shards);

        // --- Add more docs (only to shard 0) ---
        {
            let mut g = handles[0].writer.lock().unwrap();
            let w = g.as_mut().unwrap();
            for i in 100u64..110 {
                let mut doc = ld_lucivy::LucivyDocument::new();
                doc.add_u64(nid_field, i);
                doc.add_text(body_field, &format!("new doc {i} only shard zero"));
                w.add_document(doc).unwrap();
            }
            w.commit().unwrap();
        }
        handles[0].reader.reload().unwrap();

        // --- Export sharded delta ---
        let shard_handle_refs: Vec<(usize, &LucivyHandle, &Path)> = handles.iter()
            .enumerate()
            .map(|(i, h)| (i, h, shard_paths[i].as_path()))
            .collect();

        let sharded_delta = export_sharded_delta(
            &shard_handle_refs,
            &client_versions,
            None, // all shards
            Some(config_json.as_bytes().to_vec()),
        ).unwrap();

        assert_eq!(sharded_delta.num_shards, 2);
        // Only shard 0 changed.
        assert_eq!(sharded_delta.shard_deltas.len(), 1, "only shard 0 should have delta");
        assert_eq!(sharded_delta.shard_deltas[0].0, 0);

        // Binary roundtrip.
        let blob = serialize_sharded_delta(&sharded_delta);
        let rt = deserialize_sharded_delta(&blob).unwrap();
        assert_eq!(rt.shard_deltas.len(), 1);

        // --- Apply sharded delta ---
        apply_sharded_delta(&tmp_dst, &sharded_delta).unwrap();

        // --- Verify: open both shards from dst and check doc counts ---
        fn remove_locks(dir: &Path) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let entry = entry.unwrap();
                let name = entry.file_name().to_string_lossy().to_string();
                if name.contains(".lock") {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }

        let mut total_docs = 0u64;
        for i in 0..num_shards {
            let dst_shard = tmp_dst.join(format!("shard_{i}"));
            remove_locks(&dst_shard);
            let dir = StdFsDirectory::open(dst_shard.to_str().unwrap()).unwrap();
            let h = LucivyHandle::open(dir).unwrap();
            h.reader.reload().unwrap();
            let n = h.reader.searcher().num_docs();
            eprintln!("shard {i}: {n} docs");
            total_docs += n;
            h.close().unwrap();
        }
        // 20 original + 10 new on shard 0 = 30
        assert_eq!(total_docs, 30, "should have 30 total docs after sharded delta");

        // Cleanup.
        for h in &handles { h.close().unwrap(); }
        let _ = std::fs::remove_dir_all(&tmp_src);
        let _ = std::fs::remove_dir_all(&tmp_dst);
    }

    /// Test partial sync: only request shard 1, shard 0 should be skipped.
    #[test]
    fn test_sharded_delta_partial_sync() {
        use crate::directory::StdFsDirectory;
        use crate::handle::LucivyHandle;
        use crate::query::SchemaConfig;

        let tmp_src = std::env::temp_dir().join("lucivy_partial_sync_src");
        let _ = std::fs::remove_dir_all(&tmp_src);

        let num_shards = 2;
        let config: SchemaConfig = serde_json::from_value(serde_json::json!({
            "fields": [{"name": "body", "type": "text", "stored": true}],
            "sfx": false
        })).unwrap();

        let mut handles = Vec::new();
        let mut shard_paths = Vec::new();
        for i in 0..num_shards {
            let shard_dir = tmp_src.join(format!("shard_{i}"));
            std::fs::create_dir_all(&shard_dir).unwrap();
            let dir = StdFsDirectory::open(shard_dir.to_str().unwrap()).unwrap();
            let handle = LucivyHandle::create(dir, &config).unwrap();
            shard_paths.push(shard_dir);
            handles.push(handle);
        }

        let body_field = handles[0].field("body").unwrap();
        let nid_field = handles[0].field("_node_id").unwrap();
        // Insert into both shards.
        for i in 0u64..10 {
            let shard = (i as usize) % num_shards;
            let mut g = handles[shard].writer.lock().unwrap();
            let w = g.as_mut().unwrap();
            let mut doc = ld_lucivy::LucivyDocument::new();
            doc.add_u64(nid_field, i);
            doc.add_text(body_field, &format!("doc {i}"));
            w.add_document(doc).unwrap();
        }
        for h in &handles {
            let mut g = h.writer.lock().unwrap();
            g.as_mut().unwrap().commit().unwrap();
        }

        let shard_handle_refs: Vec<(usize, &LucivyHandle, &Path)> = handles.iter()
            .enumerate()
            .map(|(i, h)| (i, h, shard_paths[i].as_path()))
            .collect();

        // Request only shard 1.
        let mut requested = HashSet::new();
        requested.insert(1);

        let delta = export_sharded_delta(
            &shard_handle_refs,
            &[], // no client versions = full export for requested shards
            Some(&requested),
            None,
        ).unwrap();

        // Only shard 1 should be in the delta.
        assert_eq!(delta.shard_deltas.len(), 1);
        assert_eq!(delta.shard_deltas[0].0, 1);

        for h in &handles { h.close().unwrap(); }
        let _ = std::fs::remove_dir_all(&tmp_src);
    }
}
