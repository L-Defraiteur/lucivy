# Knowledge Dump — Implémentation SFX v3

**Date** : 17 mai 2026  
**Branche** : `feature/sfx-v3-overlap-tokenizer`  
**État** : indexation complète + query briques complètes, wiring Query/Weight/Scorer à faire

---

## 1. Architecture fichiers

```
src/tokenizer/
  equal_chunk.rs            ← NOUVEAU : tokenizer v3

src/suffix_fst/
  builder_v3.rs             ← NOUVEAU : encoding u64 v3 + partition 0x02 stripped
  collector_v3.rs           ← NOUVEAU : overlap + extended tokens
  section_file.rs           ← NOUVEAU : format binaire à sections nommées
  termtexts_v3.rs           ← NOUVEAU : TTX3 format avec metadata
  file_v3.rs                ← NOUVEAU : SFX3 reader/writer
  briques/
    mod.rs
    fst_walk.rs             ← NOUVEAU : Tier 1 (FST primitives)
    resolve.rs              ← NOUVEAU : Tier 2 (posting resolution)
    composite.rs            ← NOUVEAU : Tier 3 (find_literal, trigrams, multi-token)
    orchestrator.rs         ← NOUVEAU : contains_v3, fuzzy_v3
    regex_v3.rs             ← NOUVEAU : regex orchestrator
  index_registry.rs         ← MODIFIÉ : build_derived_indexes_v3 (ByteMap sans overlap)

src/indexer/
  sfx_dag_v3.rs             ← NOUVEAU : DAG build + merge
  mod.rs                    ← MODIFIÉ : +sfx_dag_v3

docs/16-mai-2026-19h29/
  01-12 : docs de design (tokenizer, overlap, siblings, etc.)
  13 : design regex v3 + bytemap
  14 : ce knowledge dump
```

---

## 2. Tokenizer — EqualChunkTokenizer

**Fichier** : `src/tokenizer/equal_chunk.rs` (20 tests)

### Principe

1. Split le texte sur les frontières alphanum/non-alphanum → segments
2. Chaque segment = mot + trailing sep (unité indivisible)
3. Si `segment.len() > MAX_TOKEN (8)` → division égale en N chunks
4. Pas d'orphelins : chunk_min = `segment.len() / ceil(segment.len() / MAX_TOKEN)`

### Exemples

```
"mutex_lock"         → ["mutex_", "lock"]           (6, 4)
"pthread_"           → ["pthread_"]                  (8, fits)
"getElementById"     → ["getEleme", "ntById"]        (8, 6)
"a________b"         → ["a____", "____", "b"]        (5, 4, 1)
```

### Metadata par token

```rust
ChunkMeta {
    content_len: usize,   // bytes alphanumériques
    sep_len: usize,       // bytes de séparateur trailing
    is_word_start: bool,  // premier chunk du segment
    word_id: usize,       // mot logique
}
```

### Pas de CamelCase

Le CamelCase split est remplacé par la division égale. Plus simple, plus prévisible, compatible avec l'overlap.

---

## 3. SFX Builder v3

**Fichier** : `src/suffix_fst/builder_v3.rs` (15 tests)

### Encoding output u64

```
Single parent (bit 63 = 0) :
  [63]     multi_flag = 0
  [62]     is_word_start
  [61..58] overlap_len    (4 bits, 0..15)
  [57..55] sep_len        (3 bits, 0..7)
  [54..40] own_len        (15 bits)
  [39..24] sti            (16 bits)
  [23..0]  token_ordinal  (24 bits)

Multi parent (bit 63 = 1) :
  [62..0]  offset dans OutputTable
```

### 3 partitions FST

```
0x00 — STI=0 (début de token)
  Usage : startsWith, term (filtre is_word_start)

0x01 — STI>0 (substring, avec seps)
  Usage : contains strict_sep=true

0x02 — STI≥0 (substring, sep-stripped : content + overlap, sep retiré)
  Usage : contains/fuzzy strict_sep=false
  Seulement pour les tokens avec sep_len > 0
  Coût : +content_len entrées par token avec sep (~10-15% du FST)
```

### Overlap

Chaque token (sauf le dernier) est étendu de `min(2, len(next_token))` bytes du token suivant. Garantit que tout trigram cross-boundary est dans un seul token.

```
Token "mutex_" (own_len=6) + overlap "lo" → extended "mutex_lo" (8 bytes)
Suffixe à STI=4 : "x_lo" ← contient le trigram cross-boundary "x_l" ✓
```

### Pré-filtrage postings par overlap

L'ordinal de "mutex_lo" est différent de "mutex_co". Les posting lists sont naturellement filtrées par le contexte droit (les 2 bytes d'overlap).

---

## 4. Collector v3

**Fichier** : `src/suffix_fst/collector_v3.rs` (12 tests)

### Ce qu'il fait

1. Tokenize avec `segment_and_chunk()` (EqualChunkTokenizer)
2. Calcule l'overlap (2 bytes du token suivant)
3. Intern les tokens **extended** ("mutex_lo" pas "mutex_")
4. Track les postings : `(doc_id, ti, byte_from, byte_to)`
5. Track les metadata : `TokenMetaV3 { own_len, sep_len, overlap_len, is_word_start, word_id }`

