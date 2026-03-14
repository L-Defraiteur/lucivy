# Rapport de progression — Suffix FST contains search

Date : 14 mars 2026 — 16h30

## Résumé

Session de design + implémentation du nouveau système de recherche contains
basé sur un Suffix FST avec redirection vers les posting lists `._raw`.

Objectif : éliminer le stored text du hot path de recherche contains
(~300ms d=0 → <1ms, ~1400ms d=1 → <5ms).

## Design (docs/14-mars-2026-03h00/)

### Documents produits

| Doc | Contenu | Statut |
|-----|---------|--------|
| 01 | Format .gaps (séparateurs binaires) | Brouillon, base pour GapMap |
| 02 | Contains sans stored text (FST substring + .gaps) | Brouillon, dépassé |
| 03 | Token Map + Ngram positions composées | Dépassé par doc 06 |
| 04 | Séparateurs comme termes ngram | Piste exploratoire, non retenue |
| 05 | Plan implémentation token map | Dépassé par doc 07 |
| **06** | **Suffix FST avec redirection — design final** | **ACTIF** |
| **07** | **Plan d'implémentation Suffix FST** | **ACTIF** |
| 08 | Review ChatGPT | Feedback intégré dans 06/07 |

### Idée centrale (doc 06)

```
Avant :  trigrams → candidats → stored text → vérification → lent
Après :  suffix FST → redirection → ._raw posting → PREUVE DIRECTE
```

- **Suffix FST (.sfx)** : indexe tous les suffixes (≥3 chars) de chaque token.
  Chaque suffix pointe vers son token parent dans le FST `._raw` + un offset SI.
  **Zéro posting list** dans le .sfx — pur index de redirection.

- **Redirection** : `posting(suffix) = posting(parent) + SI`.
  Un seul lookup ._raw supplémentaire (~μs) pour économiser ~350MB de postings dupliquées.

- **Encoding u64** : bit 63 = multi-parent flag.
  Single parent (95%) : bits 0-23 = raw_ordinal (16M tokens), bits 24-31 = SI (256 chars).
  Multi-parent (5%) : offset dans une parent_list séparée.

- **GapMap** : séparateurs entre tokens, pour validation mode strict (optionnel).

