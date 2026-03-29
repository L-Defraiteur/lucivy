# 13 — Knowledge dump : session 28-29 mars 2026

## Ce qui a été fait

### 1. Fix bug cross-token fuzzy DFA validation
- Le DFA était feedé avec des gap bytes (espaces entre tokens) pour les queries
  single-word → d=2 au lieu de d=1 pour "rak3weaver" matchant "rag3" + " " + "weaver"
- Fix : `include_gaps = query_text.contains(' ')` — skip gaps pour single-word queries
- Le highlight utilisait `adj_bf = first_bf - query_positions[first_tri_idx]` qui
  mélange bytes content (avec séparateurs) et bytes query (sans) → décalé
- Fix : token span tracking + content byte walk depuis ancre connue

### 2. Multi-match per doc (fuzzy + regex)
- `intersect_trigrams_with_threshold` : émettait 1 chain par doc → émet TOUTES les chains valides
- `intersect_literals_ordered` (regex multi-literal) : `break 'outer` → supprimé
- Cap à MAX_CHAINS_PER_DOC=20 pour éviter l'explosion sur les bigrams communs
- Threshold min=2 pour éviter le flooding single-bigram

### 3. Bug critique : ordinal mismatch SFX vs term dict
- **Trouvé** : `ord_to_term(sfx_ordinal)` utilisait le term dict tantivy dont les
  ordinals ≠ ordinals SFX. Le CamelCaseSplitFilter produit des tokens différents
  du SimpleTokenizer → les ordinals ne correspondent pas.
- **Diagnostiqué** via test natif `test_merge_contains` : 57 docs contiennent
  "rag3weaver" mais seulement 6-7 résultats retournés.
- **Fix** : nouveau fichier `.termtexts` — lookup O(1) ordinal SFX → texte.
  Les queries utilisent maintenant `TermTextsReader.text(ord)` avec fallback
  sur le term dict pour les vieux index.

### 4. Bug critique : sibling chain greedy match
- **Trouvé** : `contiguous_siblings` retournait les siblings triés par ordinal
  (alphabétique). Le code prenait le PREMIER match partial : "w" matchait
  avant "weaver" via `rem.starts_with("w")`.
- **Fix** : DFS worklist qui explore TOUTES les branches de sibling matching.
  Chaque sibling qui matche (exact, terminal, ou partial) crée une branche.

### 5. SfxIndexFile abstraction + registry
- Trait `SfxIndexFile` : `id()`, `extension()`, `build()`, `merge()`
- `SfxBuildContext` / `SfxMergeContext` avec token_texts, postings, doc_mapping, etc.
- Registry `all_indexes()` : ajouter un index = 1 ligne
- Implémentations : SfxPostIndex, PosMapIndex, ByteMapIndex, GapMapIndex,
  SiblingIndex, TermTextsIndex — TOUS passent par le registry
- segment_writer : boucle sur registry au lieu de code dupliqué
- segment_reader : `sfx_index_file(id, field)` accessor générique
- segment_component : `CustomSfxIndex` variante pour GC protection

### 6. Propagation sfx_field_ids dans le merger
- Le `segment_updater` ne propageait PAS les sfx_field_ids des segments sources
  vers le segment mergé → posmap/bytemap/termtexts invisibles après merge
- Fix dans `segment_updater.rs` : collect + dedup des sfx_field_ids des sources
- Switch de `merge_sfx_deferred` (FST vide) vers `merge_sfx_legacy` (full rebuild)
- Ajout de sibling merge dans le legacy path

### 7. Split .sfx planifié (design doc 11)
- .sfx garde FST + parent lists
- .gapmap → fichier séparé (via registry)
- .sibling → fichier séparé (via registry)
- GapMapIndex et SiblingIndex implémentés et dans le registry
- .sfx garde encore gapmap/sibling dedans pour backward compat (migration query à faire)

## Scripts et commandes utiles

