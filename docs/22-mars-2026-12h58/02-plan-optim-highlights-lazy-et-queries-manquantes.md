# Doc 02 — Plan : highlights lazy, term query optim, queries manquantes

Date : 22 mars 2026

## 1. Highlights lazy pour term/phrase (priorité haute)

### Problème

`term 'mutex'` prend 1004ms alors que `phrase 'mutex lock'` prend 1ms.
Cause : `build_term_query` met `with_prefer_sfxpost(true)` → le scorer ouvre le SFX file
et résout TOUS les postings (8850 pour "mutex") via sfxpost, même sans highlight demandé.

### Solution : scoring et highlights séparés

```
Phase 1 (scoring) : inverted index standard → BM25 → top-K       (~1ms)
Phase 2 (highlights) : sfxpost pour les K résultats seulement     (~1ms)
```

#### Implémentation

1. **Term/Phrase sans highlights** :
   - `prefer_sfxpost = false` quand pas de highlight_sink
   - Chemin standard inverted index : ~1ms

2. **Term/Phrase avec highlights** :
   - Scorer utilise le chemin standard (fast BM25 scoring)
   - Après top-K collection, step `resolve_highlights()` :
     - Lookup ordinal dans le term dict (O(1))
     - Lire sfxpost entries pour cet ordinal
     - Filtrer par les K doc_ids résultats
     - Insérer dans le HighlightSink
   - Total : ~2ms au lieu de ~1000ms

3. **Contains/startsWith** :
   - Inchangé : le SFX walk est inévitable pour trouver les matches
   - Les highlights sont un sous-produit gratuit du walk (même struct)
   - Optim mineure : skip la construction du Vec highlights quand pas de sink

#### Où implémenter resolve_highlights

Option A : dans `ShardedHandle::search()`, après le DAG
```rust
if let Some(ref sink) = highlight_sink {
    for result in &results {
        resolve_highlights_for_doc(shard, query, result.doc_address, sink);
    }
}
```

Option B : nouveau noeud DAG `ResolveHighlightsNode` après merge_results
```
... → merge_results → resolve_highlights
```

Option A est plus simple. Option B permet de paralléliser par shard.
Recommandation : Option A pour commencer, refactor en B si nécessaire.

#### Méthode resolve_highlights_for_doc

```rust
fn resolve_highlights_for_doc(
    shard: &LucivyHandle,
    term: &Term,
    doc_address: DocAddress,
    sink: &HighlightSink,
) {
    let searcher = shard.reader.searcher();
    let seg_reader = searcher.segment_reader(doc_address.segment_ord);
    let field = term.field();

    // 1. Get ordinal from term dict
    let inverted_index = seg_reader.inverted_index(field)?;
    let term_info = inverted_index.get_term_info(term)?;

    // 2. Read sfxpost for this ordinal
    let resolver = build_resolver(seg_reader, field)?;
    let entries = resolver.resolve(term_info.ordinal);

    // 3. Filter for our doc_id only
    let offsets: Vec<[usize; 2]> = entries.iter()
        .filter(|e| e.doc_id == doc_address.doc_id)
        .map(|e| [e.byte_from as usize, e.byte_to as usize])
        .collect();

    if !offsets.is_empty() {
        sink.insert(seg_reader.segment_id(), doc_address.doc_id, field_name, offsets);
    }
}
```

#### Impact attendu

| Query | Avant | Après (sans hl) | Après (avec hl) |
|-------|-------|-----------------|-----------------|
| term 'mutex' | 1004ms | ~1ms | ~5ms |
| phrase 'mutex lock' | 1ms | 1ms | ~3ms |
| contains 'mutex' | 900ms | 900ms | 900ms |

## 2. Contains/startsWith : skip highlights Vec quand pas de sink

### Problème

`run_sfx_walk` construit toujours un `Vec<(DocId, usize, usize)>` de highlights,
même quand personne ne les demande.

### Solution

Passer un flag `need_highlights: bool` à `run_sfx_walk`.
Quand false, ne pas construire le Vec highlights → skip allocations.

```rust
pub fn run_sfx_walk<F>(
    ...
    need_highlights: bool,  // NEW
) -> (Vec<(DocId, u32)>, Vec<(DocId, usize, usize)>)
```

Impact : mineur (les allocs ne sont pas le bottleneck, c'est le walk FST),
mais propre.

## 3. Queries manquantes à exposer

### Héritées de tantivy, non exposées via `build_query`

| Query | Description | Utilité |
|-------|------------|---------|
| `MoreLikeThisQuery` | "Documents similaires à celui-ci" | Recommandations, exploration |
| `DisjunctionMaxQuery` | Max score parmi N sous-queries | Multi-field search avec boost field |
| `PhrasePrefixQuery` | Phrase avec dernier token en prefix | Autocomplétion "mutex loc..." |

### Plan d'exposition

Ajouter dans `build_query` :

```rust
"more_like_this" => build_more_like_this_query(config, schema),
"disjunction_max" => build_disjunction_max_query(config, schema, index, highlight_sink),
"phrase_prefix" => build_phrase_prefix_query(config, schema, index, highlight_sink),
```

#### QueryConfig extensions nécessaires

```rust
pub struct QueryConfig {
    // ... existant ...
    // Pour more_like_this :
    pub like_doc_id: Option<u64>,
    pub min_term_freq: Option<u64>,
    pub max_query_terms: Option<usize>,
    // Pour disjunction_max :
    pub queries: Option<Vec<QueryConfig>>,  // sous-queries
    pub tie_breaker: Option<f32>,
}
```

### Priorité

1. `PhrasePrefixQuery` — autocomplétion, high value
2. `DisjunctionMaxQuery` — multi-field search
3. `MoreLikeThisQuery` — recommandations

## 4. IDF distribué — compatibilité confirmée

### Term/Phrase (standard)

IDF global via `ExportableStats` :
- `num_docs` : total docs across nodes
- `num_tokens_per_field` : pour average_fieldnorm
- `doc_freq_per_term` : depuis le term dict standard

Protocole : `export_stats()` → merge → `search_with_global_stats()`
Pas besoin de prescan — le term dict a déjà les doc_freqs.

### Contains/startsWith (SFX)

IDF global via `ExportableStats.contains_doc_freqs` :
- Prescan local → `collect_prescan_doc_freqs()` → export
- Coordinateur merge → `set_global_contains_doc_freqs()`
- 1 round-trip

### Tous les types de query sont distribués-safe.

## 5. DiagBus — câblage des events manquants

Plan détaillé dans doc 27 (dossier 20-mars-2026).

Priorité :
1. `SearchMatch` + `SearchComplete` dans `run_sfx_walk` (ground truth bench)
2. `SfxWalk` dans `suffix_contains_single_token_inner` (profiling)
3. `SfxResolve` (debug posting resolution)

Bloquant : les fonctions bas-niveau n'ont pas accès au `segment_id`.
Solution recommandée : passer `segment_id: Option<&str>` en paramètre.
