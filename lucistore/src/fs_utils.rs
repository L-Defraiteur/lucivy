//! Filesystem utilities shared across snapshot and delta operations.

use std::path::Path;

use crate::delta::IndexDelta;
use crate::delta_sharded::ShardedDelta;

/// Files to exclude from snapshots and directory reads (lock files, internal).
pub const EXCLUDED_FILES: &[&str] = &[
    ".lock",
    ".tantivy-writer.lock",
    ".lucivy-writer.lock",
    ".managed.json",
];

/// Read all files from a filesystem directory.
/// Excludes lock files that should not be part of a snapshot or delta.
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

/// Apply a single-shard delta to a directory on disk.
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

    // 3. Write new meta.json atomically (write to temp then rename).
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

/// Apply a sharded delta to a base directory.
///
/// Each shard's delta is applied to `base_path/shard_{id}/`.
/// Creates shard directories if they don't exist.
pub fn apply_sharded_delta(base_path: &Path, delta: &ShardedDelta) -> Result<(), String> {
    if let Some(config) = &delta.shard_config {
        std::fs::create_dir_all(base_path)
            .map_err(|e| format!("cannot create base dir: {e}"))?;
        std::fs::write(base_path.join("_shard_config.json"), config)
            .map_err(|e| format!("cannot write _shard_config.json: {e}"))?;
    }

    for (shard_id, shard_delta) in &delta.shard_deltas {
        let shard_dir = base_path.join(format!("shard_{shard_id}"));
        std::fs::create_dir_all(&shard_dir)
            .map_err(|e| format!("cannot create shard_{shard_id} dir: {e}"))?;
        apply_delta(&shard_dir, shard_delta)?;
    }

    Ok(())
}

/// Remove lock files from a directory (useful before reopening an index).
pub fn remove_lock_files(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.contains(".lock") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}
