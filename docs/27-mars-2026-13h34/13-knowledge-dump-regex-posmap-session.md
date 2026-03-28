# Doc 13 — Knowledge dump : session regex + PosMap + ByteBitmap

Date : 28 mars 2026
Branche : `feature/regex-contains-literal`
Base : `feature/cross-token-search` (commit 82441db)

## Ce qu'on a construit

### Regex contains via literal extraction
Le regex ne scanne PLUS le SFX FST entier. Il extrait les littéraux du pattern,
les résout via le même code que le contains exact (cross-token aware via sibling
chain), puis valide le DFA entre les positions connues.

Architecture finale :
```
1. extract_all_literals(pattern) → ["rag3", "ver"]
2. find_literal(sfx_reader, literal, resolver, ord_to_term)
     → suffix_contains_single_token_with_terms (réutilise contains exact)
     → cross-token via falling_walk + sibling chain
     → Vec<LiteralMatch { doc_id, position, byte_from, byte_to }>
3. Si multi-literal : intersect_literals_ordered + position ordering (byte offsets)
4. validate_path via PosMap : lire ordinals entre positions, feeder gap+text au DFA
5. Single-literal : feed literal bytes au DFA, puis PosMap walk cross-token
```

### PosMap — position-to-ordinal reverse map
Fichier `.posmap` par segment. Pour chaque (doc_id, position) → ordinal O(1).
L'inverse du posting index. Permet de lire les tokens entre deux positions connues
sans explorer les sibling links.

- Format : `[PMAP magic][num_docs][offset table][ordinals per doc]`
- Taille : ~4 bytes par (doc, position). 872 docs ≈ 344 KB.
- Construit par SfxCollector pendant l'indexation
- Reconstruit pendant le merge (même boucle que sfxpost)

### ByteBitmap — byte presence per ordinal
Fichier `.bytemap` par segment. 256 bits (32 bytes) par ordinal : quels bytes
apparaissent dans le texte du token.

- Format : `[BMAP magic][num_ordinals][32 bytes × num_ordinals]`
- Taille : ~160 KB pour 5K ordinals
- Construit par SfxCollector
- Reconstruit pendant le merge
- **Pas encore câblé à la recherche** — futur pré-filtre pour `[a-z]+` etc.

### SfxBuildOutput — abstraction pour les fichiers d'index
```rust
pub struct SfxBuildOutput {
    pub sfx: Vec<u8>,       // .sfx
    pub sfxpost: Vec<u8>,   // .sfxpost
    pub posmap: Vec<u8>,    // .posmap (NOUVEAU)
    pub bytemap: Vec<u8>,   // .bytemap (NOUVEAU)
}
```
Ajouter un nouveau type de fichier d'index = ajouter un champ ici +
un variant dans `SegmentComponent` + c'est tout.

### literal_resolve.rs — briques réutilisables
Module `src/query/phrase_query/literal_resolve.rs` :
- `find_literal()` : résout un littéral via contains exact (cross-token aware)
- `intersect_literals_ordered()` : intersection multi-littérale avec position ordering
- `validate_path()` : DFA validation entre deux positions via PosMap (early return on match)
- `group_by_doc()` : grouper les matches par doc_id
- `dfa_accepts_anything()` : détecter les DFA `.*` (tout accepte)

### SegmentComponent enum — source de vérité
On a ajouté `PosMap { field_id }` et `ByteMap { field_id }` à l'enum.
**Critique** : sans ça, les fichiers étaient créés puis supprimés par le GC car non reconnus.
`all_components()`, `is_per_field()`, `extension()` tous mis à jour.

### Merger — reconstruction PosMap/ByteBitmap
Le merger reconstruit PosMap et ByteBitmap pendant le merge (même boucle que sfxpost,
zéro I/O supplémentaire). Le path legacy écrit des placeholders vides.

## Leçons apprises

