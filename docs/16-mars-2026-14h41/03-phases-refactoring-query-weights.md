# Refactoring des Query Weights — Plan par phases

## Objectif

Faire que `AutomatonWeight`, `TermWeight` et `AutomatonPhraseWeight` puissent fonctionner sans l'inverted index du ._raw, en utilisant le `PostingResolver` (.sfxpost) comme source de données.

## Architecture actuelle

```
SfxTermDictionary (ou TermDictionary)
  → TermInfo (doc_freq, postings_range, positions_range, offsets_range)
    → InvertedIndexReader::read_postings_from_terminfo()
      → SegmentPostings (bloc-encoded, lazy iteration)
        → Scorer (TermScorer / AutomatonScorer / PhraseScorer / ContainsScorer)
```

## Architecture cible

```
SfxTermDictionary
  → raw_ordinal (u64)
    → PostingResolver::resolve(ordinal)
      → Vec<PostingEntry> (doc_id, position, byte_from, byte_to)
        → ResolvedPostings (adapte PostingEntry → trait Postings)
          → Scorer (même scorers, juste une autre source de données)
```

## Constats clés

1. **PostingEntry a tout ce qu'il faut** : doc_id, position, byte_from, byte_to — c'est exactement ce que les scorers lisent depuis SegmentPostings
2. **3 code paths dans AutomatonWeight** : highlight (positions+offsets), BM25 (freqs only), fast (doc_ids only) — tous dérivables de PostingEntry
3. **TermScorer utilise block_max optimization** — on la perd avec PostingResolver mais c'est acceptable (perf marginale pour la plupart des queries)
4. **PhraseScorer/ContainsScorer** ont besoin de positions par doc pour valider l'adjacence — PostingEntry les fournit
5. **BM25 doc_freq** : actuellement lu depuis TermInfo. Avec PostingResolver, c'est `resolver.doc_freq(ordinal)`

## Phases

### Phase 1 : ResolvedPostings — adaptateur PostingEntry → Postings trait

**Nouveau fichier** : `src/query/resolved_postings.rs`

Implémente `DocSet + Postings` à partir d'un `Vec<PostingEntry>` pré-groupé par doc_id :

```rust
pub struct ResolvedPostings {
    docs: Vec<DocGroup>,  // sorted by doc_id
    cursor: usize,
}

struct DocGroup {
    doc_id: DocId,
    entries: Vec<PostingEntry>,  // positions+offsets for this doc
}
```

Méthodes :
- `DocSet::doc()`, `advance()`, `seek()` — itère les DocGroups
- `Postings::term_freq()` — `self.docs[cursor].entries.len()`
- `Postings::append_positions_with_offset()` — slice depuis entries
- `Postings::append_offsets()` — (byte_from, byte_to) depuis entries
- `Postings::append_positions_and_offsets()` — les trois

Constructeur :
```rust
pub fn from_entries(entries: Vec<PostingEntry>) -> Self
// Groupe par doc_id (entries déjà triées par (doc_id, position))
```

**Tests** : vérifier que `ResolvedPostings` produit les mêmes résultats qu'un `SegmentPostings` pour les mêmes données.

**Estimation** : ~150 lignes

### Phase 2 : AutomatonWeight — utilise PostingResolver

**Fichier** : `src/query/automaton_weight.rs`

Changements :
1. `collect_term_infos()` → `collect_ordinals()` : retourne `Vec<(u64, u32)>` (ordinal, doc_freq) au lieu de `Vec<TermInfo>`
2. Quand .sfxpost dispo : `SfxTermDictionary.search_automaton()` retourne les ordinals, `PostingResolver` fournit les entries
3. `scorer()` : pour chaque ordinal, `resolver.resolve(ord)` → `ResolvedPostings` → même logique de scoring (3 paths)
4. BM25 : `doc_freq` depuis `resolver.doc_freq(ord)`, `total_num_tokens` depuis fieldnorms

Changements dans `SfxTermDictionary` :
- Ajouter `search_automaton_ordinals()` → `Vec<(String, u64)>` (terme, ordinal) au lieu de `Vec<(String, TermInfo)>`
- Idem pour `get_ordinal()`, `range_scan_ordinals()`

