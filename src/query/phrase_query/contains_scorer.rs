use std::cmp::min;
use std::sync::Arc;

use super::phrase_scorer::{intersection, intersection_exists, PostingsWithOffset};
use super::scoring_utils::{edit_distance, tokenize_raw, HighlightSink};
use crate::docset::{DocSet, TERMINATED};
use crate::fieldnorm::FieldNormReader;
use crate::postings::Postings;
use crate::query::bm25::Bm25Weight;
use crate::query::{BitSetDocSet, Intersection, Scorer};
use crate::schema::document::Value;
use crate::schema::Field;
use crate::store::StoreReader;
use crate::index::SegmentId;
use crate::{DocId, Score, LucivyDocument};

/// ContainsScorer: multi-token scorer with separator validation.
///
/// Phase 1: Intersection of posting lists with position check (like PhraseScorer, slop=0).
/// Phase 2: For each candidate doc, load stored text, re-tokenize, validate separators
///          and cumulative distance budget.
pub struct ContainsScorer<TPostings: Postings> {
    intersection_docset: Intersection<PostingsWithOffset<TPostings>, PostingsWithOffset<TPostings>>,
    num_terms: usize,
    max_offset: usize,
    left_positions: Vec<u32>,
    right_positions: Vec<u32>,

    // Separator validation
    query_separators: Vec<String>,
    query_prefix: String,
    query_suffix: String,
    distance_budget: u32,
    strict_separators: bool,
    cascade_distances: Vec<u32>,

    // Stored text access
    store_reader: StoreReader,
    field: Field,

    // Scoring
    fieldnorm_reader: FieldNormReader,
    similarity_weight_opt: Option<Bm25Weight>,
    phrase_count: u32,

    // Highlighting
    highlight_sink: Option<Arc<HighlightSink>>,
    highlight_field_name: String,
    segment_id: SegmentId,
}

impl<TPostings: Postings> ContainsScorer<TPostings> {
    pub fn new(
        term_postings_with_offset: Vec<(usize, TPostings)>,
        similarity_weight_opt: Option<Bm25Weight>,
        fieldnorm_reader: FieldNormReader,
        query_separators: Vec<String>,
        query_prefix: String,
        query_suffix: String,
        distance_budget: u32,
        strict_separators: bool,
        cascade_distances: Vec<u32>,
        store_reader: StoreReader,
        field: Field,
        highlight_sink: Option<Arc<HighlightSink>>,
        highlight_field_name: String,
        segment_id: SegmentId,
    ) -> ContainsScorer<TPostings> {
        let num_docs = fieldnorm_reader.num_docs();
        let max_offset = term_postings_with_offset
            .iter()
            .map(|&(offset, _)| offset)
            .max()
            .unwrap_or(0);
        let num_terms = term_postings_with_offset.len();
        let postings_with_offsets = term_postings_with_offset
            .into_iter()
            .map(|(offset, postings)| {
                PostingsWithOffset::new(postings, (max_offset - offset) as u32)
            })
            .collect::<Vec<_>>();
        let intersection_docset = Intersection::new(postings_with_offsets, num_docs);
        let mut scorer = ContainsScorer {
            intersection_docset,
            num_terms,
            max_offset,
            left_positions: Vec::with_capacity(100),
            right_positions: Vec::with_capacity(100),
            query_separators,
            query_prefix,
            query_suffix,
            distance_budget,
            strict_separators,
            cascade_distances,
            store_reader,
            field,
            fieldnorm_reader,
            similarity_weight_opt,
            phrase_count: 0,
            highlight_sink,
            highlight_field_name,
            segment_id,
        };
        if scorer.doc() != TERMINATED && !scorer.phrase_match() {
            scorer.advance();
        }
        scorer
    }

    /// Returns true if no separator/prefix/suffix validation is needed.
    fn needs_validation(&self) -> bool {
        !self.query_separators.is_empty()
            || !self.query_prefix.is_empty()
            || !self.query_suffix.is_empty()
    }

    /// Compute position intersection (slop=0), storing starting positions in left_positions.
    fn compute_phrase_match(&mut self) {
        self.intersection_docset
            .docset_mut_specialized(0)
            .positions(&mut self.left_positions);
        for i in 1..self.num_terms - 1 {
            self.intersection_docset
                .docset_mut_specialized(i)
                .positions(&mut self.right_positions);
            intersection(&mut self.left_positions, &self.right_positions);
            if self.left_positions.is_empty() {
                return;
            }
        }
        self.intersection_docset
            .docset_mut_specialized(self.num_terms - 1)
            .positions(&mut self.right_positions);
    }

