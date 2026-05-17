# Roadmap v3 — Plan d'implémentation (1-2 semaines)

**Date** : 16 mai 2026  
**Objectif** : implémenter le nouveau tokenizer, SFX builder, term dict, et adapter toutes les queries.

---

## Principe d'ordonnancement

1. **Bottom-up** : d'abord les structures de données, puis les writers, puis les readers, puis les queries
2. **Testable à chaque étape** : chaque phase produit un artefact testable indépendamment
3. **Pas de régression** : les tests existants continuent de passer (v2 et v3 coexistent via un flag `sfx_version`)

---

## Phase 1 — Tokenizer (jour 1-2)

### 1.1 MaxLenEqualChunkTokenizer

Nouveau tokenizer dans `src/tokenizer/equal_chunk.rs` :

- Split sur non-alphanum → segments (mot + trailing sep)
- Division égale : `ceil(segment.len() / MAX_TOKEN)` chunks
- Respect frontières UTF-8
- Metadata par token : `content_len`, `sep_len`, `is_word_start`, `word_id`

### 1.2 Tests unitaires tokenizer

- Cas standards : `"mutex_lock"`, `"getElementById"`, `"pthread_mutex_lock"`
- Cas limites : longs seps `"a________b"`, UTF-8 `"café_latte"`, single char `"a_b"`
- Vérifier : pas d'orphelins, chunks ~égaux, sep toujours dans le dernier chunk du mot

### 1.3 Intégration TextAnalyzer

- Remplacer `CamelCaseSplitFilter` par le nouveau tokenizer (ou ajouter comme option)
- Le BM25 tokenizer reste inchangé (split sur non-alphanum, pas de maxlen)
- Seul le SFX tokenizer change

**Livrable** : tokenizer fonctionnel, tests verts, pas encore connecté au SFX builder.

---

## Phase 2 — SFX Builder v3 (jour 2-4)

### 2.1 Nouveau encoding output u64

```
[63: multi_flag]
[62: is_word_start]
[61..58: overlap_len]     4 bits (0..15)
[57..55: sep_len]         3 bits (0..7)
[54..40: own_len]         15 bits (max 32767)
[39..24: sti]             16 bits
[23..0: token_ordinal]    24 bits
```

Implémenter `encode_single_parent_v3()` et `decode_parents_v3()`.

### 2.2 Overlap dans le builder

- Pour chaque token (sauf le dernier), étendre avec `min(2, len(next_token))` bytes
- `own_len` = longueur du chunk (content + sep), SANS overlap
- Suffixes générés sur `chunk_bytes + overlap_bytes` (= extended_len)
- Multi-parent naturel pour les bytes d'overlap (partagés entre 2 tokens)

### 2.3 Suppression sibling table + gapmap + sepmap

- Ne plus appeler `record_sibling_pair()` dans le collector
- Ne plus écrire la section sibling dans le `.sfx`
- Ne plus écrire `.gapmap` ni `.sepmap`
- Garder la lecture v2 (compat) derrière un flag version

### 2.4 Term dict builder

- Nouveau `TermDictBuilder` : FST de mots entiers → posting lists
- Construit en parallèle du SFX builder dans le collector
- Pour chaque mot complet : `term_dict.add(word_text, doc_id, word_position)`
- Sérialise dans `.terms`

### 2.5 Sep dict builder

- `SepDictBuilder` : FST de séparateurs → posting lists
- Pour chaque séparateur non-vide : `sep_dict.add(sep_text, doc_id, position)`
- Sérialise dans `.seps`

### 2.6 Word map + next_word

- `token_to_word[TI] → WI` (word ordinal = ordinal dans le term dict)
- `word_start_token[WI] → TI`
- `next_word[TI] → TI` (prochain token qui est `is_word_start=true`)
- Sérialise dans le `.sfx` (section additionnelle après parent lists)

**Livrable** : indexation v3 fonctionnelle. Peut indexer un document et écrire tous les fichiers. Tests : re-indexer le dataset linux et vérifier la taille de l'index.

---

## Phase 3 — Falling walk v3 (jour 4-5)

### 3.1 Falling walk exact

