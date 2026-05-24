# Rapport final Session 6 — 24 mai 2026

## Score

| Échelle | Score | Progression |
|---------|-------|-------------|
| 500 docs | **15/15** | était 13/15 (session 5) |
| 5000 docs | **11/15** | nouveau benchmark |

## Résumé des fixes

### 1. WordSfxPost cross-join (collector_v3.rs)
Multi-chunk words avaient des postings corrompus car `content_key_to_interns`
agrège des chunks de mots différents. Fix : utiliser directement
`token_postings[dws.first_chunk_intern]` au lieu de passer par la map.
**Impact** : TableFunction relax 4/5 → 5/5.

### 2. Sibling table v3 (collector_v3.rs + fst_walk.rs)
Table index-time `ordinal → [(next_ordinal, dest_content_len)]` pour
chunk et word siblings. Le DFS query-time suit les sibling links avec
comparaison textuelle du remainder. Remplace le re-falling_walk pour
les continuations.
**Impact** : uint64_t relax 18/23 → 23/23.

### 3. Sibling content_len = destination (collector_v3.rs)
Bug : gap_len stockait le content_len du SOURCE au lieu du DESTINATION.
Le DFS comparait le remainder avec trop peu de bytes du sibling text.
Exemple : "unique" (6 bytes content) était tronqué à 3 bytes ("uni")
car le content_len du source "std" (3 bytes) était utilisé.
**Impact** : std::unique_ptr relax 971/1000 → 1000/1000.

### 4. Grep word-adjacency break fix (test_sfx_v3_ground_truth.rs)
Le `break` dans la boucle inner du grep sortait quand `concat.len() >= query.len()`
même si le query chevauchait deux mots. Fix : continuer tant que
`concat.len() < query.len() * 2`.
**Impact** : uint64_t relax FP résolu.

### 5. BriquesContext (context.rs)
Struct unique remplaçant 10+ params Option. Champs : reader, resolver,
filter_docs, debug, trace_id, posmap, bytemap, word_sfxpost, sibling_v3,
termtexts. `require_*()` panic si manquant. `has_word_pipeline()` /
`has_sibling_chains()` pour vérifier la disponibilité.

### 6. QueryTrace (trace.rs)
Store global `LazyLock<Mutex<HashMap<u64, QueryTrace>>>` (cross-thread).
Events : label + data pairs + depth (arbre). Export JSON par query dans
`/tmp/v3_trace_{query}_{mode}.json`. Instrumenté dans find_literal_v3
et sibling_chain_dfs.

### 7. V3_DIAG mode (test_sfx_v3_ground_truth.rs)
`V3_DIAG=1` active : export fails JSON + re-test FN docs en isolation
(verdict SCALE-DEPENDENT vs PER-DOC BUG) + re-run avec traces.

## Les 4 FN restants à 5K docs

Tous sont : 1 doc, SCALE-DEPENDENT (passent en isolation),
NOT segment-dependent (persistent avec ou sans merge).

### function strict : 1478 grep, 1477 v3

**Doc** : `language_parser.rs` (chemin varie selon l'ordre read_dir)
**Contenu** : "functionality" contient "function" — SEULE occurrence.
**Chunk** : "function" (8 bytes, own_len=8, sep_len=0, overlap="al")
**Clé FST** : `\x00functional` → trouvé par fst_candidates range query.
**Théorie** : le single-token resolve produit le match, mais quelque
chose le filtre en aval. Le content_len filter (byte_span >= 8) devrait
passer (own_len=8, sep_len=0 → span=8). Le word_pos_map filter ne
s'applique qu'aux span>1.

**À investiguer** : ajouter un trace dans resolve_single_v3 qui dump
les doc_ids produits. Comparer avec le grep pour identifier le doc manquant.
Ou faire un test de bisection : indexer ce doc + N autres docs, trouver N
minimal pour reproduire le FN.

### function relax : même FN + 1 FP

FP doc : `wal_record.cpp` — v3 trouve "finition" comme match de "function".
C'est un FP du chunk pipeline (cross-token match qui matche un mot
différent). Le highlight montre `PropertyDe>>finition::<<`.

### rag3db strict : 3084 grep, 3083 v3

**Doc** : `windows-nodejs-workflow.yml`, "rag3dbjs" contient "rag3db".
Même pattern : 1 doc, single-token match, scale-dependent.