**Fallback** : si pas de .sfxpost, garder le path actuel (TermInfo → SegmentPostings)

**Tests** : tests de parité — mêmes résultats via .sfxpost et via inverted_index

**Estimation** : ~200 lignes

### Phase 3 : TermWeight — utilise PostingResolver

**Fichier** : `src/query/term_query/term_weight.rs`

Changements :
1. `scorer()` : quand .sfxpost dispo, `SfxTermDictionary.get_ordinal()` → `resolver.resolve(ord)` → `ResolvedPostings` → `TermScorer`
2. BM25 : `doc_freq` depuis resolver
3. Perte : block_max optimization (acceptable)

**Note** : TermQuery est per-token exact match (SI=0) → c'est déjà ce que SfxTermDictionary filtre.

**Estimation** : ~100 lignes

### Phase 4 : AutomatonPhraseWeight — utilise PostingResolver

**Fichier** : `src/query/phrase_query/automaton_phrase_weight.rs`

Changements :
1. `cascade_term_infos()` → `cascade_ordinals()` : retourne ordinals
2. `prefix_term_infos()` → `prefix_ordinals()` : retourne ordinals
3. Pour chaque ordinal : `resolver.resolve(ord)` → `ResolvedPostings`
4. `PhraseScorer` / `ContainsScorer` reçoivent `ResolvedPostings` au lieu de `SegmentPostings`

**Nuance** : `PhraseScorer` construit une Union de postings par position. Avec `ResolvedPostings`, c'est le même pattern — une Union de `ResolvedPostings`.

**Estimation** : ~200 lignes

### Phase 5 : BM25 weight() sans ._raw

**Fichier** : `src/query/phrase_query/suffix_contains_query.rs`

Actuellement `Bm25Weight::for_terms(statistics_provider, &[term])` lit le ._raw term dict.

Changements :
- Déplacer la construction du `Bm25Weight` dans `scorer()` (per-segment)
- Utiliser `resolver.doc_freq(ordinal)` + `reader.max_doc()` + fieldnorms
- `Bm25Weight::for_one_term(doc_freq, total_docs, avg_fieldnorm)`

**Estimation** : ~50 lignes

### Phase 6 : Merger .sfxpost

**Fichier** : `src/indexer/merger.rs`

Pour chaque champ avec .sfx :
1. Lire les .sfx source → stream SI=0 → collecter les tokens (remplace `inverted_index.terms()`)
2. Construire le nouveau BTreeSet → nouveaux ordinals
3. Pour chaque token : lire les .sfxpost source → remapper doc_ids via doc_mapping → remapper ordinals via le BTreeSet
4. Écrire le .sfxpost mergé

**Estimation** : ~150 lignes

### Phase 7 : Supprimer ._raw

Une fois les phases 1-6 validées (tous les tests passent) :
1. `handle.rs` : ne plus créer le champ ._raw
2. Supprimer `RAW_SUFFIX`, `raw_field_pairs`
3. Bindings : ne plus auto-dupliquer vers ._raw
4. Virer les fallback `InvertedIndexResolver` et les paths `TermInfo`
5. Le SfxCollector s'attache au champ principal ou fonctionne indépendamment

**Estimation** : ~200 lignes de suppressions

## Ordonnancement

Les phases sont indépendantes verticalement (chaque phase ajoute du code sans casser l'existant grâce au fallback) :

```
Phase 1 (ResolvedPostings)
  ↓
Phase 2 (AutomatonWeight) ─┐
Phase 3 (TermWeight)       ├─ parallélisables
Phase 4 (AutomatonPhrase)  ┘
  ↓
Phase 5 (BM25)
  ↓
Phase 6 (Merger)
  ↓
Phase 7 (Supprimer ._raw)
```

Chaque phase est un commit indépendant avec fallback. On peut valider les tests à chaque étape.

## Total estimé

~1050 lignes de code nouveau/modifié, ~500 lignes supprimées (code mort ._raw).
