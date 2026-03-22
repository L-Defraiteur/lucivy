# Doc 23 — Options pour le prescan BM25 unifié

Date : 22 mars 2026

## Problème

`SuffixContainsQuery::weight(EnableScoring)` doit pré-scanner TOUS les segments
(tous les shards, tous les nodes) pour calculer un `doc_freq` global correct.

Aujourd'hui `EnableScoring::Enabled { searcher }` ne donne que les segments d'UN
seul shard (le `searcher_0` passé par le search DAG).

## Options

### Option A — Custom Searcher multi-shard

Créer un Searcher qui combine les segments de tous les shards.

```rust
let all_readers: Vec<SegmentReader> = shards.iter()
    .flat_map(|s| s.reader.searcher().segment_readers().to_vec())
    .collect();
let multi_searcher = Searcher::from_readers(all_readers); // n'existe pas
```

**Verdict** : impossible — un `Searcher` est lié à un seul `Index` (pour le schema,
les tokenizers, etc.). On ne peut pas combiner N indexes dans un Searcher.

### Option B — Étendre EnableScoring

Ajouter un champ `all_segment_readers` à `EnableScoring::Enabled`.

```rust
pub enum EnableScoring<'a> {
    Enabled {
        searcher: &'a Searcher,
        statistics_provider: &'a dyn Bm25StatisticsProvider,
        all_segment_readers: Option<Vec<&'a SegmentReader>>,
    },
    Disabled { ... },
}
```

`SuffixContainsQuery::weight()` utilise `all_segment_readers` si présent,
sinon `searcher.segment_readers()`.

**Avantages** :
- Changement minimal (un champ optionnel)
- Les query types qui n'en ont pas besoin l'ignorent
- Le `BuildWeightNode` collecte les segments de tous les shards

**Inconvénients** :
- `EnableScoring` est utilisé partout (30 fichiers) — ajouter un champ
  casse la construction dans tous les `EnableScoring::Enabled { ... }`
- Le `Option<Vec<&'a SegmentReader>>` est conceptuellement étrange dans
  un enum qui devrait juste dire "scoring on/off"
- Ne résout pas le cas distribué (pas de SegmentReader distant)

### Option C — Trait method sur Bm25StatisticsProvider

```rust
trait Bm25StatisticsProvider {
    fn total_num_docs(&self) -> Result<u64>;
    fn total_num_tokens(&self, field: Field) -> Result<u64>;
    fn doc_freq(&self, term: &Term) -> Result<u64>;
    fn segment_readers(&self) -> Vec<&SegmentReader> { vec![] }  // NEW
}
```

**Verdict** : conceptuellement bancal. Les segment readers n'ont rien à voir
avec les stats BM25. Et le trait est déjà implémenté par `Searcher`,
`AggregatedBm25StatsOwned`, `ExportableStats` — ajouter une méthode
oblige à l'implémenter partout.

### Option D — Paramètre build_query

```rust
pub fn build_query(
    config: &QueryConfig,
    schema: &Schema,
    index: &Index,
    highlight_sink: Option<Arc<HighlightSink>>,
    all_segment_readers: Option<&[&SegmentReader]>,  // NEW
) -> Result<Box<dyn Query>, String>
```

**Verdict** : plomberie. Chaque appel à `build_query` doit passer le paramètre.
Les bindings, les tests, les filter clauses — tous doivent être mis à jour.
Pas maintenable.

### Option E — Prescan lazy dans scorer()

Le Weight fait le prescan au premier appel à `scorer()`, en ayant stocké
les segment readers.

```rust
struct SuffixContainsWeight {
    all_segment_readers: Vec<SegmentReader>,  // owned
    prescan_done: AtomicBool,
    cache: Mutex<HashMap<SegmentId, CachedSfxResult>>,
    global_doc_freq: AtomicU64,
}
```

