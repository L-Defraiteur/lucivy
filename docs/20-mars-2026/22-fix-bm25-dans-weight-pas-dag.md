# Doc 22 — Fix BM25 contains : dans weight(), pas dans le DAG

Date : 22 mars 2026

## Contexte

Le two-pass dans le DAG (CountShardNode → BuildScoreWeightNode → ScoreShardNode)
fonctionne mais cause une régression perf 3x sur les boolean queries.
Cause : le DAG reconstruit tout 2x (query, weight, setup).

## Le vrai design

Le `SuffixContainsQuery::weight(EnableScoring)` reçoit le `Searcher` qui a
accès à **tous les segments** via `searcher.segment_readers()`.

Le two-pass se fait dans `weight()` :

1. Itérer tous les segments, ouvrir le SFX, faire le walk, cacher `doc_tf`,
   compter `doc_freq` global
2. Stocker le cache + `global_doc_freq` dans le Weight
3. `scorer()` lit le cache → zéro SFX walk, BM25 correct

### Pour le boolean

`BooleanQuery::weight()` appelle `sub_query.weight()` pour chaque sous-query.
Chaque `SuffixContainsQuery::weight()` fait son propre pré-scan en interne.
Chaque terme a son propre `doc_freq`. Un seul passage dans le DAG.

### Pour le shardé

Le search DAG redevient :
```
drain → flush → build_weight → search_0..N ∥ → merge
```

Le `BuildWeightNode` (l'ancien, simple) appelle `query.weight(enable_scoring)`.
Le `EnableScoring` utilise `AggregatedBm25StatsOwned` qui agrège les searchers
de tous les shards → `total_num_docs` et `total_num_tokens` sont globaux.

Mais `segment_readers()` dans `EnableScoring` donne les segments d'UN seul shard
(le `searcher_0`). Il faut donner les segment_readers de TOUS les shards.

### Solution pour le shardé

Deux options :
- **A** : passer tous les segment_readers via un custom `EnableScoring`
- **B** : passer un `SfxCache` pré-rempli au `weight()` (le cache vient du
  search DAG qui a accès à tous les shards)

Option A est plus propre. On peut créer un `EnableScoring` avec un `Searcher`
custom qui expose les segment_readers de tous les shards, ou simplement
passer un `&[&SegmentReader]` au `SuffixContainsQuery`.

En fait, la plus simple : le `SuffixContainsQuery` a un champ optionnel
`all_segment_readers: Option<Vec<SegmentReader>>`. En mode non-shardé,
il est rempli depuis `enable_scoring.searcher.segment_readers()`. En mode
shardé, il est rempli par le `BuildWeightNode` avec les segments de tous
les shards.

## Changements

### Supprimer (code du two-pass DAG)
- `CountShardNode` — supprimer
- `ScoreShardNode` — supprimer
- `BuildCountWeightNode` — supprimer
- `BuildScoreWeightNode` — supprimer
- `SfxCache` — garder mais simplifier (pas besoin de Arc, c'est interne au Weight)
- `SfxScoringOptions` — supprimer (plus besoin de passer ça via build_query)

### Modifier
- `SuffixContainsQuery::weight()` — pré-scan des segments, cache dans le Weight
- `SuffixContainsWeight` — stocke le cache + global_doc_freq
- `SuffixContainsWeight::scorer()` — lit le cache au lieu de faire le SFX walk
- `build_search_dag()` — revenir à l'ancien DAG simple (1 BuildWeightNode)
- `BuildWeightNode` — passer les segment_readers de tous les shards

### Garder tel quel
- `search_with_docs()`, `export_stats()`, `search_with_global_stats()` — API distribuée
- `ExportableStats`, `CorpusStats` — sérialisables pour le réseau
- Les tests ACID et ground truth

## Invariant

```
Pour tout document D et toute query Q (single-term, boolean, fuzzy) :
  score(D, Q, 1_shard) == score(D, Q, 4_shards) == ground_truth_bm25
```

Le pré-scan dans `weight()` garantit un `doc_freq` global peu importe
le nombre de segments ou de shards.
