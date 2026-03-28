# 12 — Design : BM25 scoring pour regex contains

## Problème

Tous les résultats regex ont un score de 1.0. Le scorer utilise `ConstScorer::new(doc_bitset, boost)`.
Les résultats ne sont pas triés par pertinence.

## Ce qu'il faut pour du BM25 correct

Le BM25 a besoin de 3 choses :

### 1. Term frequency (tf) — combien de fois la regex matche dans chaque doc

On l'a déjà : `highlights` contient un entry par match. Il suffit de compter par doc_id :

```rust
// highlights: Vec<(DocId, usize, usize)>
let mut doc_tf: Vec<(DocId, u32)> = Vec::new();
let mut counts: HashMap<DocId, u32> = HashMap::new();
for &(doc_id, _, _) in &highlights {
    *counts.entry(doc_id).or_default() += 1;
}
doc_tf = counts.into_iter().collect();
doc_tf.sort_by_key(|&(d, _)| d);
```

### 2. Document frequency (df) — combien de docs matchent au total (cross-segment)

Deux options :

**Option A : Per-segment df (fallback)**
- `df = doc_tf.len()` — le nombre de docs dans CE segment qui matchent
- Utilisé par le contains exact quand il n'a pas de prescan cache
- Pas parfait cross-shard mais déjà bien meilleur que 1.0
- Zéro changement au search DAG

**Option B : Global df via prescan**
- Implémenter `sfx_prescan_params()` sur `RegexContinuationQuery`
- Le search DAG fait le prescan en parallèle, accumule le df global
- Le weight reçoit le df global via `EnableScoring`
- Scoring parfait cross-shard
- Nécessite : étendre `SfxPrescanParam` pour supporter le mode regex, ou
  créer un nouveau type de prescan

### 3. Average field norm — longueur moyenne des champs

Disponible via `EnableScoring::Enabled` qui porte un `Bm25StatisticsProvider`.
Le `total_num_tokens` et `total_num_docs` sont déjà agrégés cross-shard.

## Option A : BM25 per-segment (quick win)

```rust
// Dans RegexContinuationWeight.scorer() :

// Convertir highlights → doc_tf
let mut counts: HashMap<DocId, u32> = HashMap::new();
for &(doc_id, _, _) in &highlights {
    *counts.entry(doc_id).or_default() += 1;
}
let mut doc_tf: Vec<(DocId, u32)> = counts.into_iter().collect();
doc_tf.sort_by_key(|&(d, _)| d);

if doc_tf.is_empty() {
    return Ok(Box::new(EmptyScorer));
}

// BM25 weight avec df per-segment
let fieldnorm_reader = reader.fieldnorms_readers()
    .get_field(self.field)?
    .unwrap_or_else(|| FieldNormReader::constant(max_doc, 1));

let inv_idx = reader.inverted_index(self.field)?;
let total_num_tokens = inv_idx.total_num_tokens();
let total_num_docs = max_doc as u64;
let average_fieldnorm = total_num_tokens as f32 / total_num_docs.max(1) as f32;
let doc_freq = doc_tf.len() as u64;

let bm25 = Bm25Weight::for_one_term(doc_freq, total_num_docs, average_fieldnorm);

Ok(Box::new(SuffixContainsScorer::new(doc_tf, bm25.boost_by(boost), fieldnorm_reader)))
```

**Avantage** : 10 lignes de changement, scoring correct per-segment.
**Inconvénient** : IDF pas global cross-shard. Un terme rare dans un shard mais fréquent dans un autre aura un IDF différent.

## Option B : Global df via prescan (correct cross-shard)

### Approche 1 : Nouveau type de prescan pour regex

Ajouter un `RegexPrescanParam` :

```rust
pub struct RegexPrescanParam {
    pub field: Field,
    pub pattern: String,
}
```

Le prescan exécute `regex_contains_via_literal` sur chaque segment, retourne `(doc_tf, highlights)` + `doc_freq`. Le weight accumule le df global.

**Problème** : le search DAG (`search_dag.rs`) est construit autour de `SfxPrescanParam` + `run_sfx_walk`. Ajouter un nouveau type de prescan demande de modifier le DAG, les nodes `PrescanShardNode` et `MergePrescanNode`.

### Approche 2 : Prescan dans le Query.weight() via les stats provider

`EnableScoring::Enabled` porte un `Arc<dyn Bm25StatisticsProvider>`.
On pourrait ajouter une méthode `contains_doc_freq(field, query)` au provider.

**Déjà fait pour le contains exact** : `Bm25StatisticsProvider.contains_doc_freqs` est un `HashMap<String, u64>` peuplé pendant le prescan. Le weight du contains exact lit sa doc_freq depuis cette map.

Pour le regex : ajouter une clé `"regex:rag3.*ver"` dans `contains_doc_freqs`.

```rust
// Dans le prescan (search_dag.rs) :
// Pour chaque regex query, exécuter le prescan et stocker le df
contains_doc_freqs.insert(format!("regex:{}", pattern), doc_freq);

// Dans le weight :
let global_df = stats.contains_doc_freq(&format!("regex:{}", self.pattern));
```

### Approche 3 : Prescan DANS le RegexContinuationQuery (comme SuffixContainsQuery)

Ajouter `prescan()` et `with_prescan_cache()` sur `RegexContinuationQuery` :

```rust
impl RegexContinuationQuery {
    pub fn prescan(&self, segment_readers: &[&SegmentReader])
        -> Result<(HashMap<SegmentId, RegexCachedResult>, u64)>
    {
        // Pour chaque segment : regex_contains_via_literal → (doc_tf, highlights)
        // Accumuler doc_freq
    }
}
```

Le `build_contains_regex` dans `lucivy_core/src/query.rs` appellerait `prescan()` avant `weight()`.

**Avantage** : même pattern que le contains exact, pas de changement au DAG.
**Inconvénient** : le prescan est séquentiel (pas parallèle comme le DAG).

## Recommandation

**Phase 1 (immédiat)** : Option A — BM25 per-segment. 10 lignes. Scoring fonctionnel immédiatement. 99% des cas d'usage sont OK.

**Phase 2 (futur)** : Option B approche 2 — ajouter la clé regex dans `contains_doc_freqs`. Le prescan du search DAG appelle `regex_contains_via_literal` pour les regex queries et stocke le df dans la map existante. Cross-shard correct.

## Impact sur le code

### Phase 1

Fichiers à modifier :
- `regex_continuation_query.rs` : remplacer `ConstScorer` par `SuffixContainsScorer` avec BM25

### Phase 2

Fichiers à modifier :
- `regex_continuation_query.rs` : ajouter `prescan()`, `with_prescan_cache()`, `sfx_prescan_params()`
- `search_dag.rs` : étendre `PrescanShardNode` pour gérer les regex prescan params
- `query.rs` (lucivy_core) : appeler `prescan()` dans `build_contains_regex`

## Note sur le scoring existant du contains exact

Le contains exact utilise `SuffixContainsScorer` qui implémente `DocSet + Scorer` :
- `doc_tf` trié par doc_id — `DocSet::advance()` itère séquentiellement
- `Scorer::score()` utilise `bm25_weight.score(fieldnorm)` avec la tf du doc courant
- Le `Bm25Weight` est construit avec le global df (si prescan dispo) ou local df (fallback)

Le regex peut réutiliser EXACTEMENT le même `SuffixContainsScorer` — il a juste besoin de `doc_tf` et `bm25_weight`.