- **Règle multi-token** :
  - Premier token : .sfx exact, tout SI (peut être suffix d'un doc token)
  - Tokens milieu : ._raw exact, SI=0 (vrais tokens complets)
  - Dernier token : .sfx prefix walk, SI=0 (peut être prefix d'un doc token)

### Taille estimée

```
                     Avant       Après
._ngram              ~2MB        0 (SUPPRIMÉ)
._raw                ~5MB        ~5MB (INCHANGÉ)
.tmap                ~450MB      0 (SUPPRIMÉ)
.sfx FST             —           ~20-40MB
.sfx parent lists    —           ~2MB
.sfx GapMap          —           ~50MB
TOTAL hot path       ~537MB      ~75-95MB
```

### Pistes explorées et non retenues

- **Token Map + ngram positions composées** (doc 03) — trop complexe, .tmap ~450MB
- **Trigrams** — remplacés par le suffix FST (preuve directe vs pré-filtre)
- **Cascade skip + DFA réduit** — théoriquement valide mais pas clairement plus
  rapide que DFA direct (9 walks vs 1 walk gros DFA)
- **Séparateurs comme termes FST** — pollue les lookups (termes ultra-courts)
- **TID registry** — ~même taille totale, ajoute une indirection
- **Arbre de filtrage +/-** — double lookup (doc-level + per-doc), pas plus rapide

## Implémentation

### Phase 1 — Structures de données pures ✅

Module `src/suffix_fst/`, zéro dépendance lucivy, testable en isolation.

| Fichier | Rôle | Tests |
|---------|------|-------|
| `builder.rs` | SuffixFstBuilder + encoding u64 + multi-parent | 7 |
| `gapmap.rs` | GapMapWriter / GapMapReader (format binaire) | 5 |
| `file.rs` | SfxFileWriter / SfxFileReader (assemblage .sfx) | 5 |
| `collector.rs` | SfxCollector (accumulation per-segment per-field) | 4 |
| `interceptor.rs` | SfxTokenInterceptor (tap sur TokenStream) | 3 |
| **Total** | | **24 tests** |

### Phase 2 — Intégration indexation ✅

Branchement dans le segment writer. Les .sfx sont écrits automatiquement
à chaque segment flush, un par champ `._raw`.

| Fichier modifié | Changement |
|-----------------|------------|
| `segment_component.rs` | + `SuffixFst` variant |
| `index_meta.rs` | + `".sfx"` extension |
| `segment.rs` | + `open_write_custom()` / `open_read_custom()` |
| `segment_serializer.rs` | + `write_sfx(field_id, data)` |
| `segment_writer.rs` | + `SfxCollector` per `._raw` field, interceptor |
| `space_usage/mod.rs` | + `SuffixFst` dans le match |

**Zéro régression** : 1129 tests passent (1123 existants + 6 nouveaux).

### Phase 3 — Recherche v2 (single token) ✅ partiel

Chemin parallèle `suffix_contains.rs` à côté de `ngram_contains_query.rs`.
L'ancien code n'est pas touché — on peut comparer les deux paths.

| Fonctionnel | Statut |
|-------------|--------|
| Single token exact (d=0) | ✅ testé avec fake postings |
| Single token substring | ✅ (prefix walk sur suffix FST) |
| Single token prefix match | ✅ |
| Highlights (byte_from + SI) | ✅ |
| Multi-token | 🔲 placeholder, TODO |
| Fuzzy (d>0) | 🔲 TODO |
| Branchement inverted index réel | 🔲 TODO |
| BM25 scoring | 🔲 TODO (vient du champ principal, inchangé) |

### Tests totaux

```
suffix_fst::builder        7 tests ✅
suffix_fst::gapmap         5 tests ✅
suffix_fst::file           5 tests ✅
suffix_fst::collector      4 tests ✅
suffix_fst::interceptor    3 tests ✅
suffix_contains (search)   6 tests ✅
────────────────────────────────────
Total nouveau code          30 tests ✅
Existant                    1099 tests ✅
Total                       1129 tests, 0 régression
```

## Prochaines étapes

### Phase 3 suite — Branchement inverted index réel

- Ouvrir le .sfx depuis le `SegmentReader`
- Résoudre les raw_ordinals via le `TermDictionary` du `._raw` field
- Lire les posting lists réelles (doc_id, positions, offsets)
- Benchmark sur vrai corpus vs ngram_contains actuel

### Phase 4 — Multi-token + GapMap

- Intersection curseurs triés pour Ti consécutifs
- Validation séparateurs via GapMap reader
- Premier=.sfx tout SI, milieu=._raw SI=0, dernier=.sfx prefix SI=0

### Phase 5 — Fuzzy (d>0)

- Levenshtein DFA sur le suffix FST (même mécanisme que startsWith fuzzy)
- Réutilisation de `AutomatonWeight` / `FuzzyTermQuery`

### Phase 6 — Merger + suppression ._ngram

- Merger les .sfx au merge de segments (rebuild FST + concat GapMap)
- Retirer le champ `._ngram` de l'indexation

## Fichiers créés/modifiés

```
NOUVEAUX (6 fichiers, ~850 lignes) :
  src/suffix_fst/mod.rs
  src/suffix_fst/builder.rs
  src/suffix_fst/gapmap.rs
  src/suffix_fst/file.rs
  src/suffix_fst/collector.rs
  src/suffix_fst/interceptor.rs
  src/query/phrase_query/suffix_contains.rs

MODIFIÉS (6 fichiers, ~50 lignes ajoutées) :
  src/lib.rs                        + pub mod suffix_fst
  src/index/segment_component.rs    + SuffixFst variant
  src/index/index_meta.rs           + ".sfx" extension
  src/index/segment.rs              + open_write_custom / open_read_custom
  src/indexer/segment_serializer.rs + write_sfx()
  src/indexer/segment_writer.rs     + sfx_collectors + interceptor
  src/space_usage/mod.rs            + SuffixFst match arm
  src/query/phrase_query/mod.rs     + pub mod suffix_contains

DOCS (docs/14-mars-2026-03h00/) :
  06-design-suffix-fst-contains.md  (design final, itéré 4 fois)
  07-implementation-suffix-fst.md   (plan d'implémentation)
  08-note-chatgpt.md                (review externe)
```