### TableFunction relax : 224 grep, 223 v3

Même pattern, 1 doc manquant.

## Commandes utiles

```bash
# Ground truth 5K docs (release, ~25s index + ~30s queries)
cargo test -p lucivy-core --test test_sfx_v3_ground_truth --release \
  -- v3_ground_truth_contains --nocapture

# Ground truth avec diagnostics (re-test FN en isolation + traces JSON)
V3_DIAG=1 cargo test -p lucivy-core --test test_sfx_v3_ground_truth \
  --release -- v3_ground_truth_contains --nocapture

# Tests lib (1400+ tests, ~150s debug)
cargo test --lib

# Analyser un trace JSON
python3 -c "
import json
traces = json.load(open('/tmp/v3_trace_function_strict.json'))
from collections import Counter
labels = Counter()
for t in traces:
    for ev in t['events']:
        labels[ev['label']] += 1
for l, c in labels.most_common(15):
    print(f'  {l}: {c}')
"

# Chercher des events spécifiques dans un trace
python3 -c "
import json
traces = json.load(open('/tmp/v3_trace_std_unique_ptr_relax.json'))
for i, t in enumerate(traces):
    for ev in t['events']:
        if 'PARTIAL' in ev['label']:
            print(f'seg {i}: {ev[\"label\"]}')
"
```

## Fichiers clés de cette session

| Fichier | Rôle |
|---------|------|
| `src/suffix_fst/briques/context.rs` | BriquesContext |
| `src/suffix_fst/briques/trace.rs` | QueryTrace store |
| `src/suffix_fst/briques/fst_walk.rs` | sibling_chain_dfs, splits_from_fst_candidates |
| `src/suffix_fst/briques/composite.rs` | find_literal_v3 avec sibling chains + traces |
| `src/suffix_fst/briques/orchestrator.rs` | contains_v3, fuzzy_v3 refacto ctx |
| `src/suffix_fst/collector_v3.rs` | WordSfxPost fix + sibling pairs + num_chunks |
| `src/suffix_fst/sibling_table.rs` | Format v2 réutilisé (gap_len = dest content_len) |
| `lucivy_core/tests/test_sfx_v3_ground_truth.rs` | 5K docs, V3_DIAG, trace JSON |
| `docs/24-mai-2026-15h04/06-design-sibling-table-v3.md` | Design sibling table |
| `docs/24-mai-2026-15h04/10-design-query-trace.md` | Design QueryTrace |

## Branche

`feature/sibling-table-v3` — 15 commits depuis `feature/sfx-v3-overlap-tokenizer`.
WIP branch `tmp/split-table-wip` contient les approches abandonnées (split table,
content-only keys, DFS look-ahead).

## Plan prochaine session

1. **Bisect le FN "function" strict** : indexer le doc FN avec N=10, 100, 500,
   1000 autres docs. Trouver le N minimal qui reproduit le FN. Puis identifier
   le doc "perturbateur" qui cause la collision.

2. **Instrumenter resolve_single_v3** : ajouter un trace qui dump les doc_ids
   produits par chaque candidat. Comparer la liste des docs trouvés par le
   segment contenant le doc FN avec la liste attendue.

3. **Vérifier le posting** : pour le segment contenant le doc FN, vérifier que
   l'ordinal de "functional" a bien une entrée pour ce doc dans le sfxpost.
   Si l'entrée est absente → bug d'indexation. Si présente → bug de resolve.

4. **FP "finition"** : investiguer pourquoi v3 matche "finition" pour la query
   "function". C'est un cross-token match chunk qui traverse un mauvais
   boundary. Probablement besoin du word_pos_map post-filter.

5. **Objectif** : 15/15 à 5K docs, puis étendre à 10K+.

## Conseil pour la prochaine session

Commencer par lire ce document + les docs 09 et 10 dans le même dossier.
La branche est `feature/sibling-table-v3`. Le ground truth se lance avec
la commande ci-dessus. Le V3_DIAG produit des JSON queryables en Python.

Le problème restant est probablement SIMPLE (un edge case de posting ou
de byte_span) mais nécessite un diagnostic ciblé sur UN doc. Le QueryTrace
est en place pour ça — il suffit d'instrumenter `resolve_single_v3` pour
voir quels docs chaque candidat produit.