### Test natif merge + contains
```bash
cargo test -p lucivy-core --test test_merge_contains -- --nocapture > /tmp/merge_diag.txt 2>&1
# Indexe les fichiers du repo, vérifie contains/fuzzy results
# PAS besoin de .luce ni Python binding
```

### Test natif sur .luce existant
```bash
cargo test -p lucivy-core --test test_luce_roundtrip -- --nocapture > /tmp/luce_test.txt 2>&1
# Importe playground/dataset.luce, teste contains/fuzzy/regex
```

### Rebuild .luce
```bash
# Rebuild Python binding (nécessaire si code Rust changé)
cd bindings/python && maturin develop --release
# Rebuild .luce
cd ../.. && python3 playground/build_dataset.py
# Le .luce est dans playground/dataset.luce
```

### Build WASM
```bash
bash bindings/emscripten/build.sh
# Copie dans playground/pkg/lucivy.{js,wasm}
# Servir : node playground/serve.mjs → http://localhost:9877/
```

### Vérifier présence des index files dans .luce
```python
python3 -c "
import re
data = open('playground/dataset.luce', 'rb').read()
for ext in ['sfxpost', 'posmap', 'bytemap', 'termtexts', 'gapmap', 'sibling']:
    count = len(re.findall(rb'[0-9a-f]{32}\.\d+\.' + ext.encode(), data))
    print(f'.{ext}: {count} files')
"
```

### Test rapide si un token existe dans le term dict
```rust
// Dans un test natif :
if let Ok(inv_idx) = reader.inverted_index(field) {
    let has = inv_idx.terms().get(b"rag3weaver").ok().flatten().is_some();
    eprintln!("term 'rag3weaver' in dict: {}", has);
}
```

## Pièges et leçons

### 1. Le term dict tantivy ≠ ordinals SFX
**LE piège majeur de la session.** `ord_to_term()` du term dict retourne le
mauvais token quand on passe un ordinal SFX. Toujours utiliser `.termtexts`
via `TermTextsReader` pour résoudre les ordinals SFX.

### 2. CamelCaseSplitFilter split digit→letter
"rag3weaver" → "rag3" + "weaver". Le SimpleTokenizer NE split PAS ça (il
split sur non-alphanumeric). C'est le CamelCaseSplitFilter dans le
`raw_code` tokenizer qui le fait. Le term dict n'a PAS "rag3weaver" comme
token — il a "rag3" et "weaver" séparément.

### 3. Le tokenizer de la query ≠ tokenizer de l'index
`tokenize_query()` dans `suffix_contains_query.rs` utilise SimpleTokenizer +
LowerCaser (PAS de CamelCaseSplit). Donc la query "rag3weaver" reste 1 token.
Le cross-token search via falling_walk + sibling links comble la différence.

### 4. Le merge NE merge PAS toujours
`drain_merges()` ne trigger un merge que si la merge policy le décide. Avec
888 docs, le merge NE se produit PAS (7 segments restent). Le debug print
`[MERGE]` ne sort pas = pas de merge, pas un bug.

### 5. Python binding toujours call drain_merges
Le `commit()` du Python binding appelle `writer.drain_merges()`. Le WASM
ne le fait PAS. C'est pourquoi les résultats peuvent différer.

### 6. eprintln! ne sort pas dans les tests Python
Les `eprintln!` du code Rust ne sont PAS visibles quand on lance via Python.
Il faut tester en natif (cargo test) pour voir les debug prints.

### 7. Le .luce snapshot lit TOUS les fichiers du répertoire
`read_directory_files()` fait un `read_dir` et inclut tout. Si un fichier
d'index est sur disque, il est dans le .luce. Si il n'est PAS sur disque
(ex: posmap non écrit car builder pas rebuild), il n'est pas dans le .luce.

### 8. sfx_field_ids dans le SegmentMeta
Sans `sfx_field_ids`, le segment_reader ne charge aucun fichier SFX
(.sfx, .sfxpost, .posmap, etc.). C'est propagé par le segment_writer
(via `with_sfx_field_ids()`) et par le segment_updater (pour les merges).
Oublier = fichiers invisibles = search retourne 0.