    fn phrase_match(&mut self) -> bool {
        self.compute_phrase_match();
        if self.left_positions.is_empty() {
            return false;
        }
        // Position intersection: check existence first
        if !intersection_exists(&self.left_positions, &self.right_positions) {
            return false;
        }

        if !self.needs_validation() {
            // No separator validation needed: fast count (highlights collected via validate_separators when needed)
            let mut count = 0u32;
            let mut li = 0;
            let mut ri = 0;
            while li < self.left_positions.len() && ri < self.right_positions.len() {
                let lv = self.left_positions[li];
                let rv = self.right_positions[ri];
                if lv == rv {
                    count += 1;
                    li += 1;
                    ri += 1;
                } else if lv < rv {
                    li += 1;
                } else {
                    ri += 1;
                }
            }
            self.phrase_count = count;
            return count > 0;
        }

        // Collect matching starting positions (left ∩ right)
        let mut matched_positions = Vec::new();
        {
            let mut li = 0;
            let mut ri = 0;
            while li < self.left_positions.len() && ri < self.right_positions.len() {
                let lv = self.left_positions[li];
                let rv = self.right_positions[ri];
                if lv == rv {
                    matched_positions.push(lv);
                    li += 1;
                    ri += 1;
                } else if lv < rv {
                    li += 1;
                } else {
                    ri += 1;
                }
            }
        }

        // Validate separators for each matched starting position
        if let Some(count) = self.validate_separators(&matched_positions) {
            self.phrase_count = count;
            count > 0
        } else {
            false
        }
    }

    /// Validate separators using byte offsets from postings when available
    /// (WithFreqsAndPositionsAndOffsets), falling back to re-tokenization otherwise.
    /// Returns Some(count) of valid occurrences, or None on error.
    fn validate_separators(&mut self, starting_positions: &[u32]) -> Option<u32> {
        if starting_positions.is_empty() {
            return Some(0);
        }

        // Collect (position, byte_from, byte_to) from postings for each term
        let mut term_tuples: Vec<Vec<(u32, u32, u32)>> = Vec::with_capacity(self.num_terms);
        for i in 0..self.num_terms {
            let mut tuples = Vec::new();
            self.intersection_docset
                .docset_mut_specialized(i)
                .positions_and_offsets(&mut tuples);
            term_tuples.push(tuples);
        }
        let has_offsets = term_tuples
            .iter()
            .any(|t| t.iter().any(|&(_, _, to)| to > 0));

        // Load stored text
        let doc_id = self.intersection_docset.doc();
        let doc: LucivyDocument = self.store_reader.get(doc_id).ok()?;
        let stored_text = doc.get_first(self.field)?.as_str()?.to_string();

        // Fallback: tokenize if no offsets in index
        let doc_tokens = if !has_offsets {
            Some(tokenize_raw(&stored_text))
        } else {
            None
        };

        let mut count = 0u32;
        for &start_pos in starting_positions {
            if (start_pos as usize) < self.max_offset {
                continue;
            }
            let first_doc_pos = (start_pos as usize) - self.max_offset;

            // Get byte offsets for each token in this phrase occurrence
            let token_offsets: Option<Vec<(usize, usize)>> = if has_offsets {
                // Use byte offsets from postings (all terms have adjusted pos == start_pos)
                let mut offsets = Vec::with_capacity(self.num_terms);
                let mut ok = true;
                for tuples in &term_tuples {
                    if let Some(&(_, from, to)) =
                        tuples.iter().find(|&&(pos, _, _)| pos == start_pos)
                    {
                        offsets.push((from as usize, to as usize));
                    } else {
                        ok = false;
                        break;
                    }
                }
                if ok { Some(offsets) } else { None }
            } else {
                // Fallback: use re-tokenized offsets
                let doc_tokens = doc_tokens.as_ref().unwrap();
                let last_doc_pos = first_doc_pos + self.num_terms - 1;
                if last_doc_pos < doc_tokens.len() {
                    Some(
                        (0..self.num_terms)
                            .map(|i| doc_tokens[first_doc_pos + i])
                            .collect(),
                    )
                } else {
                    None
                }
            };
            let token_offsets = match token_offsets {
                Some(o) => o,
                None => continue,
            };

            let mut total_distance: u32 = 0;

            // Add cascade distances for each token
            for d in &self.cascade_distances {
                total_distance += d;
                if total_distance > self.distance_budget {
                    break;
                }
            }
            if total_distance > self.distance_budget {
                continue;
            }

            // Validate separators between consecutive tokens
            let mut valid = true;
            for (sep_idx, query_sep) in self.query_separators.iter().enumerate() {
                let (_, end_i) = token_offsets[sep_idx];
                let (start_next, _) = token_offsets[sep_idx + 1];
                if end_i > stored_text.len()
                    || start_next > stored_text.len()
                    || end_i > start_next
                {
                    valid = false;
                    break;
                }
                let doc_sep = &stored_text[end_i..start_next];
                if self.strict_separators {
                    total_distance += edit_distance(query_sep, doc_sep);
                    if total_distance > self.distance_budget {
                        valid = false;
                        break;
                    }
                } else {
                    // Relaxed: just check non-alnum chars exist between tokens
                    if doc_sep.is_empty()
                        || !doc_sep.bytes().any(|b| !b.is_ascii_alphanumeric())
                    {
                        valid = false;
                        break;
                    }
                }
            }
            if !valid {
                continue;
            }

            // Validate prefix (chars before first token)
            if !self.query_prefix.is_empty() {
                let (first_start, _) = token_offsets[0];
                if self.strict_separators {
                    let prefix_len = self.query_prefix.len();
                    let doc_prefix_start = first_start.saturating_sub(prefix_len);
                    let doc_prefix = &stored_text[doc_prefix_start..first_start];
                    total_distance += edit_distance(&self.query_prefix, doc_prefix);
                    if total_distance > self.distance_budget {
                        continue;
                    }
                } else {
                    // Relaxed: check that at least one non-alnum char exists before token
                    if first_start == 0 {
                        continue;
                    }
                    let before = &stored_text[..first_start];
                    if before.as_bytes().last().is_none_or(|b| b.is_ascii_alphanumeric()) {
                        continue;
                    }
                }
            }

            // Validate suffix (chars after last token)
            if !self.query_suffix.is_empty() {
                let (_, last_end) = token_offsets[token_offsets.len() - 1];
                if self.strict_separators {
                    let suffix_len = self.query_suffix.len();
                    let doc_suffix_end = min(last_end + suffix_len, stored_text.len());
                    let doc_suffix = &stored_text[last_end..doc_suffix_end];
                    total_distance += edit_distance(&self.query_suffix, doc_suffix);
                    if total_distance > self.distance_budget {
                        continue;
                    }
                } else {
                    // Relaxed: check that at least one non-alnum char exists after token
                    if last_end >= stored_text.len() {
                        continue;
                    }
                    let after_byte = stored_text.as_bytes()[last_end];
                    if after_byte.is_ascii_alphanumeric() {
                        continue;
                    }
                }
            }

            if let Some(ref sink) = self.highlight_sink {
                let offsets: Vec<[usize; 2]> = token_offsets
                    .iter()
                    .map(|&(from, to)| [from, to])
                    .collect();
                sink.insert(
                    self.segment_id,
                    self.intersection_docset.doc(),
                    &self.highlight_field_name,
                    offsets,
                );
            }
            count += 1;
        }
        Some(count)
    }
}

