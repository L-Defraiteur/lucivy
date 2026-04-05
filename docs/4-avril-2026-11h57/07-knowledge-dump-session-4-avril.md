# 07 — Knowledge dump session 4 avril 2026

---

## Ce qui a été fait

### 1. Fix greedy chain builder — mots répétés (doc 02)
Query "Build rag3weaver... WASM builds Native" d=1 retournait 0 résultats car "WASM" apparaît 2× dans la query. Les trigrams "was"/"asm" ont deux tri_idx → le greedy chain sautait au mauvais et cassait la chaîne.

**Fix** : group-by byte_from. Pour chaque bf, on pick le plus petit tri_idx qui continue la chaîne. Si aucun ne continue → check chain + restart.

### 2. Separator-agnostic fuzzy (doc 03)
Le span_diff global comparait les byte spans bruts incluant les séparateurs. Un séparateur "---\n#" (7 bytes) dans le contenu vs " " (1 byte) dans la query → rejeté.

**Fix** :
- `generate_ngrams` retourne `word_ids` — chaque trigram sait de quel mot il vient
- `check_chain` : span_diff uniquement intra-word. Cross-word = tolérance libre (×64 bytes)
- DFA concat : gap normalisé → single space (au lieu de vrais gap bytes)
- DFA query : normalisée (`normalize_query_separators` : runs non-alpha → single space)
- `validate_path` : paramètre `normalize_gaps: bool` (true pour fuzzy, false pour regex)
- Empty gaps (CamelCase split) : pas de space inséré (tokens restent contigus)

### 3. Dual-anchor DFA window
Le DFA window était centré uniquement sur le premier trigram. Si la chaîne mélange des trigrams de positions éloignées (tri 0 de "rag3db", tri 4-7 de "rak3weaver"), le match réel est hors window.

**Fix** :
- `intersect_trigrams_with_threshold` retourne `last_bf` + `last_si` en plus
- DFA window couvre [min(anchor_first, anchor_last), max(...)] — les deux extrêmes
- Calcul symétrique : anchor = concat_start + si - query_positions[tri_idx]

### 4. Resolve all trigrams pour doc filter
Le doc filter ne résolvait que les `threshold` trigrams les plus rares. Si ceux-ci sont tous "cassés" par l'edit, le doc cible est exclu.

**Fix** : `filter_count = exact_grams.len()` — résout tous les trigrams sans filter.
Coût perf acceptable (+15ms sur queries normales).
SAUF pour les bigrams ultra-communs ("3db_val") → 800ms+ → problème ouvert.

### 5. Multi-match DFA
Le DFA walk ne gardait qu'un seul match (le meilleur) par candidat. Pour les queries fuzzy comme "rak3weaver" d=1, chaque candidat = 1 highlight → tf=1 → mauvais scoring.

**Fix** : collecter TOUS les non-overlapping DFA matches dans le concat window. Chaque match = un highlight séparé → tf correct pour BM25.

### 6. Ground truth test hash collision
`file_content_map: HashMap<u64, usize>` perdait les fichiers avec contenu identique (README duplicates). Faux MISSED dans le test.

**Fix** : `HashMap<u64, Vec<usize>>` pour mapper un hash à plusieurs file indices.

### 7. Content gap tolerance
Pour les queries single-word cross-token ("rag3weaver" matchant "rag3" + "weaver"), le gap CamelCase gonflait le span_diff au-delà de distance.

**Fix** : détecter les content gaps (bf_diff > qp_diff + 1 entre paires consécutives) et ajouter ×64 bytes de tolérance par gap.

### 8. Regex multi-literal split + continuous DFA walk
Deux bugs pré-existants dans le regex cross-token :
- Les literals comme "db i" contenaient des espaces (séparateurs SFX) → fst_candidates trouvait rien
- La validation gap-by-gap redémarrait le DFA depuis start_state à chaque gap → impossible de valider "rag.db" intra-token

**Fix** :
- `walk_hir` pour `HirKind::Literal` : split sur non-alpha, chaque segment = literal séparé
- Validation multi-literal : remplacé le gap-by-gap par un seul DFA walk continu (feed premier token depuis offset, puis validate_path forward)

### 9. Disjunction assert + mmap test
- `debug_assert!(min_match > 1)` → `assert!` (sinon pas de panic en release)
- `sfx_extra_per_segment` = 4 → 10 (pour tous les fichiers SFX ajoutés)

### 10. Monotonicity test
Nouveau test `test_fuzzy_monotonicity` avec 2 suites :
- Real repo : 9 queries edge-case (cross-token, mid-token, compound names, underscores)
- SKU catalog synthétique : 50 SKUs × 200 docs, exact + fuzzy + typo