### 9. Sibling chain greedy = bug subtil
Le `contiguous_siblings()` retourne les ordinals triés. L'itération
séquentielle prend le premier match. Avec des tokens courts ("w") avant
des tokens longs ("weaver"), le court matche en partial et la chain meurt.
Fix : DFS worklist qui explore toutes les branches.

## Branches et état

| Branche | Contenu | État |
|---------|---------|------|
| `feature/fuzzy-via-literal-resolve` | Tout ce qui est décrit ici | active, HEAD |
| `feature/regex-contains-literal` | BM25 regex prescan (session précédente) | poussé, à merger |
| `feature/merge-incremental-sfx` | N-way merge, contiguous buffer (session précédente) | poussé |

## Fichiers clés modifiés cette session

### Nouveau
- `src/suffix_fst/index_registry.rs` — SfxIndexFile trait + registry
- `src/suffix_fst/termtexts.rs` — TermTextsWriter/Reader + TermTextsIndex
- `lucivy_core/tests/test_merge_contains.rs` — test natif merge + contains

### Modifié
- `src/suffix_fst/collector.rs` — SfxBuildOutput.registry_files + SfxBuildContext
- `src/suffix_fst/posmap.rs` — PosMapIndex impl SfxIndexFile
- `src/suffix_fst/bytemap.rs` — ByteMapIndex impl SfxIndexFile
- `src/suffix_fst/sfxpost_v2.rs` — SfxPostIndex impl SfxIndexFile
- `src/suffix_fst/gapmap.rs` — GapMapIndex impl SfxIndexFile
- `src/suffix_fst/sibling_table.rs` — SiblingIndex impl SfxIndexFile
- `src/suffix_fst/mod.rs` — exports termtexts, index_registry
- `src/indexer/segment_writer.rs` — registry loop au lieu de code dupliqué
- `src/indexer/segment_serializer.rs` — write_custom_index()
- `src/indexer/segment_updater.rs` — propagate sfx_field_ids dans merge
- `src/indexer/merger.rs` — merge_sfx_legacy avec sibling + posmap/bytemap
- `src/indexer/sfx_merge.rs` — write_sfx() accepte sibling_data
- `src/index/segment_component.rs` — CustomSfxIndex variante
- `src/index/segment_reader.rs` — registry loading + sfx_index_file() accessor
- `src/query/phrase_query/suffix_contains.rs` — DFS sibling chain + debug logs
- `src/query/phrase_query/suffix_contains_query.rs` — TermTexts migration
- `src/query/phrase_query/regex_continuation_query.rs` — TermTexts migration + fuzzy fixes
- `src/query/phrase_query/literal_resolve.rs` — multi-match per doc

### Docs
- `07-design-fuzzy-highlight-token-mapping.md`
- `08-bug-ordinal-mismatch-sfx-vs-termdict.md`
- `09-inventory-sfx-index-files-and-term-texts-plan.md`
- `10-design-sfx-index-abstraction.md`
- `11-design-split-sfx-into-3-files.md`
- `12-fuzzy-highlight-bugs-and-test-plan.md`
- `13-knowledge-dump-session-29-mars.md` (ce fichier)
- `arsenal.md` mis à jour

## Prochaines étapes

1. **Test ground truth fuzzy** (doc 12) — valider recall/precision/highlights
2. **Fix bug highlight fuzzy** — les highlights cross-token sont encore décalés
3. **Migrer les accès gapmap/sibling** — les queries lisent encore via sfx_reader,
   migrer vers les fichiers séparés
4. **Enlever gapmap/sibling du .sfx** — ne plus composer dans SfxFileWriter
5. **IndexFeature + check_features** — crash explicite si feature manquante
6. **Supprimer les debug eprintln** — [fuzzy-debug], [cross-token-diag], etc.
7. **Bench perf** — re-mesurer après toutes les modifications
8. **Merger l'ancien code** — les 3 branches feature/ sont à merger
