# Doc 05 — Knowledge dump complet : session cross-token + fuzzy

Date : 27 mars 2026
Branche : `feature/cross-token-search`

## Ce qu'on a construit

### Sibling links
Chaque token indexé stocke ses successeurs possibles (ordinal + gap_len) dans
une sibling table. Construit à l'indexation par le SfxCollector, stocké dans le
.sfx entre parent list et gapmap.

- `gap_len == 0` : tokens contigus (CamelCaseSplit) → cross-token viable
- `gap_len > 0` : séparateur (espace, etc.) → multi-token phrase search
- Coût : ~6 bytes/paire, ~30KB pour 10K tokens, ~9MB estimé pour 1M tokens (90K docs)

### Cross-token exact
`falling_walk(query)` → premier split → sibling chain O(1)/step → resolve → adjacency.
Multi-split gratuit : la chaîne traverse N tokens naturellement.
Byte continuity check : `byte_to == byte_from` élimine les faux positifs.

### Multi-token + cross-token unifié
`cross_token_resolve_for_multi()` résout chaque sous-token via falling_walk + sibling.
`MultiTokenPosting` avec span (positions occupées). Adjacency : `token_index + span`.
Pivot sur le sous-token le plus sélectif.

### Fuzzy cross-token
- Terminal fuzzy : `levenshtein_prefix_match(remainder, next_text, d)` sur le dernier token
- Left typo : `fuzzy_falling_walk` avec `fst_depth` (pas `max_prefix_len` !)
- Filtre : garder seulement les candidats avec sibling OU qui consomment toute la query (700→24)
- Non-last tokens : `fuzzy_walk_si0` pour les intermédiaires

### Highlight fix
Playground JS : `charIdx += (len === 4) ? 2 : 1` pour les surrogate pairs UTF-16.
Un seul emoji 🔧 décalait tous les highlights après lui.

## Leçons apprises (à retenir)

### Performance
1. **Pas de DFS sur tout le SFX FST en fuzzy** — O(FST_size) × DFA = 10s sur 862 docs
2. **Filtrer les candidats tôt** — 700 candidats fuzzy → 24 utiles (ont sibling ou consomment query)
3. **Pas de HashMap dans le hot path WASM** — Vec trié + partition_point
4. **eprintln! est catastrophique en WASM** — chaque appel traverse la frontière WASM→JS
5. **Le graph/worklist explose** — O(paths) pas O(unique_remainders). Les sibling links éliminent le problème
6. **fst_depth ≠ max_prefix_len** en fuzzy — fst_depth = bytes parcourus dans le FST (split point), max_prefix_len = bytes de query consommés (peut différer avec insertions/deletions)

### Correctness
7. **byte_to == byte_from** pour la continuité — élimine les splits parasites (r+ag3weaver)
8. **Le falling_walk est un superset** du resolve_suffix pour les non-last tokens cross-token
9. **Le cross-token doit aussi être essayé** quand le single-token trouve des résultats (fuzzy)
10. **Clamp `split_at = min(fst_depth, query.len())`** — fst_depth peut dépasser query.len() en fuzzy

### Architecture
11. **Sibling links = pré-calcul à l'indexation** — zéro coût au query time
12. **ord_to_term** via le term dict standard — zéro stockage additionnel
13. **La sibling table ne remplace PAS le GapMap** — le GapMap donne le contenu exact des gaps
14. **Le gap_dict est une fausse bonne idée** — les gaps sont arbitraires (binaire, garbage)

## Build commands

### Tests
```bash
cd /home/luciedefraiteur/LR_CodeRag/community-docs/packages/rag3db/extension/lucivy/ld-lucivy

# Tests ld-lucivy (1173 tests)
cargo test --lib -p ld-lucivy > /tmp/test.txt 2>&1; grep "test result" /tmp/test.txt

# Tests lucivy-core (89 tests)
cargo test --lib -p lucivy-core > /tmp/test_core.txt 2>&1; grep "test result" /tmp/test_core.txt

# Test spécifique avec output
cargo test --lib -p lucivy-core test_fuzzy_contains -- --nocapture > /tmp/test.txt 2>&1

# Tests suffix/cross-token seulement
cargo test --lib -p ld-lucivy suffix_contains > /tmp/test.txt 2>&1
```