impl<TPostings: Postings> DocSet for ContainsScorer<TPostings> {
    fn advance(&mut self) -> DocId {
        loop {
            let doc = self.intersection_docset.advance();
            if doc == TERMINATED || self.phrase_match() {
                return doc;
            }
        }
    }

    fn seek(&mut self, target: DocId) -> DocId {
        debug_assert!(target >= self.doc());
        let doc = self.intersection_docset.seek(target);
        if doc == TERMINATED || self.phrase_match() {
            return doc;
        }
        self.advance()
    }

    fn doc(&self) -> DocId {
        self.intersection_docset.doc()
    }

    fn size_hint(&self) -> u32 {
        self.intersection_docset.size_hint() / (10 * self.num_terms as u32)
    }

    fn cost(&self) -> u64 {
        self.intersection_docset.size_hint() as u64 * 10 * self.num_terms as u64
    }
}

impl<TPostings: Postings> Scorer for ContainsScorer<TPostings> {
    fn score(&mut self) -> Score {
        let doc = self.doc();
        let fieldnorm_id = self.fieldnorm_reader.fieldnorm_id(doc);
        if let Some(similarity_weight) = self.similarity_weight_opt.as_ref() {
            similarity_weight.score(fieldnorm_id, self.phrase_count)
        } else {
            1.0f32
        }
    }
}

/// ContainsSingleScorer: single-token scorer with prefix/suffix validation.
pub struct ContainsSingleScorer {
    bitset_docset: BitSetDocSet,
    store_reader: StoreReader,
    field: Field,
    token: String,
    query_prefix: String,
    query_suffix: String,
    distance_budget: u32,
    strict_separators: bool,
    cascade_distance: u32,
    boost: Score,
    highlight_sink: Option<Arc<HighlightSink>>,
    highlight_field_name: String,
    segment_id: SegmentId,
}