3 queries underscore violent encore la monotonie (problème ouvert → Plan 06).

---

## Tests et scripts

### Test playground repro (LE test de référence)
```bash
# Clone le repo si pas déjà fait
git clone --depth 1 https://github.com/L-Defraiteur/rag3db.git /tmp/test_rag3db_clone

# Lancer (release mode, 4300 fichiers)
RAG3DB_ROOT=/tmp/test_rag3db_clone cargo test -p lucivy-core \
  --test test_playground_repro --release -- --nocapture > /tmp/output.txt 2>&1
```
Reproduit exactement le flow WASM : mêmes TEXT_EXTENSIONS, isBinaryContent, MAX_FILE_SIZE=100KB, COMMIT_EVERY=200, pas de drain_merges, skip symlinks. Inclut check de monotonie d=0 ⊆ d=1.

### Test ground truth fuzzy
```bash
cargo test -p lucivy-core --test test_fuzzy_ground_truth --release -- --nocapture > /tmp/output.txt 2>&1
```
Indexe les fichiers du repo ld-lucivy (~940 fichiers). UN commit + drain_merges → 7 segments. Ground truth brute-force : CamelCaseSplit + lowercase + semi-global Levenshtein. Vérifie recall, precision, ET chaque highlight.

### Test monotonie + SKU
```bash
RAG3DB_ROOT=/tmp/test_rag3db_clone cargo test -p lucivy-core \
  --test test_fuzzy_monotonicity --release -- --nocapture > /tmp/output.txt 2>&1
```
Deux suites : real repo edge-cases + SKU synthétique. Le SKU test passe. 3 queries underscore échouent (Plan 06).

### Lib tests
```bash
cargo test --lib --release -q > /tmp/output.txt 2>&1
```
1203 passed, 0 failed (+ 1 flaky proptest), 7 ignored. Les 5 failures pré-existants sont fixés.

### Build WASM
```bash
cd packages/rag3db/extension/lucivy/ld-lucivy
bash bindings/emscripten/build.sh
```
Copie automatiquement dans playground/pkg/. Serveur : `node playground/serve.mjs` → http://localhost:9877

### IMPORTANT
- **Toujours rediriger stdout+stderr vers un fichier** : `> /tmp/fichier.txt 2>&1`
- **Jamais `| tail`** — ça coupe la sortie des tests
- Le serveur playground tourne en background — reload suffit après rebuild WASM

---

## Mécanismes en profondeur

### Falling walk (file.rs:388)

Le falling_walk parcourt le SFX FST byte par byte avec la query. À chaque noeud final, vérifie si le prefix consomme exactement la fin d'un token parent (si + prefix_len == token_len). C'est le "fall" : le point où le prefix tombe hors du token.

```
query = "rag3weaver"
FST walk: r→a→g→3→w→e→a→v→e→r

À chaque noeud final du FST, decode les parents.
Noeud final après "rag3" (4 bytes) → parent "rag3" (si=0, len=4)
  → si + 4 == 4 ✓ → SplitCandidate { prefix_len: 4, parent: rag3 }
  → remainder: "weaver" → chercher chez les siblings
```

Le `fuzzy_falling_walk` (file.rs:440) fait pareil mais en DFS guidé par un DFA Levenshtein. Le DFA tolère les edits. `fst_depth` = position dans le SFX (peut diverger de la position query à cause des edits).

**Coût** : O(2L) pour exact (2 partitions SI0/SI_REST × L bytes), O(2 × branching^L) pour fuzzy DFS (borné par le DFA qui prune les branches mortes).

### Sibling table (sibling_table.rs)

Stocke pour chaque ordinal les tokens qui le suivent immédiatement dans le contenu.

```rust
struct SiblingEntry {
    next_ordinal: u32,  // ordinal du token suivant
    gap_len: u16,       // longueur du gap entre les deux (0 = contiguous/CamelCase)
}
```

- `contiguous_siblings(ord)` : retourne les ordinals avec gap_len == 0 (CamelCase split, pas de séparateur)
- `siblings(ord)` : retourne TOUS les siblings (avec gap_len quelconque)

**Usage dans cross_token_falling_walk** : après le falling_walk, pour chaque SplitCandidate, on fait un DFS sur les contiguous_siblings. Pour chaque sibling, on regarde si son texte (via `ord_to_term`) matche le remainder de la query :
- `text == remainder` → match terminal
- `text.starts_with(remainder)` → le token couvre le reste → terminal
- `remainder.starts_with(text)` → consommation partielle → continue DFS

