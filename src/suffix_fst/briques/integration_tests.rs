//! Integration tests — full chain: text → tokenizer → collector → builder → file → query.
//!
//! Tests edge cases from doc 16: multi-split, long seps, emoji, UTF-8, stripped partition, etc.

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use crate::suffix_fst::builder_v3::SuffixFstBuilderV3;
    use crate::suffix_fst::collector_v3::SfxCollectorV3;
    use crate::suffix_fst::file_v3::{SfxFileReaderV3, SfxFileWriterV3};
    use crate::suffix_fst::sfxpost_v2::SfxPostReaderV2;
    use crate::suffix_fst::briques::orchestrator;
    use crate::suffix_fst::briques::fst_walk;
    use crate::suffix_fst::briques::resolve;
    use crate::suffix_fst::briques::context::BriquesContext;
    use crate::query::posting_resolver::{PostingEntry, PostingResolver};

    // ─── Test harness ──────────────────────────────────────────────────

    struct TestIndex {
        sfx_bytes: Vec<u8>,
        sfxpost_bytes: Vec<u8>,
        posmap_bytes: Vec<u8>,
        bytemap_bytes: Vec<u8>,
        word_sfxpost_bytes: Vec<u8>,
    }

    struct TestResolver(SfxPostReaderV2);

    impl PostingResolver for TestResolver {
        fn resolve(&self, ordinal: u64) -> Vec<PostingEntry> {
            self.0.entries(ordinal as u32).into_iter().map(|e| PostingEntry {
                doc_id: e.doc_id,
                position: e.token_index,
                byte_from: e.byte_from,
                byte_to: e.byte_to,
            }).collect()
        }
    }

    /// Build a complete v3 index from text values. Returns (sfx_bytes, sfxpost_bytes).
    fn build(texts: &[&str]) -> TestIndex {
        let mut collector = SfxCollectorV3::new();
        for text in texts {
            collector.begin_doc();
            collector.add_value(text);
            collector.end_doc();
        }
        let data = collector.into_data();

        let mut builder = SuffixFstBuilderV3::with_min_suffix_len(1);
        // Chunk-level (partitions 0x00/0x01) — skip word-stripped entries
        for &intern_ord in &data.sorted_indices {
            let meta = &data.token_meta[intern_ord as usize];
            if meta.is_word_stripped { continue; }
            let text = &data.token_texts[intern_ord as usize];
            let content_ord = data.intern_to_final[intern_ord as usize];
            builder.add_token(text, content_ord as u64, meta.own_len, meta.sep_len,
                meta.overlap_len, meta.is_word_start);
        }
        // Word-level stripped (partition 0x02)
        for ws in &data.word_stripped {
            let content_ord = data.intern_to_final[ws.first_intern_ord as usize];
            builder.add_word_stripped(
                &ws.word_content, &ws.content_overlap,
                content_ord as u64, ws.first_own_len, ws.last_sep_len, ws.is_word_start,
            );
        }
        let (fst_data, parent_data) = builder.build().unwrap();

        let num_terms = data.num_content_ords;
        let mut post_writer = crate::suffix_fst::sfxpost_v2::SfxPostWriterV2::new(num_terms);
        for (content_ord, postings) in data.content_postings.iter().enumerate() {
            for &(doc_id, ti, bf, bt) in postings {
                post_writer.add_entry(content_ord as u32, doc_id, ti, bf, bt);
            }
        }
        let sfxpost = post_writer.finish();
        let writer = SfxFileWriterV3::new(fst_data, parent_data, data.num_docs);

        // Build derived indexes (posmap, bytemap) for relaxed chain verification
        let derived = crate::suffix_fst::index_registry::build_derived_indexes_v3(
            &data.tokens, Some(&sfxpost), Some(&data.own_lens),
        );
        let posmap_bytes = derived.iter()
            .find(|(ext, _)| ext == "posmap")
            .map(|(_, d)| d.clone())
            .unwrap_or_default();
        let bytemap_bytes = derived.iter()
            .find(|(ext, _)| ext == "bytemap")
            .map(|(_, d)| d.clone())
            .unwrap_or_default();

        TestIndex {
            sfx_bytes: writer.to_bytes(),
            sfxpost_bytes: sfxpost,
            posmap_bytes,
            bytemap_bytes,
            word_sfxpost_bytes: data.word_sfxpost,
        }
    }

    /// Run contains query, return list of doc_ids.
    fn query_contains(idx: &TestIndex, query: &str, strict_sep: bool) -> Vec<u32> {
        let reader = SfxFileReaderV3::open(&idx.sfx_bytes).unwrap();
        let resolver = TestResolver(SfxPostReaderV2::open_slice(&idx.sfxpost_bytes).unwrap());
        let posmap = crate::suffix_fst::posmap::PosMapReader::open(&idx.posmap_bytes);
        let bytemap = crate::suffix_fst::bytemap::ByteBitmapReader::open(&idx.bytemap_bytes);
        let word_sfxpost = crate::suffix_fst::word_sfxpost::WordSfxPostReader::open(&idx.word_sfxpost_bytes);
        let ctx = BriquesContext {
            reader: &reader, resolver: &resolver, filter_docs: None,
            posmap, bytemap, word_sfxpost, sibling_v3: None, termtexts: None,
        };
        let matches = orchestrator::contains_v3(&ctx, query, false, false, strict_sep);
        let mut docs: Vec<u32> = matches.iter().map(|m| m.doc_id).collect();
        docs.sort_unstable();
        docs.dedup();
        docs
    }

    /// Run contains query with anchor_start.
    fn query_starts_with(idx: &TestIndex, query: &str) -> Vec<u32> {
        let reader = SfxFileReaderV3::open(&idx.sfx_bytes).unwrap();
        let resolver = TestResolver(SfxPostReaderV2::open_slice(&idx.sfxpost_bytes).unwrap());
        let ctx = BriquesContext {
            reader: &reader, resolver: &resolver, filter_docs: None,
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None,
        };
        let matches = orchestrator::contains_v3(&ctx, query, true, false, true);
        matches.iter().map(|m| m.doc_id).collect()
    }

    /// Run contains query, return all matches with highlight byte ranges.
    fn query_contains_hl(idx: &TestIndex, query: &str, strict_sep: bool) -> Vec<(u32, u32, u32)> {
        let reader = SfxFileReaderV3::open(&idx.sfx_bytes).unwrap();
        let resolver = TestResolver(SfxPostReaderV2::open_slice(&idx.sfxpost_bytes).unwrap());
        let posmap = crate::suffix_fst::posmap::PosMapReader::open(&idx.posmap_bytes);
        let bytemap = crate::suffix_fst::bytemap::ByteBitmapReader::open(&idx.bytemap_bytes);
        let word_sfxpost = crate::suffix_fst::word_sfxpost::WordSfxPostReader::open(&idx.word_sfxpost_bytes);
        let ctx = BriquesContext {
            reader: &reader, resolver: &resolver, filter_docs: None,
            posmap, bytemap, word_sfxpost, sibling_v3: None, termtexts: None,
        };
        let matches = orchestrator::contains_v3(&ctx, query, false, false, strict_sep);
        let mut hl: Vec<(u32, u32, u32)> = matches.iter()
            .map(|m| (m.doc_id, m.byte_from, m.byte_to))
            .collect();
        hl.sort();
        hl.dedup();
        hl
    }

    /// Run fuzzy query, return list of doc_ids.
    fn query_fuzzy(idx: &TestIndex, query: &str, distance: u8, strict_sep: bool) -> Vec<u32> {
        let reader = SfxFileReaderV3::open(&idx.sfx_bytes).unwrap();
        let resolver = TestResolver(SfxPostReaderV2::open_slice(&idx.sfxpost_bytes).unwrap());
        let posmap = crate::suffix_fst::posmap::PosMapReader::open(&idx.posmap_bytes);
        let bytemap = crate::suffix_fst::bytemap::ByteBitmapReader::open(&idx.bytemap_bytes);
        let word_sfxpost = crate::suffix_fst::word_sfxpost::WordSfxPostReader::open(&idx.word_sfxpost_bytes);
        let ctx = BriquesContext {
            reader: &reader, resolver: &resolver, filter_docs: None,
            posmap, bytemap, word_sfxpost, sibling_v3: None, termtexts: None,
        };
        let (bitset, _, _) = orchestrator::fuzzy_v3(&ctx, query, distance, strict_sep, 100);
        (0..100).filter(|&d| bitset.contains(d)).collect()
    }

    // ═══════════════════════════════════════════════════════════════════
    // 1. Tokenizer edge cases
    // ═══════════════════════════════════════════════════════════════════

    #[test]
    fn t1_short_token() {
        let idx = build(&["ab"]);
        assert!(!query_contains(&idx, "ab", true).is_empty());
        assert!(!query_contains(&idx, "a", true).is_empty());
    }

    #[test]
    fn t5_sep_absorbed() {
        let idx = build(&["a_b"]);
        assert!(!query_contains(&idx, "a", true).is_empty());
        assert!(!query_contains(&idx, "b", true).is_empty());
        assert!(!query_contains(&idx, "a_", true).is_empty());
    }

    #[test]
    fn t6_long_sep_split() {
        let idx = build(&["a________b"]);
        // "a" and "b" both findable
        assert!(!query_contains(&idx, "a", true).is_empty());
        assert!(!query_contains(&idx, "b", true).is_empty());
        // "ab" without seps → found with strict_sep=false
        assert!(!query_contains(&idx, "ab", false).is_empty());
    }

    #[test]
    fn t7_only_seps() {
        let idx = build(&["________"]);
        // Pure seps → should be indexed
        assert!(!query_contains(&idx, "____", true).is_empty());
    }

    #[test]
    fn t8_leading_seps() {
        let idx = build(&["__init"]);
        assert!(!query_contains(&idx, "init", true).is_empty());
        assert!(!query_contains(&idx, "__", true).is_empty());
    }

    #[test]
    fn t10_utf8_multibyte() {
        let idx = build(&["café_latte"]);
        assert!(!query_contains(&idx, "café", true).is_empty());
        assert!(!query_contains(&idx, "latte", true).is_empty());
        assert!(!query_contains(&idx, "afé", true).is_empty());
    }

    #[test]
    fn t11_emoji() {
        let idx = build(&["🦀_rust"]);
        assert!(!query_contains(&idx, "🦀", true).is_empty());
        assert!(!query_contains(&idx, "rust", true).is_empty());
    }

    #[test]
    fn t12_empty_text() {
        let idx = build(&[""]);
        assert!(query_contains(&idx, "a", true).is_empty());
    }

    #[test]
    fn t14_double_colon() {
        let idx = build(&["std::vector"]);
        assert!(!query_contains(&idx, "std", true).is_empty());
        assert!(!query_contains(&idx, "vector", true).is_empty());
        assert!(!query_contains(&idx, "::", true).is_empty());
        assert!(!query_contains(&idx, "std::vector", true).is_empty());
    }

    // ═══════════════════════════════════════════════════════════════════
    // 3. Stripped partition
    // ═══════════════════════════════════════════════════════════════════

    #[test]
    fn s1_trigram_cross_sep_found() {
        let idx = build(&["mutex_lock"]);
        // "exl" crosses sep boundary → found in stripped partition
        let reader = SfxFileReaderV3::open(&idx.sfx_bytes).unwrap();
        let cands = fst_walk::fst_candidates_v3(&reader, "exlo", false, false);
        assert!(!cands.is_empty(), "exlo should be in stripped partition");
    }

    #[test]
    fn s2_trigram_cross_sep_not_found_strict() {
        let idx = build(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&idx.sfx_bytes).unwrap();
        // strict=true → "exlo" not in partitions 0x00/0x01
        let cands = fst_walk::fst_candidates_v3(&reader, "exlo", false, true);
        assert!(cands.is_empty(), "exlo should NOT be found with strict=true");
    }

    #[test]
    fn s3_query_no_sep_text_with_sep() {
        let idx = build(&["mutex_lock"]);
        assert!(!query_contains(&idx, "mutexlock", false).is_empty());
    }

    #[test]
    fn s4_query_different_sep() {
        let idx = build(&["mutex_lock"]);
        assert!(!query_contains(&idx, "mutex lock", false).is_empty());
    }

    #[test]
    fn s5_query_same_sep() {
        let idx = build(&["mutex_lock"]);
        assert!(!query_contains(&idx, "mutex_lock", true).is_empty());
    }

    #[test]
    fn s6_query_more_seps() {
        let idx = build(&["mutex_lock"]);
        assert!(!query_contains(&idx, "mutex__lock", false).is_empty());
    }

    #[test]
    fn s7_text_long_seps() {
        let idx = build(&["mutex________lock"]);
        assert!(!query_contains(&idx, "mutexlock", false).is_empty());
    }

    #[test]
    fn s8_query_only_seps_stripped_to_empty() {
        let idx = build(&["a___b"]);
        // Query "___" stripped → "" → empty result
        assert!(query_contains(&idx, "___", false).is_empty());
    }

    #[test]
    fn s9_query_only_seps_strict() {
        let idx = build(&["a___b"]);
        // "___" strict=true → search for literal "___" in partition 0x01
        assert!(!query_contains(&idx, "___", true).is_empty());
    }

    // ═══════════════════════════════════════════════════════════════════
    // 4. Falling walk — split and chaining
    // ═══════════════════════════════════════════════════════════════════

    #[test]
    fn f1_split_two_tokens() {
        let idx = build(&["mutex_lock"]);
        assert!(!query_contains(&idx, "mutex_lock", true).is_empty());
    }

    #[test]
    fn f2_split_three_tokens() {
        let idx = build(&["mutex_lock_init"]);
        assert!(!query_contains(&idx, "mutex_lock_init", true).is_empty());
    }

    #[test]
    fn f3_single_token_substring() {
        let idx = build(&["mutex_lock"]);
        assert!(!query_contains(&idx, "tex", true).is_empty());
    }

    #[test]
    fn f4_query_starts_at_end_of_token() {
        let idx = build(&["mutex_lock_init"]);
        // "ck_init" starts near end of "lock_" token
        assert!(!query_contains(&idx, "ck_init", true).is_empty());
    }

    #[test]
    fn f6_query_in_overlap_zone() {
        let idx = build(&["mutex_lock"]);
        // "lo" is in the overlap zone of "mutex_lo" AND at SI=0 of "lock"
        assert!(!query_contains(&idx, "lo", true).is_empty());
    }

    #[test]
    fn f7_chain_four_tokens() {
        let idx = build(&["a_b_c_d"]);
        assert!(!query_contains(&idx, "a_b_c_d", true).is_empty());
    }

    #[test]
    fn f8_traverse_pure_sep_tokens() {
        let idx = build(&["mutex________lock"]);
        // "mutexlock" strict_sep=false → must traverse pure-sep chunks
        assert!(!query_contains(&idx, "mutexlock", false).is_empty());
    }

    // ═══════════════════════════════════════════════════════════════════
    // 5. Fuzzy
    // ═══════════════════════════════════════════════════════════════════

    #[test]
    fn fz1_exact_d0() {
        let idx = build(&["mutex_lock"]);
        assert!(!query_fuzzy(&idx, "mutex_lock", 0, true).is_empty());
    }

    #[test]
    fn fz2_typo_1_char() {
        let idx = build(&["mutex_lock"]);
        assert!(!query_fuzzy(&idx, "mutex_lck", 1, true).is_empty());
    }

    #[test]
    fn fz3_typo_2_chars() {
        let idx = build(&["mutex_lock"]);
        assert!(!query_fuzzy(&idx, "mutx_lk", 2, true).is_empty());
    }

    #[test]
    fn fz4_no_match() {
        let idx = build(&["mutex_lock"]);
        assert!(query_fuzzy(&idx, "zzzzzzzzz", 1, true).is_empty());
    }

    #[test]
    fn fz7_d0_strict_false() {
        let idx = build(&["mutex_lock"]);
        assert!(!query_fuzzy(&idx, "mutexlock", 0, false).is_empty());
    }

    #[test]
    fn fz8_d1_strict_false() {
        let idx = build(&["mutex_lock"]);
        assert!(!query_fuzzy(&idx, "mutexlck", 1, false).is_empty());
    }

    #[test]
    fn fz11_very_long_word_with_sep() {
        // Word of 20K+ chars followed by sep and another word.
        // Stripped partition should still find cross-sep matches.
        let long_word = "a".repeat(20_000);
        let text = format!("{long_word}_short");
        let idx = build(&[&text]);
        // "short" in normal partition → found
        assert!(!query_contains(&idx, "short", true).is_empty());
        // Substring deep in the long word → found via chunks 0x00/0x01
        assert!(!query_contains(&idx, "aaaaaaa", true).is_empty());
        // Cross-sep stripped: "aaaaashort" → needs stripped partition
        // The query stripped becomes "aaaaashort" which should find
        // the word content (clamped) + overlap "sh" in partition 0x02
        assert!(!query_contains(&idx, "aaaaashort", false).is_empty(),
            "cross-sep query past clamp limit should still work via falling walk chain");
    }

    #[test]
    fn fz10_long_cross_token_d1_strict_false() {
        // Same case as pipeline test: long identifier with typo
        let idx = build(&["ku_dynamic_cast is used everywhere"]);
        let docs = query_fuzzy(&idx, "ku_dinamic_cast", 1, false);
        assert!(!docs.is_empty(), "fuzzy d=1 strict_sep=false should find ku_dynamic_cast with typo y→i");
    }

    #[test]
    fn fz9_multi_occurrence() {
        let idx = build(&["lock_lock_lock"]);
        let reader = SfxFileReaderV3::open(&idx.sfx_bytes).unwrap();
        let resolver = TestResolver(SfxPostReaderV2::open_slice(&idx.sfxpost_bytes).unwrap());
        let ctx = BriquesContext {
            reader: &reader, resolver: &resolver, filter_docs: None,
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None,
        };
        let matches = orchestrator::contains_v3(&ctx, "lock", false, false, true);
        assert!(matches.len() >= 3, "should find 3 occurrences, got {}", matches.len());
    }

    // ═══════════════════════════════════════════════════════════════════
    // 7. Adjacence et multi-doc
    // ═══════════════════════════════════════════════════════════════════

    #[test]
    fn a1_correct_doc() {
        let idx = build(&["mutex_lock", "hello_world"]);
        let docs = query_contains(&idx, "mutex", true);
        assert_eq!(docs, vec![0]);
    }

    #[test]
    fn a2_two_docs() {
        let idx = build(&["mutex_lock", "mutex_core"]);
        let docs = query_contains(&idx, "mutex", true);
        assert!(docs.contains(&0) && docs.contains(&1));
    }

    #[test]
    fn a4_no_false_positive_cross_doc() {
        let idx = build(&["mutex", "lock"]);
        // "mutex_lock" should NOT match across docs
        let docs = query_contains(&idx, "mutex_lock", true);
        assert!(docs.is_empty(), "should not match across separate docs");
    }

    // ═══════════════════════════════════════════════════════════════════
    // 8. Highlights
    // ═══════════════════════════════════════════════════════════════════

    // "mutex_lock": m=0 u=1 t=2 e=3 x=4 _=5 l=6 o=7 c=8 k=9

    #[test]
    fn h1_single_token_substring() {
        // "tex" in "mutex_lock" → bytes [2, 5)
        let hl = query_contains_hl(&build(&["mutex_lock"]), "tex", true);
        assert_eq!(hl.len(), 1);
        assert_eq!(hl[0], (0, 2, 5), "tex → byte_from=2 byte_to=5");
    }

    #[test]
    fn h2_full_token() {
        // "mutex" → token "mutex_" content ends at 5
        let hl = query_contains_hl(&build(&["mutex_lock"]), "mutex", true);
        assert!(!hl.is_empty());
        let (doc, bf, bt) = hl[0];
        assert_eq!((doc, bf), (0, 0));
        assert_eq!(bt, 5, "mutex content ends at 5 (sep excluded)");
    }

    #[test]
    fn h3_second_token() {
        // "lock" → starts at byte 6, ends at 10
        let idx = build(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&idx.sfx_bytes).unwrap();
        let resolver = TestResolver(SfxPostReaderV2::open_slice(&idx.sfxpost_bytes).unwrap());
        let ctx = BriquesContext {
            reader: &reader, resolver: &resolver, filter_docs: None,
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None,
        };
        let matches = orchestrator::contains_v3(&ctx, "lock", false, false, true);
        eprintln!("h3 all matches:");
        for m in &matches {
            eprintln!("  doc={} pos={} span={} byte=[{}..{}] sti={} ord={}",
                m.doc_id, m.position, m.span, m.byte_from, m.byte_to, m.sti, m.ordinal);
        }
        let hl: Vec<(u32, u32, u32)> = matches.iter()
            .map(|m| (m.doc_id, m.byte_from, m.byte_to)).collect();
        assert_eq!(hl.len(), 1, "expected 1 highlight, got {:?}", hl);
        assert_eq!(hl[0], (0, 6, 10));
    }

    #[test]
    fn h4_cross_token() {
        // "mutex_lock" → full text match bytes [0, 10)
        let hl = query_contains_hl(&build(&["mutex_lock"]), "mutex_lock", true);
        assert!(!hl.is_empty());
        assert_eq!(hl[0].1, 0, "starts at 0");
        assert_eq!(hl[0].2, 10, "ends at 10");
    }

    #[test]
    fn h5_cross_sep_boundary() {
        // "x_l" crosses token boundary, found in overlap zone
        let idx = build(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&idx.sfx_bytes).unwrap();
        // Trace
        let cands = fst_walk::fst_candidates_v3(&reader, "x_l", false, true);
        eprintln!("h5 fst_candidates('x_l' strict): {}", cands.len());
        let splits = fst_walk::falling_walk_v3(&reader, "x_l", true);
        eprintln!("h5 falling_walk('x_l' strict): {} splits", splits.len());
        for s in &splits { eprintln!("  consumed={} rem={} sti={} own={}", s.query_consumed, s.remainder_start, s.parent.sti, s.parent.own_len); }
        let chains = fst_walk::cross_token_chain_v3(&reader, "x_l", true);
        eprintln!("h5 chains('x_l' strict): {} chains", chains.len());
        for c in &chains { eprintln!("  ords={:?} sti={}", c.ordinals, c.first_sti); }

        let hl = query_contains_hl(&idx, "x_l", true);
        eprintln!("h5 hl: {:?}", hl);
        assert!(!hl.is_empty(), "x_l should match via cross-token chain");
        assert_eq!(hl[0].1, 4, "x starts at byte 4");
    }

    #[test]
    fn h6_stripped() {
        // "mutexlock" strict_sep=false → should cover bytes 0..something
        let hl = query_contains_hl(&build(&["mutex_lock"]), "mutexlock", false);
        assert!(!hl.is_empty());
        assert_eq!(hl[0].1, 0, "starts at byte 0");
    }

    #[test]
    fn h7_multi_occurrence() {
        // "ab" in "ab_ab_ab" → 3 occurrences at distinct byte offsets
        let hl = query_contains_hl(&build(&["ab_ab_ab"]), "ab", true);
        assert!(hl.len() >= 3, "expected 3 occurrences, got {}", hl.len());
        let byte_froms: Vec<u32> = hl.iter().map(|h| h.1).collect();
        assert!(byte_froms.contains(&0), "first ab at byte 0");
        assert!(byte_froms.contains(&3), "second ab at byte 3");
        assert!(byte_froms.contains(&6), "third ab at byte 6");
    }

    #[test]
    fn h8_multi_doc() {
        // "lock" appears in doc 0 and doc 1 at different offsets
        let idx = build(&["mutex_lock", "lock_free"]);
        let hl = query_contains_hl(&idx, "lock", true);
        let doc0: Vec<_> = hl.iter().filter(|h| h.0 == 0).collect();
        let doc1: Vec<_> = hl.iter().filter(|h| h.0 == 1).collect();
        assert!(!doc0.is_empty(), "should match in doc 0");
        assert!(!doc1.is_empty(), "should match in doc 1");
        assert_eq!(doc0[0].1, 6, "in doc 0 lock starts at 6");
        assert_eq!(doc1[0].1, 0, "in doc 1 lock starts at 0");
    }

    #[test]
    fn h9_emoji() {
        // "🦀" is 4 bytes, "_" is 1 byte, "rust" is 4 bytes
        // "🦀_rust": 🦀=0..4 _=4 r=5 u=6 s=7 t=8
        let hl = query_contains_hl(&build(&["🦀_rust"]), "🦀", true);
        assert!(!hl.is_empty());
        assert_eq!(hl[0].1, 0, "emoji starts at byte 0");
        assert_eq!(hl[0].2, 4, "emoji ends at byte 4");
    }

    #[test]
    fn h10_utf8_multibyte() {
        // "café_latte": c=0 a=1 f=2 é=3..4 _=5 l=6 a=7 t=8 t=9 e=10
        let hl = query_contains_hl(&build(&["café_latte"]), "café", true);
        assert!(!hl.is_empty());
        assert_eq!(hl[0].1, 0);
        assert_eq!(hl[0].2, 5, "café = 5 bytes (é is 2 bytes)");
    }

    #[test]
    fn h11_byte_from_not_before_text() {
        // For any match, byte_from must be < text.len()
        let text = "hello_world_test";
        let idx = build(&[text]);
        for q in &["hello", "world", "test", "llo", "orl", "ello_world"] {
            let hl = query_contains_hl(&idx, q, true);
            for (_, bf, bt) in &hl {
                assert!(*bf < text.len() as u32, "{q}: byte_from={bf} >= text.len()");
                assert!(*bt <= text.len() as u32, "{q}: byte_to={bt} > text.len()");
                assert!(bf < bt, "{q}: byte_from={bf} >= byte_to={bt}");
            }
        }
    }

    #[test]
    fn h12_stripped_long_sep() {
        // "ab" in "a________b" strict_sep=false
        let text = format!("a{}b", "_".repeat(20));
        let hl = query_contains_hl(&build(&[&text]), "ab", false);
        assert!(!hl.is_empty());
        assert_eq!(hl[0].1, 0, "starts at byte 0");
    }

    // ═══════════════════════════════════════════════════════════════════
    // 9. Exact match and anchor_start
    // ═══════════════════════════════════════════════════════════════════

    #[test]
    fn e2_anchor_start() {
        let idx = build(&["mutex_lock"]);
        assert!(!query_starts_with(&idx, "mutex_lo").is_empty());
    }

    #[test]
    fn e3_anchor_rejects_substring() {
        let idx = build(&["mutex_lock"]);
        assert!(query_starts_with(&idx, "tex_lo").is_empty());
    }

    // ═══════════════════════════════════════════════════════════════════
    // 10. Extreme cases
    // ═══════════════════════════════════════════════════════════════════

    #[test]
    fn x1_query_too_long() {
        let idx = build(&["hello"]);
        let long_query = "a".repeat(3000);
        assert!(query_contains(&idx, &long_query, true).is_empty());
    }

    #[test]
    fn x2_empty_query() {
        let idx = build(&["hello"]);
        assert!(query_contains(&idx, "", true).is_empty());
    }

    #[test]
    fn x4_single_char_doc() {
        let idx = build(&["x"]);
        assert!(!query_contains(&idx, "x", true).is_empty());
    }

    #[test]
    fn x5_emoji_in_text_and_query() {
        let idx = build(&["🦀_rust"]);
        assert!(!query_contains(&idx, "🦀_rust", true).is_empty());
        assert!(!query_contains(&idx, "🦀rust", false).is_empty());
    }

    #[test]
    fn x6_chinese_characters() {
        let idx = build(&["漢字_テスト"]);
        assert!(!query_contains(&idx, "漢字", true).is_empty());
        assert!(!query_contains(&idx, "テスト", true).is_empty());
    }

    #[test]
    fn x8_long_word_no_sep() {
        let long = "a".repeat(100);
        let idx = build(&[&long]);
        // Should be split into 13 chunks of 8 + 1 chunk of 4
        assert!(!query_contains(&idx, "aaaaaaaaa", true).is_empty()); // 9 a's → cross-chunk
        assert!(!query_contains(&idx, "aaaa", true).is_empty()); // within chunk
    }

    #[test]
    fn x9_empty_index() {
        let idx = build(&[]);
        assert!(query_contains(&idx, "hello", true).is_empty());
    }

    // ═══════════════════════════════════════════════════════════════════
    // 11. Multi-split content + sep
    // ═══════════════════════════════════════════════════════════════════

    #[test]
    fn x11e_single_token_overlap() {
        // "internati" is within TI=0 extended (content "interna" + overlap "ti")
        let idx = build(&["internationalization________initialization"]);
        assert!(!query_contains(&idx, "internati", true).is_empty());
    }

    #[test]
    fn x11f_cross_chunk_same_word() {
        // "tionalization" crosses TI=1 → TI=2 (same word, content chunks)
        let idx = build(&["internationalization________initialization"]);
        assert!(!query_contains(&idx, "tionali", true).is_empty()); // within TI=1
    }

    #[test]
    fn x11g_second_word_cross_chunk() {
        // "initialization" spans TI=4 → TI=5
        let idx = build(&["internationalization________initialization"]);
        assert!(!query_contains(&idx, "initial", true).is_empty());
        assert!(!query_contains(&idx, "ization", true).is_empty());
    }

    #[test]
    fn x11b_stripped_traverse_pure_sep() {
        // "nationalizationinit" strict_sep=false → traverse pure-sep TI=3
        let idx = build(&["internationalization________initialization"]);
        let reader = SfxFileReaderV3::open(&idx.sfx_bytes).unwrap();
        // Trace: what does fst_candidates find for the stripped query?
        let cands = fst_walk::fst_candidates_v3(&reader, "nationalizationinit", false, false);
        eprintln!("x11b fst_candidates: {}", cands.len());
        let splits = fst_walk::falling_walk_v3(&reader, "nationalizationinit", false);
        eprintln!("x11b falling_walk: {} splits", splits.len());
        for s in &splits {
            eprintln!("  consumed={} rem={} sti={} own={} ovl_valid={}",
                s.query_consumed, s.remainder_start, s.parent.sti, s.parent.own_len, s.overlap_validated);
        }
        let chains = fst_walk::cross_token_chain_v3(&reader, "nationalizationinit", false);
        eprintln!("x11b chains: {}", chains.len());
        for c in &chains {
            eprintln!("  ords={:?} sti={}", c.ordinals, c.first_sti);
        }
        // Manual resolve to trace the issue
        let resolver = TestResolver(SfxPostReaderV2::open_slice(&idx.sfxpost_bytes).unwrap());
        let posmap = crate::suffix_fst::posmap::PosMapReader::open(&idx.posmap_bytes);
        let bytemap = crate::suffix_fst::bytemap::ByteBitmapReader::open(&idx.bytemap_bytes);
        eprintln!("x11b posmap={} bytemap={}", posmap.is_some(), bytemap.is_some());

        // Resolve with strict to see if chains match positionally
        let strict_matches = resolve::resolve_chains_v3(&chains, &resolver, None);
        eprintln!("x11b strict_resolve: {} matches", strict_matches.len());
        for m in &strict_matches {
            eprintln!("  doc={} pos={} span={} byte=[{}..{}]", m.doc_id, m.position, m.span, m.byte_from, m.byte_to);
        }

        // Trace all chain ordinals and their postings
        let sfxpost_reader = SfxPostReaderV2::open_slice(&idx.sfxpost_bytes).unwrap();
        eprintln!("x11b sfxpost num_terms={}", sfxpost_reader.num_terms());
        // Check word sfxpost
        if let Some(wsp) = crate::suffix_fst::word_sfxpost::WordSfxPostReader::open(&idx.word_sfxpost_bytes) {
            eprintln!("x11b word_sfxpost num_ords={}", wsp.num_ordinals());
            for ord in 0..wsp.num_ordinals() {
                let entries = wsp.entries(ord);
                if !entries.is_empty() {
                    eprintln!("x11b word_sfxpost[{ord}]: {:?}",
                        entries.iter().map(|e| (e.doc_id, e.first_position, e.last_position, e.byte_from, e.byte_to)).collect::<Vec<_>>());
                }
            }
        } else {
            eprintln!("x11b word_sfxpost: EMPTY/INVALID");
        }
        // Dump ALL postings in sfxpost
        for ord in 0..sfxpost_reader.num_terms() {
            let entries = sfxpost_reader.entries(ord);
            if !entries.is_empty() {
                eprintln!("x11b sfxpost[{ord}]: {:?}",
                    entries.iter().map(|e| (e.doc_id, e.token_index, e.byte_from, e.byte_to)).collect::<Vec<_>>());
            }
        }
        for (ci, chain) in chains.iter().enumerate() {
            let all_ords: Vec<Vec<u64>> = chain.ordinals.clone();
            eprintln!("x11b chain[{ci}] sti={} ords={:?}", chain.first_sti, all_ords);
        }

        if let (Some(pm), Some(bm)) = (posmap.as_ref(), bytemap.as_ref()) {
            let relaxed_matches = resolve::resolve_chains_v3_relaxed(&chains, &resolver, None, pm, bm);
            eprintln!("x11b relaxed_resolve: {} matches", relaxed_matches.len());
        }

        let docs = query_contains(&idx, "nationalizationinit", false);
        eprintln!("x11b final result: {:?}", docs);
        assert!(!docs.is_empty());
    }

    #[test]
    fn x11d_stripped_skip_multiple_seps() {
        let idx = build(&["internationalization________initialization"]);
        // "zationinitial" → skip seps in TI=2 and pure-sep TI=3
        assert!(!query_contains(&idx, "zationinitial", false).is_empty());
    }

    // ─── X12 — Very long separator ─────────────────────────────────────

    #[test]
    fn x12b_stripped_long_sep() {
        let text = format!("a{}b", "_".repeat(20));
        let idx = build(&[&text]);
        assert!(!query_contains(&idx, "ab", false).is_empty());
    }

    #[test]
    fn x12d_substring_in_seps() {
        let text = format!("a{}b", "_".repeat(20));
        let idx = build(&[&text]);
        // "______" is a substring within the sep tokens
        assert!(!query_contains(&idx, "______", true).is_empty());
    }

    // ─── X13 — Long word without separator ─────────────────────────────

    #[test]
    fn x13a_cross_chunk_long_word() {
        let long = "a".repeat(100);
        let idx = build(&[&long]);
        // 12 a's → must cross at least 1 chunk boundary (chunk = 8 bytes)
        assert!(!query_contains(&idx, &"a".repeat(12), true).is_empty());
    }

    // ─── X14 — Emoji + seps + content ──────────────────────────────────

    #[test]
    fn x14a_emoji_found() {
        let idx = build(&["🦀__rust_lang"]);
        assert!(!query_contains(&idx, "🦀", true).is_empty());
    }

    #[test]
    fn x14b_emoji_with_sep() {
        let idx = build(&["🦀__rust_lang"]);
        assert!(!query_contains(&idx, "🦀__rust", true).is_empty());
    }

    #[test]
    fn x14c_emoji_stripped() {
        let idx = build(&["🦀__rust_lang"]);
        assert!(!query_contains(&idx, "🦀rust", false).is_empty());
    }

    #[test]
    fn x14d_cross_token_after_emoji() {
        let idx = build(&["🦀__rust_lang"]);
        assert!(!query_contains(&idx, "rust_lang", true).is_empty());
    }

    #[test]
    fn x14e_stripped_after_emoji() {
        let idx = build(&["🦀__rust_lang"]);
        assert!(!query_contains(&idx, "rustlang", false).is_empty());
    }

    // ─── Bug investigation: false positives cross-token ─────────────

    /// Diagnostic: trace what happens when we search "function" in a corpus
    /// containing both legitimate matches and the false positive text.
    #[test]
    fn diag_false_positive_function() {
        // Doc 0: legitimate match (has "function")
        // Doc 1: false positive candidate (has "WriteTransaction\n-STATEMENT")
        // Doc 2: another false positive candidate (has "dysfunction")
        let idx = build(&[
            "function_result",
            "WriteTransaction\n-STATEMENT BEGIN TRANSACTION",
            "dysfunction_is_a_word",
        ]);
        let reader = SfxFileReaderV3::open(&idx.sfx_bytes).unwrap();
        let resolver = TestResolver(SfxPostReaderV2::open_slice(&idx.sfxpost_bytes).unwrap());
        let ctx = BriquesContext {
            reader: &reader, resolver: &resolver, filter_docs: None,
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None,
        };

        // strict_sep=false (default for contains queries)
        let matches = orchestrator::contains_v3(&ctx, "function", false, false, false);

        eprintln!("\n=== DIAG: 'function' strict_sep=false ===");
        for m in &matches {
            eprintln!("  doc={} pos={} span={} byte=[{}..{}] sti={}",
                m.doc_id, m.position, m.span, m.byte_from, m.byte_to, m.sti);
        }

        let doc_ids: Vec<u32> = matches.iter().map(|m| m.doc_id).collect();
        eprintln!("  doc_ids: {:?}", doc_ids);

        // Doc 0 should match (has "function")
        assert!(doc_ids.contains(&0), "doc 0 should match (has function)");

        // Doc 1 should NOT match (WriteTransaction ≠ function)
        assert!(!doc_ids.contains(&1),
            "doc 1 should NOT match (WriteTransaction != function). Matches: {:?}", matches);

        // Doc 2: "dysfunction" DOES contain "function" as substring, should match
        assert!(doc_ids.contains(&2), "doc 2 should match (dysfunction contains function)");
    }

    /// Check strict_sep=true vs false for known edge cases.
    #[test]
    fn diag_false_positive_uint64t() {
        let idx = build(&[
            "uint64_t value",
            "Uint64ToInt64OutOfRange",
        ]);
        let reader = SfxFileReaderV3::open(&idx.sfx_bytes).unwrap();
        let resolver = TestResolver(SfxPostReaderV2::open_slice(&idx.sfxpost_bytes).unwrap());
        let ctx = BriquesContext {
            reader: &reader, resolver: &resolver, filter_docs: None,
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None,
        };

        // strict_sep=false: strips "_" from query → "uint64t"
        let matches_relaxed = orchestrator::contains_v3(&ctx, "uint64_t", false, false, false);
        eprintln!("\n=== DIAG: 'uint64_t' strict_sep=false ===");
        for m in &matches_relaxed {
            eprintln!("  doc={} pos={} span={} byte=[{}..{}]",
                m.doc_id, m.position, m.span, m.byte_from, m.byte_to);
        }

        // strict_sep=true: keeps "_" in query → "uint64_t"
        let matches_strict = orchestrator::contains_v3(&ctx, "uint64_t", false, false, true);
        eprintln!("\n=== DIAG: 'uint64_t' strict_sep=true ===");
        for m in &matches_strict {
            eprintln!("  doc={} pos={} span={} byte=[{}..{}]",
                m.doc_id, m.position, m.span, m.byte_from, m.byte_to);
        }

        let relaxed_docs: Vec<u32> = matches_relaxed.iter().map(|m| m.doc_id).collect();
        let strict_docs: Vec<u32> = matches_strict.iter().map(|m| m.doc_id).collect();

        // Doc 0 should match in both modes
        assert!(relaxed_docs.contains(&0), "doc 0 should match relaxed");
        assert!(strict_docs.contains(&0), "doc 0 should match strict");

        // Doc 1: "Uint64ToInt64OutOfRange" lowered = "uint64toint64outofrange"
        // stripped query "uint64t" is substring → match in relaxed is expected (sep-agnostic)
        // BUT is this the intended behavior? Log for investigation.
        eprintln!("\n  Doc 1 in relaxed: {}", relaxed_docs.contains(&1));
        eprintln!("  Doc 1 in strict: {}", strict_docs.contains(&1));
    }

    /// Check TableFunction false positive.
    #[test]
    fn diag_false_positive_tablefunction() {
        let idx = build(&[
            "TableFunction_entry",
            "TABLE_FUNCTION_ENTRY",
            "table function call",
        ]);
        let reader = SfxFileReaderV3::open(&idx.sfx_bytes).unwrap();
        let resolver = TestResolver(SfxPostReaderV2::open_slice(&idx.sfxpost_bytes).unwrap());
        let ctx = BriquesContext {
            reader: &reader, resolver: &resolver, filter_docs: None,
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None,
        };

        let matches_relaxed = orchestrator::contains_v3(&ctx, "TableFunction", false, false, false);
        let matches_strict = orchestrator::contains_v3(&ctx, "TableFunction", false, false, true);

        eprintln!("\n=== DIAG: 'TableFunction' ===");
        eprintln!("  relaxed matches:");
        for m in &matches_relaxed {
            eprintln!("    doc={} pos={} span={} byte=[{}..{}]",
                m.doc_id, m.position, m.span, m.byte_from, m.byte_to);
        }
        eprintln!("  strict matches:");
        for m in &matches_strict {
            eprintln!("    doc={} pos={} span={} byte=[{}..{}]",
                m.doc_id, m.position, m.span, m.byte_from, m.byte_to);
        }

        let relaxed_docs: HashSet<u32> = matches_relaxed.iter().map(|m| m.doc_id).collect();
        let strict_docs: HashSet<u32> = matches_strict.iter().map(|m| m.doc_id).collect();

        // Doc 0: "TableFunction_entry" → has "TableFunction" literally
        assert!(relaxed_docs.contains(&0));
        assert!(strict_docs.contains(&0));

        eprintln!("  Doc 1 (TABLE_FUNCTION_ENTRY) relaxed: {}", relaxed_docs.contains(&1));
        eprintln!("  Doc 1 (TABLE_FUNCTION_ENTRY) strict: {}", strict_docs.contains(&1));
        eprintln!("  Doc 2 (table function call) relaxed: {}", relaxed_docs.contains(&2));
        eprintln!("  Doc 2 (table function call) strict: {}", strict_docs.contains(&2));
    }

    /// Trace the FST walking + chain for "function" to understand false positive.
    #[test]
    fn diag_trace_function_fst() {
        let idx = build(&[
            "function_result",
            "WriteTransaction\n-STATEMENT BEGIN TRANSACTION",
        ]);
        let reader = SfxFileReaderV3::open(&idx.sfx_bytes).unwrap();

        eprintln!("\n=== TRACE: FST walk for 'function' ===");

        // 1. Single-token candidates
        let cands = fst_walk::fst_candidates_v3(&reader, "function", false, false);
        eprintln!("  fst_candidates (strict=false): {} found", cands.len());
        for c in &cands {
            eprintln!("    ord={} sti={} own_len={} sep={} overlap={} ws={}",
                c.raw_ordinal, c.sti, c.own_len, c.sep_len, c.overlap_len, c.is_word_start);
        }

        let cands_strict = fst_walk::fst_candidates_v3(&reader, "function", false, true);
        eprintln!("  fst_candidates (strict=true): {} found", cands_strict.len());
        for c in &cands_strict {
            eprintln!("    ord={} sti={} own_len={} sep={} overlap={} ws={}",
                c.raw_ordinal, c.sti, c.own_len, c.sep_len, c.overlap_len, c.is_word_start);
        }

        // 2. Falling walk splits
        let splits = fst_walk::falling_walk_v3(&reader, "function", false);
        eprintln!("\n  falling_walk (strict=false): {} splits", splits.len());
        for s in &splits {
            eprintln!("    consumed={} remainder_start={} overlap_validated={} parent: ord={} sti={} own={} sep={}",
                s.query_consumed, s.remainder_start, s.overlap_validated,
                s.parent.raw_ordinal, s.parent.sti, s.parent.own_len, s.parent.sep_len);
        }

        let splits_strict = fst_walk::falling_walk_v3(&reader, "function", true);
        eprintln!("  falling_walk (strict=true): {} splits", splits_strict.len());
        for s in &splits_strict {
            eprintln!("    consumed={} remainder_start={} overlap_validated={} parent: ord={} sti={} own={} sep={}",
                s.query_consumed, s.remainder_start, s.overlap_validated,
                s.parent.raw_ordinal, s.parent.sti, s.parent.own_len, s.parent.sep_len);
        }

        // 3. Cross-token chains
        let chains = fst_walk::cross_token_chain_v3(&reader, "function", false);
        eprintln!("\n  cross_token_chain (strict=false): {} chains", chains.len());
        for c in &chains {
            eprintln!("    ordinals={:?} first_sti={} consumed={}",
                c.ordinals, c.first_sti, c.total_query_consumed);
        }

        // 4. Resolve each chain — show what documents they hit
        let resolver = TestResolver(SfxPostReaderV2::open_slice(&idx.sfxpost_bytes).unwrap());
        eprintln!("\n  --- Resolving ordinal postings ---");
        let mut all_ords: HashSet<u64> = HashSet::new();
        for c in &chains {
            for alts in &c.ordinals {
                for &o in alts {
                    all_ords.insert(o);
                }
            }
        }
        for c in &cands {
            all_ords.insert(c.raw_ordinal);
        }
        for ord in &all_ords {
            let entries = resolver.resolve(*ord);
            if !entries.is_empty() {
                eprintln!("    ord={}: {} postings", ord, entries.len());
                for e in entries.iter().take(5) {
                    eprintln!("      doc={} pos={} byte=[{}..{}]",
                        e.doc_id, e.position, e.byte_from, e.byte_to);
                }
            }
        }

        // 5. Full resolution
        let chain_matches = resolve::resolve_chains_v3(&chains, &resolver, None);
        eprintln!("\n  chain_matches (strict adjacency): {} matches", chain_matches.len());
        for m in &chain_matches {
            eprintln!("    doc={} pos={} span={} byte=[{}..{}]",
                m.doc_id, m.position, m.span, m.byte_from, m.byte_to);
        }

        let chain_matches_relaxed = resolve::resolve_chains_v3(&chains, &resolver, None);
        eprintln!("  chain_matches (relaxed/byte-ordered): {} matches", chain_matches_relaxed.len());
        for m in &chain_matches_relaxed {
            eprintln!("    doc={} pos={} span={} byte=[{}..{}]",
                m.doc_id, m.position, m.span, m.byte_from, m.byte_to);
        }
    }

    // ─── Bug investigation: false negatives std::unique_ptr ──────────

    /// Trace the full pipeline for "std::unique_ptr" to find why 3 docs are missed.
    #[test]
    fn diag_false_negative_unique_ptr() {
        // Reproduce the 3 false negative contexts from the ground truth report
        let idx = build(&[
            // Doc 0: simple case — should match
            "std::unique_ptr<Connection> readConn;",
            // Doc 1: nested in vector — false negative in ground truth
            "std::vector<std::unique_ptr<rag3db::common::Value>> ret;",
            // Doc 2: nested in map — false negative
            "std::unordered_map<std::string, std::unique_ptr<Value>> params;",
            // Doc 3: control — no unique_ptr
            "std::string hello;",
        ]);

        let reader = SfxFileReaderV3::open(&idx.sfx_bytes).unwrap();
        let resolver = TestResolver(SfxPostReaderV2::open_slice(&idx.sfxpost_bytes).unwrap());
        let ctx = BriquesContext {
            reader: &reader, resolver: &resolver, filter_docs: None,
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None,
        };

        // Show tokenization
        eprintln!("\n=== DIAG: std::unique_ptr ===");
        for (i, text) in [
            "std::unique_ptr<Connection> readConn;",
            "std::vector<std::unique_ptr<rag3db::common::Value>> ret;",
            "std::unordered_map<std::string, std::unique_ptr<Value>> params;",
        ].iter().enumerate() {
            let chunks = crate::tokenizer::equal_chunk::segment_and_chunk(text, 8);
            eprintln!("\n  Doc {i} tokens:");
            for (j, (t, m)) in chunks.iter().enumerate() {
                eprintln!("    pos={j}: {:?} content={} sep={} ws={} wid={}",
                    t, m.content_len, m.sep_len, m.is_word_start, m.word_id);
            }
        }

        // Test with strict_sep=true (keeps "::" and "_" in query)
        let matches_strict = orchestrator::contains_v3(&ctx, "std::unique_ptr", false, false, true);
        eprintln!("\n  contains_v3 strict_sep=true:");
        for m in &matches_strict {
            eprintln!("    doc={} pos={} span={} byte=[{}..{}] sti={}",
                m.doc_id, m.position, m.span, m.byte_from, m.byte_to, m.sti);
        }

        // Test with strict_sep=false (strips "::" and "_" → "stduniqueptr")
        let matches_relaxed = orchestrator::contains_v3(&ctx, "std::unique_ptr", false, false, false);
        eprintln!("\n  contains_v3 strict_sep=false:");
        for m in &matches_relaxed {
            eprintln!("    doc={} pos={} span={} byte=[{}..{}] sti={}",
                m.doc_id, m.position, m.span, m.byte_from, m.byte_to, m.sti);
        }

        // Trace the FST walk for the STRIPPED query
        let stripped_query = "stduniqueptr";
        eprintln!("\n  --- FST walk for stripped query '{}' ---", stripped_query);

        let cands = fst_walk::fst_candidates_v3(&reader, stripped_query, false, false);
        eprintln!("  fst_candidates: {} found", cands.len());
        for c in &cands {
            eprintln!("    ord={} sti={} own_len={} sep={} overlap={}",
                c.raw_ordinal, c.sti, c.own_len, c.sep_len, c.overlap_len);
        }

        let splits = fst_walk::falling_walk_v3(&reader, stripped_query, false);
        eprintln!("\n  falling_walk: {} splits", splits.len());
        for s in &splits {
            eprintln!("    consumed={} rem_start={} overlap_val={} ord={} sti={} own={} sep={}",
                s.query_consumed, s.remainder_start, s.overlap_validated,
                s.parent.raw_ordinal, s.parent.sti, s.parent.own_len, s.parent.sep_len);
        }

        let chains = fst_walk::cross_token_chain_v3(&reader, stripped_query, false);
        eprintln!("\n  cross_token_chain: {} chains", chains.len());
        for c in &chains {
            eprintln!("    ordinals={:?} first_sti={} consumed={}",
                c.ordinals, c.first_sti, c.total_query_consumed);
        }

        // Resolve chains
        let chain_matches = resolve::resolve_chains_v3(&chains, &resolver, None);
        eprintln!("\n  resolved chains (relaxed): {} matches", chain_matches.len());
        for m in &chain_matches {
            eprintln!("    doc={} pos={} span={} byte=[{}..{}]",
                m.doc_id, m.position, m.span, m.byte_from, m.byte_to);
        }

        // Also trace the strict query "std::unique_ptr" (with seps)
        eprintln!("\n  --- FST walk for strict query 'std::unique_ptr' ---");
        let splits_s = fst_walk::falling_walk_v3(&reader, "std::unique_ptr", true);
        eprintln!("  falling_walk strict: {} splits", splits_s.len());
        for s in &splits_s {
            eprintln!("    consumed={} rem_start={} overlap_val={} ord={} sti={} own={} sep={}",
                s.query_consumed, s.remainder_start, s.overlap_validated,
                s.parent.raw_ordinal, s.parent.sti, s.parent.own_len, s.parent.sep_len);
        }

        let chains_s = fst_walk::cross_token_chain_v3(&reader, "std::unique_ptr", true);
        eprintln!("  cross_token_chain strict: {} chains", chains_s.len());
        for c in &chains_s {
            eprintln!("    ordinals={:?} first_sti={} consumed={}",
                c.ordinals, c.first_sti, c.total_query_consumed);
        }

        let chain_matches_s = resolve::resolve_chains_v3(&chains_s, &resolver, None);
        eprintln!("  resolved chains (strict): {} matches", chain_matches_s.len());
        for m in &chain_matches_s {
            eprintln!("    doc={} pos={} span={} byte=[{}..{}]",
                m.doc_id, m.position, m.span, m.byte_from, m.byte_to);
        }

        // Verify results
        let strict_docs: HashSet<u32> = matches_strict.iter().map(|m| m.doc_id).collect();
        let relaxed_docs: HashSet<u32> = matches_relaxed.iter().map(|m| m.doc_id).collect();

        eprintln!("\n  Summary:");
        eprintln!("    strict docs: {:?}", strict_docs);
        eprintln!("    relaxed docs: {:?}", relaxed_docs);

        // All 3 docs should match (doc 3 has no unique_ptr)
        for d in 0..3 {
            assert!(strict_docs.contains(&d) || relaxed_docs.contains(&d),
                "doc {d} should match std::unique_ptr in at least one mode");
        }
        assert!(!strict_docs.contains(&3) && !relaxed_docs.contains(&3),
            "doc 3 should NOT match");
    }

    /// Reproduce false positive: "include" should NOT match "inclusive"
    #[test]
    fn diag_include_vs_inclusive() {
        let idx = build(&[
            // Doc 0: has "include" literally
            "#include <stdio.h>",
            // Doc 1: has "inclusive" — NOT "include"
            "inclusive disjunction operator",
            // Doc 2: has "include" within identifier
            "doesincludework yes",
        ]);
        let reader = SfxFileReaderV3::open(&idx.sfx_bytes).unwrap();
        let resolver = TestResolver(SfxPostReaderV2::open_slice(&idx.sfxpost_bytes).unwrap());
        let ctx = BriquesContext {
            reader: &reader, resolver: &resolver, filter_docs: None,
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None,
        };

        // Trace tokenization
        eprintln!("\n=== DIAG: include vs inclusive ===");
        for (i, text) in [
            "#include <stdio.h>",
            "inclusive disjunction operator",
            "doesincludework yes",
        ].iter().enumerate() {
            let chunks = crate::tokenizer::equal_chunk::segment_and_chunk(text, 8);
            eprintln!("  Doc {i}: {:?}", chunks.iter().map(|(t,m)| format!("{:?}(c={},s={})", t, m.content_len, m.sep_len)).collect::<Vec<_>>());
        }

        let matches_strict = orchestrator::contains_v3(&ctx, "include", false, false, true);
        let matches_relaxed = orchestrator::contains_v3(&ctx, "include", false, false, false);

        // Trace FST walk internals
        eprintln!("\n  --- FST internals for 'include' strict ---");
        let cands = fst_walk::fst_candidates_v3(&reader, "include", false, true);
        eprintln!("  fst_candidates strict: {}", cands.len());
        for c in &cands { eprintln!("    ord={} sti={} own={} sep={}", c.raw_ordinal, c.sti, c.own_len, c.sep_len); }

        let splits = fst_walk::falling_walk_v3(&reader, "include", true);
        eprintln!("  falling_walk strict: {} splits", splits.len());
        for s in &splits { eprintln!("    consumed={} rem={} overlap_val={} ord={} sti={} own={} sep={}",
            s.query_consumed, s.remainder_start, s.overlap_validated, s.parent.raw_ordinal, s.parent.sti, s.parent.own_len, s.parent.sep_len); }

        let chains = fst_walk::cross_token_chain_v3(&reader, "include", true);
        eprintln!("  chains strict: {} chains", chains.len());
        for c in &chains { eprintln!("    ords={:?} sti={}", c.ordinals, c.first_sti); }

        eprintln!("\n  strict matches:");
        for m in &matches_strict {
            eprintln!("    doc={} pos={} span={} byte=[{}..{}] sti={}",
                m.doc_id, m.position, m.span, m.byte_from, m.byte_to, m.sti);
        }
        eprintln!("  relaxed matches:");
        for m in &matches_relaxed {
            eprintln!("    doc={} pos={} span={} byte=[{}..{}] sti={}",
                m.doc_id, m.position, m.span, m.byte_from, m.byte_to, m.sti);
        }

        let strict_docs: HashSet<u32> = matches_strict.iter().map(|m| m.doc_id).collect();
        let relaxed_docs: HashSet<u32> = matches_relaxed.iter().map(|m| m.doc_id).collect();

        // Doc 0: has "#include" → should match
        assert!(strict_docs.contains(&0) || relaxed_docs.contains(&0), "doc 0 should match");
        // Doc 1: has "inclusive" → should NOT match (no "include" substring)
        assert!(!strict_docs.contains(&1), "doc 1 (inclusive) should NOT match strict");
        assert!(!relaxed_docs.contains(&1), "doc 1 (inclusive) should NOT match relaxed");
        // Doc 2: has "doesincludework" → "include" IS a substring
        assert!(relaxed_docs.contains(&2), "doc 2 should match relaxed (include in doesincludework)");
    }

    #[test]
    fn diag_function_standalone() {
        // "function" as a standalone word — should be found in strict mode
        let texts = &[
            "Sort on aggregated function\n-CASE Scenario3",
            "moduloFunctionOnUINT16UINT8Test",
        ];

        // Dump collector tokens
        {
            let mut collector = SfxCollectorV3::new();
            for text in texts {
                collector.begin_doc();
                collector.add_value(text);
                collector.end_doc();
            }
            let data = collector.into_data();
            eprintln!("\n=== Collector tokens ===");
            for (i, text) in data.token_texts.iter().enumerate() {
                let meta = &data.token_meta[i];
                let escaped = text.replace('\n', "\\n").replace('\r', "\\r");
                eprintln!("  intern={} text={:?} own={} sep={} ovl={} ws={} word_stripped={}",
                    i, escaped, meta.own_len, meta.sep_len, meta.overlap_len, meta.is_word_start, meta.is_word_stripped);
            }
            eprintln!("sorted_indices: {:?}", data.sorted_indices);
            eprintln!("intern_to_final: {:?}", data.intern_to_final);
        }

        let idx = build(texts);
        let reader = SfxFileReaderV3::open(&idx.sfx_bytes).unwrap();

        // Dump ALL FST keys to see what's in there
        {
            let fst = reader.fst();
            use lucivy_fst::{IntoStreamer, Streamer};
            let mut stream = fst.stream();
            let mut count = 0;
            while let Some((key, val)) = stream.next() {
                let key_str = String::from_utf8_lossy(key);
                let prefix = key[0];
                let suffix = &key[1..];
                let suffix_str = String::from_utf8_lossy(suffix);
                if suffix_str.contains("func") || suffix_str.contains("unct") || (prefix <= 0x01 && suffix.len() > 6 && suffix[0] == b'f') {
                    let parents = reader.decode_parents(val);
                    eprintln!("FST key: prefix=0x{:02x} suffix={:?} val={} parents={}",
                        prefix, suffix_str, val, parents.len());
                    for p in &parents {
                        eprintln!("    sti={} ord={} own={} sep={} ovl={} ws={}",
                            p.sti, p.raw_ordinal, p.own_len, p.sep_len, p.overlap_len, p.is_word_start);
                    }
                }
                count += 1;
            }
            eprintln!("Total FST keys: {count}");
        }

        // Check FST candidates
        let strict_cands = fst_walk::fst_candidates_v3(&reader, "function", false, true);
        eprintln!("\nstrict candidates for 'function': {}", strict_cands.len());
        for c in &strict_cands {
            eprintln!("  sti={} ord={} own_len={} sep_len={} overlap_len={} is_ws={}",
                c.sti, c.raw_ordinal, c.own_len, c.sep_len, c.overlap_len, c.is_word_start);
        }

        let relaxed_cands = fst_walk::fst_candidates_v3(&reader, "function", false, false);
        eprintln!("relaxed candidates for 'function': {}", relaxed_cands.len());
        for c in &relaxed_cands {
            eprintln!("  sti={} ord={} own_len={} sep_len={} overlap_len={} is_ws={}",
                c.sti, c.raw_ordinal, c.own_len, c.sep_len, c.overlap_len, c.is_word_start);
        }

        // Also check falling walk
        let splits = fst_walk::falling_walk_v3(&reader, "function", true);
        eprintln!("\nfalling walk splits for 'function' strict: {}", splits.len());
        for s in &splits {
            eprintln!("  consumed={} remainder_start={} overlap_validated={} sti={} ord={} own_len={}",
                s.query_consumed, s.remainder_start, s.overlap_validated,
                s.parent.sti, s.parent.raw_ordinal, s.parent.own_len);
        }

        // Check via orchestrator
        let resolver = TestResolver(SfxPostReaderV2::open_slice(&idx.sfxpost_bytes).unwrap());
        let ctx = BriquesContext {
            reader: &reader, resolver: &resolver, filter_docs: None,
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None,
        };
        let strict_matches = orchestrator::contains_v3(&ctx, "function", false, false, true);
        let relaxed_matches = orchestrator::contains_v3(&ctx, "function", false, false, false);
        eprintln!("\nstrict matches: {:?}", strict_matches.iter().map(|m| (m.doc_id, m.byte_from, m.byte_to, m.sti)).collect::<Vec<_>>());
        eprintln!("relaxed matches: {:?}", relaxed_matches.iter().map(|m| (m.doc_id, m.byte_from, m.byte_to, m.sti)).collect::<Vec<_>>());

        // Doc 0 should be found in strict mode
        let strict_docs: Vec<u32> = strict_matches.iter().map(|m| m.doc_id).collect();
        let relaxed_docs: Vec<u32> = relaxed_matches.iter().map(|m| m.doc_id).collect();
        assert!(strict_docs.contains(&0), "doc 0 should match 'function' in strict mode");
        assert!(relaxed_docs.contains(&0), "doc 0 should match 'function' in relaxed mode");
    }

    #[test]
    fn diag_struct_fp() {
        let idx = build(&[
            "dataset/shortest-path-tests/eKnows.csv in this folder",  // doc 0 — NO "struct", has "short"
            "my_struct_field",                      // doc 1 — HAS "struct"
            "constructor_init",                     // doc 2 — has "truct" in "constructor"
            "instruction_set",                      // doc 3 — has "struct" in "instruction"
            "CreationAndDestroyWithNullDatabase",   // doc 4 — NO "struct"
        ]);
        let reader = SfxFileReaderV3::open(&idx.sfx_bytes).unwrap();
        let resolver = TestResolver(SfxPostReaderV2::open_slice(&idx.sfxpost_bytes).unwrap());
        let ctx = BriquesContext {
            reader: &reader, resolver: &resolver, filter_docs: None,
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None,
        };

        let cands = fst_walk::fst_candidates_v3(&reader, "struct", false, true);
        eprintln!("fst_candidates for 'struct': {}", cands.len());
        for c in &cands { eprintln!("  sti={} ord={} own={} sep={} ovl={}", c.sti, c.raw_ordinal, c.own_len, c.sep_len, c.overlap_len); }

        let splits = fst_walk::falling_walk_v3(&reader, "struct", true);
        eprintln!("\nfalling_walk splits: {}", splits.len());
        for s in &splits {
            eprintln!("  consumed={} remainder={} ovl_valid={} sti={} ord={} own={}",
                s.query_consumed, s.remainder_start, s.overlap_validated,
                s.parent.sti, s.parent.raw_ordinal, s.parent.own_len);
        }

        let chains = fst_walk::cross_token_chain_v3(&reader, "struct", true);
        eprintln!("\nchains: {}", chains.len());
        for c in &chains { eprintln!("  ords={:?} sti={}", c.ordinals, c.first_sti); }

        let matches = orchestrator::contains_v3(&ctx, "struct", false, false, true);
        eprintln!("\nmatches:");
        for m in &matches { eprintln!("  doc={} pos={} span={} byte=[{}..{}]", m.doc_id, m.position, m.span, m.byte_from, m.byte_to); }

        let match_docs: Vec<u32> = matches.iter().map(|m| m.doc_id).collect();
        assert!(!match_docs.contains(&0), "doc 0 (shortest-path) should NOT match 'struct'");
        assert!(match_docs.contains(&1), "doc 1 (my_struct) should match 'struct'");
        assert!(!match_docs.contains(&4), "doc 4 (Destroy...) should NOT match 'struct'");
    }
}
