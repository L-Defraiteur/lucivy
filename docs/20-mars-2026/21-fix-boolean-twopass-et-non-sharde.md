# Doc 21 — Fix boolean two-pass + appliquer au non-shardé

Date : 22 mars 2026

## Validé

Le two-pass donne le BM25 **exactement correct** (validé par ground truth) :
- `score_sharded == ground_truth == 0.004963` (diff = 0.0)
- Fonctionne pour contains, startsWith, fuzzy contains
- Pas d'overhead perf mesurable (5K bench : ~60ms identique avant/après)

## Problème 1 : boolean contains_split — compteur partagé

### Le bug

`contains_split "struct device"` → BooleanQuery { should: [contains("struct"), contains("device")] }

Les deux sous-queries partagent le même `SfxCache.doc_freq_count` (un seul AtomicU64).
En pass 1, "struct" ajoute 5000 et "device" ajoute 1000 → compteur = 6000.
En pass 2, les deux utilisent `global_doc_freq = 6000` → IDF faux pour les deux.

Il faudrait : `doc_freq("struct") = 5000`, `doc_freq("device") = 1000`.

### Fix : compteur par query_text dans le cache

```rust
pub struct SfxCache {
    segments: Mutex<HashMap<SegmentId, HashMap<String, CachedSfxResult>>>,  // keyed by query_text
    doc_freq_counts: Mutex<HashMap<String, u64>>,  // query_text → count
}
```

Chaque `SuffixContainsQuery` a son propre `query_text`. En pass 1, il stocke ses résultats
sous sa clé. En pass 2, il lit son propre `doc_freq`.

### Impact

Le cache passe de `HashMap<SegmentId, CachedSfxResult>` à
`HashMap<SegmentId, HashMap<String, CachedSfxResult>>`. Le compteur passe de
`AtomicU64` à `HashMap<String, u64>`. Minime.

### Vérification

Le pass 1 fait UN seul passage sur les segments. Chaque segment appelle `scorer()`
sur le BooleanQuery, qui appelle `scorer()` sur chaque sous-query. Chaque sous-query
popule le cache sous sa propre clé. → Toujours 2 passes au total (1 count + 1 score),
pas 2 × N sous-termes.

## Problème 2 : mode non-shardé — per-segment IDF

### Le bug

Un `LucivyHandle` brut (pas ShardedHandle) ne passe pas par le search DAG.
`searcher.search(&query, &collector)` appelle `weight.scorer(seg_reader)` par segment.
Chaque segment voit son propre `doc_tf.len()` → IDF per-segment → scores faux.

Mesuré : score no-shard = 1.10, ground truth = 0.005. Erreur 220x.

### Pourquoi ça existe

C'est le comportement **standard Lucene/Tantivy**. Personne ne le corrige parce que
"après un merge, il n'y a qu'un segment et le problème disparaît". Mais :
- Les petits index ont beaucoup de segments (writer multi-threadé)
- Les index avec des commits fréquents ont des segments non-mergés
- Le score est faux entre deux merges

### Fix : two-pass dans le Weight lui-même

Le `SuffixContainsWeight` peut faire le two-pass tout seul si pas de cache externe :

```rust
impl Weight for SuffixContainsWeight {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> Result<Box<dyn Scorer>> {
        // Si pas de cache externe ET pas encore de doc_freq global...
        if self.sfx_cache.is_none() && self.global_doc_freq.is_none() {
            // On ne peut pas faire le two-pass ici car on ne connaît pas
            // les autres segments. Le Weight reçoit UN segment à la fois.
            // → Le fix doit être au niveau du Searcher, pas du Weight.
        }
    }
}
```

**Problème** : le `Weight::scorer()` reçoit un seul `SegmentReader`. Il ne peut pas
itérer les autres segments pour compter. Le two-pass doit être orchestré par l'appelant.

### Options

**Option A : `Searcher::search_two_pass()`**

