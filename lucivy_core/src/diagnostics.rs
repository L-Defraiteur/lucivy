//! Post-indexation diagnostic tools for inspecting index state.
//!
//! Usage:
//!   let report = inspect_term(&handle, "content", "mutex");
//!   eprintln!("{}", report);

use ld_lucivy::schema::{Field, Term};
use ld_lucivy::schema::document::{Document, Value};
use ld_lucivy::termdict::TermDictionary;
use ld_lucivy::LucivyDocument;

use crate::handle::LucivyHandle;
use crate::sharded_handle::ShardedHandle;

/// Report for a single segment's view of a term.
#[derive(Debug)]
pub struct SegmentTermInfo {
    pub segment_id: String,
    pub num_docs: u32,
    pub term_found: bool,
    pub term_ordinal: Option<u64>,
    pub doc_freq: Option<u32>,
    /// Whether this segment has a valid .sfx for this field.
    pub has_sfx: bool,
    /// num_suffix_terms in the .sfx (0 = deferred).
    pub sfx_num_terms: Option<u32>,
}

/// Report for a term across all segments of a handle.
#[derive(Debug)]
pub struct TermReport {
    pub field_name: String,
    pub term_text: String,
    pub segments: Vec<SegmentTermInfo>,
    pub total_doc_freq: u32,
    /// Ground truth: actual number of stored docs containing the substring.
    /// Only populated when verify_stored=true.
    pub ground_truth_count: Option<u32>,
}

impl std::fmt::Display for TermReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Term {:?} in field {:?}:", self.term_text, self.field_name)?;
        write!(f, "  total doc_freq: {}", self.total_doc_freq)?;
        if let Some(gt) = self.ground_truth_count {
            let match_str = if gt == self.total_doc_freq { "MATCH" } else { "MISMATCH" };
            writeln!(f, " | ground_truth: {} ({})", gt, match_str)?;
        } else {
            writeln!(f)?;
        }
        for seg in &self.segments {
            let sfx_status = match (seg.has_sfx, seg.sfx_num_terms) {
                (true, Some(0)) => "DEFERRED".to_string(),
                (true, Some(n)) => format!("ok ({n} terms)"),
                (true, None) => "ok".to_string(),
                (false, _) => "MISSING".to_string(),
            };
            if seg.term_found {
                writeln!(f, "  {} ({} docs): ordinal={} doc_freq={} sfx={}",
                    &seg.segment_id[..8], seg.num_docs,
                    seg.term_ordinal.unwrap_or(0),
                    seg.doc_freq.unwrap_or(0),
                    sfx_status)?;
            } else {
                writeln!(f, "  {} ({} docs): NOT FOUND sfx={}",
                    &seg.segment_id[..8], seg.num_docs, sfx_status)?;
            }
        }
        Ok(())
    }
}

/// Inspect a term across all segments of a LucivyHandle.
/// If `verify_stored` is true, iterates all stored docs to count actual substring matches.
pub fn inspect_term(handle: &LucivyHandle, field_name: &str, term_text: &str) -> TermReport {
    inspect_term_opts(handle, field_name, term_text, false)
}

/// Like inspect_term but with ground truth verification.
pub fn inspect_term_verified(handle: &LucivyHandle, field_name: &str, term_text: &str) -> TermReport {
    inspect_term_opts(handle, field_name, term_text, true)
}

