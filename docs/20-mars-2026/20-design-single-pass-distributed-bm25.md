# Doc 20 — Design : BM25 distribué en une seule passe

Date : 21 mars 2026

## Problème

Le BM25 du `SuffixContainsQuery` calcule `doc_freq` per-segment (via `doc_tf.len()`
dans le scorer, après le SFX walk). Ça signifie que le même doc aura un score
différent selon la répartition des segments/shards.

**Ce problème affecte DEUX modes :**
- **Local shardé** : chaque shard a son propre `doc_freq` per-segment dans le scorer.
  Le search DAG dispatch le même Weight à N shards, mais le `doc_freq` est local.
- **Distribué** : même problème amplifié (N nodes × M shards chacun).

La fix doit corriger les deux d'un coup.

## Rappel : composantes du BM25

```
score(term, doc) = IDF(term) × TF_component(term, doc)

IDF(term) = log(1 + (total_docs - doc_freq + 0.5) / (doc_freq + 0.5))

TF_component = (TF × (k1 + 1)) / (TF + k1 × (1 - b + b × doc_len / avg_doc_len))
```

**Ce qui est global** (dépend du corpus entier) :
- `total_docs` — nombre total de docs
- `avg_doc_len` = `total_tokens / total_docs` — longueur moyenne
- `doc_freq` — nombre de docs contenant le terme/substring

**Ce qui est local** (dépend uniquement du document) :
- `TF` — fréquence du terme dans le doc
- `doc_len` — longueur du doc (fieldnorm)

## Observation clé

Pour un **single-term contains** (ex: `contains "mutex"`), l'IDF est une
**constante multiplicative** — tous les résultats matchent le même terme.
L'IDF ne change pas le ranking relatif, seulement la valeur absolue du score.

```
score_doc_A = IDF × TF_component_A
score_doc_B = IDF × TF_component_B

ranking(A vs B) = TF_component_A vs TF_component_B  (IDF s'annule)
```

**Conclusion** : on peut calculer TF_component localement, et appliquer l'IDF en post-hoc.

## Solution : skip IDF dans le scorer, appliquer post-hoc

### Mode local shardé (search DAG)

Le `MergeResultsNode` dans le search DAG reçoit les résultats de chaque shard.
Chaque shard retourne ses hits (scorés sans IDF) + son `doc_freq_count`.
Le merge node somme les doc_freq, calcule l'IDF global, multiplie les scores.

```
search_shard_0 → {hits: [...], doc_freq: 200}  ──┐
search_shard_1 → {hits: [...], doc_freq: 180}  ──┼── merge_results
search_shard_2 → {hits: [...], doc_freq: 220}  ──┘
                                                   global_doc_freq = 600
                                                   IDF = bm25_idf(600, 90000)
                                                   final_score = score × IDF
```

Ça fixe le scoring local sans aucun changement d'architecture.

### Mode distribué (N nodes)

Même pattern, mais le merge est fait par le coordinateur au lieu du merge node.

### Phase 1 — Post-indexation (une fois, pas par query)

Après que tous les nodes aient commit, échange de stats corpus :

```
1. Chaque node : export_corpus_stats()
   → CorpusStats { total_docs, total_tokens_per_field }

2. Coordinateur : merge
   → GlobalCorpusStats { total_docs: 10000, total_tokens: {body: 5000000} }

3. Chaque node : store_global_corpus_stats(merged)
   → Persisté dans le BlobStore (_corpus_stats.json)
```

Coût : un seul échange après chaque batch d'indexation. ~50 bytes JSON.

### Phase 2 — Recherche (une seule passe par query)

```
1. Coordinateur → tous les nodes : query "mutex"

2. Chaque node :
   a. SFX walk → matches + doc_freq_count
   b. Score chaque match : TF_component(doc) en utilisant avg_doc_len global (stocké)
      PAS d'IDF — on retourne le score sans IDF
   c. Retourne : { top_k: [{score_no_idf, doc, highlights}], doc_freq_count: 50 }

3. Coordinateur :
   a. global_doc_freq = sum(doc_freq_counts) = 100
   b. global_idf = bm25_idf(global_doc_freq, global_total_docs)
   c. Pour chaque résultat : final_score = score_no_idf × global_idf
   d. Merge top-K par final_score
```

**Un seul aller-retour réseau. Scores identiques au single-node.**

### Multi-term (contains_split "struct device")

Chaque sous-terme a son propre IDF. Le node retourne les scores décomposés :

```
Chaque node retourne :
{
  results: [{doc, highlights, per_term_scores: [0.5, 0.3]}],
  per_term_doc_freqs: [500, 200]   // "struct": 500 docs, "device": 200 docs
}

Coordinateur :
  global_idf_struct = bm25_idf(sum(500, ...), global_total_docs)
  global_idf_device = bm25_idf(sum(200, ...), global_total_docs)
  final_score = per_term_score[0] × global_idf_struct + per_term_score[1] × global_idf_device
```

Un peu plus de données retournées (un float par sous-terme par résultat + un int
par sous-terme global), mais toujours un seul aller-retour.

## Changements dans le code

### 1. CorpusStats (nouveau, simple)