### Ce qu'il ne fait PAS (supprimé vs v2)

- Pas de gapmap (seps dans les tokens)
- Pas de sepmap (seps dans le FST)
- Pas de sibling_pairs (TI+1 implicite)

### into_data()

Produit `SfxCollectorDataV3` : tokens triés, ordinal remapping, postings, metadata. Prêt pour le DAG.

---

## 5. Format fichiers

### SectionFile (`section_file.rs`, 11 tests)

Format binaire extensible à sections nommées :

```
[4 bytes]  magic
[1 byte]   version
[2 bytes]  num_sections
[section table: N × 12 bytes]
  [2 bytes] section_id
  [2 bytes] reserved
  [4 bytes] offset
  [4 bytes] length
[data area]
```

### SFX3 (`file_v3.rs`, 8 tests)

Utilise SectionFile avec magic "SFX3" :

```
Section 0x01 — FST bytes
Section 0x02 — OutputTable (parent lists v3)
Section 0x03 — WordMap (token_to_word, word_start_token, word_content_len)
Section 0x04 — NextWord table (next_word[TI] → TI)
```

Supprimé vs v2 : sibling table, gapmap inline.

### TTX3 (`termtexts_v3.rs`, 8 tests)

Utilise SectionFile avec magic "TTX3" :

```
Section 0x01 — TEXTS (offset table + concatenated UTF-8)
Section 0x02 — META (own_len, sep_len, overlap_len, is_word_start par ordinal)
```

Stocke les tokens **extended** (avec overlap). Nécessaire pour le merge (relire sans retokenizer).

---

## 6. DAG + Merge

**Fichier** : `src/indexer/sfx_dag_v3.rs` (8 tests)

### DAG initial

```
prepare ──┬── build_fst_v3 ────┐
          └── build_sfxpost ───┼── assemble_v3 → SfxBuildOutputV3
```

Assemble produit : `.sfx` (SFX3) + `.sfxpost` + `.termtexts` (TTX3) + registry files (bytemap, freqmap, posmap).

