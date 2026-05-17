# Knowledge Dump — Session 2 (17 mai 2026, soir)

**Branche** : `feature/sfx-v3-overlap-tokenizer`  
**Dernier commit** : `10b0d97` (refactor standalone query types)  
**État** : pipeline E2E fonctionnel, 6/10 ground truth pass, 3 bugs identifiés

---

## 1. Ce qui a été fait dans cette session

### Bugs fixés
- **word-level stripped own_len** : `add_word_stripped` stockait `own_len = content_len` (sans sep), mais `content_len()` fait `own_len - sep_len` → split trop tôt. Fix : `own_len = content_len + sep_len`.
- **resolve_single_v3 byte_to** : formule `byte_from + sti + content_len` au lieu de `byte_from + content_len` → highlight past token boundary. Fix : retirer le `+ sti` en trop.
- **is_content_char** : `is_alphanumeric()` classait emoji/CJK comme seps. Fix : `!c.is_ascii() || c.is_ascii_alphanumeric()` → non-ASCII = toujours contenu.
- **intermediates_are_pure_sep** : bytes >= 0x80 pas détectés comme contenu dans ByteMap. Fix : ajout range `(0x80, 0xFF)`.
- **termtexts file conflict** : `build_derived_indexes_v3` incluait `TermTextsIndex` (v2) → conflit avec termtexts v3. Fix : filtrer termtexts/sibling/sepmap des derived indexes v3.
- **prescan cache key mismatch** : prescan utilisait `"field:query"`, scorer `"query"`. Fix : scorer utilise `"field:query"` aussi.
- **SFX3 crash in scorer fallback** : scorer v2 tentait `SfxFileReader::open` sur SFX3. Fix : detect_sfx_version → EmptyScorer pour SFX3.
- **own_len overflow 14 bits** pour long words : words de 20K+ bytes. Fix : `sep_len == 0 → skip`, clamp pour les rares cas avec sep.
- **tail entry pour long words** : cross-sep query near end of long word. Fix : collector ajoute une entrée tail (last 8 bytes + overlap).

### Pipeline intégré
- `IndexSettings.sfx_version: u8` (default 2, set 3 pour v3)
- `SchemaConfig.sfx_version: Option<u8>` propagé dans `LucivyHandle::create()`
- `SegmentWriter` : `SfxCollectorSlot` enum route V2/V3
- V3 collector fait sa propre tokenisation via `add_value(raw_text)`
- V3 finalize : `sfx_dag_v3::build_initial_sfx_dag_v3()`
- Query auto-detect SFX3 via magic bytes

### Refactor query types standalone
- **`sfx_scoring.rs`** (NOUVEAU) : `CachedPrescan`, `SfxWeight`, `SfxScorer` — layer partagé
- **`ContainsQueryV3`** : standalone, possède son cache, prescan v2/v3 par segment, `make_weight()` → `SfxWeight` directement
- **`FuzzyQueryV3`** : standalone, même pattern, v2 fallback via `run_fuzzy_prescan()`
- **`RegexQueryV3`** : standalone, même pattern, v2 fallback via `run_regex_prescan()`
- Plus de wrappers, plus de double prescan, plus de cache format conversion
- `CachedSfxResult` = type alias pour `CachedPrescan` (backward compat)
- Cache key `"field_id:query_text"` cohérent partout

### Tests ajoutés
- 12 tests highlight (h1-h12) dans integration_tests.rs
- 1 test long word + tail entry (fz11)
- 1 test fuzzy cross-token long identifier (fz10)
- 9 tests pipeline E2E dans `test_sfx_v3_pipeline.rs`
- 1 test ground truth sur 500 fichiers rag3db dans `test_sfx_v3_ground_truth.rs`

---

## 2. Architecture fichiers — état actuel