```rust
// bm25_global.rs

#[derive(Serialize, Deserialize)]
pub struct CorpusStats {
    pub total_num_docs: u64,
    pub total_num_tokens: HashMap<u32, u64>,  // field_id → token count
}

impl CorpusStats {
    pub fn from_handle(handle: &ShardedHandle) -> Self { ... }
    pub fn merge(stats: &[CorpusStats]) -> CorpusStats { ... }
    pub fn avg_fieldnorm(&self, field_id: u32) -> f32 {
        let tokens = self.total_num_tokens.get(&field_id).copied().unwrap_or(0);
        tokens as f32 / self.total_num_docs.max(1) as f32
    }
}
```

### 2. ShardedHandle — store/load corpus stats

```rust
// sharded_handle.rs

const CORPUS_STATS_FILE: &str = "_corpus_stats.json";

impl ShardedHandle {
    /// Export corpus stats (total_docs, total_tokens per field).
    pub fn export_corpus_stats(&self) -> CorpusStats { ... }

    /// Store global corpus stats (from coordinator merge).
    pub fn store_global_corpus_stats(&self, stats: &CorpusStats) -> Result<(), String> {
        let json = serde_json::to_vec(stats)?;
        self.storage.write_root_file(CORPUS_STATS_FILE, &json)
    }

    /// Load stored global corpus stats (if available).
    pub fn load_global_corpus_stats(&self) -> Option<CorpusStats> {
        let data = self.storage.read_root_file(CORPUS_STATS_FILE).ok()?;
        serde_json::from_slice(&data).ok()
    }
}
```

### 3. SuffixContainsWeight — scorer sans IDF

Le scorer calcule `TF_component` en utilisant `avg_doc_len` global.
Il NE calcule PAS l'IDF. Il retourne le score partiel + le doc_freq count.

```rust
// suffix_contains_query.rs

struct SuffixContainsWeight {
    // ... existing fields ...
    global_num_docs: u64,
    global_num_tokens: u64,  // pour avg_fieldnorm
    skip_idf: bool,          // true en mode distribué
}

// Dans scorer() :
let bm25_weight = if self.skip_idf {
    // Mode distribué : TF × fieldnorm seulement, IDF sera appliqué par le coordinateur
    Bm25Weight::for_one_term(1, 1, average_fieldnorm)  // IDF ≈ 1.0
} else {
    // Mode local : BM25 complet
    Bm25Weight::for_one_term(doc_tf.len() as u64, total_num_docs, average_fieldnorm)
};
```

### 4. Résultat de search enrichi

```rust
pub struct DistributedSearchResult {
    pub hits: Vec<ShardedSearchResult>,   // top-K avec scores sans IDF
    pub doc_freq_count: u64,             // nombre de docs matchés sur ce node
    pub highlights: ...,
}
```

### 5. Coordinateur — post-hoc IDF

```rust
// coordinator (hors lucivy, dans l'application)

let results: Vec<DistributedSearchResult> = nodes.par_search(&query);
let global_doc_freq: u64 = results.iter().map(|r| r.doc_freq_count).sum();
let global_idf = bm25_idf(global_doc_freq, global_corpus_stats.total_num_docs);

let mut merged: Vec<_> = results.into_iter()
    .flat_map(|r| r.hits.into_iter().map(|h| {
        let final_score = h.score * global_idf;  // appliquer IDF post-hoc
        (final_score, h)
    }))
    .collect();
merged.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
merged.truncate(top_k);
```

## Fichiers modifiés

| Fichier | Changement |
|---------|-----------|
| `bm25_global.rs` | `CorpusStats` struct + merge + serde |
| `sharded_handle.rs` | export/store/load corpus stats, `search_distributed()` |
| `suffix_contains_query.rs` | `skip_idf` mode dans le scorer |
| test `acid_postgres.rs` | test score consistency 1 node vs 2 nodes |

## Invariant vérifié

```
Pour tout document D et toute query Q single-term :
  score(D, Q, 1_shard) == score(D, Q, 4_shards) == score(D, Q, distributed_N_nodes)

Preuve :
  - avg_doc_len est global (via EnableScoring ou stocké post-indexation) ✓
  - total_docs est global (via EnableScoring ou stocké post-indexation) ✓
  - TF est local au document ✓
  - doc_len est local au document ✓
  - IDF est appliqué post-hoc avec global_doc_freq (somme des shards/nodes) ✓
  → Toutes les composantes sont identiques

Cet invariant s'applique aux deux modes :
  - Local shardé : merge_results node applique IDF post-hoc
  - Distribué : coordinateur applique IDF post-hoc
```

## Ordre d'implémentation

```
1. CorpusStats struct + merge                      ~30 lignes
2. ShardedHandle: export/store/load corpus stats   ~40 lignes
3. SuffixContainsWeight: skip_idf mode             ~10 lignes
4. search_distributed() retournant doc_freq_count  ~30 lignes
5. Test: 1 node vs 2 nodes → scores identiques     ~50 lignes
6. Mettre à jour l'exemple distributed_postgres     ~20 lignes
```

Total : ~180 lignes. Un seul aller-retour réseau pour la recherche.