### Build WASM
```bash
bash bindings/emscripten/build.sh > /tmp/wasm.txt 2>&1; echo "EXIT: $?"
# Output dans playground/pkg/lucivy.{js,wasm}
```

### Build Python binding
```bash
cd bindings/python && maturin develop --release > /tmp/py.txt 2>&1
```

### Générer le .luce
```bash
python playground/build_dataset.py > /tmp/luce.txt 2>&1
# Output : playground/dataset.luce (~28MB, 860 docs)
```

### Lancer le playground
```bash
node playground/serve.mjs
# → http://localhost:9877
```

### Diagnostic .luce (benchmark natif)
```bash
cargo test --lib -p lucivy-core test_diag_luce_cross_token -- --nocapture > /tmp/diag.txt 2>&1
# Montre : segments, timings, sibling counts
```

### Diagnostic fuzzy
```bash
cargo test --lib -p lucivy-core test_fuzzy_contains -- --nocapture > /tmp/fz.txt 2>&1
# Montre : tokens, siblings, falling_walk candidates, highlights
```

### Diagnostic highlight
```bash
cargo test --lib -p lucivy-core test_diag_highlight_rag3weaver -- --nocapture > /tmp/hl.txt 2>&1
# Vérifie 24 occurrences de "rag3weaver" dans le doc 11
```

### Test highlight JS (Node.js)
```bash
node playground/test_highlight_mapping.mjs
# Vérifie le byteToChar mapping avec les vrais offsets
```

### Bench 90K docs (Linux kernel)
```bash
# Index déjà construit dans :
# /home/luciedefraiteur/lucivy_bench_sharding/single/ (1 shard)
# /home/luciedefraiteur/lucivy_bench_sharding/round_robin/ (4 shards)

cd lucivy_core
cargo bench --bench bench_sharding > /tmp/bench.txt 2>&1

# Ground truth (37 checks)
cargo bench --bench bench_sharding -- ground_truth > /tmp/gt.txt 2>&1

# Score consistency (5/5 single vs 4-shard)
cargo bench --bench bench_sharding -- test_score_consistency > /tmp/scores.txt 2>&1
```

**IMPORTANT** : toujours rediriger vers fichier (`> /tmp/xxx.txt 2>&1`), jamais `| tail` ou `| head`.

## Fichiers clés

### Sibling table
- `src/suffix_fst/sibling_table.rs` — SiblingTableWriter/Reader, SiblingEntry
- `src/suffix_fst/collector.rs` — collecte paires sibling dans end_value()
- `src/suffix_fst/file.rs` — SfxFileWriter/Reader avec sibling data + fuzzy_falling_walk
- `src/indexer/sfx_merge.rs` — merge_sibling_links()
- `src/indexer/sfx_dag.rs` — MergeSiblingLinksNode

### Cross-token search
- `src/query/phrase_query/suffix_contains.rs` — cross_token_search_with_terms, cross_token_resolve_for_multi, MultiTokenPosting, levenshtein_prefix_match
- `src/query/phrase_query/suffix_contains_query.rs` — run_sfx_walk avec ord_to_term, suffix_contains_multi_token_impl_pub

### Playground
- `playground/index.html` — buildSnippets byteToChar mapping (surrogate pair fix)
- `playground/test_highlight_mapping.mjs` — test mapping Node.js
- `playground/build_dataset.py` — génère dataset.luce

### Tests diagnostiques
- `lucivy_core/src/handle.rs` — test_fuzzy_contains, test_diag_luce_cross_token, test_diag_highlight_rag3weaver, test_contains_flexible_positions

## Pistes d'optimisation fuzzy (doc 03)

1. ✅ Filtrer candidats par sibling présence (700→24) — implémenté
2. Fuzzy walk sur term dict au lieu du SFX (~10× plus petit)
3. Fuzzy sur partition SI=0 du SFX seulement
4. Sibling-first (itérer tokens avec siblings, Levenshtein CPU pur)
5. Early termination dans le DFS
6. **Trigrams fantômes** : utiliser les suffixes SI comme trigrams naturels, intersection d'ordinals, verify Levenshtein. O(query_len) lookups au lieu de O(FST_size) DFS.

## Prochaines étapes

1. **Regex cross-token** via sibling links
2. **Bench 90K docs** avec LUCIVY_VERIFY
3. **Optimisation fuzzy** (trigrams fantômes ou term dict walk)
4. **Release** : merge feature/cross-token-search → main