```
src/
├── tokenizer/
│   └── equal_chunk.rs            — EqualChunkTokenizer + is_content_char (pub)
│
├── suffix_fst/
│   ├── builder_v3.rs             — encoding u64 v3, partitions 0x00/0x01/0x02
│   ├── collector_v3.rs           — overlap, word-stripped, tail entries
│   ├── section_file.rs           — format binaire sections, detect_sfx_version()
│   ├── termtexts_v3.rs           — TTX3 format
│   ├── file_v3.rs                — SFX3 reader/writer
│   ├── index_registry.rs         — build_derived_indexes_v3 (exclut termtexts v2)
│   ├── bytemap.rs                — bytes_in_ranges, content ranges include 0x80-0xFF
│   └── briques/
│       ├── mod.rs
│       ├── fst_walk.rs           — Tier 1 : candidates, falling walk, cross-token chain
│       ├── resolve.rs            — Tier 2 : posting resolution, adjacency
│       ├── composite.rs          — Tier 3 : find_literal, trigrams, multi-token
│       ├── orchestrator.rs       — contains_v3, fuzzy_v3
│       ├── regex_v3.rs           — regex orchestrator
│       └── integration_tests.rs  — 71 tests (tokenizer → collector → builder → query)
│
├── indexer/
│   ├── segment_writer.rs         — SfxCollectorSlot V2/V3, finalize avec sfx_dag_v3
│   ├── sfx_dag_v3.rs             — DAG build + merge v3
│   └── indexer_actor.rs          — background finalize error handling
│
├── index/
│   └── index_meta.rs             — IndexSettings.sfx_version
│
├── query/
│   ├── contains_query_v3.rs      — STANDALONE : prescan v2/v3, SfxWeight
│   ├── fuzzy_query_v3.rs         — STANDALONE : prescan v2/v3, SfxWeight
│   ├── regex_query_v3.rs         — STANDALONE : prescan v2/v3, SfxWeight
│   ├── query.rs                  — trait Query : cache uses CachedPrescan
│   ├── term_query/
│   │   └── term_weight.rs        — SFX3 skip guard
│   └── phrase_query/
│       ├── sfx_scoring.rs        — NOUVEAU : CachedPrescan, SfxWeight, SfxScorer
│       ├── suffix_contains_query.rs — CachedSfxResult = alias CachedPrescan
│       └── regex_continuation_query.rs — SFX3 skip guards

lucivy_core/
├── src/
│   ├── handle.rs                 — sfx_version propagation dans IndexSettings
│   └── query.rs                  — SchemaConfig.sfx_version, build_query crée types v3
└── tests/
    ├── test_sfx_v3_pipeline.rs   — 9 tests E2E (contains, fuzzy, highlights, multi-doc)
    └── test_sfx_v3_ground_truth.rs — ground truth sur 500 fichiers rag3db

docs/16-mai-2026-19h29/
├── 14-knowledge-dump-v3-implementation.md — état après session 1
├── 19-recap-bugs-word-level-stripped-en-cours.md
├── 20-edge-cases-benchmark-rag3db.md
├── 21-design-query-v3-standalone.md
├── 22-ground-truth-bugs-analysis.md — ← analyse des 4 failures
└── 23-knowledge-dump-v3-session-2.md — ← ce fichier
```

---

## 3. Encoding u64 v3 (rappel)

```
Single parent (bit 63 = 0) :
  [63]     multi_flag = 0
  [62]     is_word_start
  [61..58] overlap_len    (4 bits, 0..15)
  [57..50] sep_len        (8 bits, 0..255)
  [49..36] own_len        (14 bits, max 16383)
  [35..24] sti            (12 bits, max 4095)
  [23..0]  token_ordinal  (24 bits)
```

Pour partition 0x02 (word-stripped) : `own_len = word_content_len + sep_len`.  
`content_len() = own_len - sep_len` donne la bonne valeur partout.

---

## 4. Tokenizer is_content_char

```rust
pub fn is_content_char(c: char) -> bool {
    !c.is_ascii() || c.is_ascii_alphanumeric()
}
```

Non-ASCII (emoji, CJK, accents) = contenu. ASCII non-alphanum (`_`, `-`, `.`, `::`, espaces) = séparateurs.