**Avantages** : pas de changement d'API, transparent.
**Inconvénients** :
- Le Weight doit stocker des `SegmentReader` owned → lourd
- Synchronization (Mutex) dans le scorer hot path
- Premier scorer() est lent (fait le prescan), les suivants sont rapides
- Pour le shardé, le scorer est appelé par le shard pool actor sur un
  seul shard → ne voit pas les autres shards

### Option F — EnableScoring étendu + fallback global_doc_freq

Combine B avec le chemin distribué. Trois niveaux de résolution :

1. **`all_segment_readers` fourni** (local shardé) → prescan dans `weight()`
2. **`global_doc_freq` fourni manuellement** (distribué) → skip prescan
3. **Ni l'un ni l'autre** → `searcher.segment_readers()` (non-shardé)

**Le fallback (niveau 3) existe pour** :
- Code existant qui appelle `searcher.search()` sans rien changer
- Tests unitaires
- Bindings qui construisent la query sans connaître le sharding

**Avantages** :
- Unified : un seul `weight()` gère tout
- Backward compatible : le niveau 3 est l'ancien comportement
- Optimal : prescan une seule fois dans weight(), pas de DAG spécial

**Inconvénients** :
- Même pb que B : changer `EnableScoring` touche 30 fichiers
- Trois niveaux = complexité mentale

## Question de l'utilisateur : pourquoi pas le distribué comme modèle de base ?

Citation : "pourquoi distribué serait pas le modele, et les autres juste
des wrapper qui simplifient en local ?"

### Le modèle distribué

```
1. prescan(segment_readers) → (cache, local_doc_freq)
2. aggregate(local_doc_freqs) → global_doc_freq
3. query.with_cache(cache).with_global_doc_freq(global_doc_freq)
4. weight(enable_scoring) → lit le cache, utilise global_doc_freq
5. scorer() → zéro SFX walk
```

### En local, c'est le même flow

```rust
// Non-shardé : 1 handle, N segments
let searcher = handle.reader.searcher();
let seg_readers = searcher.segment_readers();
let query = SuffixContainsQuery::new(field, text);
let (cache, doc_freq) = query.prescan(&seg_readers)?;
let query = query.with_prescan_cache(cache).with_global_doc_freq(doc_freq);
let weight = query.weight(enable_scoring)?;

// Shardé : N handles, N*M segments
let all_segs: Vec<_> = shards.iter()
    .flat_map(|s| s.reader.searcher().segment_readers().to_vec())
    .collect();
let (cache, doc_freq) = query.prescan(&all_segs)?;
let query = query.with_prescan_cache(cache).with_global_doc_freq(doc_freq);
let weight = query.weight(enable_scoring)?;

// Distribué : chaque node fait son prescan, coordinateur merge
let (cache_a, freq_a) = query.prescan(&node_a_segs)?;
let (cache_b, freq_b) = query.prescan(&node_b_segs)?;
let global_freq = freq_a + freq_b;
// Node A:
let query_a = query.with_prescan_cache(cache_a).with_global_doc_freq(global_freq);
// Node B:
let query_b = query.with_prescan_cache(cache_b).with_global_doc_freq(global_freq);
```

### Le wrapper local serait

```rust
impl ShardedHandle {
    fn search(&self, config: &QueryConfig, top_k: usize, sink: ...) {
        // Collect all segment readers from all shards
        let all_segs: Vec<&SegmentReader> = self.shards.iter()
            .flat_map(|s| s.reader.searcher().segment_readers())
            .collect();

        // Build query
        let mut query = build_query(config, &self.schema, &self.index, sink)?;

        // Prescan (the "distributed" model, but local)
        if let Some(sfx_q) = query.downcast_mut::<SuffixContainsQuery>() {
            let (cache, freq) = sfx_q.prescan(&all_segs)?;
            *sfx_q = sfx_q.clone().with_prescan_cache(cache).with_global_doc_freq(freq);
        }

        // Build weight + search (standard path)
        let weight = query.weight(enable_scoring)?;
        // ...
    }
}
```

**Problème** : `downcast_mut` sur `Box<dyn Query>` ne marche pas facilement
pour les boolean queries (il faut descendre dans les sous-queries).

