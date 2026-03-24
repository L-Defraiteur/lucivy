# Doc 06 — Plan : Arc<StatsProvider> unifié pour BM25 global

Date : 23 mars 2026
Branche : `feature/optional-sfx`

## Problème

`EnableScoring` porte un `&dyn Bm25StatisticsProvider` (borrow) et un `&Searcher`.
Le borrow ne survit pas au-delà de `weight()` — impossible de le stocker dans le Weight
pour l'utiliser dans `scorer()`.

Conséquences :
- `AutomatonWeight` (fuzzy/regex/TermSet) ne peut pas accéder aux stats globales au scorer time
  → patché avec `with_global_stats(num_docs, num_tokens)` (approximation : total global, doc_freq per-segment)
- `MoreLikeThisQuery` utilise `searcher.doc_freq()` au lieu du provider global
  → IDF faussé en multi-shard (voit seulement shard_0)
- Chaque nouvelle query héritée de tantivy est un risque de bug IDF

## Solution : Arc<dyn Bm25StatisticsProvider>

### EnableScoring refactoré

```rust
pub enum EnableScoring<'a> {
    Enabled {
        searcher: &'a Searcher,
        // Remplace: statistics_provider: &'a dyn Bm25StatisticsProvider
        stats: Arc<dyn Bm25StatisticsProvider + Send + Sync>,
    },
    Disabled {
        schema: &'a Schema,
        searcher_opt: Option<&'a Searcher>,
    },
}
```

Le `searcher` reste un borrow (pour schema, tokenizers, doc retrieval).
Le `stats` devient un `Arc` — partageable, stockable dans les Weights.

### Constructeurs

```rust
impl EnableScoring {
    // Local single-shard (le searcher est aussi le provider)
    pub fn enabled_from_searcher(searcher: &Searcher) -> Self {
        EnableScoring::Enabled {
            searcher,
            stats: Arc::new(SearcherStatsAdapter(searcher_clone)),
        }
    }

    // Local multi-shard (provider agrégé)
    pub fn enabled_from_stats(
        stats: Arc<dyn Bm25StatisticsProvider + Send + Sync>,
        searcher: &Searcher,
    ) -> Self {
        EnableScoring::Enabled { searcher, stats }
    }
}
```

Note : `enabled_from_searcher` a besoin d'un adapteur car le `Searcher` est
emprunté mais l'`Arc` a besoin d'un owned. Options :
- A. Le Searcher implémente `Bm25StatisticsProvider` → on clone les stats au moment de la construction
- B. On passe toujours un `Arc<AggregatedBm25StatsOwned>` même pour single-shard

Option B est plus simple : `AggregatedBm25StatsOwned::new(vec![searcher])` marche pour 1 shard.

### Impact sur les Weights

Chaque Weight qui a besoin de stats globales les stocke :

```rust
struct AutomatonWeight<A> {
    stats: Option<Arc<dyn Bm25StatisticsProvider + Send + Sync>>,
    // Supprimé : global_num_docs, global_num_tokens
    ...
}

impl AutomatonWeight {
    fn bm25_for_term(&self, term: &Term, segment_max_doc: u32, inverted_index: &InvertedIndexReader) -> Bm25Weight {
        if let Some(ref stats) = self.stats {
            let total_num_docs = stats.total_num_docs().unwrap_or(segment_max_doc as u64);
            let total_num_tokens = stats.total_num_tokens(self.field).unwrap_or(inverted_index.total_num_tokens());
            let doc_freq = stats.doc_freq(term).unwrap_or(/* per-segment fallback */);
            let avg_fieldnorm = total_num_tokens as f32 / total_num_docs as f32;
            Bm25Weight::for_one_term(doc_freq, total_num_docs, avg_fieldnorm)
        } else {
            // per-segment fallback
        }
    }
}
```

### Impact sur MoreLikeThisQuery

