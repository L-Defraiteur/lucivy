# Progression : fork lucivy-fst + intégration suffix search

Date : 14 mars 2026 — 16h30 (session 2)

## Résumé

Fork du crate `fst` (BurntSushi) réalisé, renommé en `lucivy-fst`, intégré
dans le pipeline suffix search (.sfx). Le suffix builder et le file reader
utilisent maintenant `lucivy-fst` au lieu de `tantivy-fst`. Le reste du
codebase (TermDictionary, queries, merger) reste sur `tantivy-fst` — migration
globale prévue en F5 quand nécessaire.

## Ce qui a été fait cette session

### 1. Fork lucivy-fst (Phase F1)

Source : `fst` (BurntSushi) v0.4.7, MIT/Unlicense, ~4-5K LOC core.

- Cloné dans `ld-lucivy/lucivy-fst/`
- Supprimé : `.git`, `fst-bin/`, `fst-levenshtein/`, `fst-regex/`, `scripts/`,
  licences redondantes
- Conservé : `bench/`, `data/`, `tests/`, `build.rs`, `LICENSE-MIT`
- Renommé : crate `fst` → `lucivy-fst`, edition 2021
- Renommé tous les imports (`use fst::` → `use lucivy_fst::`, `extern crate`)
- Ajouté au workspace ld-lucivy
- **141 tests passent** (97 lib + 3 integ + 34 doc)

### 2. OutputTable (Phase F2)

Nouvelle structure dans `lucivy-fst/src/output_table.rs` :

```rust
OutputTableBuilder::new()
  .add(record: &[u8]) -> u64  // retourne l'offset à stocker dans le FST
  .into_inner() -> Vec<u8>

OutputTable::new(data: &[u8])
  .get(offset: u64) -> &[u8]      // lecture par offset
  .try_get(offset: u64) -> Option  // lecture safe
```

Format : `[varint length][record bytes]...` (LEB128).
Pattern identique au TermInfoStore de tantivy : le FST garde son u64 standard,
la résolution multi est dans une table annexe.

7 tests couvrent : empty, single, multiple, large, varint boundaries, invalid.

### 3. Migration suffix builder (Phases F3/F4)

**`suffix_fst/builder.rs`** :
- `tantivy_fst::MapBuilder` → `lucivy_fst::MapBuilder`
- Multi-parent : `parent_list_data` custom → `OutputTableBuilder`
- Nouvelles fonctions : `encode_parent_entries()` / `decode_parent_entries()`
- L'offset multi-parent passe de `u32` à `u64` (OutputTable offset)
- 9 tests passent (encoding, builder, prefix walk, UTF-8, dedup)

**`suffix_fst/file.rs`** :
- `tantivy_fst::Map` → `lucivy_fst::Map`
- `Map::from_bytes()` → `Map::new()` (API BurntSushi)
- `decode_parents()` utilise `OutputTable::get()` + `decode_parent_entries()`
- 5 tests passent (roundtrip, prefix walk, gapmap, multi-parent, invalid)

**`suffix_fst/collector.rs`** :
- Type d'erreur `tantivy_fst::Error` → `lucivy_fst::Error`

**Total : 1159 tests ld-lucivy passent, 0 échec.**

### 4. Documentation

- **Doc 05 mis à jour** : "piste fork FST" → plan complet avec recherche
  comparative de 6 crates Rust FST, verdict `fst` (BurntSushi), approche
  généraliste (full UTF-8, pas de restriction ASCII)
- **Doc 06 créé** : plan d'action fork lucivy-fst, 7 phases (F1-F7),
  recommandation Approche A (offset dans table externe)

## État des fichiers modifiés/créés

