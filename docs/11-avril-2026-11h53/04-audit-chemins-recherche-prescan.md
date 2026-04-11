# Audit : chemins de recherche, prescan, et fallbacks

Date : 11 avril 2026

## Constat

Il existe **deux chemins** pour exécuter une recherche fuzzy/contains :

1. **Chemin DAG** : prescan parallèle → merge stats → IDF global → build_weight → search
2. **Chemin direct** : `searcher.search(&query, &collector)` → scorer inline → IDF local

Le problème : **seul ShardedHandle utilise le DAG**. Tous les autres chemins
(LucivyHandle, les 6 bindings, tous les tests) passent par le chemin direct
et n'ont ni prescan, ni IDF global, ni coverage boost correct.

## Qui utilise quoi

### Chemin DAG (avec prescan + IDF global)

| Appelant | Fichier | Prescan | IDF | Coverage |
|----------|---------|---------|-----|----------|
| ShardedHandle.search() | sharded_handle.rs:1390 | Oui (parallèle) | Global | Oui |
| ShardedHandle.search_with_global_stats() | sharded_handle.rs:1477 | Oui (local) | Global (injecté) | Oui |
| ShardedHandle.export_stats() | sharded_handle.rs:1436 | Oui (tous) | Global | N/A |

### Chemin direct (SANS prescan)

| Appelant | Fichier | Prescan | IDF | Coverage |
|----------|---------|---------|-----|----------|
| LucivyHandle (toutes méthodes) | handle.rs:379+ | Non | Local | Fallback |
| CXX bridge (rag3db) | lucivy_fts/bridge.rs:313 | Non | Local | Fallback |
| WASM emscripten | bindings/emscripten:740 | Non | Local | Fallback |
| WASM wasm-bindgen | bindings/wasm:551 | Non | Local | Fallback |
| Node.js napi | bindings/nodejs:250 | Non | Local | Fallback |
| Python PyO3 | bindings/python:350 | Non | Local | Fallback |
| C++ standalone | bindings/cpp:345 | Non | Local | Fallback |
| test_fuzzy_monotonicity | tests:147 | Non | Local | Fallback |
| test_playground_repro | tests:209 | Non | Local | Fallback |
| test_fuzzy_ground_truth | tests:417 | Non | Local | Fallback |
| bench_sharding | benches:177 | Non | Local | Fallback |

## Comment le fallback fonctionne

Dans `RegexContinuationWeight::scorer()` :

```rust
// FAST PATH : cache du prescan dispo (DAG)
if let Some(cached) = self.regex_prescan_cache.get(&segment_id) {
    // → utilise doc_tf, highlights, doc_coverage du cache
    return self.build_scorer(reader, boost, cached.doc_tf, cached.doc_coverage);
}

// SLOW PATH : fallback inline
let (doc_tf, highlights, doc_coverage) = self.run_regex_fallback(reader)?;
// → DFA compilé à la volée, SFX walk per-segment
self.build_scorer(reader, boost, doc_tf, doc_coverage)
```

Le fallback `run_regex_fallback` :
- Pour fuzzy d>0 : appelle `fuzzy_contains()` → retourne doc_coverage ✓
- Pour d=0 : DFA continuation → pas de coverage (Vec::new())
- Pour regex : DFA continuation → pas de coverage

**Le coverage EST propagé** dans le fallback (fix du 11 avril). Mais l'IDF
est local (pas globalisé cross-segments).

## Pourquoi le playground/emscripten fonctionne différemment

Le playground utilise le binding WASM emscripten qui fait :

```rust
// bindings/emscripten/src/lib.rs
let query = build_query(&config, &handle.schema, &handle.index, sink)?;
let searcher = handle.reader.searcher();
execute_top_docs(&searcher, query.as_ref(), limit)?
```

C'est le chemin direct. Pas de DAG, pas de prescan. Le `scorer()` tombe
dans le slow path et fait le fuzzy/regex inline par segment.

**Pourquoi c'est comme ça** : le playground est single-shard (un seul index),
pas besoin de coordination multi-shard. Le DAG a été conçu pour le sharding
distribué.

