//! What an index is made of: where its bytes go.
//!
//! Written after the 14 November 2026 measurement of a code index that weighed
//! 4.6 GB for 37 MB of tracked sources: the text indexed was 928 MB, 92 % of it
//! minified bundles, and 844 MB were exact duplicates. That measurement was done
//! by hand; this report does it in one call, so that a change in what is indexed
//! (an exclusion rule, a fix of what a document holds) can prove its gain.
//!
//! For each segment: documents written and deleted, and bytes on disk. For each
//! text field, over the live documents: count, stored bytes, the size
//! distribution (median, p90, p99, max), the ten largest by `_node_id`, and the
//! exact duplicates (by a hash of the content). For the files: the bytes of the
//! positions, the stored text, the shard dictionary and the postings. And the
//! ratio of the index's bytes to its stored text.
//!
//! Read-only: it walks the committed segments the reader sees and reads every
//! stored document once, so it costs a full read of the store. It opens no
//! writer and changes nothing.

use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};

use ld_lucivy::directory::Directory;
use ld_lucivy::schema::{Field, FieldType};

use crate::handle::{LucivyHandle, NODE_ID_FIELD};

/// How many of the largest documents a field report names.
pub const LARGEST: usize = 10;

/// One committed segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentReport {
    /// The shard it belongs to (0 on a single index).
    pub shard: usize,
    /// Its id, short form.
    pub segment: String,
    /// Documents written into it.
    pub max_doc: u32,
    /// Of those, deleted but still in the segment until a merge purges them.
    pub deleted: u32,
    /// Its files on disk.
    pub bytes: u64,
}

/// One text field, over the live documents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldReport {
    pub field: String,
    /// Live documents with a non-empty value.
    pub docs: u64,
    /// Stored bytes of those values.
    pub bytes: u64,
    /// Distinct contents.
    pub distinct: u64,
    /// Documents whose content another document already has.
    pub duplicate_docs: u64,
    /// Their bytes: what removing the copies would save.
    pub duplicate_bytes: u64,
    /// Nearest-rank percentiles of the value sizes.
    pub p50: u64,
    pub p90: u64,
    pub p99: u64,
    pub max: u64,
    /// The largest values, `(node_id, bytes)`, largest first.
    pub largest: Vec<(u64, u64)>,
}

/// The result of [`LucivyHandle::index_report`] or
/// [`crate::sharded_handle::ShardedHandle::index_report`].
#[derive(Debug, Clone, PartialEq)]
pub struct IndexReport {
    pub segments: Vec<SegmentReport>,
    pub fields: Vec<FieldReport>,
    /// Bytes on disk by family: "positions", "stored text", "dictionary",
    /// "postings" (everything else a segment holds).
    pub bytes_by_kind: BTreeMap<String, u64>,
    /// All of the above.
    pub index_bytes: u64,
    /// Stored bytes of every text field of the live documents.
    pub stored_text_bytes: u64,
}

impl IndexReport {
    /// Index bytes per byte of stored text.
    pub fn ratio(&self) -> f64 {
        self.index_bytes as f64 / self.stored_text_bytes.max(1) as f64
    }

    /// Documents written, deleted.
    pub fn documents(&self) -> (u64, u64) {
        let written = self.segments.iter().map(|s| s.max_doc as u64).sum();
        let deleted = self.segments.iter().map(|s| s.deleted as u64).sum();
        (written, deleted)
    }

    /// The report in a few lines, for a person.
    pub fn summary(&self) -> String {
        let mb = |b: u64| b as f64 / 1e6;
        let pct = |a: u64, b: u64| 100.0 * a as f64 / b.max(1) as f64;
        let (written, deleted) = self.documents();
        let mut out = format!(
            "{:.1} MB on disk for {:.1} MB of stored text (×{:.1}); {} segments, {} documents written, {} deleted ({:.1} %).\n",
            mb(self.index_bytes), mb(self.stored_text_bytes), self.ratio(), self.segments.len(),
            written, deleted, pct(deleted, written));
        for (kind, bytes) in &self.bytes_by_kind {
            out += &format!("  {kind}: {:.1} MB ({:.1} %)\n", mb(*bytes), pct(*bytes, self.index_bytes));
        }
        for f in &self.fields {
            out += &format!(
                "  field {}: {} documents, {:.1} MB; sizes p50 {} B, p90 {} B, p99 {} B, max {} B; {} exact duplicates ({:.1} MB)\n",
                f.field, f.docs, mb(f.bytes), f.p50, f.p90, f.p99, f.max, f.duplicate_docs, mb(f.duplicate_bytes));
        }
        out
    }
}

/// The family of an index file, by its name.
fn kind_of(name: &str) -> &'static str {
    if name.starts_with("dict-") {
        return "dictionary";
    }
    match name.rsplit('.').next().unwrap_or("") {
        "store" => "stored text",
        "posmap" | "word_pos_map" | "sibling_v3" | "pos" | "offsets" => "positions",
        _ => "postings",
    }
}