### Nouveaux fichiers
```
lucivy-fst/                          ← crate complet (fork BurntSushi/fst)
  Cargo.toml                         ← name="lucivy-fst", v0.1.0
  LICENSE-MIT
  build.rs                           ← tables CRC32 + tag lookup
  src/
    lib.rs                           ← exports publics + OutputTable
    output_table.rs                  ← NOUVEAU : OutputTable + OutputTableBuilder
    raw/                             ← FST core (inchangé sauf imports)
    map.rs, set.rs, automaton/       ← inchangé sauf imports
    bytes.rs, error.rs, stream.rs    ← inchangé
  bench/                             ← benchmarks criterion
  data/                              ← corpus test (wiki-urls, words)
  tests/test.rs                      ← tests intégration

docs/14-mars-2026-16h30/
  05-piste-fork-fst-variantes-unicode.md  ← mis à jour (recherche libs)
  06-plan-fork-fst-lucivy-fst.md          ← NOUVEAU (plan d'action)
  08-progression-lucivy-fst-fork.md       ← NOUVEAU (ce fichier)
```

### Fichiers modifiés
```
Cargo.toml                           ← +lucivy-fst dep + workspace member
src/suffix_fst/builder.rs            ← tantivy_fst → lucivy_fst + OutputTable
src/suffix_fst/file.rs               ← tantivy_fst → lucivy_fst + OutputTable
src/suffix_fst/collector.rs          ← type erreur lucivy_fst::Error
```

## Branche et commits

Branche : `feature/sfx-contains`

Commits existants :
```
dcf743e feat: multi-value support for GapMap + SfxCollector
0fa05f9 feat: suffix FST contains search — structures + indexation + search v2
8b8a50f chore: perf-optis WIP — WASM rebuild, docs, playground dataset update
```

**Non committé** : tout le travail lucivy-fst + migration builder/file/collector
+ docs 05-08 + stress_tests + query/mod.rs pub(crate). À committer après
résolution du bug lock (doc 07).

## Phases restantes

### Chemin critique (suffix search opérationnel)
```
PHASE-6   Branchement inverted index réel     ← PROCHAINE ÉTAPE
            Connecter suffix_contains aux vraies posting lists ._raw
            via SegmentReader/TermDictionary

PHASE-4   Multi-token search
            Implémenter suffix_contains_multi_token
            (premier=.sfx tout SI, milieu=._raw SI=0, dernier=.sfx SI=0)

PHASE-5   Fuzzy d>0
            Levenshtein DFA sur suffix FST

PHASE-10  Benchmark corpus réel
            Indexer 5201 docs, comparer .sfx vs ._ngram
```

### Chemin secondaire (production ready)
```
PHASE-8   Merger .sfx              ← nécessaire pour segments mergés
PHASE-9   Supprimer ._ngram        ← cleanup après validation
F5        Migration TermDict       ← swap global tantivy-fst → lucivy-fst
F6        Compression code source  ← registry tuning, benchmarks
F7        WASM layout              ← feature flags, 32-bit
```

### Résolu par le fork
```
PHASE-7   Unicode BUG-1/2          ← résolu par F3 (multi-output OutputTable)
            Le multi-parent via OutputTable permet de stocker des SI
            différents par variante byte-width. Plus besoin du
            ByteWidthPreservingFilter.
```

## Bloqué par

Bug lock (doc 07) : une autre instance Claude Code travaille sur le bug
`LockBusy` à la réouverture d'un index dans le même process (rag3weaver
persistence tests). Ce bug est dans le pipeline lucivy_core/handle.rs et
affecte les tests E2E. La résolution de ce bug est nécessaire avant de
pouvoir tester le suffix search de bout en bout.

## Pour reprendre

1. Résoudre le bug lock (doc 07)
2. Committer le travail en cours (lucivy-fst + migration + docs)
3. Attaquer PHASE-6 : branchement inverted index réel
   - Lire `src/query/phrase_query/suffix_contains.rs` (search v2 actuel)
   - Explorer `SegmentReader`, `TermDictionary::term_info_from_ord()`
   - Explorer `InvertedIndexReader::read_postings()`
   - Connecter le flow : suffix FST → raw_ordinal → TermInfo → posting list
