# Recap Session 6 — 24 mai 2026

## Résultat

**Ground truth : 15/15 (500 docs, 15 queries). 0 FP, 0 FN.**

- Strict : 10/10 (inchangé)
- Relaxed : 5/5 (était 3/5)
  - `uint64_t` relaxed : 23/23 (était 18/23, +5 FN résolus)
  - `TableFunction` relaxed : 5/5 (était 4/5, +1 FN résolu)

## Problèmes résolus

### 1. WordSfxPost cross-join (index-time)

**Bug** : la construction du WordSfxPost pour les mots multi-chunk
utilisait `content_key_to_interns` pour trouver les postings du premier
et dernier chunk. Ce mapping agrège TOUS les chunks avec le même contenu,
y compris ceux de mots différents ("funct" de "function" ET "functions").
Résultat : produit cartésien → entries avec spans de milliers de positions.

**Fix** : utiliser directement `token_postings[dws.first_chunk_intern]`
et `token_postings[dws.last_chunk_intern]` au lieu de passer par
content_key_to_interns. Le tokenizer est déterministe — toutes les
occurrences du même mot produisent le même intern_ord.
Ajout de `num_chunks` au WordStrippedEntry pour vérifier la distance
exacte (last_pos - first_pos == num_chunks - 1).

**Impact** : TableFunction relaxed 4/5 → 5/5.

### 2. Falling walk dépendant de la finalité FST (query-time)

**Bug** : le falling walk détecte les splits aux noeuds finaux du FST.
Quand l'index grandit (plus de docs → plus de clés), des noeuds finaux
disparaissent → splits perdus. Le mot "uint64" suivi de "to" a une
clé "\x02uint64to" (8 bytes). La query "uint64t" (7 bytes) n'atteint
pas le noeud final → split raté.

**Fix** : sibling table v3 + fst_candidates splits.

### 3. Sibling table v3 (index-time + query-time)

**Index-time** : construction d'une sibling table (format v2 identique)
pour les deux partitions :
- Chunk siblings : chunk consécutifs dans la même value
- Word siblings : mots consécutifs (word-stripped entries)

**Query-time** : `sibling_chain_dfs()` — DFS via sibling links avec
comparaison textuelle du remainder (même algo que v2 suffix_contains.rs).
`splits_from_fst_candidates()` — rattrape les premiers splits que le
falling walk a ratés (query épuisé dans l'overlap zone).

Branchement dans `find_literal_v3` pour les deux pipelines (chunk + word).
Les sibling chains s'exécutent en supplément des falling walk chains.

**Impact** : uint64_t relaxed 18/23 → 23/23.

### 4. BriquesContext (refacto)

Remplacement de 10+ params `Option<&Reader>` par un struct unique
`BriquesContext` passé à toutes les briques.

- `require_*()` : panic si fichier manquant (pas de fallback silencieux)
- `has_word_pipeline()` / `has_sibling_chains()` : vérification disponibilité
- Ajout d'un index file = un champ dans BriquesContext, rien d'autre
- Pave le chemin pour : skip de fichiers non-demandés à l'indexation

### 5. Ground truth grep word-adjacency

Le grep relaxed du ground truth utilise maintenant des mots adjacents
au lieu de la concaténation linéaire du fichier entier. Reflète la
sémantique v3 (séparateurs ignorés, frontières de mots respectées).

## Approches testées et abandonnées

| Approche | Raison abandon |
|----------|---------------|
| Content-only keys (FST) | DFS look-ahead explose avec le FST élargi |
| DFS look-ahead (falling walk) | 99% CPU pendant 6+ minutes à 500 docs |
| Split table (fichier séparé) | Ajouté puis retiré, sibling table plus complète |

## Fichiers modifiés

| Fichier | Type | Description |
|---------|------|-------------|
| `briques/context.rs` | NEW | BriquesContext struct |
| `briques/fst_walk.rs` | MOD | sibling_chain_dfs, splits_from_fst_candidates, sort_and_dedup_splits pub |
| `briques/composite.rs` | MOD | find_literal_v3 refacto ctx + sibling chains |
| `briques/orchestrator.rs` | MOD | contains_v3/fuzzy_v3 refacto ctx |
| `briques/regex_v3.rs` | MOD | Refacto ctx |
| `briques/integration_tests.rs` | MOD | Migration ctx |
| `collector_v3.rs` | MOD | WordSfxPost direct intern_ord, sibling pairs, num_chunks |
| `builder_v3.rs` | MOD | Revert content-only keys |
| `index_registry.rs` | MOD | SiblingV3Index |
| `sfx_dag_v3.rs` | MOD | sibling_v3 registry file |
| `query/contains_query_v3.rs` | MOD | BriquesContext loading |
| `query/fuzzy_query_v3.rs` | MOD | BriquesContext loading |
| `ground_truth_test.rs` | MOD | Word-adjacency grep |

## Commits (branche feature/sibling-table-v3)

| Hash | Description |
|------|-------------|
| `42e2efa` | Sibling table v3 — index-time construction |
| `1c6836a` | Plan sibling table query-time + BriquesContext |
| `eed30b3` | BriquesContext refacto signatures publiques |
| `3f3cb13` | Migration tests → BriquesContext |
| `b42b1fa` | Sibling chain DFS + fst_candidates splits — query-time |

## Prochaines étapes

1. **Ground truth 5K docs** — valider sur le repo complet
2. **Merge dans feature/sfx-v3-overlap-tokenizer** — squash ou merge
3. **Nettoyer le dead code** : cross_chunk_chain_v3, cross_word_chain_v3,
   build_chains_from_splits, best_consumed → retirables une fois sibling
   table confirmée stable
4. **Optimisation index-time** : 103s vs 68s — la sibling table ajoute ~35s.
   Optimisable (dedup des pairs en batch au lieu de Vec::contains)
5. **Markers FST** : retirables quand le sibling DFS remplace le falling
   walk pour le chain building chunk. Le falling walk resterait comme
   fast path pour le premier split.
