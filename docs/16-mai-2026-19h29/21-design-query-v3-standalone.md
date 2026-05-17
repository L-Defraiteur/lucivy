# Design : Query types v3 standalone (suppression wrappers)

**Date** : 17 mai 2026  
**Problème** : les types v3 (`ContainsQueryV3`, `FuzzyQueryV3`, `RegexQueryV3`) wrappent les types v2 (`SuffixContainsQuery`, `RegexContinuationQuery`). Ça crée des bugs de cache injection, mismatch de clés, double prescan, conversions de formats.  
**Solution** : types v3 standalone qui possèdent leur propre prescan/cache/weight. Les v2 deviennent des alias.

---

## 1. Ce qui existe aujourd'hui

```
ContainsQueryV3 { inner: SuffixContainsQuery }
  prescan_segments() → détecte v3/v2, remplit cache, injecte dans inner
  weight() → inner.weight() → SuffixContainsWeight → scorer

FuzzyQueryV3 { inner: RegexContinuationQuery, prescan_cache: ... }
  prescan_segments() → remplit cache local, PAS injecté dans inner
  weight() → convertit cache → inject dans inner → inner.weight()

RegexQueryV3 { inner: RegexContinuationQuery, prescan_cache: ... }
  même pattern que fuzzy
```

**Problèmes :**
- Cache key mismatch (`"field:query"` vs `"query"`)
- Cache format mismatch (`CachedSfxResult` vs `CachedPrescanResult`)
- `weight()` auto-prescan dans inner → crash sur SFX3
- Double indirection pour highlights et BM25 global
- `inject_regex_prescan_cache` = conversion manuelle fragile

## 2. Architecture cible

### Principe

Le **Weight/Scorer est générique** : il consomme `(doc_tf, highlights)` du cache prescan et fait du BM25. Que le prescan soit v2 ou v3, le scoring est identique. Donc :

- Extraire `SfxWeight` + `SfxScorer` comme structs publiques réutilisables
- Les query types v3 possèdent directement leur cache et créent le weight sans inner

### Types

```rust
// ═══ Shared scoring layer (pub) ═══

/// Prescan result for one segment.
pub struct CachedPrescan {
    pub doc_tf: Vec<(DocId, u32)>,
    pub highlights: Vec<(DocId, usize, usize)>,
}

/// Weight qui consomme un cache prescan → produit un SfxScorer.
pub struct SfxWeight {
    raw_field: Field,
    cache_key: String,
    prescan_cache: HashMap<(String, SegmentId), CachedPrescan>,
    global_doc_freq: u64,
    highlight_sink: Option<Arc<HighlightSink>>,
    highlight_field_name: String,
}

/// Scorer qui itère sur doc_tf avec BM25.
pub struct SfxScorer { ... }  // = l'actuel SuffixContainsScorer, renommé
```

```rust
// ═══ Query types v3 (standalone) ═══

pub struct ContainsQueryV3 {
    field: Field,
    query_text: String,
    anchor_start: bool,
    exact_match: bool,
    strict_separators: bool,
    highlight_sink: Option<Arc<HighlightSink>>,
    highlight_field_name: String,
    // Prescan state
    prescan_cache: HashMap<(String, SegmentId), CachedPrescan>,
    global_doc_freq: u64,
}

pub struct FuzzyQueryV3 {
    field: Field,
    query_text: String,
    distance: u8,
    strict_separators: bool,
    highlight_sink: Option<Arc<HighlightSink>>,
    highlight_field_name: String,
    prescan_cache: HashMap<(String, SegmentId), CachedPrescan>,
    global_doc_freq: u64,
}

pub struct RegexQueryV3 {
    field: Field,
    pattern: String,
    anchor_start: bool,
    highlight_sink: Option<Arc<HighlightSink>>,
    highlight_field_name: String,
    prescan_cache: HashMap<(String, SegmentId), CachedPrescan>,
    global_doc_freq: u64,
}
```

### Aliases v2

```rust
// Pour compat build_query() et le reste du code
pub type SuffixContainsQuery = ContainsQueryV3;
// RegexContinuationQuery reste pour les cas non-SFX (phrase regex pur)
// mais le fuzzy d>0 et regex SFX passent par FuzzyQueryV3/RegexQueryV3
```

## 3. Trait Query — implémentation commune

Chaque type v3 implémente `Query` avec le même pattern :

