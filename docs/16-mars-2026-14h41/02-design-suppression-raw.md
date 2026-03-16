# Design : Suppression complète du ._raw — Plan détaillé

## Objectif

Supprimer le champ ._raw du schema. Le `.sfx` + `.sfxpost` fournissent tout : suffix FST, GapMap, posting entries. Plus besoin du ._raw inverted index (FST + TermInfoStore + postings).

## Dépendances restantes sur ._raw

### 1. SfxTermDictionary wraps TermDictionary du ._raw

**Fichier** : `src/suffix_fst/term_dictionary.rs`

**Problème** : `SfxTermDictionary::new(&sfx_reader, inverted_index.terms())` — le 2ème arg est le term dict du ._raw. Utilisé par :
- `get()` → retourne `TermInfo`
- `term_info_from_ord()` → retourne `TermInfo`
- `search_automaton()` → retourne `Vec<(String, TermInfo)>`
- `range_scan()` → retourne `Vec<(String, TermInfo)>`

**Appelants** (tous font `inverted_index.terms()` pour obtenir le term dict) :
- `automaton_phrase_weight.rs` : `cascade_term_infos()` + `prefix_term_infos()` (startsWith/fuzzy)
- `automaton_weight.rs` : `AutomatonWeight::scorer()` (fuzzy/regex standard)
- `term_weight.rs` : `TermWeight::scorer()` (term exact)
- `regex_continuation_query.rs` : `continuation_score()` (déjà rerouté via PostingResolver, mais construit encore un SfxTermDictionary)

**Solution** : `SfxTermDictionary` retourne des `raw_ordinal` au lieu de `TermInfo`. Les appelants résolvent via `PostingResolver` (le trait qu'on a créé dans regex_continuation_query.rs).

### 2. BM25 `for_terms` lit le ._raw term dict

**Fichier** : `src/query/phrase_query/suffix_contains_query.rs` (ligne 118-127)

**Problème** : `Bm25Weight::for_terms(statistics_provider, &[term])` lit `doc_freq` depuis le champ ._raw.

**Solution** : Remplacer par `Bm25Weight::for_one_term(doc_freq, total_docs, avg_fieldnorm)` où `doc_freq` vient du `.sfxpost`. Mais `for_one_term` est appelé dans `weight()` (avant d'avoir accès aux segments). Deux options :
- **Option A** : Déplacer le calcul BM25 dans le `scorer()` (per-segment) où on a accès au `.sfxpost`. Construire le Bm25Weight par segment.
- **Option B** : Stocker `doc_freq` global dans le `.sfxpost` (agrégé across segments). Plus complexe.

**Choix** : Option A — le BM25 par segment est standard (c'est ce que tantivy fait en interne de toute façon, le weight agrège mais chaque segment a ses propres stats).

### 3. Merger lit `inverted_index(field).terms()` du ._raw

**Fichier** : `src/indexer/merger.rs` (lignes 648-660)

**Problème** : Collecte les unique_tokens depuis le term dict du ._raw pour reconstruire le suffix FST.

**Solution** : Lire les tokens depuis les `.sfx` source (stream SI=0 du suffix FST). Les tokens SI=0 sont exactement les termes du ._raw.

### 4. Merger ne reconstruit pas le `.sfxpost`

**Problème** : Après merge, pas de `.sfxpost` → fallback sur inverted_index.

**Solution** : Pour chaque token (ordinal dans le nouveau BTreeSet), lire les `.sfxpost` source et remapper :
- `doc_id` : via `doc_mapping` (old_doc → new_doc)
- `ordinal` : via mapping `token_text → new_ordinal` (BTreeSet position dans le nouveau segment)
- `token_index` et `byte_from/byte_to` : inchangés (ils sont relatifs au document)

### 5. AutomatonPhraseQuery, AutomatonWeight, TermWeight

**Fichiers** : `automaton_phrase_weight.rs`, `automaton_weight.rs`, `term_weight.rs`

**Problème** : Tous font `reader.inverted_index(self.field)` puis utilisent les postings pour scorer.

**Solution** : Même pattern que SuffixContainsQuery et RegexContinuationQuery — préférer `.sfxpost` si disponible, sinon fallback.

Pour ça, il faut généraliser le `PostingResolver` trait (actuellement dans regex_continuation_query.rs) et le rendre accessible à tous les scorers. Le déplacer dans un module partagé.

## Plan d'exécution (ordre)

### Étape A : Extraire PostingResolver dans un module commun
- Déplacer `PostingResolver`, `SfxPostResolver`, `InvertedIndexResolver`, `PostingEntry` depuis `regex_continuation_query.rs` vers un nouveau fichier `src/query/posting_resolver.rs`
- Rendre disponible pour tous les query weights

### Étape B : Rerouter term/fuzzy/regex via .sfx

**Réalisation en cours d'étude** : les queries `term`, `fuzzy`, `regex` utilisent le pipeline tantivy classique (`TermQuery`, `FuzzyTermQuery`, `RegexQuery`) qui est très couplé aux `TermInfo` et posting lists du ._raw via `AutomatonWeight`, `TermWeight`, etc.

Plutôt que refactorer ces fichiers lourds, l'approche est de **rerouter dans `query.rs`** :
- `term` → `SuffixContainsQuery` distance=0, mais filtrer SI=0 pour match exact token (pas substring)
- `fuzzy` → `SuffixContainsQuery` distance=N, filtrer SI=0
- `regex` → `RegexContinuationQuery`

**Nuance importante** : `term "program"` ≠ `contains "program"`. Le term matche le token exact, pas les substrings. Il faut un mode "exact token" dans SuffixContainsQuery qui ne retourne que les matches SI=0 (début de token) ET qui vérifie que le match couvre le token entier.

**Alternative** : garder les anciennes queries comme fallback et supprimer ._raw seulement quand tous les paths sont reroutés. Le ._raw resterait dans le schema mais ne serait plus auto-dupliqué — les postings seraient construites depuis le SfxCollector.

### Ancien étape B : SfxTermDictionary retourne raw_ordinal au lieu de TermInfo
- `get()` → `Option<u64>` (raw_ordinal)
- `search_automaton()` → `Vec<(String, u64)>`
- `range_scan()` → `Vec<(String, u64)>`
- Les appelants utilisent le PostingResolver pour résoudre les postings

### Étape C : Rerouter automaton_phrase_weight, automaton_weight, term_weight
- Même pattern : charger `.sfxpost` → PostingResolver → résoudre postings
- `cascade_term_infos()` et `prefix_term_infos()` retournent des ordinals

### Étape D : BM25 par segment
- Déplacer la construction de `Bm25Weight` de `weight()` vers `scorer()`
- Lire `doc_freq` depuis le `.sfxpost` par segment
- `total_num_docs` = `reader.max_doc()`, `avg_fieldnorm` dérivé des fieldnorms

### Étape E : Merger sans ._raw
- Collecter unique_tokens depuis les `.sfx` source (SI=0 stream)
- Reconstruire `.sfxpost` avec remapping doc_ids et ordinals
- Écrire `.sfxpost` via `serializer.write_sfxpost()`

### Étape F : Supprimer ._raw du schema
- `handle.rs` : ne plus créer le champ ._raw
- Supprimer `RAW_SUFFIX`, `raw_field_pairs`
- Bindings : ne plus auto-dupliquer vers ._raw
- Virer les fallback `InvertedIndexResolver` partout
- Le SfxCollector s'attache au champ principal (pas ._raw)

### Validation
Chaque étape doit compiler et passer les 1196 tests. Ajouter des tests de parité (mêmes résultats avec et sans ._raw).