impl ContainsSingleScorer {
    pub fn new(
        bitset_docset: BitSetDocSet,
        store_reader: StoreReader,
        field: Field,
        token: String,
        query_prefix: String,
        query_suffix: String,
        distance_budget: u32,
        strict_separators: bool,
        cascade_distance: u32,
        boost: Score,
        highlight_sink: Option<Arc<HighlightSink>>,
        highlight_field_name: String,
        segment_id: SegmentId,
    ) -> ContainsSingleScorer {
        let mut scorer = ContainsSingleScorer {
            bitset_docset,
            store_reader,
            field,
            token,
            query_prefix,
            query_suffix,
            distance_budget,
            strict_separators,
            cascade_distance,
            boost,
            highlight_sink,
            highlight_field_name,
            segment_id,
        };
        // Advance to the first valid doc
        if scorer.bitset_docset.doc() != TERMINATED && !scorer.validate_current() {
            scorer.advance();
        }
        scorer
    }

    fn needs_validation(&self) -> bool {
        !self.query_prefix.is_empty() || !self.query_suffix.is_empty()
    }

    fn validate_current(&self) -> bool {
        if !self.needs_validation() {
            return true;
        }
        let doc_id = self.bitset_docset.doc();
        if doc_id == TERMINATED {
            return false;
        }
        let doc: LucivyDocument = match self.store_reader.get(doc_id) {
            Ok(d) => d,
            Err(_) => return false,
        };
        let stored_text = match doc.get_first(self.field).and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return false,
        };
        let doc_tokens = tokenize_raw(&stored_text);
        let token_lower = self.token.to_lowercase();

        // Find all occurrences of this token (or fuzzy/substring matches) in the doc
        for &(start, end) in &doc_tokens {
            let doc_token = stored_text[start..end].to_lowercase();
            // Check if this token could match (exact, fuzzy, or substring)
            let is_match = doc_token == token_lower
                || doc_token.contains(&token_lower)
                || token_lower.contains(&doc_token);
            if !is_match {
                continue;
            }

            let mut total_distance = self.cascade_distance;

            // Validate prefix
            if !self.query_prefix.is_empty() {
                if self.strict_separators {
                    let prefix_len = self.query_prefix.len();
                    let doc_prefix_start = start.saturating_sub(prefix_len);
                    let doc_prefix = &stored_text[doc_prefix_start..start];
                    total_distance += edit_distance(&self.query_prefix, doc_prefix);
                    if total_distance > self.distance_budget {
                        continue;
                    }
                } else {
                    // Relaxed: just check a non-alnum char exists before token
                    if start == 0 {
                        continue;
                    }
                    if stored_text.as_bytes()[start - 1].is_ascii_alphanumeric() {
                        continue;
                    }
                }
            }

            // Validate suffix
            if !self.query_suffix.is_empty() {
                if self.strict_separators {
                    let suffix_len = self.query_suffix.len();
                    let doc_suffix_end = min(end + suffix_len, stored_text.len());
                    let doc_suffix = &stored_text[end..doc_suffix_end];
                    total_distance += edit_distance(&self.query_suffix, doc_suffix);
                    if total_distance > self.distance_budget {
                        continue;
                    }
                } else {
                    // Relaxed: just check a non-alnum char exists after token
                    if end >= stored_text.len() {
                        continue;
                    }
                    if stored_text.as_bytes()[end].is_ascii_alphanumeric() {
                        continue;
                    }
                }
            }

            if let Some(ref sink) = self.highlight_sink {
                sink.insert(self.segment_id, self.bitset_docset.doc(), &self.highlight_field_name, vec![[start, end]]);
            }
            return true;
        }
        false
    }
}

impl DocSet for ContainsSingleScorer {
    fn advance(&mut self) -> DocId {
        loop {
            let doc = self.bitset_docset.advance();
            if doc == TERMINATED || self.validate_current() {
                return doc;
            }
        }
    }

    fn seek(&mut self, target: DocId) -> DocId {
        let doc = self.bitset_docset.seek(target);
        if doc == TERMINATED || self.validate_current() {
            return doc;
        }
        self.advance()
    }

    fn doc(&self) -> DocId {
        self.bitset_docset.doc()
    }

    fn size_hint(&self) -> u32 {
        self.bitset_docset.size_hint()
    }
}

impl Scorer for ContainsSingleScorer {
    fn score(&mut self) -> Score {
        self.boost
    }
}

// Tests for edit_distance and tokenize_raw are in scoring_utils.rs
