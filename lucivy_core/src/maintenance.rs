//! Maintenance of a sharded index in one call: purge a set of `_node_id` the
//! caller knows to be gone (documents whose node no longer exists in the
//! caller's graph), then compact (merge the segments, which gives back the
//! deleted documents' bytes), with an `index_report` before and after.
//!
//! The caller owns the list: lucivy cannot tell an orphan from a document.
//! An id the index does not hold costs nothing and is counted as absent.

use crate::index_report::IndexReport;
use crate::sharded_handle::ShardedHandle;

/// What to do. The default does nothing but report.
#[derive(Clone, Debug)]
pub struct MaintenanceRequest {
    /// `_node_id` to delete.
    pub purge_node_ids: Vec<u64>,
    /// Merge the segments of every shard after the purge.
    pub compact: bool,
    /// Largest merged segment, in documents (`compact`).
    pub compact_max_docs: usize,
    /// Refuse, before deleting anything, a purge of more than this share of
    /// the live documents (0.5 by default): a failed read of the caller's
    /// graph makes the whole table look orphan. 1.0 lets everything through.
    pub max_purge_ratio: f64,
}

impl Default for MaintenanceRequest {
    fn default() -> Self {
        Self { purge_node_ids: Vec::new(), compact: false, compact_max_docs: 10_000_000, max_purge_ratio: 0.5 }
    }
}

/// What was done, and the index before and after.
#[derive(Clone, Debug)]
pub struct MaintenanceReport {
    pub before: IndexReport,
    pub after: IndexReport,
    /// Distinct ids asked for.
    pub requested: u64,
    /// Live documents removed by the purge.
    pub purged: u64,
    /// Ids asked for that the index did not hold.
    pub absent: u64,
    /// Merge rounds the compaction ran.
    pub merge_rounds: usize,
    /// Texts of the shard dictionaries before and after their compaction
    /// (`compact`: the texts no live document holds leave them).
    pub dictionary_texts: (u64, u64),
    /// Texts the compaction took back from the reprieve because a live segment cites them again:
    /// `shard_<n>: field <f> id <id>, segment <uuid>` — a segment the previous compaction did not see
    /// (19 November 2026). Empty in the normal case.
    pub dictionary_revived: Vec<String>,
    pub purge_ms: u64,
    pub compact_ms: u64,
}

impl MaintenanceReport {
    pub fn summary(&self) -> String {
        let (written_b, deleted_b) = self.before.documents();
        let (written_a, deleted_a) = self.after.documents();
        let revived = if self.dictionary_revived.is_empty() {
            String::new()
        } else {
            format!("\n{} dictionary text(s) cited again after their reprieve: {}", self.dictionary_revived.len(), self.dictionary_revived.join("; "))
        };
        format!(
            "purge: {} asked, {} removed, {} absent ({} ms); compaction: {} rounds, dictionary texts {} → {} ({} ms)\n\
             documents {} → {} live, deleted in segments {} → {}, segments {} → {}, index {} → {} bytes (×{:.2} → ×{:.2} its text)",
            self.requested, self.purged, self.absent, self.purge_ms, self.merge_rounds,
            self.dictionary_texts.0, self.dictionary_texts.1, self.compact_ms,
            written_b - deleted_b, written_a - deleted_a, deleted_b, deleted_a,
            self.before.segments.len(), self.after.segments.len(),
            self.before.index_bytes, self.after.index_bytes, self.before.ratio(), self.after.ratio(),
        ) + &revived
    }
}

impl ShardedHandle {
    /// Rewrite every shard's dictionary over the ids its committed segments
    /// still cite (`IndexWriter::compact_dictionary`), each under its
    /// writer's lock so that nothing is indexed meanwhile; refused when a
    /// shard holds uncommitted documents. Returns the texts before and
    /// after, summed over the shards.
    pub fn compact_dictionaries(&self) -> Result<(u64, u64), String> {
        self.compact_dictionaries_naming_revived().map(|(texts, _)| texts)
    }