**Le test de monotonie reproduit ce flow** : il fait `searcher.search()`
directement pour simuler le même chemin que le playground. C'est correct
pour vérifier la monotonie (d=0 ⊆ d=1) mais pas pour vérifier le ranking
(car l'IDF est local).

## Le problème de l'IDF local

Avec un index multi-segments (pas de drain_merges), chaque segment a son
propre nombre de docs et son propre doc_freq par terme. L'IDF varie :

- Segment 0 (4000 docs) : IDF("rag3db") = log(4000/200) = 2.99
- Segment 3 (67 docs) : IDF("rag3db") = log(67/5) = 2.59

Le score BM25 d'un même match diffère entre segments. Avec `coverage * 1000`,
la différence est noyée. Mais sans coverage (d=0, regex), les scores ne
sont pas comparables entre segments.

## Ce qu'il faudrait faire pour unifier

### Option A : prescan automatique dans weight() (recommandé)

Quand `regex_prescan_cache` est vide et que la query est SFX-based
(contains, startsWith, regex, fuzzy), `weight()` appelle automatiquement
`prescan_segments()` avec tous les segments du searcher.

```rust
fn weight(&self, enable_scoring: EnableScoring<'_>) -> Result<Box<dyn Weight>> {
    // Si pas de cache et query SFX → prescan automatique
    if self.regex_prescan_cache.is_none() && self.needs_prescan() {
        let mut this = self.clone();
        if let EnableScoring::Enabled { searcher, .. } = &enable_scoring {
            let seg_refs: Vec<&SegmentReader> = searcher.segment_readers().collect();
            this.prescan_segments(&seg_refs)?;
        }
        return this.weight(enable_scoring);
    }
    // ... build weight avec cache peuplé
}
```

**Avantages** :
- Un seul chemin pour tout le monde
- IDF globalisé automatiquement
- Coverage boost partout
- Pas de fallback inline

**Inconvénients** :
- prescan_segments est &mut self → problème de borrowing dans weight()
  qui prend &self. Faut restructurer.
- Le prescan est séquentiel (pas parallèle comme dans le DAG)

### Option B : prescan dans Searcher::search() (intrusif)

Modifier `Searcher::search()` pour détecter les queries SFX et trigger
le prescan avant l'exécution.

**Avantages** : transparent pour tous les callers
**Inconvénients** : modifie le code tantivy-base, très intrusif

### Option C : prescan dans build_query() (pragmatique)

`build_query()` retourne un `Box<dyn Query>`. Avant de retourner, si la
query est SFX-based, on pourrait créer un wrapper qui trigger le prescan
au premier appel à `weight()`.

```rust
struct AutoPrescanWrapper {
    inner: Box<dyn Query>,
    prescanned: AtomicBool,
}
```

**Avantages** : un seul point de modification
**Inconvénients** : wrapper pattern, complexité

### Option D : forcer le DAG partout (maximal)

Remplacer `LucivyHandle.search()` par un mini-DAG (même pour single-shard).
Le DAG gère le prescan, le merge, le build_weight, le search.

```rust
impl LucivyHandle {
    pub fn search(&self, config: &QueryConfig, ...) -> Result<Vec<SearchResult>> {
        let dag = build_search_dag_single_shard(
            &self.reader, &self.schema, config, top_k, sink
        )?;
        execute_dag(&mut dag, None)?
            .take_output("merge", "results")
    }
}
```

**Avantages** : un seul chemin, même code que le sharding
**Inconvénients** : overhead du DAG pour un single-shard (threads, scheduling)

## Recommandation

**Option A** (prescan automatique dans weight) est le meilleur compromis.
Le prescan séquentiel est négligeable pour single-shard (un seul segment
après merge, ou quelques segments). L'overhead est nul quand le cache est
déjà peuplé (DAG path).

Le restructuring nécessaire :
1. `prescan_segments` prend `&mut self` → stocker le cache dans un `RefCell`
   ou `Mutex` pour permettre l'appel depuis `weight()` qui est `&self`
2. Ou : faire le prescan dans `weight()` en retournant un Weight qui contient
   le cache (pas besoin de stocker dans le Query)

### Étape par étape

1. Dans `RegexContinuationQuery::weight()` : si `regex_prescan_cache.is_none()`,
   construire le cache inline pour tous les segments
2. Passer le cache au `RegexContinuationWeight`
3. Le scorer utilise toujours le fast path (cache dispo)
4. Supprimer `run_regex_fallback` (plus de slow path)
5. Même chose pour `SuffixContainsQuery`

### Impact sur les bindings

Aucun changement dans les bindings. Le `weight()` fait le prescan
automatiquement quand nécessaire. Les bindings continuent à faire
`searcher.search()` directement.

### Impact sur les tests

Les tests obtiennent l'IDF globalisé automatiquement. Le ranking test
devrait passer sans modification.

## Code mort à supprimer après unification

1. `fuzzy_contains_via_trigram()` — l'ancien pipeline DFA (plus appelé)
2. `intersect_trigrams_with_threshold()` — plus utilisé par le fuzzy
3. `run_regex_fallback()` — plus de slow path après unification
4. Le code DFA dans la partie inline du scorer (lignes 1795-1855)
5. Les variables `posmap_bytes`, `bytemap_bytes`, `sepmap_bytes` dans le scorer
