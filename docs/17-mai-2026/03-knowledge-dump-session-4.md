# Knowledge Dump — Session 4 (17 mai 2026)

## Résumé

Investigation et fix des faux positifs/négatifs du ground truth SFX v3 sur 500 fichiers C++.

## Changements implémentés

### 1. Content-prefix ordinals (collector_v3.rs, sfx_dag_v3.rs)
- Content key = `extract_content_prefix(text)` = scan des leading content chars
- Plus basé sur metadata (own_len/sep_len) mais sur le texte réel
- Résout : "ion " et "ion\n-" → même ordinal (même contenu "ion")
- Résout : word-stripped entries (pas de sep dans le texte) correctement gérées

### 2. Vec<Vec<u64>> chain ordinals (fst_walk.rs, resolve.rs)
- TokenChainV3.ordinals changé de Vec<u64> à Vec<Vec<u64>>
- Chaque position stocke les ordinals alternatifs (pas de forking)
- resolve_chains_v3 fait l'union des postings par position avant adjacency check
- Résout : remainder "ion" matche "ion", "ions", "ionize" etc. → tous testés

### 3. anchor_start=true pour remainder (fst_walk.rs)
- fst_candidates pour le remainder du chain utilise anchor_start=true
- Empêche les matches SI>0 ("t" matchant "state" à SI=1)

### 4. Content-len filter span=1 only (orchestrator.rs)
- Le filtre `byte_to - byte_from >= query_content_len` ne s'applique qu'aux span=1
- Les chains (span>1) passent le filtre — vérification structurelle nécessaire

### 5. Word map module (word_map.rs) — CONSTRUIT, PAS ENCORE EFFICACE
- ChunkWordMapWriter/Reader : ordinal → [(word_id, chunk_index, total_chunks)]
- NextWordMapWriter/Reader : word_id → [next_word_ids]
- verify_chain_adjacency() : vérifie intra-mot ou inter-mot
- Enregistré dans index_registry (SfxIndexFile trait)
- Stocké/chargé via registry files (chunk_word_map, next_word_map)
- 5 unit tests passing

## Résultats ground truth actuels

| Query | Mode | Grep | V3 | Status |
|-------|------|------|----|--------|
| function | strict | 62 | 63 | 1 FP |
| function | relax | 62 | 94 | 32 FP |
| return | strict | 463 | 464 | 1 FP |
| struct | strict | 71 | 83 | 12 FP |
| void | strict | 18 | 18 | OK |
| rag3db | strict | 51 | 77 | 26 FP |
| include | strict | 29 | 29 | OK |
| uint64_t | strict | 11 | 15 | 4 FP |
| std::unique_ptr | strict | 8 | 8 | OK |
| ku_dynamic_cast | strict | 0 | 0 | OK |

Les FN stricts sont RÉSOLUS. Il reste des FP sur les chains.

## Problème restant : FP des cross-token chains

### Cause racine diagnostiquée
Les content-prefix ordinals agrègent postings de tous les mots partageant le même
contenu. Un ordinal "instruct" couvre "instruction", "instructor", "restructure", etc.
La chain match valide l'adjacence (pos+1, byte continuity) mais avec 241 alternatives
au dernier step, des combinaisons fortuites passent.

### Pourquoi la word map ne filtre pas (encore)
La word map valide les combinaisons **possibles** globalement :
- ordinal A = chunk 1 de word 8978
- ordinal B = chunk 2 de word 8978
→ intra-word match validé ✓

Mais dans le doc spécifique du FP, le token à position P a ordinal A mais est
chunk 1 d'un AUTRE mot (pas word 8978). Le content-prefix ordinal fait que
l'ordinal est partagé → la word map dit "c'est possible" alors que c'est pas
le cas pour CE doc.

### Ce qu'il faut : vérification per-doc

La word map actuelle est GLOBALE (tous docs confondus). Pour filtrer les FP,
il faut savoir : "dans CE doc, à position P, quel mot et quel chunk ?"

## Options pour résoudre les FP

### Option A : PosMap étendu — (doc_id, position) → (word_id, chunk_index)
- Extension du PosMap existant avec word_id + chunk_index par position
- Au resolve, pour chaque match chain : lookup(doc, pos_first) et lookup(doc, pos_last)
- Vérifier que les deux sont des chunks consécutifs du même mot (ou inter-mot)
- **Pro** : exact, per-doc, O(1) lookup
- **Con** : espace O(num_postings × 6 bytes), refactor PosMap

### Option B : Stocker word_id dans les postings
- Ajouter word_id + chunk_index dans chaque PostingEntry
- Le resolve a directement l'info lors de l'adjacency check
- **Pro** : pas de lookup supplémentaire, vérification inline
- **Con** : augmente la taille des postings (sfxpost format change)

### Option C : Post-filtre par byte content verification
- Après le resolve, relire le texte du doc et vérifier que les bytes matchent la query
- Via le stored field ou un byte-level index
- **Pro** : 100% exact
- **Con** : I/O (lire le stored field), lent pour beaucoup de matches

### Option D : Réduire les alternatives au dernier step
- Ne pas collecter TOUS les ordinals de fst_candidates pour le remainder
- Filtrer les alternatives par falling walk overlap validation
- Un remainder "uct" (3 bytes) qui ne matche PAS l'overlap du premier token → rejeté
- **Pro** : pas de nouvelle structure, réduit les alternatives à la source
- **Con** : complexe, peut ne pas couvrir tous les cas

### Option E : Ne pas utiliser content-prefix ordinals pour les chains
- Garder les old-style ordinals (avec sep) pour le chain resolve
- Utiliser content-prefix ordinals seulement pour les single-token matches
- Dual ordinal system : content_ord pour single-token, sep_ord pour chains
- **Pro** : élimine le problème à la racine
- **Con** : double la taille des postings, complexe

### Option F : Overlap validation dans le resolve
- Le falling walk a déjà validé N bytes d'overlap dans le premier token
- Passer overlap_validated au chain, et dans le resolve vérifier que
  byte_to_first - byte_from_first >= own_len + overlap_validated
- Si le posting a un own_len plus court (autre sep), byte_to ne correspond pas
- **Pro** : simple, pas de nouvelle structure
- **Con** : vérifie la taille, pas le contenu exact

## Fichiers modifiés cette session
- src/suffix_fst/word_map.rs (NEW)
- src/suffix_fst/mod.rs
- src/suffix_fst/collector_v3.rs
- src/suffix_fst/index_registry.rs
- src/suffix_fst/file_v3.rs
- src/suffix_fst/briques/fst_walk.rs
- src/suffix_fst/briques/resolve.rs
- src/suffix_fst/briques/orchestrator.rs
- src/suffix_fst/briques/composite.rs
- src/suffix_fst/briques/integration_tests.rs
- src/indexer/sfx_dag_v3.rs
- src/query/contains_query_v3.rs
- lucivy_core/tests/test_sfx_v3_ground_truth.rs
- docs/17-mai-2026/ (3 docs)

## Lib tests : 1419/1419 pass, 0 fail
