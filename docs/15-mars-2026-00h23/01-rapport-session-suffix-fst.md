# Rapport de session — suffix FST : fork, câblage, multi-token, fuzzy

Date : 15 mars 2026 — 00h23

## Résumé

Session complète : fork lucivy-fst, câblage inverted index réel, multi-token
avec GapMap, fuzzy Levenshtein, manifeste .sfx, architecture unifiée.
7 commits sur `feature/sfx-contains`, 1172 tests passent.

## Travail réalisé

### 1. Fork lucivy-fst (F1-F4)

- Cloné `fst` (BurntSushi) v0.4.7 → `lucivy-fst` v0.1.0
- Ajouté `OutputTable` (varint-prefixed records pour multi-output)
- Migré suffix builder/file/collector de `tantivy-fst` vers `lucivy-fst`
- 141 tests lucivy-fst passent

### 2. PHASE-6 : inverted index réel

- `resolve_raw_ordinal()` : ordinal → TermInfo → posting list réelle
- `make_raw_resolver()` : closure pour suffix_contains_single_token
- Décommenté `TermDictionary::term_info_from_ord()`
- Test E2E avec vrai Index + Unicode (8 vérifications : accents, CJK, emoji)

### 3. SuffixContainsQuery (query autonome)

- Query/Weight/Scorer standalone, indépendant de NgramContainsQuery
- Erreur claire si pas de .sfx (pas de fallback silencieux)
- Auto-tokenise la query → route single vs multi-token
- Highlights via HighlightSink avec byte offsets exacts
- 9 tests E2E (ASCII, substring, accents, CJK, emoji, highlights)

### 4. PHASE-4 : multi-token search

- `suffix_contains_multi_token()` : chaînes de Ti consécutifs
- Validation séparateurs via GapMap (`read_separator`)
- 5 tests : exact 2-tokens, wrong separator, 3-tokens, not consecutive
- Emoji "rust🦀lang" → 2 tokens + séparateur "🦀" via GapMap

### 5. PHASE-5 : fuzzy d>0

- `SfxDfaWrapper` : implémente `lucivy_fst::Automaton` pour Levenshtein
- `SfxFileReader::fuzzy_walk()` : prefix DFA sur suffix FST
- `suffix_contains_single_token_fuzzy()` + `multi_token_fuzzy()`
- `separator_matches_fuzzy()` : edit distance sur séparateurs
- Fuzzy sur tokens ET séparateurs dans le même budget

### 6. Tri SI=0 + min_suffix_len=1

- `encode_parent_entries()` trie par SI croissant → early exit exact/prefix
- `min_suffix_len` default 1 (était 3) — tous les suffixes indexés
- `LUCIVY_MIN_SUFFIX_LEN` env var pour override benchmarking
- Nécessaire : "db is cool" doit matcher "rag3db is cool"

### 7. Manifeste .sfx

- `SegmentComponent::SuffixFst` = manifeste listant les field_ids
- Fichiers per-field `{uuid}.{field_id}.sfx` restent indépendants
- GC lit le manifeste pour préserver les per-field .sfx
- Compatible BlobDirectory, RamDirectory, MmapDirectory, StdFsDirectory
- Footer ManagedDirectory géré automatiquement

### 8. Doc architecture unifiée

- Doc 10 : le .sfx remplace ._raw FST + ._ngram pour TOUS les query types
- Regex : walk DFA premier token + continuation état à travers GapMap
- Trois modes regex : contains, startsWith, strict_regex
- Phases U1-U5 planifiées

### 9. Investigation bug rag3weaver

- Test `test_contains_neural_networks` ajouté dans lucivy_core → passe
- Le bug "neural networks" 0 résultats est dans rag3weaver, pas dans lucivy
- Probable : champs `._ngram`/`._raw` pas alimentés par l'extension C++

## Commits

Branche : `feature/sfx-contains`

```
a49aa23 feat: sfx manifest, SI=0 sort, min_suffix_len=1, architecture unifiée
2e8886e feat: suffix contains — real inverted index, multi-token, fuzzy, Unicode
2bf4038 feat: fork lucivy-fst + migrate suffix search to OutputTable
dcf743e feat: multi-value support for GapMap + SfxCollector
0fa05f9 feat: suffix FST contains search — structures + indexation + search v2
8b8a50f chore: perf-optis WIP — WASM rebuild, docs, playground dataset update
```

## Fichiers clés modifiés/créés

```
lucivy-fst/                              ← crate complet (fork BurntSushi/fst)
  src/output_table.rs                    ← OutputTable + OutputTableBuilder

src/suffix_fst/builder.rs               ← SI=0 sort, min_suffix_len env var
src/suffix_fst/collector.rs             ← min_suffix_len env var
src/suffix_fst/file.rs                  ← SfxDfaWrapper, fuzzy_walk
src/suffix_fst/stress_tests.rs          ← tests mis à jour

src/query/phrase_query/suffix_contains.rs      ← resolve_raw_ordinal, multi-token, fuzzy
src/query/phrase_query/suffix_contains_query.rs ← SuffixContainsQuery standalone

src/index/segment_reader.rs             ← sfx_files via manifeste
src/indexer/segment_serializer.rs       ← write_sfx_manifest
src/indexer/segment_writer.rs           ← sfx_field_ids + manifeste
src/indexer/segment_updater.rs          ← GC lit manifeste

src/termdict/mod.rs                     ← décommenté term_info_from_ord

lucivy_core/src/handle.rs               ← test_contains_neural_networks

docs/14-mars-2026-16h30/
  05-piste-fork-fst-variantes-unicode.md ← recherche libs FST
  06-plan-fork-fst-lucivy-fst.md         ← plan fork
  08-progression-lucivy-fst-fork.md      ← progression F1-F4
  09-progression-phase6-cablage.md       ← progression PHASE-6
  10-architecture-unifiee-sfx.md         ← design unifié
```

## État des tests

- **ld-lucivy** : 1172 tests, 0 échec, 7 ignored
- **lucivy-fst** : 141 tests, 0 échec
- **lucivy_core** : 73 tests, 0 échec

## Phases restantes

### Chemin critique
```
U1    Tri SI=0                      ✅ FAIT
U2    SfxTermDictionary             ← PROCHAINE (remplace ._raw FST)
U3    Regex continuation DFA        ← après U2
U4    Supprimer ._ngram             ← après validation benchmark
U5    Supprimer ._raw FST           ← après U2 validé
```

### Secondaire
```
PHASE-8   Merger .sfx               ← nécessaire pour segments mergés
PHASE-10  Benchmark corpus réel     ← validation perf
F5        Migration TermDict global  ← swap tantivy-fst → lucivy-fst partout
F6        Compression code source    ← registry tuning
F7        WASM layout                ← feature flags 32-bit
```

### Résolu
```
PHASE-4   Multi-token search        ✅
PHASE-5   Fuzzy d>0                 ✅
PHASE-6   Inverted index réel       ✅
PHASE-7   Unicode BUG-1/2           ✅ (multi-output OutputTable)
F1-F4     Fork lucivy-fst           ✅
```

## Pour reprendre

1. Phase U2 : créer `SfxTermDictionary` qui wrappe le .sfx FST
   - Même API que `TermDictionary` (get, term_ord, stream, search)
   - Filter SI=0 pour exact/prefix, any SI pour contains
   - Branche `feature/sfx-unified`, tests dupliqués V1 vs V2

2. Ou PHASE-8 : merger .sfx (nécessaire pour production)

3. Ou PHASE-10 : benchmark sur corpus réel (5201 docs)