Utilisé dans : `split_into_segments()` (tokenizer), `orchestrator::contains_v3` et `fuzzy_v3` (query strip), `intermediates_are_pure_sep` (resolve).

---

## 5. Flow prescan/weight/scorer (post-refactor)

```
build_query() → ContainsQueryV3 (standalone, pas de inner)
  ↓
ShardedHandle.search():
  1. query.prescan_segments(all_segs)
     → pour chaque segment : detect version
       → v3 : prescan_segment_v3 (briques orchestrator)
       → v2 : prescan_segment_v2 (run_sfx_walk)
     → cache: HashMap<("field:query", SegmentId), CachedPrescan>
  2. query.collect_prescan_doc_freqs() → {"1:mutex_lock": 42}
  3. coordinator merge across shards
  4. query.set_global_contains_doc_freqs() → self.global_doc_freq = merged
  5. query.weight() → SfxWeight { cache, global_doc_freq }
  6. weight.scorer(segment) → lookup cache → SfxScorer with BM25
```

Si `prescan_segments` n'est pas appelé (ex: LucivyHandle direct), `weight()` fait un auto-prescan.

---

## 6. Bugs ouverts (ground truth)

### Bug 1 : Faux positifs cross-token (HIGH PRIORITY)
- `"function"` → matche `"Transaction\n-STATEMENT"` via cross-token
- `"uint64_t"` → matche `"Uint64ToInt64OutOfRange"` (pas d'underscore dans le texte)
- `"TableFunction"` → matche `"TABLE_FUNCTION_ENTRY"` et `"table function"`
- **Cause** : resolve_chains_v3 vérifie adjacence par position mais pas par byte continuity
- **Fix proposé** : vérifier l'overlap dans le falling walk (Option C du doc 22)

### Bug 2 : Faux négatifs std::unique_ptr (3 docs manqués)
- Les docs contiennent bien `"std::unique_ptr"` mais v3 ne les trouve pas
- **Cause probable** : tokenisation split en 3+ tokens, chain depth ou adjacence
- **Investigation** : tracer tokenisation + falling walk chain pour ces docs

### Bug 3 : Highlights géants
- Conséquence du Bug 1 — se résoudra quand les faux positifs seront fixés

---

## 7. Merge v3 — TODO

Le merge de segments v3 (`merge_segments_v3` dans `sfx_dag_v3.rs`) est implémenté mais **pas câblé** dans `merge_dag.rs`. Pour l'instant, `NoMergePolicy` est utilisé dans les tests. Le câblage du merge nécessite :

1. Détecter la version SFX des segments sources (via magic bytes)
2. Si v3 : lire termtexts v3, extraire les tokens + metadata
3. Appeler `merge_segments_v3()` → `SfxCollectorDataV3`
4. Feed au DAG v3

---

## 8. Build et test

```bash
# Tests briques v3 (71 tests, <1s)
cargo test --lib suffix_fst::briques::integration_tests

# Tests pipeline E2E (9 tests, ~0.1s)
cargo test -p lucivy-core --test test_sfx_v3_pipeline

# Ground truth sur rag3db (500 fichiers, ~15s)
# Nécessite : git clone --depth=1 https://github.com/L-Defraiteur/rag3db.git /tmp/rag3db-bench
cargo test -p lucivy-core --test test_sfx_v3_ground_truth -- --nocapture
# Rapport : /tmp/v3_ground_truth_report.txt
```

---

## 9. Commits de la session

| Hash | Description |
|------|-------------|
| `b53f364` | fix: word-level stripped own_len = content + sep_len |
| `b378956` | fix: is_content_char + byte_to highlight fix + 12 highlight tests |
| `e1cfe40` | feat: integrate SFX v3 into indexing pipeline + sfx_version |
| `6221f36` | feat: v3 pipeline E2E — indexing + query through LucivyHandle |
| `10b0d97` | refactor: standalone v3 query types — no more wrappers |