```rust
// Avant (buggé en multi-shard)
let doc_freq = searcher.doc_freq(term)?;
let num_docs = searcher.segment_readers().iter().map(|r| r.num_docs() as u64).sum();

// Après (global)
let doc_freq = stats.doc_freq(term).unwrap_or(0);
let num_docs = stats.total_num_docs().unwrap_or(0);
```

Le `searcher` reste utilisé pour la tokenisation (`searcher.index().tokenizers()`).

### Impact sur le DAG

```rust
// BuildWeightNode::execute()
let global_stats: Arc<dyn Bm25StatisticsProvider + Send + Sync> =
    Arc::new(AggregatedBm25StatsOwned::new(searchers));

let enable_scoring = EnableScoring::enabled_from_stats(
    global_stats,
    &searcher_0,  // juste pour schema/tokenizers
);
```

### Impact distribué

```rust
// Node distant : reçoit ExportableStats du coordinateur
let stats: Arc<dyn Bm25StatisticsProvider + Send + Sync> =
    Arc::new(received_stats);

let enable_scoring = EnableScoring::enabled_from_stats(stats, &local_searcher);
```

Même code, même trait, même flow. Local et distribué identiques.

## Fichiers à modifier

| Fichier | Changement |
|---------|-----------|
| `src/query/query.rs` | `EnableScoring`: `&dyn` → `Arc<dyn>` |
| `src/query/bm25.rs` | Ajouter `Send + Sync` bounds au trait |
| `src/query/term_query/term_query.rs` | `weight()` : extraire stats depuis Arc |
| `src/query/fuzzy_query.rs` | `weight()` : passer Arc au lieu de `with_global_stats` |
| `src/query/regex_query.rs` | idem |
| `src/query/set_query.rs` | idem |
| `src/query/automaton_weight.rs` | `stats: Option<Arc<...>>`, supprimer `global_num_docs/tokens` |
| `src/query/more_like_this/` | `create_score_term()` : utiliser stats au lieu de searcher |
| `src/query/phrase_query/suffix_contains_query.rs` | `weight()` : extraire stats depuis Arc |
| `lucivy_core/src/search_dag.rs` | `BuildWeightNode` : construire Arc une seule fois |
| `lucivy_core/src/sharded_handle.rs` | `search_with_global_stats()` : passer Arc |
| `lucivy_core/src/bm25_global.rs` | Ensure `AggregatedBm25StatsOwned: Send + Sync` |

## Étapes d'implémentation

1. Ajouter `Send + Sync` bounds à `Bm25StatisticsProvider`
2. Modifier `EnableScoring` : `Arc<dyn Bm25StatisticsProvider + Send + Sync>`
3. Adapter `enabled_from_searcher` et `enabled_from_statistics_provider`
4. Adapter `BuildWeightNode` et `ShardedHandle`
5. Modifier `AutomatonWeight` : remplacer `global_num_docs/tokens` par `Option<Arc<...>>`
6. Modifier `MoreLikeThisQuery` : utiliser stats pour doc_freq/num_docs
7. Vérifier tous les `weight()` impls
8. Tests : 1155 unit tests + bench vs tantivy

## Risques

- **Breaking change** sur `EnableScoring` — tous les callers (bindings, tests) à adapter
- **Clone coût** : `Arc::clone()` est cheap (ref count), pas de copie de données
- **`Bm25StatisticsProvider` Send + Sync** : `AggregatedBm25StatsOwned` utilise des HashMap
  → OK car immutable après construction (read-only pendant search)
- **ExportableStats** : doit aussi implémenter `Bm25StatisticsProvider` pour le distribué

## Ce qu'on supprime après

- `AutomatonWeight::global_num_docs` / `global_num_tokens` / `with_global_stats()`
- `AutomatonWeight::bm25_stats()` helper (remplacé par `stats.arc`)
- Le hack `searcher_0` comme source de stats dans `BuildWeightNode`