### Performance
1. **Le scan FST (search_continuation / continuation_score) est le vrai bottleneck** —
   O(FST_size × DFA) = 16s pour `rag3.*ver` sur 862 docs. Les sibling links accélèrent
   Walk 2 mais Walk 1 reste lent.

2. **`prefix_walk` est O(results), pas O(FST_size)** — range scan ciblé. Idéal pour les
   littéraux longs. Pour `shard[a-z]+` : 11 entries en ~25µs natif.

3. **Phase 3c (gap>0 sibling walk) explose avec `.*`** — le DFA ne prune jamais.
   Même 26 docs × 64 depth × N siblings = 16 secondes. Le PosMap élimine ça : O(distance).

4. **Les gap bytes vides (tokens contigus) doivent skip Phase 3c** — sinon travail
   redondant avec Phase 2/3b (gap=0 sibling chain).

5. **`find_literal` via contains exact est le bon choix** — réutilise falling_walk +
   sibling chain, gère le cross-token (CamelCaseSplit), resolve-last.

6. **`extract_all_literals` doit traiter l'espace comme séparateur** — les tokens ne
   contiennent jamais d'espace, donc `"3db i"` n'est pas un littéral valide.

7. **DFA compilation = ~4ms en WASM par segment** — coût fixe non négligeable avec
   6 segments. Piste : cacher le DFA compilé et le partager entre segments.

8. **`validate_path` doit faire early return on DFA accept** — sinon les bytes après
   le match tuent le DFA (ex: `3db i.` matche "3db is" mais meurt sur " cool" après).

### Architecture
9. **Chaque nouveau fichier d'index nécessite 5 points de modification** :
   SfxCollector (build) → SfxBuildOutput (struct) → SegmentSerializer (write) →
   SegmentReader (load) → SegmentComponent (GC). Manquer un seul = données perdues.

10. **Le contains exact est la brique fondamentale** — tout converge vers
    `suffix_contains_single_token_with_terms`. Le regex, le fuzzy, le multi-token
    l'utilisent tous (directement ou via les mêmes primitives).

11. **L'intersection multi-littérale est un game changer** — `rag3.*ver` avec 120 docs
    contenant "rag3" et 5 contenant "ver" → intersection = 5 docs. Sans intersection = 120 × 64 walks.

12. **has_doc O(log n) et resolve_filtered sont des outils puissants** — évitent de
    décoder les payload VInt de tous les postings quand on veut juste savoir "ce doc a-t-il ce token?".

13. **doc_freq O(1) pour choisir le littéral le plus sélectif** — le header sfxpost v2
    contient `num_unique_docs` par ordinal, lisible sans decoder.

### Bugs fixés
14. **SegmentComponent enum incomplet → fichiers supprimés par GC** — PosMap et ByteBitmap
    étaient écrits puis effacés car pas dans `all_components()`.

15. **`extract_longest_literal` prenait le mauvais littéral** — pour `rag.*ver` il prenait
    "ver" (dernier parmi les égaux) au lieu de "rag" (premier, nécessaire pour le DFA).
    Fix : `pick_best_literal` préfère le premier en taille égale.

16. **Non-prefix literal : le DFA meurt sur le token complet** — pour `.*weaver` avec
    littéral "weaver", feeder "rag3weaver" au DFA `.*weaver` marchait, mais
    `rag.*ver` avec littéral "ver" feedait "weaver" au DFA qui attendait "rag" d'abord.
    Fix : feeder le texte complet du token via ord_to_term pour les non-prefix literals.

## Scoring — problème ouvert

Tous les résultats regex ont un score de 1.0 (ConstScorer). Le BM25 n'est pas câblé.
Design doc 12 propose :
- **Phase 1** : BM25 per-segment (highlights → doc_tf → Bm25Weight) — 10 lignes
- **Phase 2** : Global df via prescan pour cross-shard correct

## Fichiers clés

### Nouveau module
- `src/query/phrase_query/literal_resolve.rs` — find_literal, intersect, validate_path

