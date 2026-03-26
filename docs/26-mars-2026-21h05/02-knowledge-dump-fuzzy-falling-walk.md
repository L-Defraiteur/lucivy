# Doc 02 — Knowledge dump : fuzzy falling walk + état complet de la branche

Date : 26 mars 2026
Branche : `feature/cross-token-search` (dernier commit : `798d1a9`)

## Bug en cours : fuzzy falling walk ne trouve pas les cross-token avec typo LEFT

### Symptôme

"rag3weavr" distance=1 → 0 résultats (typo dans la partie RIGHT → devrait matcher)
"rak3weaver" distance=1 → 0 résultats (typo dans la partie LEFT → devrait matcher)

Le fuzzy_falling_walk s'exécute (3 candidates, ~1ms) mais les candidates semblent
venir du fallback exact (distance=0), pas du DFA fuzzy.

### Diagnostic partiel

Les 3 candidates de `fuzzy_falling_walk("rag3weavr", 1)` ont `right_parents=0`.
Cela signifie que le split trouvé est probablement "rag3weavr" → split au mauvais endroit,
et le remainder ne match rien.

Le DFA Levenshtein est construit pour la query COMPLÈTE "rag3weavr". Quand il
traverse le FST et arrive au noeud final "\x00rag3" (token "rag3", 4 bytes), le DFA
state devrait avoir `max_prefix_len = 4` (les 4 premiers chars "rag3" matchent exactement).
Le remainder serait "weavr" → fuzzy_walk_si0("weavr", 1) → devrait trouver "weaver".

### Ce qu'il faut vérifier

1. **Le `max_prefix_len` est-il correct ?** Ajouter un `eprintln!` dans le fuzzy_falling_walk
   quand `node.is_final()` pour afficher `prefix_len` et les parents.

2. **Le DFA pruning est-il trop agressif ?** `can_match` coupe les branches.
   Peut-être qu'il coupe le chemin vers "\x00rag3" trop tôt.

3. **Le `fuzzy_falling_walk` explore-t-il le bon espace ?** Le DFS avec le DFA
   pourrait ne jamais atteindre "\x00rag3" si le DFA state après "r","a","g","3"
   n'est pas compatible avec le DFA construit pour "rag3weavr".

4. **Le remainder "weavr" matche-t-il "weaver" via fuzzy_walk_si0 ?**
   Tester séparément : `sfx_reader.fuzzy_walk_si0("weavr", 1)` → devrait retourner "weaver".

### Comment débugger

Ajouter dans `fuzzy_falling_walk` après le check `node.is_final()` :
```rust
if node.is_final() {
    let prefix_len = lev.max_prefix_len(&lev_state);
    eprintln!("[fuzzy_fall] final node: prefix_len={}, lev_state={:?}", prefix_len, lev_state);
    // ... rest of the check
}
```

Et dans le DFS loop :
```rust
eprintln!("[fuzzy_fall] stack size={}, visiting addr={}", stack.len(), addr);
```

### Fichiers concernés

| Fichier | Quoi |
|---------|------|
| `lucivy-fst/src/automaton/levenshtein.rs` | DFA State + max_prefix_len |
| `src/suffix_fst/file.rs` | fuzzy_falling_walk |
| `src/query/phrase_query/suffix_contains.rs` | cross_token_search (appelle fuzzy_falling_walk) |

## État complet de la branche

### Commits sur `feature/cross-token-search`

```
798d1a9 fix: propagate max_prefix_len to UTF-8 intermediate DFA states
5b3132c wip: fuzzy falling walk — Levenshtein re-export fixed, prefix tracking WIP
658cebc docs: recap cross-token search + sfxpost V2 + fuzzy falling walk design
74f83a8 fix: remove CamelCaseSplit from tokenize_query — SFX + cross_token handles all
66343aa perf: dynamic pivot selection in cross_token_search
34ae9d2 perf: HashMap adjacency check in cross_token_search — O(left+right)
5e62a6a test: update stress tests for cross-token search
96563f4 feat: cross_token_search — fallback for queries spanning token boundaries
f16718f feat: full sfxpost V2 migration — merger, sfx_merge, validator, resolver
5f9f2b5 feat: PostingResolver V2 integration with auto-format detection
b88495f refactor: SfxPostReaderV2 owns its data (Vec<u8>, no lifetime)
53ffd68 feat: collector now produces sfxpost V2 format
df19950 feat: sfxpost V2 writer + reader with binary-searchable doc_ids
d1e7b61 feat: falling_walk on SfxFileReader — O(L) cross-token split detection
6f60cb1 feat: add token_len to ParentEntry and SFX inline encoding
+ docs (c4ce766, 6560f4d, e4e1f8c)
+ fuzzy cross-token right side (commit after 66343aa)
+ tokenize_query simplification (74f83a8)
```