ByteMap v3 : construit sur `token[..own_len]` seulement (pas les bytes d'overlap).

### Merge

`merge_segments_v3()` :
1. Lit les termtexts v3 de chaque segment source
2. Vérifie version (magic "TTX3", sinon erreur "reindex required")
3. Global intern : union des tokens extended avec metadata
4. Remap doc_ids, filtre les docs supprimés
5. Produit un `SfxCollectorDataV3` → re-feed au DAG initial

---

## 7. Query briques — 3 tiers + orchestrateurs

### Tier 1 — FST Walk (`briques/fst_walk.rs`, 12 tests)

Primitives FST pures. Pas de posting resolution.

**`fst_candidates_v3(reader, query, anchor_start, strict_sep)`**
- Prefix range scan dans le FST
- Partitions : 0x00+0x01 (strict=true), 0x00+0x01+0x02 (strict=false)
- Retourne `Vec<FstCandidateV3>` (ordinal, sti, own_len, sep_len, overlap_len, is_word_start)

**`falling_walk_v3(reader, query, strict_sep)`**
- Walk byte-par-byte dans le FST
- Split point : détecté au noeud final quand `prefix_len >= own_len - sti`
  (le split est au MILIEU de la clé FST, pas à la fin)
- Overlap validation : `overlap_validated = prefix_len - split_byte`
- Partition stripped (0x02) : split à `content_len - sti` pour strict_sep=false
- Retourne `Vec<SplitCandidateV3>` trié par query_consumed desc

**`cross_token_chain_v3(reader, query, strict_sep)`**
- Boucle : falling_walk → split → fst_candidates ou falling_walk sur remainder
- Pas de sibling DFS, pas de text lookup de candidats
- `snap_to_char_boundary()` pour les slices query UTF-8 safe
- Max depth = 8

### Tier 2 — Resolve (`briques/resolve.rs`, 6 tests)

Posting resolution + adjacency.

**`resolve_single_v3(candidates, resolver, filter_docs)`**
- Candidates → posting entries → `Vec<MatchV3>`
- byte_from ajusté par STI

**`resolve_chains_v3(chains, resolver, filter_docs)`**
- Active set : (doc_id, expected_pos, byte_from, byte_to)
- Adjacence : `position[i+1] == position[i] + 1`
- Single match per active entry per ordinal

**`selectivity_v3(reader, query, strict_sep)`**
- Estimation sans résolution : fst_candidates.len() + chains.len()

**`MatchV3`** — type unifié (remplace LiteralMatch + SuffixContainsMatch)
```rust
MatchV3 { doc_id, position, span, byte_from, byte_to, sti, ordinal }
```

### Tier 3 — Composite (`briques/composite.rs`, 13 tests)

**`find_literal_v3(reader, query, resolver, anchor_start, strict_sep, filter)`**
- = fst_candidates_v3 + resolve_single_v3 + cross_token_chain_v3 + resolve_chains_v3
- Dedup par (doc_id, position)

**`find_multi_token_v3(reader, tokens, resolver, anchor, exact, strict_sep, filter)`**
- Par token : find_literal_v3
- Pivot = token le plus sélectif (fewest matches)
- Adjacence bidirectionnelle depuis le pivot

**`resolve_trigrams_v3(reader, query, distance, resolver, strict_sep, max_doc)`**
- generate_trigrams (query telle quelle, seps inclus, n=2 ou 3)
- threshold = max(T - n*d, 1) — PAS de correction boundary
- Sélectivité rarest-first → resolve_single_v3
- Group hits by doc → count distinct trigrams ≥ threshold
- Coverage = -(miss_count as f32)

### Orchestrateurs

**`contains_v3(reader, query, resolver, anchor, exact, strict_sep, filter)`** (`orchestrator.rs`)
- strict_sep=false : strip non-alphanum de la query → une seule recherche
- Appelle find_literal_v3 + filtre exact_match si nécessaire

**`fuzzy_v3(reader, query, distance, resolver, strict_sep, max_doc)`** (`orchestrator.rs`)
- strict_sep=false : strip non-alphanum de la query
- d=0 → route vers contains_v3 (pas de trigrams)
- d>0 → resolve_trigrams_v3

**`regex_v3<A>(automaton, pattern, reader, resolver, ord_to_term, ...)`** (`regex_v3.rs`)
- strict_sep=true toujours (le regex définit ce qui matche)
- Pipeline : analyze_regex → find_literal_v3 rarest-first → intersect_ordered → gap validation
- 3 gap types : AcceptAnything (accept), ByteRangeCheck (bytemap), DfaValidation (PosMap DFA walk)
- validate_path_v3 : pas de gapmap, seps dans les tokens → DFA traverse naturellement

---

## 8. strict_separators — comment ça fonctionne

### strict_sep=true (défaut)

Le falling walk compare TOUS les bytes y compris les seps, byte par byte. `"mutex_lock"` matche `"mutex_lock"` mais pas `"mutex lock"`.

### strict_sep=false

Deux mécanismes complémentaires :

1. **Partition stripped (0x02)** : le FST contient des suffixes sans seps (content + overlap). Les trigrams comme "exl" (qui traversent une frontière content/sep) sont trouvables ici.

2. **Strip de la query** : dans contains_v3 et fuzzy_v3, la query est strippée (non-alphanum retirés) avant la recherche. `"mutex_lock"`, `"mutex lock"`, `"mutexlock"` → tous deviennent `"mutexlock"`.

Le falling walk via la partition stripped fait le split à `content_len - sti` (pas `own_len - sti`), ce qui correspond à la frontière content/overlap sans les seps.

---

## 9. Highlights v3

Beaucoup plus simples qu'en v2 :

- Pas de concat_query → byte offsets directement dans le texte original
- Pas de token_parts decomposition → chaque MatchV3 a son propre (byte_from, byte_to)
- Cross-token : byte_from du premier token, byte_to du dernier
- Multi-occurrence : toutes les occurrences retournées, dedup par (doc_id, position)
- Fuzzy : sliding window produit (doc_id, min_byte_from, max_byte_to) par zone

---

## 10. Ce qui reste à faire

### Wiring Query/Weight/Scorer

Les briques v3 sont des fonctions pures. Il faut les connecter au système Query/Weight/Scorer existant :

- `SuffixContainsQuery` : détecter SFX3 → router vers contains_v3/fuzzy_v3
- `RegexContinuationQuery` : détecter SFX3 → router vers regex_v3
- Prescan two-pass pattern : prescan_segments → cache → weight → scorer

**Attention nommage** : les Query/Weight/Scorer actuels s'appellent `SuffixContainsQuery` et `RegexContinuationQuery`. Pour v3, soit :
- Option A : même noms, routing interne par version (if SFX3 → v3, else → v2)
- Option B : nouveaux noms séparés (ContainsQueryV3, FuzzyQueryV3, RegexQueryV3)

Option A est plus simple (pas de changement dans le routing `build_query()`), option B est plus propre (séparation claire).

### Benchmarks

- Index size v2 vs v3 sur linux 90K docs
- Latence par query type
- Ground truth 37/37

### Cleanup

- Flag sfx_version dans les segments
- Documentation CLAUDE.md

---

## 11. Tests — récapitulatif

| Module | Tests |
|--------|:-----:|
| EqualChunkTokenizer | 20 |
| SuffixFstBuilderV3 | 15 |
| SfxCollectorV3 | 12 |
| SectionFile | 11 |
| TermTextsV3 | 8 |
| SfxFileV3 | 8 |
| SfxDagV3 + merge | 8 |
| Tier 1 fst_walk | 12 |
| Tier 2 resolve | 6 |
| Tier 3 composite | 13 |
| Orchestrateur contains+fuzzy | 11 |
| Orchestrateur regex | 5 |
| **Total v3** | **129** |
| Tests existants v2 | 1197 |
| **Total global** | **1326** |

Tous verts. Zéro régression v2.