- Nouveau `falling_walk_v3()` (ou modifier l'existant derrière flag version)
- Split condition : `STI + consumed == own_len`
- Continue après split dans l'overlap zone (ne pas break)
- Retourne `SplitCandidate` avec `overlap_validated: usize`

### 3.2 Sep-skip (strict_separators=false)

- Quand `STI + consumed == content_len` et `!strict_separators` :
  - Skip `sep_len` bytes dans le token (avancer le curseur FST sans comparer)
  - Skip bytes non-alphanum dans la query
  - Reprendre la comparaison dans l'overlap zone

### 3.3 Fuzzy falling walk

- Ajustement : `STI + fst_depth == own_len` au lieu de `token_len`
- Le DFS continue naturellement dans l'overlap (pas de changement structural)

### 3.4 Cross-token par falling walk chaîné

- Quand un split est détecté : nouveau `falling_walk` sur `query[split_point..]`
- Pas de sibling lookup, pas de DFS
- Le résultat donne les ordinals candidats pour TI+1
- Position adjacency vérifié lors de la résolution des postings

**Livrable** : falling walk v3 fonctionnel. Test : queries cross-token exactes sur le dataset indexé en phase 2.

---

## Phase 4 — Fuzzy contains v3 (jour 5-6)

### 4.1 Simplification pipeline

- Supprimer `concat_query()` : query utilisée telle quelle (lowercase seulement)
- Supprimer `boundary_positions()` et `boundary_trigram_indices()`
- Threshold simplifié : `max(T - n*d, 1)`
- Résolution : seulement `fst_candidates` + `resolve_candidates` (single-token)
- Supprimer tous les appels à `cross_token_falling_walk` dans la boucle trigram

### 4.2 Selectivity estimation

- `fst_candidates(sfx, trigram)` → count (inchangé)
- Plus de `cross_token_falling_walk` dans l'estimation → plus rapide
- Tri par sélectivité croissante → résolution rarest-first (inchangé)

### 4.3 Hit matching

- `build_hits_by_doc` et `find_matches` : inchangés structurellement
- Les hits sont tous single-token (pas de `token_parts` multi-token)
- Simplifier `TrigramHit` : supprimer `token_parts`

### 4.4 Scoring

- `miss_count` inchangé conceptuellement
- Plus de boundary trigram compensation → scoring plus strict et plus juste
- `miss_penalty * 1000 + bm25` (inchangé)

**Livrable** : fuzzy contains v3. Benchmark : comparer latence v2 vs v3 sur "mutex_lock" d=1, "pthread_mutex" d=2, etc.

---

## Phase 5 — Exact contains v3 (jour 6-7)

### 5.1 Single-token contains

- `prefix_walk` / `prefix_walk_si0` : inchangé (le FST a les mêmes partitions)
- Ajuster `byte_from += parent.sti` (STI au lieu de SI, même sémantique)

### 5.2 Cross-token contains

- Remplacer le sibling DFS dans `cross_token_search_with_terms()` par :
  1. `falling_walk(query)` → split candidates
  2. Pour chaque split : `falling_walk(remainder)` → ordinals candidats
  3. Résoudre postings des deux côtés
  4. Vérifier `position_right == position_left + 1`
- Supprimer le code sibling DFS (lignes 806-936 de `suffix_contains.rs`)

### 5.3 Multi-token contains

- Résolution par sous-token + pivot optimization : inchangé
- Chaque sous-token résolu via le pipeline simplifié (pas de sibling)
- Position adjacency : inchangé

### 5.4 Term dict fast-path

- Si `anchor_start=true` et `exact_match=true` → route vers term dict direct
- `term("mutex")` → `term_dict.get("mutex")` → posting list → done
- `startsWith("get")` → `term_dict.prefix_scan("get")` → posting lists → done
- Fallback vers SFX si le term dict n'existe pas (compat v2)

**Livrable** : exact contains v3. Tests : 1200 tests existants doivent passer.

---

## Phase 6 — Regex continuation v3 (jour 7-8)

### 6.1 Suppression continuation_score_sibling

- Supprimer `continuation_score_sibling()` entièrement
- Utiliser uniquement `continuation_score()` (le fallback actuel)
- Supprimer les appels au gapmap dans `continuation_score`

### 6.2 Adaptation DFA walk

- Le DFA traverse naturellement les sep bytes (ils sont dans le token)
- L'overlap fait avancer le DFA de 2 bytes supplémentaires par walk
- `search_continuation` : inchangé (opère sur le FST, agnostique du format)

### 6.3 Sep dict pour regex

- Pour les patterns avec littéraux séparateurs (ex: `"[a-z]+::[a-z]+"`):
  - Extraire le sep littéral `"::"`
  - Lookup dans sep dict → candidats documents
  - Intersect avec les résultats du SFX walk
- Optimisation optionnelle (pas bloquante pour la v3 initiale)

**Livrable** : regex v3 fonctionnel. Tests regex existants verts.

---

## Phase 7 — Intégration et benchmarks (jour 8-10)

### 7.1 Routing des queries

Mettre à jour `build_query()` dans `lucivy_core/src/query.rs` :

| Query type | v3 route |
|------------|----------|
| `term` | **term dict direct** (fast-path) |
| `startsWith` | **term dict prefix scan** (fast-path) |
| `contains` | SFX falling walk v3 |
| `contains_split` | SFX (split whitespace → boolean should) |
| `fuzzy` | SFX trigram pipeline v3 |
| `regex` | SFX DFA walk v3 (+ sep dict optionnel) |
| `phrase` | SFX multi-token v3 |
| `boolean` / `disjunction_max` | composite (inchangé) |

### 7.2 Benchmark complet

Sur le dataset linux 90K docs :
- Index size v2 vs v3
- term("mutex") : v2 (SFX route) vs v3 (term dict)
- contains("mutex_lock") exact : v2 vs v3
- fuzzy("mutex_lock", d=1) : v2 vs v3
- regex("mutex.*lock") : v2 vs v3
- Multi-word fuzzy("pthread mutex lock", d=2) : v2 vs v3

Objectifs :
- term/prefix : **×10-100 plus rapide** (term dict vs SFX route)
- fuzzy : **×40-100 plus rapide** (overlap élimine boundary trigrams)
- exact contains : **×2-5 plus rapide** (falling walk chaîné vs sibling DFS)
- regex : **×2-5 plus rapide** (DFA avance plus loin par walk)
- index size : **×1.10-1.15** (term/sep dict ajoutés, gapmap/sepmap/sibling supprimés)

### 7.3 Ground truth validation

- 37/37 sur le dataset linux (mêmes résultats que v2)
- Score consistency single vs 4-shard (diff=0.0000)
- Vérifier highlights byte positions

### 7.4 Flag sfx_version

- `sfx_version=2` : lecture et écriture v2 (défaut pendant la transition)
- `sfx_version=3` : lecture et écriture v3
- Un segment v3 ne peut pas être lu par du code v2 (reindex nécessaire)
- Migration : reindex complet (pas de migration incrémentale)

---

## Phase 8 — Cleanup et stabilisation (jour 10-12)

### 8.1 Suppression code mort v2

- `sibling_table.rs` → supprimé (garder le reader derrière `#[cfg(feature = "v2_compat")]`)
- `gapmap.rs` → supprimé (idem)
- `sepmap.rs` → supprimé (idem)
- `concat_query()`, `boundary_positions()`, `boundary_trigram_indices()` → supprimés
- `cross_token_falling_walk()` (pour fuzzy) → supprimé
- `continuation_score_sibling()` → supprimé
- `CamelCaseSplitFilter` → deprecated (gardé pour compat, mais plus utilisé par défaut)

### 8.2 Documentation

- Mettre à jour CLAUDE.md avec la nouvelle architecture
- Mettre à jour les docs des query types (routing v3)
- Knowledge dump v3 (équivalent du doc 06 mais pour v3)

### 8.3 Bindings

- Vérifier que les bindings (Python, Node.js, Emscripten, C++) fonctionnent avec v3
- Le JSON QueryConfig ne change pas (les paramètres sont les mêmes)
- Seul changement visible : performance + nécessité de reindex

---

## Résumé timeline

| Jour | Phase | Livrable |
|:---:|-------|----------|
| 1-2 | Tokenizer | `MaxLenEqualChunkTokenizer` testé |
| 2-4 | SFX Builder | Indexation v3 + term dict + sep dict |
| 4-5 | Falling walk | Cross-token v3 fonctionnel |
| 5-6 | Fuzzy contains | Pipeline simplifié, ×40 speedup |
| 6-7 | Exact contains | Term dict fast-path + falling walk chaîné |
| 7-8 | Regex continuation | DFA étendu sans siblings |
| 8-10 | Intégration | Benchmarks, ground truth, routing |
| 10-12 | Cleanup | Code mort supprimé, docs à jour |

---

## Risques et mitigation

| Risque | Impact | Mitigation |
|--------|--------|------------|
| Régression sur les 1200 tests | Bloquant | Flag `sfx_version` : v2 par défaut pendant dev |
| Index size explosion | Perf | Benchmark taille à chaque phase, ajuster MAX_TOKEN |
| Highlights byte positions décalées | UX | Tester highlights dès phase 5 |
| Overlap multi-parent explose le FST | Taille | Monitorer ratio multi/single parents |
| UTF-8 boundary issues dans equal-chunk | Correctness | Tests exhaustifs phase 1 |

---

## Décisions fixes (pas de retour en arrière)

1. **Overlap = 2 bytes** (fixe, pas variable)
2. **MAX_TOKEN = 8** (à benchmarker, mais point de départ)
3. **Division égale** du segment mot+sep (pas de maxlen fixe sur contenu seul)
4. **Sep-skip dans le falling walk** (pas post-filtering)
5. **Term dict séparé** du SFX (deux FSTs indépendants)
6. **Suppression siblings** (TI+1 implicite + falling walk chaîné)
7. **Suppression gapmap + sepmap** (seps dans les tokens)
8. **Reindex obligatoire** v2 → v3 (pas de migration incrémentale)