Nouvelle méthode sur `Searcher` qui fait le two-pass :
```rust
impl Searcher {
    pub fn search_two_pass<C: Collector>(&self, query: &dyn Query, collector: &C) -> Result<C::Fruit> {
        // Pass 1: count (créer le cache, construire le Weight avec cache)
        let cache = Arc::new(SfxCache::default());
        // ... build query with cache, call scorer() per segment
        // Pass 2: score (lire le cache, reconstruire le Weight avec global_doc_freq)
        // ... build query with cache + doc_freq, collect results
    }
}
```

Inconvénient : le `Searcher` est dans ld-lucivy (core), `SfxCache` est dans
suffix_contains_query. Dépendance circulaire potentielle.

**Option B : dans `LucivyHandle`**

`LucivyHandle` fait le two-pass dans sa méthode `search()` :
```rust
impl LucivyHandle {
    pub fn search(&self, config: &QueryConfig, top_k: usize) -> Result<Vec<SearchResult>> {
        let searcher = self.reader.searcher();
        let cache = Arc::new(SfxCache::default());

        // Pass 1: count
        let query1 = build_query_ex(config, &self.schema, &self.index, None,
            Some(&SfxScoringOptions { sfx_cache: Some(cache.clone()), global_doc_freq: None }));
        for seg_reader in searcher.segment_readers() {
            let _ = query1.weight(enable)?.scorer(seg_reader, 1.0)?;
        }
        let global_doc_freq = cache.doc_freq_count.load(Relaxed);

        // Pass 2: score
        let query2 = build_query_ex(config, &self.schema, &self.index, sink,
            Some(&SfxScoringOptions { sfx_cache: Some(cache), global_doc_freq: Some(global_doc_freq) }));
        searcher.search(&*query2, &collector)
    }
}
```

Avantage : pas de changement dans ld-lucivy core. Le two-pass est dans lucivy_core.
Inconvénient : `LucivyHandle` n'a pas de `search()` aujourd'hui — les bindings
utilisent `searcher.search()` directement.

**Option C : dans `build_query` directement**

Le `build_query_ex` détecte que c'est un contains/startsWith et auto-injecte un cache.
Le `SuffixContainsQuery::weight()` fait le two-pass en interne en demandant au
`EnableScoring` la liste des segments.

Problème : `EnableScoring` ne donne pas accès aux segments.

### Recommandation

**Option B** — ajouter une méthode `search()` à `LucivyHandle` dans lucivy_core.
C'est l'endroit naturel. Les bindings appelleraient `handle.search()` au lieu de
`searcher.search()`. Le ShardedHandle fait déjà ça.

Ça uniformise l'API : `LucivyHandle.search()` et `ShardedHandle.search()` donnent
tous les deux le BM25 correct.

## Bench 90K avec two-pass (22 mars 2026)

Single-term contains/startsWith/fuzzy : **~620ms sur 4 shards** — pas de régression.
Le `contains_split` a doublé (2692ms vs 1525ms) → bug compteur partagé boolean.

## Décision architecture : plus de LucivyHandle public

`ShardedHandle` devient l'API publique unique :
- `shards` default à **4** (2.5-3x plus rapide que 1 shard)
- `LucivyHandle` reste en interne, utilisé par ShardedHandle
- Pas de fix two-pass pour le non-shardé — il n'existe plus côté public
- Les bindings exposent uniquement `ShardedHandle`

Avantages :
- Un seul chemin de scoring (two-pass DAG, toujours correct)
- API simple — un seul type
- Scale up transparent — changer `shards: 8` sans changer le code
- Même en mono-thread, 4 petits FST walks sont plus rapides qu'un gros

## Ordre d'implémentation

1. **Fix boolean** : compteur par query_text dans SfxCache (~20 lignes)
2. **Bench** : vérifier que contains_split revient à ~1500ms
3. **Default shards=4** dans SchemaConfig
4. **Adapter les bindings** pour utiliser uniquement ShardedHandle
