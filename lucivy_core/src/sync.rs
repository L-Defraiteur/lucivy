//! Lucivy-specific incremental sync built on lucistore primitives.
//!
//! Re-exports all lucistore types and adds lucivy-specific functions
//! that depend on `LucivyHandle`, `Index`, and `SegmentMeta`.

use std::collections::HashSet;
use std::path::Path;

use ld_lucivy::directory::Directory;
use ld_lucivy::Index;

use crate::handle::LucivyHandle;

// Re-export everything from lucistore for backwards compatibility.
pub use lucistore::delta::*;
pub use lucistore::delta_sharded::*;
pub use lucistore::fs_utils::{apply_delta, apply_sharded_delta, read_directory_files};
pub use lucistore::sync_server::*;
pub use lucistore::version::*;

// ── Lucivy-specific functions ────────────────────────────────────────────────

/// Read raw meta.json bytes from an index's directory.
fn read_meta_bytes(index: &Index) -> Result<Vec<u8>, String> {
    let dir = index.directory();
    dir.atomic_read(Path::new("meta.json"))
        .map_err(|e| format!("cannot read meta.json: {e}"))
}

/// Compute the version of a LucivyHandle's current committed state.
pub fn compute_version(handle: &LucivyHandle) -> Result<String, String> {
    let meta_bytes = read_meta_bytes(&handle.index)?;
    Ok(compute_version_from_bytes(&meta_bytes))
}