### Ce qui marche

| Feature | Status |
|---------|--------|
| sfxpost V2 (SFP2 format) | ✅ writer + reader + migration complète |
| token_len dans ParentEntry | ✅ inline u64 + OutputTable |
| falling_walk exact | ✅ O(L) split detection |
| cross_token_search exact | ✅ fallback automatique |
| Dynamic pivot (left vs right) | ✅ |
| HashMap adjacency O(left+right) | ✅ |
| Fuzzy single-token | ✅ "weavr" d=1 → "weaver" |
| Fuzzy cross-token RIGHT | ✅ "rag3weavr" → "rag3" exact + "weavr" fuzzy → marche si typo après le split |
| Fuzzy cross-token LEFT | ❌ "rak3weaver" → DFA walk ne trouve pas le split |
| tokenize_query sans CamelCaseSplit | ✅ SimpleTokenizer + LowerCaser |
| All query types (term, parse, contains, regex, fuzzy, startsWith) | ✅ testés |

### Performances playground (5k docs code source)

| Query | Temps |
|-------|-------|
| Substring intra-token ("weaver") | ~5ms |
| Cross-token exact ("rag3weaver") | ~19ms |
| Cross-token remainder court ("rag3w") | ~19ms (après HashMap fix) |
| Fuzzy intra-token ("weavr" d=1) | ~10ms |

### Ce qui a changé dans le tokenizer/search pipeline

**Indexation** : `SimpleTokenizer → CamelCaseSplitFilter(MIN=4, max 2 merged, no backward) → LowerCaser`
Inchangé depuis `feature/optional-sfx`.

**Query** : `SimpleTokenizer → LowerCaser` (plus de CamelCaseSplit dans la query).
Chaque mot de la query va en single-token au SFX. Si 0 résultats → `cross_token_search` fallback.

**SFX search flow** :
```
1. suffix_contains_single_token(query)
   → SFX walk pour le query entier
   → Si résultats → return

2. cross_token_search(query, fuzzy_distance)
   → fuzzy_falling_walk (ou falling_walk si d=0) → split candidates
   → Pour chaque candidate : remainder walk (prefix_walk_si0 ou fuzzy_walk_si0)
   → Dynamic pivot : résoudre le côté le plus sélectif d'abord
   → Filtered resolve de l'autre côté par pivot doc_ids
   → HashMap adjacency check
```

## Comment builder / tester

### Tests Rust

```bash
cd packages/rag3db/extension/lucivy/ld-lucivy

# Tests ld-lucivy (1167 tests)
cargo test --lib -p ld-lucivy

# Tests lucivy-core (86 tests)
cargo test -p lucivy-core --lib

# Tests SFX spécifiques
cargo test --lib -p ld-lucivy suffix_fst -- --nocapture
cargo test --lib -p ld-lucivy stress_tests -- --nocapture
cargo test --lib -p ld-lucivy sfxpost_v2 -- --nocapture
cargo test --lib -p ld-lucivy test_falling_walk -- --nocapture

# Tests fuzzy / cross-token
cargo test -p lucivy-core --lib test_fuzzy_contains -- --nocapture
cargo test -p lucivy-core --lib test_contains_flexible -- --nocapture
cargo test -p lucivy-core --lib test_all_query_types_v2 -- --nocapture
```

### Build WASM emscripten

```bash
# Nécessite emsdk installé (~$HOME/emsdk)
bash bindings/emscripten/build.sh
# Produit: bindings/emscripten/pkg/lucivy.{js,wasm}
# Copié automatiquement dans playground/pkg/
```