### Nouveaux fichiers d'index
- `src/suffix_fst/posmap.rs` — PosMapWriter/Reader
- `src/suffix_fst/bytemap.rs` — ByteBitmapWriter/Reader

### Fichiers modifiés
- `src/query/phrase_query/regex_continuation_query.rs` — réécriture complète du flow regex
- `src/suffix_fst/collector.rs` — SfxBuildOutput, collecte posmap + bytemap
- `src/suffix_fst/mod.rs` — exports posmap, bytemap
- `src/index/segment_component.rs` — PosMap + ByteMap dans l'enum
- `src/index/segment_reader.rs` — chargement .posmap + .bytemap
- `src/indexer/segment_writer.rs` — écriture .posmap + .bytemap
- `src/indexer/segment_serializer.rs` — write_posmap, write_bytemap
- `src/indexer/merger.rs` — reconstruction posmap/bytemap pendant merge

### Benchmark / test
- `playground/test_regex_bench.mjs` — benchmark Playwright avec timers Rust
- `playground/test_regex_perf.mjs` — benchmark Node.js WASM

### Docs
- `docs/arsenal.md` — inventaire complet de toutes les structures d'indexation
- `docs/27-mars-2026-13h34/07` — design regex via falling_walk + literal extraction
- `docs/27-mars-2026-13h34/08` — findings optim regex/contains/fuzzy
- `docs/27-mars-2026-13h34/09` — design PosMap + ByteBitmap
- `docs/27-mars-2026-13h34/10` — plan regex via contains exact reuse
- `docs/27-mars-2026-13h34/11` — rapport de session
- `docs/27-mars-2026-13h34/12` — design BM25 scoring regex

## Commits (branche `feature/regex-contains-literal`)

```
f9c5103 feat: regex contains via literal extraction + multi-literal intersection
8397898 feat: add PosMap + ByteBitmap index files, SfxBuildOutput abstraction
3132a34 feat: wire PosMap into regex search — O(distance) cross-token validation
9e7bb9e fix: register PosMap + ByteMap in SegmentComponent enum
4ef42fc feat: rewrite regex via literal_resolve — reuse exact contains logic
d7f805b fix: rebuild PosMap + ByteBitmap during segment merge
```

## Build commands

```bash
cd /home/luciedefraiteur/LR_CodeRag/community-docs/packages/rag3db/extension/lucivy/ld-lucivy

# Tests (1181)
cargo test --lib -p ld-lucivy > /tmp/test.txt 2>&1; grep "test result" /tmp/test.txt

# Tests regex seulement
cargo test --lib -p ld-lucivy regex_continuation > /tmp/test_regex.txt 2>&1

# Build WASM
bash bindings/emscripten/build.sh > /tmp/wasm.txt 2>&1; echo "EXIT: $?"

# Build Python binding
cd bindings/python
unset CONDA_PREFIX && source .venv/bin/activate
touch ../../src/indexer/segment_writer.rs  # force rebuild si nécessaire
maturin develop --release

# Générer le .luce (avec posmap + bytemap)
source .venv/bin/activate && python3 ../../playground/build_dataset.py

# Lancer le playground
node playground/serve.mjs  # → http://localhost:9877

# Benchmark Playwright
node playground/test_regex_bench.mjs
```

## Prochaines étapes

1. **BM25 scoring regex** — remplacer ConstScorer par SuffixContainsScorer (doc 12)
2. **ByteBitmap câblé à la recherche** — pré-filtre `[a-z]+` etc. dans validate_path
3. **Cache DFA compilé** — partager entre segments (éviter 6× dfa_compile)
4. **MIN_LITERAL_LEN = 2** — supporter `[a-z]+ment` (littéral "me" ou "nt")
5. **sfx_prescan_params pour regex** — prescan parallèle dans le search DAG
6. **Cleanup legacy merge** — supprimer merge_sfx_legacy
7. **Bench 90K docs** — vérifier que tout scale