    /// `compact_dictionaries`, with the texts taken back from the reprieve, each with its shard and the
    /// segment that cites it (`MaintenanceReport::dictionary_revived`).
    pub fn compact_dictionaries_naming_revived(&self) -> Result<((u64, u64), Vec<String>), String> {
        let mut revived = Vec::new();
        let mut texts = (0u64, 0u64);
        for shard in 0..self.num_shards() {
            let handle = self.shard(shard).ok_or_else(|| format!("shard_{shard} missing"))?;
            let mut guard = handle.writer.lock().map_err(|_| "writer lock poisoned")?;
            if handle.has_uncommitted() {
                return Err(format!("shard_{shard}: uncommitted documents — commit before compacting the dictionary"));
            }
            if let Some(writer) = guard.as_mut() {
                let report = writer.compact_dictionary().map_err(|e| format!("shard_{shard}: {e}"))?;
                texts.0 += report.texts_before;
                texts.1 += report.texts_after;
                revived.extend(report.revived.iter().map(|r| format!("shard_{shard}: {r}")));
            }
            drop(guard);
            handle.reader.reload().map_err(|e| format!("shard_{shard} reload: {e}"))?;
        }
        Ok((texts, revived))
    }

    /// The `_node_id` of every live document, ascending: what the caller
    /// compares with its graph to find the orphans it then gives `maintain`.
    pub fn live_node_ids(&self) -> Result<Vec<u64>, String> {
        let mut ids = Vec::new();
        for shard in 0..self.num_shards() {
            let handle = self.shard(shard).ok_or_else(|| format!("shard_{shard} missing"))?;
            for segment in handle.reader.searcher().segment_readers() {
                let column = segment.fast_fields().u64(crate::handle::NODE_ID_FIELD)
                    .map_err(|e| format!("fast field {}: {e}", crate::handle::NODE_ID_FIELD))?;
                let alive = segment.alive_bitset();
                for doc in 0..segment.max_doc() {
                    if alive.is_some_and(|a| !a.is_alive(doc)) {
                        continue;
                    }
                    if let Some(id) = column.first(doc) {
                        ids.push(id);
                    }
                }
            }
        }
        ids.sort_unstable();
        Ok(ids)
    }

    /// Purge `request.purge_node_ids`, commit, wait for the merges, then
    /// compact when asked; `index_report` before and after.
    ///
    /// Meant for a maintenance window: the counts are live documents before
    /// and after, so a writer adding documents meanwhile would skew `purged`.
    /// A request with no id and no compaction writes nothing.
    pub fn maintain(&self, request: &MaintenanceRequest) -> Result<MaintenanceReport, String> {
        let ids: std::collections::BTreeSet<u64> = request.purge_node_ids.iter().copied().collect();
        let requested = ids.len() as u64;
        let live = self.num_docs();
        if requested > 0 && requested as f64 > request.max_purge_ratio * live as f64 {
            return Err(format!(
                "maintain: refusing to purge {requested} ids out of {live} live documents \
                 (more than {:.0} %, max_purge_ratio): a failed read of the caller's graph makes every document look orphan; \
                 nothing was deleted",
                request.max_purge_ratio * 100.0,
            ));
        }
        let before = self.index_report()?;

        let started = std::time::Instant::now();
        let live_before = self.num_docs();
        if !ids.is_empty() {
            for &id in &ids {
                self.delete_by_node_id(id)?;
            }
            self.commit()?;
            self.wait_merges_quiet()?;
        }
        let purged = live_before.saturating_sub(self.num_docs());
        let purge_ms = started.elapsed().as_millis() as u64;

        let started = std::time::Instant::now();
        let (merge_rounds, dictionary_texts, dictionary_revived) = if request.compact {
            let rounds = self.compact(request.compact_max_docs)?;
            self.wait_merges_quiet()?;
            let (texts, revived) = self.compact_dictionaries_naming_revived()?;
            (rounds, texts, revived)
        } else {
            (0, (0, 0), Vec::new())
        };
        let compact_ms = started.elapsed().as_millis() as u64;

        let after = self.index_report()?;
        Ok(MaintenanceReport {
            before,
            after,
            requested,
            purged,
            absent: requested.saturating_sub(purged),
            merge_rounds,
            dictionary_texts,
            dictionary_revived,
            purge_ms,
            compact_ms,
        })
    }
}