La sibling table est construite à l'indexation par le SfxCollector. Chaque token voit ses voisins dans le document. OrMergeWithRemap au merge des segments.

### Pipeline fuzzy actuel (ce qui marche + ce qui reste)

```
fuzzy_contains_via_trigram:
  1. generate_ngrams → bigrams/trigrams + positions + word_ids
  2. fst_candidates + cross_token_falling_walk par ngram (Phase A, pas de resolve)
  3. Selectivity sort → resolve rarest first (Phase B)
  4. intersect_trigrams_with_threshold → chains → candidates
  5. DFA walk par candidate → highlights

Problèmes restants (Plan 06) :
  - Bigrams ultra-communs → trop de candidates → DFA explosion
  - Pas de vérification d'adjacence par siblings dans intersect
  - Monotonie violée pour queries multi-mots avec underscore
```

### Plan 06 : ngram checkpoint + sibling verification

Remplace l'étape 4-5 par :
1. Pour chaque ngram, `fst_candidates` donne des checkpoints : (ordinal, si, token_len)
2. Construire des chaînes cohérentes en vérifiant :
   - Même token (ordinal identique, si+1)
   - Cross-token (siblings contiguous)
   - Cross-word (siblings avec gap)
3. Resolve postings SEULEMENT sur les chaînes validées
4. Pas de DFA walk — validation intégrée dans les checkpoints

---

## État des commits

```
b0f6f5e fix: fuzzy multi-token separator-agnostic + greedy chain group-by
72f040e fix: content gap tolerance in span_diff for cross-token CamelCase matches
fbf2cec fix: ground truth test hash collision + clean up diagnostics
af0aa50 fix: dual-anchor DFA window + resolve all trigrams for doc filter
fc0fefc fix: add normalize_gaps param to validate_path for regex compatibility
4da212e fix: regex multi-literal split on non-alpha + continuous DFA walk
6cea39f fix: disjunction debug_assert → assert + mmap test sfx file count
40ef0b1 fix: regex literal split on non-alpha + continuous DFA walk + misc
7270576 fix: collect all DFA matches per candidate for correct fuzzy tf scoring
d142e9b test: add monotonicity check d=0 ⊆ d=1 in playground repro
4a88ebd fix: include_gaps based on normalized query + add monotonicity tests
9c8c681 docs: plan fuzzy multi-segment walk via siblings
b80f42c docs: plan 06 ngram checkpoint + sibling adjacency verification
```

### Timings actuels (4307 docs, release, pas de drain_merges)
| Query | Temps |
|-------|-------|
| rag3weaver d=0 | 7ms |
| rag3weaver d=1 | 35ms |
| rak3weaver d=1 | 59ms |
| rag3db d=1 | 81ms |
| Multi-token longue d=1 | 196ms |
| 3db_val d=1 | 877ms ← problème perf ouvert |

### Résultats tests
| Suite | Résultat |
|-------|----------|
| Lib tests | 1203 passed, 0 failed, 7 ignored |
| Ground truth | 80/80 + 79/79 recall, PASS |
| Playground repro | PASS, monotonie OK |
| Monotonie + SKU | SKU PASS, 3 underscore queries FAIL |

---

## Problèmes ouverts (pour prochaine session)

1. **3 queries underscore violant la monotonie** : "3db_val", "rag3db_value_destroy", "alue_dest". Cause : bigrams trop communs → faux candidats → DFA explosion. Fix : Plan 06 (ngram checkpoint + sibling verification).

2. **Perf "3db_val" d=1 = 877ms** : 21000+ DFA walks sur des bigrams ultra-communs. Fix : Plan 06 élimine les faux candidats avant le resolve.

3. **WASM build** : le dernier build WASM inclut le multi-match DFA fix mais PAS les derniers fixes (position gap, empty gap, etc.). Rebuilder après Plan 06.

---

## Manière de travailler de l'utilisateur

- Veut des **résultats exacts** — pas de tradeoff, pas de "ça marche pour 99% des cas"
- Préfère **comprendre avant de coder** — diagnostiquer d'abord, discuter, puis implémenter
- Ne veut pas de **scotch** — si une brique n'a pas l'info nécessaire, la remonter proprement
- Critique constructive : "c'est quoi le plus correct ?" plutôt que "le plus simple"
- Commits fréquents comme checkpoints
- Docs plans dans des dossiers horodatés
- **Ne pas mentionner Claude** dans les commits
- **lucivy est sa propre lib** — ne jamais dire "fork de Tantivy"