```rust
impl Query for ContainsQueryV3 {
    fn prescan_segments(&mut self, segments: &[&SegmentReader]) -> Result<()> {
        for seg_reader in segments {
            let sfx_bytes = ...;
            let version = detect_sfx_version(&sfx_bytes);
            let (doc_tf, highlights) = match version {
                Some(3) => self.prescan_v3(seg_reader, &sfx_bytes),
                _       => self.prescan_v2(seg_reader, &sfx_bytes),
            }?;
            let key = self.cache_key();
            self.prescan_cache.insert((key, segment_id), CachedPrescan { doc_tf, highlights });
        }
        self.global_doc_freq = self.prescan_cache.values()
            .map(|c| c.doc_tf.len() as u64).sum();
        Ok(())
    }

    fn weight(&self, enable_scoring: EnableScoring) -> Result<Box<dyn Weight>> {
        // Auto-prescan si pas encore fait
        if self.prescan_cache.is_empty() {
            if let Some(searcher) = enable_scoring.searcher() {
                let mut clone = self.clone();
                let segs: Vec<&SegmentReader> = searcher.segment_readers().iter().collect();
                clone.prescan_segments(&segs)?;
                return clone.make_weight(enable_scoring);
            }
        }
        self.make_weight(enable_scoring)
    }

    fn collect_prescan_doc_freqs(&self, out: &mut HashMap<String, u64>) {
        out.insert(self.cache_key(), self.global_doc_freq);
    }

    fn set_global_contains_doc_freqs(&mut self, freqs: &HashMap<String, u64>) {
        if let Some(&freq) = freqs.get(&self.cache_key()) {
            self.global_doc_freq = freq;
        }
    }
}
```

`make_weight()` crée un `SfxWeight` avec le cache, qui dans son `scorer()` :
1. Cherche dans le cache par `(cache_key, segment_id)` — toujours trouvé car prescan peuple tout
2. Crée un `SfxScorer` avec BM25 + highlights
3. Pas de fallback SFX v2 dans le scorer (le prescan a déjà tout résolu)

## 4. Prescan v2 fallback

Chaque query type a une méthode `prescan_v2()` qui réutilise le code existant :

```rust
fn prescan_v2(&self, seg_reader: &SegmentReader, sfx_bytes: &[u8])
    -> Result<(Vec<(DocId, u32)>, Vec<(DocId, usize, usize)>)>
{
    // Réutilise run_sfx_walk / prescan_regex / prescan_fuzzy existants
    // Ces fonctions restent dans suffix_contains.rs et regex_continuation_query.rs
    // comme fonctions pub standalone (pas méthodes de query)
}
```

Les fonctions v2 (`run_sfx_walk`, `prescan_regex`, `prescan_fuzzy`) deviennent des fonctions libres réutilisables, pas des méthodes de query.

## 5. Distributed / multi-shard BM25

Le flow sharded ne change pas :

```
ShardedHandle.search()
  1. query.prescan_segments(all_segs)        → cache rempli, doc_freq calculé
  2. query.collect_prescan_doc_freqs()       → {"1:mutex_lock": 42}
  3. coordinator merge across shards          → global IDF
  4. query.set_global_contains_doc_freqs()   → self.global_doc_freq = merged
  5. query.weight()                          → SfxWeight avec global IDF correct
  6. weight.scorer(segment)                  → SfxScorer avec BM25 global
```

Tout est direct — pas de délégation à un inner. Le cache_key est cohérent partout. Le scorer ne fait JAMAIS de fallback SFX.

## 6. Fichiers à modifier

| Fichier | Action |
|---------|--------|
| `src/query/contains_query_v3.rs` | Réécrire standalone, pas de inner |
| `src/query/fuzzy_query_v3.rs` | Réécrire standalone |
| `src/query/regex_query_v3.rs` | Réécrire standalone |
| `src/query/phrase_query/suffix_contains_query.rs` | Extraire `SfxWeight`/`SfxScorer` pub. `SuffixContainsQuery` → alias |
| `src/query/phrase_query/regex_continuation_query.rs` | Extraire `prescan_regex`/`prescan_fuzzy` comme fn libres |
| `src/query/phrase_query/mod.rs` | Re-exports |
| `src/query/mod.rs` | Re-exports, SuffixContainsQuery alias |
| `lucivy_core/src/query.rs` | `build_query` simplifié (crée toujours les types v3) |

## 7. Ce qu'on garde tel quel

- `SfxFileReader` (v2) et `SfxFileReaderV3` — les deux reader restent
- `run_sfx_walk`, `prescan_regex`, `prescan_fuzzy` — deviennent des fn libres pub
- Les briques v3 (`fst_walk`, `resolve`, `composite`, `orchestrator`) — inchangées
- `SfxBuildOutput`/`SfxBuildOutputV3` — inchangés (indexation séparée de la query)
- Le pipeline d'indexation v3 dans `segment_writer.rs` — inchangé

## 8. Ordre d'implémentation

1. **Extraire SfxWeight/SfxScorer** de `suffix_contains_query.rs` → struct pub dans un fichier séparé (`sfx_scoring.rs`)
2. **Réécrire ContainsQueryV3** standalone avec prescan v2/v3 + `make_weight()` → `SfxWeight`
3. **Type alias** `SuffixContainsQuery = ContainsQueryV3`
4. **Réécrire FuzzyQueryV3** standalone
5. **Réécrire RegexQueryV3** standalone
6. **Tests** : les 9 pipeline tests + les 69 briques tests doivent passer
7. **Cleanup** : supprimer le code wrapper mort

## 9. Risque

Le seul risque : le prescan v2 fallback utilise du code qui est actuellement dans des méthodes privées de `SuffixContainsQuery` / `RegexContinuationQuery`. Il faut les extraire en fonctions libres. C'est du refactoring mécanique, pas de changement logique.