### Solution propre : prescan via build_query

Au lieu de downcast, `build_query` accepte les segment readers et fait le
prescan PENDANT la construction :

```rust
pub fn build_query(
    config: &QueryConfig,
    schema: &Schema,
    index: &Index,
    highlight_sink: Option<Arc<HighlightSink>>,
    prescan_segments: Option<&[&SegmentReader]>,  // NEW
) -> Result<Box<dyn Query>, String>
```

Chaque `build_contains_query` / `build_starts_with_query` appelle `prescan()`
sur les segments fournis. Pour les boolean, chaque sous-query est pré-scannée
individuellement. Pas de downcast.

**C'est propre mais c'est Option D.**

### Meilleure solution : prescan dans weight() avec segments injectés

En fait, le plus clean c'est de NE PAS changer `build_query` ni `EnableScoring`.
Le `BuildWeightNode` fait le prescan AVANT `weight()` et passe le résultat
à la query :

```rust
// Dans BuildWeightNode::execute() :
let all_segs: Vec<&SegmentReader> = self.shards.iter()
    .flat_map(|s| s.reader.searcher().segment_readers())
    .collect();

let mut query = build_query(config, schema, index, sink)?;

// Prescan toutes les sub-queries SuffixContains dans la query tree
prescan_query_tree(&mut *query, &all_segs)?;

let weight = query.weight(enable_scoring)?;
```

Avec `prescan_query_tree` qui traverse le query tree (BooleanQuery → sub-queries)
et appelle `prescan` sur chaque `SuffixContainsQuery`.

**Problème** : `Box<dyn Query>` ne permet pas de traverser l'arbre.
Il faudrait un trait method `fn prescan_if_sfx(&mut self, segs: &[&SegmentReader])`.

### Solution finale proposée : trait method sur Query

```rust
trait Query {
    fn weight(&self, enable_scoring: EnableScoring) -> Result<Box<dyn Weight>>;
    fn query_terms<'a>(&'a self, visitor: &mut dyn FnMut(&'a Term, bool)) {}

    /// Pre-scan segment readers for global BM25 statistics.
    /// Called before weight() when cross-segment/shard consistency is needed.
    /// Default: no-op (standard queries don't need it).
    fn prescan_segments(&mut self, _segments: &[&SegmentReader]) -> Result<(), String> {
        Ok(())
    }
}
```

- `SuffixContainsQuery` implémente `prescan_segments` → fait le SFX walk, cache, count
- `BooleanQuery` implémente `prescan_segments` → appelle sur chaque sous-query
- Tous les autres : no-op (défaut)
- Le `BuildWeightNode` appelle `query.prescan_segments(&all_segs)` avant `weight()`
- Non-shardé : le code standard appelle `prescan_segments(&searcher.segment_readers())`

**Avantages** :
- Un seul modèle (distribué = base, local = wrapper)
- Pas de changement à EnableScoring
- Pas de changement à build_query
- Boolean fonctionne nativement (propagation aux sous-queries)
- Default no-op → aucun impact sur les query types existants

**Inconvénients** :
- Ajoute une méthode au trait `Query` (mais avec défaut no-op)
- `&mut self` sur `prescan_segments` → la query doit être mutable après construction

## Recommandation

**La solution "trait method sur Query"** est la plus unifiée :
- Le modèle distribué EST le modèle de base
- Local non-shardé = wrapper qui prescan avant weight()
- Local shardé = BuildWeightNode prescan avec tous les segments
- Distribué = prescan par node, coordinateur merge doc_freq, chaque node set global_doc_freq
- Boolean = propagation native via l'implémentation BooleanQuery::prescan_segments

Changements :
- `Query` trait : +1 méthode avec défaut no-op
- `SuffixContainsQuery` : implémente prescan_segments
- `BooleanQuery` : implémente prescan_segments (propage)
- `BuildWeightNode` : appelle prescan_segments avant weight()
- Aucun changement à EnableScoring, build_query, Bm25StatisticsProvider