### Build Python binding + .luce

```bash
cd bindings/python
maturin develop --release

cd ../../playground
python build_dataset.py
# Produit: playground/dataset.luce (~23MB, 839 fichiers du repo lucivy)
```

### Playground

```bash
cd packages/rag3db/extension/lucivy/ld-lucivy
node playground/serve.mjs
# → http://localhost:9877
# COOP/COEP headers pour SharedArrayBuffer
# Cache-Control: no-store pour WASM frais à chaque reload
```

Le playground peut :
- Charger un .luce (dataset pré-indexé)
- Cloner un repo GitHub (indexation dans le browser)
- Modes : contains, contains_split, startsWith, term, phrase, fuzzy, regex, parse
- Fuzzy distance configurable (0 par défaut)
- Strict separators toggle
- Highlights
- Debounce + search guard (1 search à la fois, pending re-trigger)
- OPFS sync skip pendant bulk indexing
- MAXIMUM_MEMORY=4GB

### Branches

- `main` : release stable
- `feature/optional-sfx` : session 24-25 mars (SFX optional, lucistore, bindings, etc.)
- `feature/cross-token-search` : session 25-26 mars (sfxpost V2, falling walk, cross-token)

La branche `feature/cross-token-search` est basée sur un commit WIP de `feature/optional-sfx`.

## Structure des fichiers SFX

### .sfx file

```
[4 bytes] magic "SFX1"
[1 byte] version
[4 bytes] num_docs
[4 bytes] num_suffix_terms
[8 bytes] fst_offset
[8 bytes] fst_length
[8 bytes] parent_list_offset
[8 bytes] parent_list_length
[8 bytes] gapmap_offset
[FST data]
[Parent list (OutputTable)]
[GapMap data]
```

FST keys : `\x00<suffix>` (SI=0) ou `\x01<suffix>` (SI>0)
FST values : inline u64 ou offset dans OutputTable

Inline u64 layout : `[63:flag][55..40:token_len][39..24:si][23..0:ordinal]`

ParentEntry : `{ raw_ordinal: u64, si: u16, token_len: u16 }`

### .sfxpost file (V2 — "SFP2")

```
[4 bytes] magic "SFP2"
[4 bytes] num_terms
[4 bytes × (num_terms+1)] offset table
Entry data per ordinal:
  [4 bytes] num_unique_docs
  [4 bytes × num_unique_docs] doc_ids (sorted, binary searchable)
  [4 bytes × num_unique_docs] payload_offsets
  [2 bytes × num_unique_docs] entry_counts
  Payload (VInt packed per doc):
    [VInt token_index, VInt byte_from, VInt byte_to] × count
```

### SplitCandidate (falling walk result)

```rust
pub struct SplitCandidate {
    pub prefix_len: usize,     // bytes of query consumed by left part
    pub parent: ParentEntry,   // the parent token that reaches its boundary
}
```

Condition : `parent.si + prefix_len == parent.token_len` (match reaches token end)

## Levenshtein DFA — modifications

### State struct (lucivy-fst)

```rust
struct State {
    next: [Option<usize>; 256],
    is_match: bool,
    max_prefix_len: usize,  // NOUVEAU — max prefix de la query matché ≤ distance
}
```

Calculé dans `DfaBuilder::cached()` depuis le `lev_state` vector :
```rust
let max_prefix_len = lev_state.iter().enumerate().rev()
    .find(|(_, &d)| d <= self.lev.dist)
    .map(|(i, _)| i)
    .unwrap_or(0);
```

Exposé via `Levenshtein::max_prefix_len(&self, state: &Option<usize>) -> usize`.

UTF-8 intermediate states héritent le `max_prefix_len` du parent state.

### Export

`lucivy-fst/src/lib.rs` re-exporte :
```rust
pub use crate::inner_automaton::{Levenshtein, LevenshteinError};
```

Feature gate : `#[cfg(feature = "levenshtein")]`
Activée dans `Cargo.toml` : `lucivy-fst = { path = "lucivy-fst", features = ["levenshtein"] }`