/// Export a delta from the current index state vs. a known set of client segment IDs.
///
/// Reads segment files from disk via `SegmentMeta::list_files()`.
pub fn export_delta(
    handle: &LucivyHandle,
    index_path: &Path,
    client_segment_ids: &HashSet<String>,
    client_version: &str,
) -> Result<IndexDelta, String> {
    if handle.has_uncommitted() {
        return Err("index has uncommitted changes — commit before export".into());
    }

    let meta_bytes = read_meta_bytes(&handle.index)?;
    let to_version = compute_version_from_bytes(&meta_bytes);

    let meta = handle.index.load_metas()
        .map_err(|e| format!("cannot load index metas: {e}"))?;
    let current_ids: HashSet<String> = meta.segments.iter()
        .map(|s| s.id().uuid_string())
        .collect();

    let added_ids: Vec<&String> = current_ids.difference(client_segment_ids).collect();
    let removed_ids: Vec<String> = client_segment_ids.difference(&current_ids)
        .cloned()
        .collect();

    let mut added_segments = Vec::with_capacity(added_ids.len());
    for seg_id_str in &added_ids {
        let seg_meta = meta.segments.iter()
            .find(|s| &s.id().uuid_string() == *seg_id_str)
            .ok_or_else(|| format!("segment {} not found in meta", seg_id_str))?;

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
        added_segments.push(SegmentBundle {
            segment_id: (*seg_id_str).clone(),
            files,
        });
    }

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

/// Export a sharded delta from multiple shard handles.
///
/// Skips shards that haven't changed. Supports partial sync via `requested_shards`.
pub fn export_sharded_delta(
    shard_handles: &[(usize, &LucivyHandle, &Path)],
    client_versions: &[ShardVersion],
    requested_shards: Option<&HashSet<usize>>,
    shard_config: Option<Vec<u8>>,
) -> Result<ShardedDelta, String> {
    let num_shards = shard_handles.len();
    let mut shard_deltas = Vec::new();

    for (shard_id, handle, shard_path) in shard_handles {
        if let Some(requested) = requested_shards {
            if !requested.contains(shard_id) {
                continue;
            }
        }

        let client_sv = client_versions.iter().find(|sv| sv.shard_id == *shard_id);

        let delta = match client_sv {
            Some(sv) => {
                let current_version = compute_version(handle)?;
                if current_version == sv.version {
                    continue;
                }
                export_delta(handle, shard_path, &sv.segment_ids, &sv.version)?
            }
            None => {
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

/// Record a commit in a SyncServer for a LucivyHandle.
///
/// Reads the meta.json, computes version + segment IDs, and records in the server.
pub fn record_commit(
    server: &mut SyncServer,
    shard_id: usize,
    handle: &LucivyHandle,
) -> Result<String, String> {
    let meta_bytes = read_meta_bytes(&handle.index)?;
    let version = compute_version_from_bytes(&meta_bytes);
    let segment_ids = segment_ids_from_meta(&meta_bytes)?;
    server.record_version(shard_id, version.clone(), segment_ids)?;
    Ok(version)
}

/// Convenience: record a commit for shard 0 (single-shard index).
pub fn record_commit_single(
    server: &mut SyncServer,
    handle: &LucivyHandle,
) -> Result<String, String> {
    record_commit(server, 0, handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directory::StdFsDirectory;
    use crate::handle::LucivyHandle;
    use crate::query::SchemaConfig;
    use lucistore::delta::{serialize_delta, deserialize_delta};
    use lucistore::delta_sharded::{serialize_sharded_delta, deserialize_sharded_delta, compute_shard_versions};
    use lucistore::fs_utils::remove_lock_files;

    fn test_config() -> SchemaConfig {
        serde_json::from_value(serde_json::json!({
            "fields": [{"name": "body", "type": "text", "stored": true}],
            "sfx": false
        })).unwrap()
    }

    fn insert_docs(handle: &LucivyHandle, start: u64, count: u64) {
        let body = handle.field("body").unwrap();
        let nid = handle.field("_node_id").unwrap();
        let mut g = handle.writer.lock().unwrap();
        let w = g.as_mut().unwrap();
        for i in start..start + count {
            let mut doc = ld_lucivy::LucivyDocument::new();
            doc.add_u64(nid, i);
            doc.add_text(body, &format!("document number {i}"));
            w.add_document(doc).unwrap();
        }
        w.commit().unwrap();
    }

    // ── Single shard delta E2E ───────────────────────────────────────────

    #[test]
    fn test_export_apply_delta_e2e() {
        let tmp_src = std::env::temp_dir().join("lucivy_sync2_src");
        let tmp_dst = std::env::temp_dir().join("lucivy_sync2_dst");
        let _ = std::fs::remove_dir_all(&tmp_src);
        let _ = std::fs::remove_dir_all(&tmp_dst);
        std::fs::create_dir_all(&tmp_src).unwrap();
        std::fs::create_dir_all(&tmp_dst).unwrap();

        let config = test_config();
        let dir = StdFsDirectory::open(tmp_src.to_str().unwrap()).unwrap();
        let handle = LucivyHandle::create(dir, &config).unwrap();

        // Phase 1: initial docs.
        insert_docs(&handle, 0, 10);
        handle.reader.reload().unwrap();

        let v1 = compute_version(&handle).unwrap();
        let meta_v1 = read_meta_bytes(&handle.index).unwrap();
        let client_segments = segment_ids_from_meta(&meta_v1).unwrap();

        // Copy to dst (full snapshot).
        read_directory_files(&tmp_src).unwrap().iter()
            .for_each(|(n, d)| { std::fs::write(tmp_dst.join(n), d).unwrap(); });

        // Phase 2: more docs.
        insert_docs(&handle, 10, 10);
        handle.reader.reload().unwrap();

        let v2 = compute_version(&handle).unwrap();
        assert_ne!(v1, v2);

        // Phase 3: export delta.
        let delta = export_delta(&handle, &tmp_src, &client_segments, &v1).unwrap();
        assert!(!delta.added_segments.is_empty());

        // Binary roundtrip.
        let blob = serialize_delta(&delta);
        let _ = deserialize_delta(&blob).unwrap();

        // Phase 4: apply.
        apply_delta(&tmp_dst, &delta).unwrap();

        // Phase 5: verify.
        remove_lock_files(&tmp_dst);
        let dir_dst = StdFsDirectory::open(tmp_dst.to_str().unwrap()).unwrap();
        let handle_dst = LucivyHandle::open(dir_dst).unwrap();
        handle_dst.reader.reload().unwrap();
        assert_eq!(handle_dst.reader.searcher().num_docs(), 20);

        handle.close().unwrap();
        handle_dst.close().unwrap();
        let _ = std::fs::remove_dir_all(&tmp_src);
        let _ = std::fs::remove_dir_all(&tmp_dst);
    }

    // ── Sharded delta E2E ────────────────────────────────────────────────

    #[test]
    fn test_sharded_delta_e2e() {
        let tmp_src = std::env::temp_dir().join("lucivy_sharded_sync2_src");
        let tmp_dst = std::env::temp_dir().join("lucivy_sharded_sync2_dst");
        let _ = std::fs::remove_dir_all(&tmp_src);
        let _ = std::fs::remove_dir_all(&tmp_dst);

        let num_shards = 2;
        let config = test_config();
        let config_json = serde_json::to_string(&config).unwrap();

        let mut handles = Vec::new();
        let mut shard_paths = Vec::new();
        for i in 0..num_shards {
            let shard_dir = tmp_src.join(format!("shard_{i}"));
            std::fs::create_dir_all(&shard_dir).unwrap();
            let dir = StdFsDirectory::open(shard_dir.to_str().unwrap()).unwrap();
            handles.push(LucivyHandle::create(dir, &config).unwrap());
            shard_paths.push(shard_dir);
        }

        let body = handles[0].field("body").unwrap();
        let nid = handles[0].field("_node_id").unwrap();
        for i in 0u64..20 {
            let shard = (i as usize) % num_shards;
            let mut g = handles[shard].writer.lock().unwrap();
            let w = g.as_mut().unwrap();
            let mut doc = ld_lucivy::LucivyDocument::new();
            doc.add_u64(nid, i);
            doc.add_text(body, &format!("doc {i}"));
            w.add_document(doc).unwrap();
        }
        for h in &handles {
            h.writer.lock().unwrap().as_mut().unwrap().commit().unwrap();
            h.reader.reload().unwrap();
        }

        // Full snapshot to dst.
        std::fs::create_dir_all(&tmp_dst).unwrap();
        std::fs::write(tmp_dst.join("_shard_config.json"), &config_json).unwrap();
        for i in 0..num_shards {
            let dst_shard = tmp_dst.join(format!("shard_{i}"));
            std::fs::create_dir_all(&dst_shard).unwrap();
            read_directory_files(&shard_paths[i]).unwrap().iter()
                .for_each(|(n, d)| { std::fs::write(dst_shard.join(n), d).unwrap(); });
        }

        let client_versions = compute_shard_versions(&tmp_dst, num_shards).unwrap();

        // Add docs only to shard 0.
        {
            let mut g = handles[0].writer.lock().unwrap();
            let w = g.as_mut().unwrap();
            for i in 100u64..110 {
                let mut doc = ld_lucivy::LucivyDocument::new();
                doc.add_u64(nid, i);
                doc.add_text(body, &format!("new {i}"));
                w.add_document(doc).unwrap();
            }
            w.commit().unwrap();
        }
        handles[0].reader.reload().unwrap();

        let shard_refs: Vec<(usize, &LucivyHandle, &Path)> = handles.iter()
            .enumerate()
            .map(|(i, h)| (i, h, shard_paths[i].as_path()))
            .collect();

        let sd = export_sharded_delta(&shard_refs, &client_versions, None, None).unwrap();
        assert_eq!(sd.shard_deltas.len(), 1);
        assert_eq!(sd.shard_deltas[0].0, 0);

        // Binary roundtrip.
        let blob = serialize_sharded_delta(&sd);
        let _ = deserialize_sharded_delta(&blob).unwrap();

        // Apply.
        apply_sharded_delta(&tmp_dst, &sd).unwrap();

        let mut total = 0u64;
        for i in 0..num_shards {
            let dst_shard = tmp_dst.join(format!("shard_{i}"));
            remove_lock_files(&dst_shard);
            let dir = StdFsDirectory::open(dst_shard.to_str().unwrap()).unwrap();
            let h = LucivyHandle::open(dir).unwrap();
            h.reader.reload().unwrap();
            total += h.reader.searcher().num_docs();
            h.close().unwrap();
        }
        assert_eq!(total, 30);

        for h in &handles { h.close().unwrap(); }
        let _ = std::fs::remove_dir_all(&tmp_src);
        let _ = std::fs::remove_dir_all(&tmp_dst);
    }

    // ── SyncServer with LucivyHandle ─────────────────────────────────────

    #[test]
    fn test_sync_server_with_handle() {
        let tmp = std::env::temp_dir().join("lucivy_sync_server2");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let config = test_config();
        let dir = StdFsDirectory::open(tmp.to_str().unwrap()).unwrap();
        let handle = LucivyHandle::create(dir, &config).unwrap();

        let mut server = SyncServer::new(10);

        insert_docs(&handle, 0, 5);
        let v1 = record_commit_single(&mut server, &handle).unwrap();

        assert!(matches!(server.lookup_single(&v1).unwrap(), SyncResponse::UpToDate));

        insert_docs(&handle, 5, 5);
        let v2 = record_commit_single(&mut server, &handle).unwrap();
        assert_ne!(v1, v2);

        // v1 client should get their segments back.
        match server.lookup_single(&v1).unwrap() {
            SyncResponse::ClientSegments { segment_ids, .. } => {
                assert!(!segment_ids.is_empty());
            }
            _ => panic!("expected ClientSegments"),
        }

        // Unknown → FullSnapshot.
        assert!(matches!(server.lookup_single("unknown").unwrap(), SyncResponse::FullSnapshot));

        handle.close().unwrap();
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
