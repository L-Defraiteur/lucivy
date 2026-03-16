# Progression Phases 2-5 — 16 mars 2026

## État

- **Branche** : `feature/sfx-unified`
- **Compilation** : OK, 0 erreurs
- **Tests** : 1203 passed, 0 failed, 7 ignored

## Phases terminées

### Phase 1 : ResolvedPostings ✅ (session précédente)
- Commit `bbcb225`
- `src/query/resolved_postings.rs` — adapte `Vec<PostingEntry>` → `Postings + DocSet`
- 7 tests unitaires

### Phase 2 : AutomatonWeight → PostingResolver ✅
- **Fichier** : `src/query/automaton_weight.rs`
- `collect_ordinals()` dans `impl<A>` — collecte ordinals via .sfx
- `scorer_from_ordinals()` dans `impl<A>` — 4 branches (highlight+scoring, highlight only, scoring only, fast) via PostingResolver
- `scorer_from_term_infos()` dans `impl<A>` — ancien code extrait (fallback)
- `scorer()` dans `impl Weight` — try ordinals quand .sfx + .sfxpost, sinon fallback
- `for_each_doc_group()` — helper zero-alloc itération entries groupées par doc_id
- **Guard .sfxpost** : le path ordinal n'est activé que si .sfxpost existe (sinon InvertedIndexResolver échoue pour champs Basic)

### Phase 3 : TermWeight → PostingResolver ✅
- **Fichier** : `src/query/term_query/term_weight.rs`
- `ResolvedTermScorer` — scorer léger wraps `ResolvedPostings` + BM25 + highlights, pas de BlockWAND
- `TermOrEmptyOrAllScorer::ResolvedScorer` — nouvelle variante enum
- `specialized_scorer()` — try .sfxpost (get_ordinal → resolver → ResolvedPostings → ResolvedTermScorer), fallback TermInfo → SegmentPostings → TermScorer
- `count()` — rerouté via .sfxpost (doc_freq depuis resolver)
- `for_each_pruning` — itération linéaire sans BlockWAND pour ResolvedScorer
- Tous les match arms mis à jour (explain, for_each, for_each_no_score, quickwit async)

### Phase 4 : AutomatonPhraseWeight → PostingResolver ✅
- **Fichier** : `src/query/phrase_query/automaton_phrase_weight.rs`
- `cascade_ordinals()` / `prefix_ordinals()` — versions ordinales de cascade_term_infos / prefix_term_infos
- `build_union_from_ordinals()` — construit `SimpleUnion<Box<dyn Postings>>` depuis ordinals, même type que `get_union_from_term_infos`
- `phrase_scorer()` — conditionnel .sfxpost : ordinals → union → PhraseScorer/ContainsScorer, fallback TermInfo
- `single_token_scorer()` — conditionnel .sfxpost : ordinals → resolver → bitset, fallback block_postings

### Phase 5 : BM25 weight() sans ._raw ✅
- **Fichier** : `src/query/phrase_query/suffix_contains_query.rs`
- `weight()` ne lit plus `Bm25Weight::for_terms(statistics_provider, &[term])` (qui lisait ._raw)
- `scorer()` calcule BM25 per-segment : `doc_freq = doc_tf.len()` (réel), `total_num_tokens` depuis metadata inverted_index
- Plus précis qu'avant (doc_freq du nombre réel de docs matchés vs lookup du terme exact)

## Architecture résultante

```
Quand .sfx + .sfxpost disponibles :

  SfxTermDictionary
    → raw_ordinal (u64)
      → PostingResolver::resolve(ordinal)
        → Vec<PostingEntry> (doc_id, position, byte_from, byte_to)
          → ResolvedPostings / for_each_doc_group / build_union_from_ordinals
            → Scorer (même scorers, source de données différente)

Fallback (pas de .sfxpost, JSON fields, vieux segments) :

  SfxTermDictionary ou TermDictionary
    → TermInfo
      → InvertedIndexReader::read_postings_from_terminfo()
        → SegmentPostings
          → Scorer (path classique inchangé)
```

## Reste à faire

### Phase 6 : Merger .sfxpost
- **Fichier** : `src/indexer/merger.rs`
- `merge_sfx()` ne reconstruit pas le .sfxpost après merge
- Plan : collecter tokens depuis .sfx source (SI=0), reconstruire .sfxpost avec remapping doc_ids et ordinals

### Phase 7 : Supprimer ._raw
- `handle.rs` : ne plus créer le champ ._raw
- Supprimer `RAW_SUFFIX`, `raw_field_pairs`
- Bindings : ne plus auto-dupliquer vers ._raw
- Virer tous les fallback InvertedIndexResolver et paths TermInfo
- SfxCollector s'attache au champ principal
- Le guard `.sfxpost_file().is_some()` devient inutile (toujours vrai)

## Points d'attention

1. **Guard .sfxpost** : Toutes les phases 2-4 vérifient `reader.sfxpost_file(field).is_some()` avant d'utiliser le path ordinal. Sans .sfxpost, InvertedIndexResolver ne gère pas les champs Basic (pas de positions/offsets). Ce guard sera supprimé en Phase 7.

2. **BlockWAND perdu en Phase 3** : `ResolvedTermScorer` n'a pas de block structure → pas de BlockWAND dans `for_each_pruning`. Impact marginal (BlockWAND surtout utile en union queries).

3. **inverted_index encore chargé** : Les phases 2-4 chargent encore `reader.inverted_index(field)` pour :
   - `SfxTermDictionary::new` (besoin du termdict pour le constructeur)
   - `total_num_tokens()` (metadata BM25)
   Mais ne lisent plus de posting data depuis l'inverted index quand .sfxpost est disponible. Ce sera supprimé en Phase 7.

4. **total_num_tokens** : En Phase 7, sans inverted_index, il faudra soit :
   - Stocker cette metadata dans le .sfxpost
   - Calculer depuis les fieldnorms : `sum(fieldnorm(doc) for doc in 0..max_doc)`
