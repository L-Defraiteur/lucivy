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
    use crate::query::posting_resolver::{PostingEntry, PostingResolver};

    // ─── Test harness ──────────────────────────────────────────────────

    struct TestIndex {
        sfx_bytes: Vec<u8>,
        sfxpost_bytes: Vec<u8>,
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
        // Chunk-level (partitions 0x00/0x01)
        for (final_ord, &intern_ord) in data.sorted_indices.iter().enumerate() {
            let text = &data.token_texts[intern_ord as usize];
            let meta = &data.token_meta[intern_ord as usize];
            builder.add_token(text, final_ord as u64, meta.own_len, meta.sep_len,
                meta.overlap_len, meta.is_word_start);
        }
        // Word-level stripped (partition 0x02)
        for ws in &data.word_stripped {
            let final_ord = data.intern_to_final[ws.first_intern_ord as usize];
            builder.add_word_stripped(
                &ws.word_content, &ws.content_overlap,
                final_ord as u64, ws.first_own_len, ws.last_sep_len, ws.is_word_start,
            );
        }
        let (fst_data, parent_data) = builder.build().unwrap();

        let num_terms = data.tokens.len();
        let mut post_writer = crate::suffix_fst::sfxpost_v2::SfxPostWriterV2::new(num_terms);
        for (final_ord, &old_ord) in data.sorted_indices.iter().enumerate() {
            for &(doc_id, ti, bf, bt) in &data.token_postings[old_ord as usize] {
                post_writer.add_entry(final_ord as u32, doc_id, ti, bf, bt);
            }
        }
        let sfxpost = post_writer.finish();
        let writer = SfxFileWriterV3::new(fst_data, parent_data, data.num_docs);

        TestIndex {
            sfx_bytes: writer.to_bytes(),
            sfxpost_bytes: sfxpost,
        }
    }

    /// Run contains query, return list of doc_ids.
    fn query_contains(idx: &TestIndex, query: &str, strict_sep: bool) -> Vec<u32> {
        let reader = SfxFileReaderV3::open(&idx.sfx_bytes).unwrap();
        let resolver = TestResolver(SfxPostReaderV2::open_slice(&idx.sfxpost_bytes).unwrap());
        let matches = orchestrator::contains_v3(&reader, query, &resolver, false, false, strict_sep, None);
        let mut docs: Vec<u32> = matches.iter().map(|m| m.doc_id).collect();
        docs.sort_unstable();
        docs.dedup();
        docs
    }

    /// Run contains query with anchor_start.
    fn query_starts_with(idx: &TestIndex, query: &str) -> Vec<u32> {
        let reader = SfxFileReaderV3::open(&idx.sfx_bytes).unwrap();
        let resolver = TestResolver(SfxPostReaderV2::open_slice(&idx.sfxpost_bytes).unwrap());
        let matches = orchestrator::contains_v3(&reader, query, &resolver, true, false, true, None);
        matches.iter().map(|m| m.doc_id).collect()
    }

    /// Run contains query, return all matches with highlight byte ranges.
    fn query_contains_hl(idx: &TestIndex, query: &str, strict_sep: bool) -> Vec<(u32, u32, u32)> {
        let reader = SfxFileReaderV3::open(&idx.sfx_bytes).unwrap();
        let resolver = TestResolver(SfxPostReaderV2::open_slice(&idx.sfxpost_bytes).unwrap());
        let matches = orchestrator::contains_v3(&reader, query, &resolver, false, false, strict_sep, None);
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
        let (bitset, _, _) = orchestrator::fuzzy_v3(&reader, query, distance, &resolver, strict_sep, 100);
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
        let matches = orchestrator::contains_v3(&reader, "lock", &resolver, false, false, true, None);
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
        let hl = query_contains_hl(&build(&["mutex_lock"]), "lock", true);
        assert_eq!(hl.len(), 1);
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
        let hl = query_contains_hl(&build(&["mutex_lock"]), "x_l", true);
        assert!(!hl.is_empty());
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
        assert!(!query_contains(&idx, "nationalizationinit", false).is_empty());
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
}