/// Nearest-rank percentile of ascending values.
fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((p * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted[rank - 1]
}

/// What the walk of one or several shards gathers, before the report.
#[derive(Default)]
pub(crate) struct Gathered {
    segments: Vec<SegmentReport>,
    bytes_by_kind: BTreeMap<String, u64>,
    /// Per field: (node_id, size) of each live value, and size by content hash.
    fields: BTreeMap<String, (Vec<(u64, u64)>, HashMap<u64, (u64, u64)>)>,
    /// Field names in schema order, for a stable report.
    order: Vec<String>,
}

impl Gathered {
    pub(crate) fn into_report(self) -> IndexReport {
        let mut fields = Vec::new();
        for name in &self.order {
            let Some((values, hashes)) = self.fields.get(name) else { continue };
            let mut sizes: Vec<u64> = values.iter().map(|(_, s)| *s).collect();
            sizes.sort_unstable();
            let mut largest = values.clone();
            largest.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            largest.truncate(LARGEST);
            let duplicate_docs = hashes.values().map(|(n, _)| n - 1).sum();
            let duplicate_bytes = hashes.values().map(|(n, size)| (n - 1) * size).sum();
            fields.push(FieldReport {
                field: name.clone(),
                docs: values.len() as u64,
                bytes: sizes.iter().sum(),
                distinct: hashes.len() as u64,
                duplicate_docs,
                duplicate_bytes,
                p50: percentile(&sizes, 0.5),
                p90: percentile(&sizes, 0.9),
                p99: percentile(&sizes, 0.99),
                max: sizes.last().copied().unwrap_or(0),
                largest,
            });
        }
        let index_bytes = self.bytes_by_kind.values().sum();
        let stored_text_bytes = fields.iter().map(|f| f.bytes).sum();
        IndexReport { segments: self.segments, fields, bytes_by_kind: self.bytes_by_kind, index_bytes, stored_text_bytes }
    }
}

impl LucivyHandle {
    /// What this index is made of (see [`crate::index_report`]). Read-only;
    /// reads every stored document once.
    pub fn index_report(&self) -> Result<IndexReport, String> {
        let mut gathered = Gathered::default();
        self.gather_report(0, &mut gathered)?;
        Ok(gathered.into_report())
    }

    pub(crate) fn gather_report(&self, shard: usize, out: &mut Gathered) -> Result<(), String> {
        let directory = self.index.directory();
        let size_of = |path: &std::path::Path| directory.open_read(path).map(|f| f.len() as u64).unwrap_or(0);
        let mut add = |path: &std::path::Path, out: &mut Gathered| -> u64 {
            let bytes = size_of(path);
            let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            *out.bytes_by_kind.entry(kind_of(&name).to_string()).or_default() += bytes;
            bytes
        };

        // Files: the segments', then the shard dictionary's.
        let settings = self.index.settings().clone();
        let metas = self.index.searchable_segment_metas().map_err(|e| format!("segment metas: {e}"))?;
        for meta in &metas {
            let bytes: u64 = meta.list_files_for(settings.sfx_version, settings.skips_derived_files())
                .iter().map(|p| add(p, out)).sum();
            out.segments.push(SegmentReport {
                shard,
                segment: meta.id().short_uuid_string(),
                max_doc: meta.max_doc(),
                deleted: meta.num_deleted_docs(),
                bytes,
            });
        }
        let index_meta = self.index.load_metas().map_err(|e| format!("meta.json: {e}"))?;
        if let Some(dictionary) = &index_meta.sfx_dictionary {
            for path in dictionary.files() {
                add(&path, out);
            }
        }

        // Stored text of the live documents.
        let text_fields: Vec<(String, Field)> = self.field_map.iter()
            .filter(|(_, f)| matches!(self.schema.get_field_entry(*f).field_type(), FieldType::Str(_)))
            .cloned()
            .collect();
        for (name, _) in &text_fields {
            if !out.order.contains(name) {
                out.order.push(name.clone());
            }
        }
        let node_field = self.field(NODE_ID_FIELD);
        let searcher = self.reader.searcher();
        for segment in searcher.segment_readers() {
            let store = segment.get_store_reader(64).map_err(|e| format!("store: {e}"))?;
            let alive = segment.alive_bitset();
            for doc_id in 0..segment.max_doc() {
                if alive.is_some_and(|a| !a.is_alive(doc_id)) {
                    continue;
                }
                let doc: ld_lucivy::LucivyDocument = store.get(doc_id).map_err(|e| format!("document {doc_id}: {e}"))?;
                let node = node_field.and_then(|f| doc.get_first(f)).and_then(|v| {
                    let owned: ld_lucivy::schema::OwnedValue = v.into();
                    match owned { ld_lucivy::schema::OwnedValue::U64(n) => Some(n), _ => None }
                }).unwrap_or(u64::MAX);
                for (name, field) in &text_fields {
                    let text = doc.get_first(*field).and_then(|v| {
                        let owned: ld_lucivy::schema::OwnedValue = v.into();
                        match owned { ld_lucivy::schema::OwnedValue::Str(s) => Some(s), _ => None }
                    });
                    let Some(text) = text.filter(|t| !t.is_empty()) else { continue };
                    let size = text.len() as u64;
                    let mut hasher = std::collections::hash_map::DefaultHasher::new();
                    text.hash(&mut hasher);
                    let (values, hashes) = out.fields.entry(name.clone()).or_default();
                    values.push((node, size));
                    let slot = hashes.entry(hasher.finish()).or_insert((0, size));
                    slot.0 += 1;
                }
            }
        }
        Ok(())
    }
}