fn inspect_term_opts(handle: &LucivyHandle, field_name: &str, term_text: &str, verify_stored: bool) -> TermReport {
    let field = handle.field(field_name);
    let searcher = handle.reader.searcher();
    let mut segments = Vec::new();
    let mut total_doc_freq = 0u32;

    let Some(field) = field else {
        return TermReport {
            field_name: field_name.to_string(),
            term_text: term_text.to_string(),
            segments,
            total_doc_freq: 0,
            ground_truth_count: None,
        };
    };

    let term = Term::from_field_text(field, term_text);

    for seg_reader in searcher.segment_readers() {
        let seg_id = seg_reader.segment_id().uuid_string();
        let num_docs = seg_reader.num_docs();

        // Check term in term dictionary
        let (term_found, term_ordinal, doc_freq) = match seg_reader.inverted_index(field) {
            Ok(inv_idx) => {
                let term_dict = inv_idx.terms();
                match term_dict.term_ord(term.serialized_value_bytes()) {
                    Ok(Some(ord)) => {
                        let ti = term_dict.term_info_from_ord(ord);
                        (true, Some(ord), Some(ti.doc_freq))
                    }
                    Ok(None) => (false, None, None),
                    Err(_) => (false, None, None),
                }
            }
            Err(_) => (false, None, None),
        };

        if let Some(df) = doc_freq {
            total_doc_freq += df;
        }

        // Check .sfx status
        let has_sfx = seg_reader.sfx_file(field).is_some();
        let sfx_num_terms = seg_reader.sfx_file(field).and_then(|file_slice| {
            file_slice.read_bytes().ok().and_then(|b| {
                if b.len() >= 13 {
                    Some(u32::from_le_bytes([b[9], b[10], b[11], b[12]]))
                } else {
                    None
                }
            })
        });

        segments.push(SegmentTermInfo {
            segment_id: seg_id,
            num_docs,
            term_found,
            term_ordinal,
            doc_freq,
            has_sfx,
            sfx_num_terms,
        });
    }

    // Ground truth: iterate stored docs to count actual substring matches
    let ground_truth_count = if verify_stored {
        let search_lower = term_text.to_lowercase();
        let mut count = 0u32;
        for seg_reader in searcher.segment_readers() {
            let store = seg_reader.get_store_reader(0).ok();
            if let Some(store) = store {
                for doc_id in 0..seg_reader.max_doc() {
                    if seg_reader.alive_bitset().map_or(true, |bs| bs.is_alive(doc_id)) {
                        if let Ok(doc) = store.get::<LucivyDocument>(doc_id) {
                            for (f, val) in doc.field_values() {
                                if f == field {
                                    if let Some(text) = val.as_value().as_str() {
                                        if text.to_lowercase().contains(&search_lower) {
                                            count += 1;
                                            break; // count doc once
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        Some(count)
    } else {
        None
    };

    TermReport {
        field_name: field_name.to_string(),
        term_text: term_text.to_string(),
        segments,
        total_doc_freq,
        ground_truth_count,
    }
}

/// Inspect a term across all shards of a ShardedHandle.
pub fn inspect_term_sharded(handle: &ShardedHandle, field_name: &str, term_text: &str) -> Vec<(usize, TermReport)> {
    inspect_term_sharded_opts(handle, field_name, term_text, false)
}

/// Like inspect_term_sharded but with ground truth verification on stored docs.
pub fn inspect_term_sharded_verified(handle: &ShardedHandle, field_name: &str, term_text: &str) -> Vec<(usize, TermReport)> {
    inspect_term_sharded_opts(handle, field_name, term_text, true)
}

fn inspect_term_sharded_opts(handle: &ShardedHandle, field_name: &str, term_text: &str, verify: bool) -> Vec<(usize, TermReport)> {
    let mut results = Vec::new();
    for i in 0.. {
        match handle.shard(i) {
            Some(shard) => results.push((i, inspect_term_opts(shard, field_name, term_text, verify))),
            None => break,
        }
    }
    results
}

/// Summary of all segments in a handle.
#[derive(Debug)]
pub struct SegmentSummary {
    pub segment_id: String,
    pub num_docs: u32,
    pub num_deleted: u32,
    pub sfx_fields: Vec<(u32, bool, u32)>, // (field_id, has_sfx, num_terms)
}

// ─── SFX diagnostic ─────────────────────────────────────────────────────────

/// Full SFX diagnostic for a search term: traces the entire query path
/// from suffix FST → parents → sfxpost → doc_ids, per segment.
#[derive(Debug)]
pub struct SfxTermReport {
    pub search_term: String,
    pub segments: Vec<SfxSegmentInfo>,
    /// Total unique doc_ids found via SFX path.
    pub total_sfx_docs: u32,
}

#[derive(Debug)]
pub struct SfxSegmentInfo {
    pub segment_id: String,
    pub num_docs: u32,
    pub has_sfx: bool,
    pub has_sfxpost: bool,
    /// Number of suffix FST entries matching prefix_walk(term)
    pub sfx_walk_hits: usize,
    /// Number of parent entries from the walk
    pub sfx_parent_count: usize,
    /// Number of unique doc_ids resolved from sfxpost
    pub sfx_doc_count: u32,
    /// Details of first few parents (for debugging)
    pub sample_parents: Vec<String>,
}

impl std::fmt::Display for SfxTermReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "SFX search {:?}: {} total docs found", self.search_term, self.total_sfx_docs)?;
        for seg in &self.segments {
            let sfx_str = match (seg.has_sfx, seg.has_sfxpost) {
                (true, true) => "sfx+post",
                (true, false) => "sfx only (NO post!)",
                (false, _) => "NO SFX",
            };
            writeln!(f, "  {} ({} docs): {} | walk={} parents={} → {} docs",
                &seg.segment_id[..8.min(seg.segment_id.len())], seg.num_docs,
                sfx_str, seg.sfx_walk_hits, seg.sfx_parent_count, seg.sfx_doc_count)?;
            for sample in &seg.sample_parents {
                writeln!(f, "    {}", sample)?;
            }
        }
        Ok(())
    }
}

/// Trace the full SFX query path for a search term across all segments.
pub fn inspect_sfx(handle: &LucivyHandle, field_name: &str, search_term: &str) -> SfxTermReport {
    use ld_lucivy::suffix_fst::file::SfxFileReader;
    use ld_lucivy::query::posting_resolver::{self, PostingResolver};
    use std::collections::HashSet;

    let field = handle.field(field_name);
    let searcher = handle.reader.searcher();
    let mut segments = Vec::new();
    let mut all_docs = HashSet::new();

    let Some(field) = field else {
        return SfxTermReport {
            search_term: search_term.to_string(),
            segments,
            total_sfx_docs: 0,
        };
    };

    let search_lower = search_term.to_lowercase();

    for seg_reader in searcher.segment_readers() {
        let seg_id = seg_reader.segment_id().uuid_string();
        let num_docs = seg_reader.num_docs();

        let has_sfx = seg_reader.sfx_file(field).is_some();
        let has_sfxpost = seg_reader.sfxpost_file(field).is_some();

        let mut sfx_walk_hits = 0;
        let mut sfx_parent_count = 0;
        let mut sfx_doc_count = 0u32;
        let mut sample_parents = Vec::new();

        if has_sfx && has_sfxpost {
            // Open SFX reader
            if let Some(sfx_slice) = seg_reader.sfx_file(field) {
                if let Ok(sfx_bytes) = sfx_slice.read_bytes() {
                    if let Ok(sfx_reader) = SfxFileReader::open(sfx_bytes.as_ref()) {
                        // prefix_walk for the search term
                        let walk = sfx_reader.prefix_walk(&search_lower);
                        sfx_walk_hits = walk.len();

                        for (suffix_key, parents) in &walk {
                            sfx_parent_count += parents.len();
                            if sample_parents.len() < 3 {
                                for p in parents.iter().take(2) {
                                    sample_parents.push(format!(
                                        "suffix={:?} parent=(ord={}, si={})",
                                        suffix_key, p.raw_ordinal, p.si
                                    ));
                                }
                            }
                        }

                        // Resolve via sfxpost
                        if let Ok(resolver) = posting_resolver::build_resolver(seg_reader, field) {
                            let mut seg_docs = HashSet::new();
                            for (_suffix_key, parents) in &walk {
                                for parent in parents {
                                    let entries = resolver.resolve(parent.raw_ordinal);
                                    for e in &entries {
                                        seg_docs.insert(e.doc_id);
                                        all_docs.insert((seg_reader.segment_id(), e.doc_id));
                                    }
                                }
                            }
                            sfx_doc_count = seg_docs.len() as u32;
                        }
                    }
                }
            }
        }

        segments.push(SfxSegmentInfo {
            segment_id: seg_id,
            num_docs,
            has_sfx,
            has_sfxpost,
            sfx_walk_hits,
            sfx_parent_count,
            sfx_doc_count,
            sample_parents,
        });
    }

    SfxTermReport {
        search_term: search_term.to_string(),
        segments,
        total_sfx_docs: all_docs.len() as u32,
    }
}

/// Dump the first N keys from the term dict and first N entries from the FST
/// for a specific segment. Used to debug format mismatches between term dict
/// and suffix FST after merge/rebuild.
pub fn dump_segment_keys(handle: &LucivyHandle, field_name: &str, max_keys: usize) -> String {
    use ld_lucivy::suffix_fst::file::SfxFileReader;

    let field = match handle.field(field_name) {
        Some(f) => f,
        None => return format!("field {:?} not found", field_name),
    };

    let searcher = handle.reader.searcher();
    let mut out = String::new();

    for seg_reader in searcher.segment_readers() {
        let seg_id = seg_reader.segment_id().uuid_string();
        let num_docs = seg_reader.num_docs();
        out.push_str(&format!("\nSegment {} ({} docs):\n", &seg_id[..8], num_docs));

        // Term dict keys
        match seg_reader.inverted_index(field) {
            Ok(inv_idx) => {
                let term_dict = inv_idx.terms();
                let total_terms = term_dict.num_terms();
                match term_dict.stream() {
                    Ok(mut stream) => {
                        let mut count = 0;
                        let mut sample_keys = Vec::new();
                        while stream.advance() {
                            if count < max_keys {
                                let key = stream.key();
                                let hex: String = key.iter().take(20).map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(" ");
                                let utf8 = String::from_utf8_lossy(key);
                                sample_keys.push(format!("[{}] {:?} (hex: {})", count, &utf8[..utf8.len().min(30)], hex));
                            }
                            count += 1;
                        }
                        out.push_str(&format!("  Term dict: num_terms={} stream_count={}\n", total_terms, count));
                        for s in &sample_keys {
                            out.push_str(&format!("    {}\n", s));
                        }
                    }
                    Err(e) => out.push_str(&format!("  Term dict stream error: {}\n", e)),
                }
            }
            Err(e) => out.push_str(&format!("  No inverted index: {}\n", e)),
        }

        // SFX FST first entries via prefix_walk("")
        if let Some(sfx_slice) = seg_reader.sfx_file(field) {
            if let Ok(sfx_bytes) = sfx_slice.read_bytes() {
                if let Ok(sfx_reader) = SfxFileReader::open(sfx_bytes.as_ref()) {
                    out.push_str(&format!("  SFX: {} suffix terms\n", sfx_reader.num_suffix_terms()));
                    // Try resolve a few known tokens
                    for probe in &["mutex", "lock", "function"] {
                        let parents = sfx_reader.resolve_suffix_si0(probe);
                        let walk = sfx_reader.prefix_walk_si0(probe);
                        out.push_str(&format!("    resolve_si0({:?}): {} parents | prefix_walk_si0: {} entries\n",
                            probe, parents.len(), walk.len()));
                    }
                    // Probe FST with short prefixes to see what's inside
                    out.push_str("  FST probes:\n");
                    for probe in &["a", "m", "l", "f"] {
                        let walk = sfx_reader.prefix_walk_si0(probe);
                        let sample: Vec<&str> = walk.iter().take(3).map(|(k, _)| k.as_str()).collect();
                        out.push_str(&format!("    prefix_walk_si0({:?}): {} entries, first: {:?}\n",
                            probe, walk.len(), sample));
                    }
                }
            }
        } else {
            out.push_str("  No SFX file\n");
        }
    }
    out
}

/// Inspect SFX across all shards.
pub fn inspect_sfx_sharded(handle: &ShardedHandle, field_name: &str, search_term: &str) -> Vec<(usize, SfxTermReport)> {
    let mut results = Vec::new();
    for i in 0.. {
        match handle.shard(i) {
            Some(shard) => results.push((i, inspect_sfx(shard, field_name, search_term))),
            None => break,
        }
    }
    results
}

/// Deep comparison: for a term, compare the posting list (term dict) doc count
/// vs the sfxpost doc count at the same ordinal, per segment.
/// This reveals ordinal mismatches between FST/sfxpost and the term dict.
pub fn compare_postings_vs_sfxpost(handle: &LucivyHandle, field_name: &str, term_text: &str) -> String {
    use ld_lucivy::suffix_fst::file::{SfxFileReader, SfxPostingsReader};
    use ld_lucivy::schema::{Term, IndexRecordOption};
    use ld_lucivy::{DocSet, TERMINATED};

    let field = match handle.field(field_name) {
        Some(f) => f,
        None => return format!("field {:?} not found", field_name),
    };

    let searcher = handle.reader.searcher();
    let term = Term::from_field_text(field, term_text);
    let mut out = format!("=== Postings vs SfxPost for {:?} in {:?} ===\n", term_text, field_name);

    for seg_reader in searcher.segment_readers() {
        let seg_id = seg_reader.segment_id().uuid_string();
        let num_docs = seg_reader.num_docs();
        out.push_str(&format!("\nSegment {} ({} docs):\n", &seg_id[..8], num_docs));

        // 1. Term dict: find ordinal and doc_freq
        let (term_ord, posting_doc_count) = match seg_reader.inverted_index(field) {
            Ok(inv_idx) => {
                let term_dict = inv_idx.terms();
                match term_dict.term_ord(term.serialized_value_bytes()) {
                    Ok(Some(ord)) => {
                        let ti = term_dict.term_info_from_ord(ord);
                        // Also count actual posting list docs
                        let actual_count = match inv_idx.read_postings_from_terminfo(
                            &ti, IndexRecordOption::Basic
                        ) {
                            Ok(mut postings) => {
                                let mut count = 0u32;
                                loop {
                                    if postings.doc() == TERMINATED { break; }
                                    count += 1;
                                    postings.advance();
                                }
                                count
                            }
                            Err(_) => 0,
                        };
                        (Some(ord), actual_count)
                    }
                    _ => (None, 0),
                }
            }
            Err(_) => (None, 0),
        };

        // 2. SFX FST: find parent ordinal for this term
        let sfx_ordinal = seg_reader.sfx_file(field).and_then(|sfx_slice| {
            let bytes = sfx_slice.read_bytes().ok()?;
            let reader = SfxFileReader::open(bytes.as_ref()).ok()?;
            let parents = reader.resolve_suffix_si0(term_text);
            parents.first().map(|p| p.raw_ordinal)
        });

        // 3. SfxPost: count entries at the ordinal
        let sfxpost_doc_count = seg_reader.sfxpost_file(field).and_then(|post_slice| {
            let bytes = post_slice.read_bytes().ok()?;
            let reader = SfxPostingsReader::open(bytes.as_ref()).ok()?;
            let ord = sfx_ordinal?;
            let entries = reader.entries(ord as u32);
            let unique_docs: std::collections::HashSet<u32> = entries.iter().map(|e| e.doc_id).collect();
            Some(unique_docs.len() as u32)
        });

        let term_ord_str = term_ord.map(|o| format!("{}", o)).unwrap_or("NONE".into());
        let sfx_ord_str = sfx_ordinal.map(|o| format!("{}", o)).unwrap_or("NONE".into());
        let sfxpost_str = sfxpost_doc_count.map(|c| format!("{}", c)).unwrap_or("N/A".into());

        let status = match (term_ord, sfx_ordinal, sfxpost_doc_count) {
            (Some(t), Some(s), Some(sp)) => {
                if t == s as u64 && posting_doc_count == sp {
                    "OK"
                } else if t != s as u64 {
                    "ORDINAL MISMATCH"
                } else {
                    "DOC COUNT MISMATCH"
                }
            }
            (Some(_), None, _) => "NO SFX ENTRY",
            (None, _, _) => "NOT IN TERM DICT",
            _ => "INCOMPLETE",
        };

        out.push_str(&format!(
            "  term_ord={} posting_docs={} | sfx_ord={} sfxpost_docs={} | {}\n",
            term_ord_str, posting_doc_count, sfx_ord_str, sfxpost_str, status
        ));

        // On DOC COUNT MISMATCH: show which doc_ids are missing
        if status == "DOC COUNT MISMATCH" {
            if let (Some(_term_ord_val), Some(sfx_ord_val)) = (term_ord, sfx_ordinal) {
                // Collect posting list doc_ids
                let posting_docs: std::collections::HashSet<u32> = match seg_reader.inverted_index(field) {
                    Ok(inv_idx) => {
                        let ti = inv_idx.terms().term_info_from_ord(term_ord.unwrap());
                        match inv_idx.read_postings_from_terminfo(&ti, IndexRecordOption::Basic) {
                            Ok(mut postings) => {
                                let mut docs = std::collections::HashSet::new();
                                loop {
                                    if postings.doc() == TERMINATED { break; }
                                    docs.insert(postings.doc());
                                    postings.advance();
                                }
                                docs
                            }
                            Err(_) => std::collections::HashSet::new(),
                        }
                    }
                    Err(_) => std::collections::HashSet::new(),
                };

                // Collect sfxpost doc_ids
                let sfxpost_docs: std::collections::HashSet<u32> = seg_reader.sfxpost_file(field)
                    .and_then(|post_slice| {
                        let bytes = post_slice.read_bytes().ok()?;
                        let reader = SfxPostingsReader::open(bytes.as_ref()).ok()?;
                        let entries = reader.entries(sfx_ord_val as u32);
                        Some(entries.iter().map(|e| e.doc_id).collect())
                    })
                    .unwrap_or_default();

                let missing: Vec<u32> = posting_docs.difference(&sfxpost_docs).copied().collect();
                let extra: Vec<u32> = sfxpost_docs.difference(&posting_docs).copied().collect();
                out.push_str(&format!("    missing from sfxpost ({} docs): {:?}\n",
                    missing.len(), &missing[..missing.len().min(20)]));
                if !extra.is_empty() {
                    out.push_str(&format!("    extra in sfxpost ({} docs): {:?}\n",
                        extra.len(), &extra[..extra.len().min(20)]));
                }
                // Check if missing docs are alive
                let alive = seg_reader.alive_bitset();
                let missing_alive: Vec<u32> = missing.iter()
                    .filter(|&&d| alive.map_or(true, |bs| bs.is_alive(d)))
                    .copied().collect();
                out.push_str(&format!("    of which alive: {}/{}\n", missing_alive.len(), missing.len()));
            }
        }
    }
    out
}

/// List all segments with their status.
pub fn inspect_segments(handle: &LucivyHandle) -> Vec<SegmentSummary> {
    let searcher = handle.reader.searcher();
    let schema = handle.schema.clone();
    let mut summaries = Vec::new();

    for seg_reader in searcher.segment_readers() {
        let seg_id = seg_reader.segment_id().uuid_string();
        let num_docs = seg_reader.num_docs();
        let num_deleted = seg_reader.num_deleted_docs();

        let mut sfx_fields = Vec::new();
        for (field, _entry) in schema.fields() {
            let has_sfx = seg_reader.sfx_file(field).is_some();
            let num_terms = seg_reader.sfx_file(field)
                .and_then(|fs| fs.read_bytes().ok())
                .and_then(|b| {
                    if b.len() >= 13 {
                        Some(u32::from_le_bytes([b[9], b[10], b[11], b[12]]))
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
            if has_sfx || num_terms > 0 {
                sfx_fields.push((field.field_id(), has_sfx, num_terms));
            }
        }

        summaries.push(SegmentSummary {
            segment_id: seg_id,
            num_docs,
            num_deleted,
            sfx_fields,
        });
    }
    summaries
}

// ---------------------------------------------------------------------------
// trace_search — structured trace: why is doc X (not) found by contains "Y"?
// ---------------------------------------------------------------------------

/// One step in the search trace chain.
#[derive(Debug)]
pub enum TraceStep {
    /// The doc's stored text contains the search term as substring.
    GroundTruth { found: bool, context: Option<String> },
    /// Tokens produced by the tokenizer for this doc.
    Tokens { tokens_containing_term: Vec<String>, total_tokens: usize },
    /// Token found in the term dict (BM25 inverted index).
    TermDict { token: String, found: bool, doc_freq: Option<u32> },
    /// Suffix found in the suffix FST.
    SuffixFst { token: String, suffix: String, si: u16, found: bool, ordinal: Option<u64> },
    /// Doc found in the sfxpost for this ordinal.
    SfxPost { token: String, ordinal: u64, doc_found: bool, total_docs: usize },
}

/// Full trace of why a doc is/isn't found by a contains search.
#[derive(Debug)]
pub struct SearchTrace {
    pub query: String,
    pub doc_id: u32,
    pub segment_id: String,
    pub steps: Vec<TraceStep>,
}

impl std::fmt::Display for SearchTrace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "=== Search trace: {:?} in doc {} (segment {}) ===",
            self.query, self.doc_id, &self.segment_id[..8.min(self.segment_id.len())])?;
        for step in &self.steps {
            match step {
                TraceStep::GroundTruth { found, context } => {
                    let ctx = context.as_deref().unwrap_or("");
                    writeln!(f, "  ground_truth: {} {}", if *found { "FOUND" } else { "NOT FOUND" }, ctx)?;
                }
                TraceStep::Tokens { tokens_containing_term, total_tokens } => {
                    writeln!(f, "  tokenizer: {}/{} tokens contain query: {:?}",
                        tokens_containing_term.len(), total_tokens, tokens_containing_term)?;
                }
                TraceStep::TermDict { token, found, doc_freq } => {
                    writeln!(f, "  term_dict: {:?} {} (doc_freq={:?})",
                        token, if *found { "FOUND" } else { "NOT FOUND" }, doc_freq)?;
                }
                TraceStep::SuffixFst { token, suffix, si, found, ordinal } => {
                    writeln!(f, "  suffix_fst: {:?} suffix={:?} si={} {} (ordinal={:?})",
                        token, suffix, si, if *found { "FOUND" } else { "NOT FOUND" }, ordinal)?;
                }
                TraceStep::SfxPost { token, ordinal, doc_found, total_docs } => {
                    writeln!(f, "  sfxpost: {:?} ord={} doc {} ({} total entries)",
                        token, ordinal, if *doc_found { "FOUND" } else { "NOT FOUND" }, total_docs)?;
                }
            }
        }
        Ok(())
    }
}

/// Trace why a specific doc is/isn't found by a contains search.
///
/// Walks the entire chain: stored text → tokenizer → term dict → suffix FST → sfxpost.
/// Returns a structured trace showing where the chain breaks (if it does).
pub fn trace_search(
    handle: &LucivyHandle,
    field_name: &str,
    search_term: &str,
    doc_id: u32,
) -> SearchTrace {
    use ld_lucivy::suffix_fst::file::SfxFileReader;
    use ld_lucivy::query::posting_resolver;
    use ld_lucivy::schema::IndexRecordOption;
    use ld_lucivy::{DocSet, TERMINATED};

    let field = handle.field(field_name).unwrap();
    let searcher = handle.reader.searcher();
    let search_lower = search_term.to_lowercase();
    let mut steps = Vec::new();
    let mut segment_id = String::new();

    for seg_reader in searcher.segment_readers() {
        if doc_id >= seg_reader.max_doc() { continue; }
        if seg_reader.alive_bitset().map_or(false, |bs| !bs.is_alive(doc_id)) { continue; }

        segment_id = seg_reader.segment_id().uuid_string();

        // Step 1: Ground truth — does the stored text contain the search term?
        let mut doc_text = String::new();
        if let Ok(store) = seg_reader.get_store_reader(0) {
            if let Ok(doc) = store.get::<LucivyDocument>(doc_id) {
                for (f, val) in doc.field_values() {
                    if f == field {
                        if let Some(text) = val.as_value().as_str() {
                            doc_text = text.to_string();
                        }
                    }
                }
            }
        }
        let lower_text = doc_text.to_lowercase();
        let gt_found = lower_text.contains(&search_lower);
        let context = if gt_found {
            lower_text.find(&search_lower).map(|pos| {
                let start = pos.saturating_sub(30);
                let end = (pos + search_lower.len() + 30).min(doc_text.len());
                format!("...{}...", &doc_text[start..end])
            })
        } else { None };
        steps.push(TraceStep::GroundTruth { found: gt_found, context });

        // Step 2: Tokenize with the field's tokenizer
        let mut all_tokens = Vec::new();
        let mut matching_tokens = Vec::new();
        let tm = handle.index.tokenizers();
        let analyzer_name = "raw_code";
        match tm.get(analyzer_name) {
            Some(mut analyzer) => {
                let mut stream = analyzer.token_stream(&doc_text);
                while stream.advance() {
                    let tok = stream.token();
                    all_tokens.push(tok.text.clone());
                    if tok.text.contains(&search_lower) {
                        matching_tokens.push(tok.text.clone());
                    }
                }
            }
            None => {
                // Tokenizer not registered — show what's available
                steps.push(TraceStep::Tokens {
                    tokens_containing_term: vec![format!("ERROR: tokenizer '{}' not found", analyzer_name)],
                    total_tokens: 0,
                });
            }
        }
        steps.push(TraceStep::Tokens {
            tokens_containing_term: matching_tokens.clone(),
            total_tokens: all_tokens.len(),
        });

        // Step 3: For each matching token, check term dict
        for token in &matching_tokens {
            let term = Term::from_field_text(field, token);
            if let Ok(inv_idx) = seg_reader.inverted_index(field) {
                match inv_idx.get_term_info(&term) {
                    Ok(Some(ti)) => {
                        // Check if this doc is in the postings
                        let mut in_postings = false;
                        if let Ok(mut p) = inv_idx.read_postings_from_terminfo(&ti, IndexRecordOption::Basic) {
                            loop {
                                let d = p.doc();
                                if d == TERMINATED { break; }
                                if d == doc_id { in_postings = true; break; }
                                p.advance();
                            }
                        }
                        steps.push(TraceStep::TermDict {
                            token: token.clone(),
                            found: in_postings,
                            doc_freq: Some(ti.doc_freq),
                        });
                    }
                    _ => {
                        steps.push(TraceStep::TermDict {
                            token: token.clone(),
                            found: false,
                            doc_freq: None,
                        });
                    }
                }
            }
        }

        // Step 3b: Show tokens around the match position for debugging
        if gt_found && matching_tokens.is_empty() {
            if let Some(pos) = lower_text.find(&search_lower) {
                // Find tokens whose offset range overlaps with the match position
                let tm2 = handle.index.tokenizers();
                if let Some(mut analyzer) = tm2.get("raw_code") {
                    let mut nearby = Vec::new();
                    let mut stream = analyzer.token_stream(&doc_text);
                    while stream.advance() {
                        let tok = stream.token();
                        // Tokens within 50 bytes of the match
                        if tok.offset_from <= pos + 50 && tok.offset_to + 50 >= pos {
                            nearby.push(format!("{}[{}..{}]", tok.text, tok.offset_from, tok.offset_to));
                        }
                    }
                    steps.push(TraceStep::Tokens {
                        tokens_containing_term: nearby,
                        total_tokens: 0, // marker: these are "nearby" tokens
                    });
                }
            }
        }

        // Steps 4-5: SFX + sfxpost — read bytes directly, no build_resolver.
        // This avoids any scheduler/lock dependency → zero deadlock risk.
        if let (Some(sfx_slice), Some(sfxpost_slice)) = (seg_reader.sfx_file(field), seg_reader.sfxpost_file(field)) {
            if let (Ok(sfx_bytes), Ok(sfxpost_bytes)) = (sfx_slice.read_bytes(), sfxpost_slice.read_bytes()) {
                if let Ok(sfx_reader) = ld_lucivy::suffix_fst::file::SfxFileReader::open(sfx_bytes.as_ref()) {
                    if let Ok(sfxpost_reader) = ld_lucivy::suffix_fst::file::SfxPostingsReader::open(sfxpost_bytes.as_ref()) {
                        let walk = sfx_reader.prefix_walk(&search_lower);
                        for (suffix_key, parents) in &walk {
                            for parent in parents {
                                steps.push(TraceStep::SuffixFst {
                                    token: suffix_key.clone(),
                                    suffix: search_lower.clone(),
                                    si: parent.si,
                                    found: true,
                                    ordinal: Some(parent.raw_ordinal),
                                });

                                // Read sfxpost directly — no resolver needed
                                let entries = sfxpost_reader.entries(parent.raw_ordinal as u32);
                                let doc_found = entries.iter().any(|e| e.doc_id == doc_id);
                                steps.push(TraceStep::SfxPost {
                                    token: suffix_key.clone(),
                                    ordinal: parent.raw_ordinal,
                                    doc_found,
                                    total_docs: entries.len(),
                                });
                            }
                        }
                    }
                }
            }
        }

        break; // only process the first matching segment
    }

    SearchTrace { query: search_term.to_string(), doc_id, segment_id, steps }
}

/// Trace for sharded handles — finds which shard contains the doc.
pub fn trace_search_sharded(
    handle: &ShardedHandle,
    field_name: &str,
    search_term: &str,
    shard_idx: usize,
    doc_id: u32,
) -> Option<SearchTrace> {
    handle.shard(shard_idx).map(|shard| trace_search(shard, field_name, search_term, doc_id))
}
