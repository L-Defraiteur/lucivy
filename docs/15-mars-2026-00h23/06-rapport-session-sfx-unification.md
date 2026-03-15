# Rapport de session — U2 sfx unification + CamelCase + merge + playground

Date : 15 mars 2026

## Résumé

Session complète : SfxTermDictionary, AutomatonPhraseQuery via .sfx sans
fallback, CamelCaseSplitFilter, merge .sfx (PHASE-8), pivot token le plus
sélectif, sfx_contains/sfx_contains_split exposés dans build_query,
playground mis à jour, drain_merges.

## Branches

- `feature/sfx-contains` — STABLE : sfx contains, merge, pivot, fuzzy, playground
- `feature/sfx-unified` — WIP : U2 en cours, 1197 tests passent

## Commits clés (feature/sfx-unified)

```
ee13bed feat: CamelCaseSplitFilter, SI cap 256, skip ngram .sfx, raw_code tokenizer
ba01f6d wip: U2 sfx unification — AutomatonPhraseQuery via SfxTermDictionary
8e4d540 feat: SfxTermDictionary — term dictionary backed by suffix FST
```

## Commits clés (feature/sfx-contains)

```
b9c8773 feat: pivot most-selective token for multi-token contains, fuzzy d=3 test
df310d9 feat: sfx merge, sfx_contains query routing, fuzzy, drain_merges, playground
a49aa23 feat: sfx manifest, SI=0 sort, min_suffix_len=1, architecture unifiée
```

## Architecture actuelle

### SfxTermDictionary (`src/suffix_fst/term_dictionary.rs`)

Wraps `SfxFileReader` + `TermDictionary` existant. API :
- `get(key)` → lookup .sfx FST, filtre SI=0, résout TermInfo via ordinal
- `term_ord(key)` → idem, retourne ordinal
- `search_automaton(dfa)` → walk DFA sur .sfx, filtre SI=0, retourne Vec<(String, TermInfo)>
- `range_scan(ge, lt)` → range sur .sfx, filtre SI=0
- `stream_all()` → tous les termes SI=0
- `sfx_reader()` → accès direct pour contains (any SI)
- `termdict()` → accès au TermDictionary sous-jacent

Les ordinals sont identiques entre .sfx et TermDictionary (même BTreeSet trié).

### CamelCaseSplitFilter (`src/tokenizer/camel_case_split.rs`)

TokenFilter dans le pipeline tokenizer. Split aux transitions :
- lowercase → uppercase (camelCase)
- letter → digit, digit → letter

Merge forward : chunks < 4 chars fusionnés avec le suivant.
Dernier chunk < 4 → fusionné avec le précédent.
Force-split : chunks > 256 bytes découpés aux frontières UTF-8.

Exemples :
- "getElementById" → ["getElement", "ById"]
- "HTMLAParser" → ["HTML", "AParser"]
- "rag3db" → ["rag3db"] (tout merge)

Offsets ajustés par sub-token (byte ranges dans le texte original).
Positions incrémentées par sub-token.

Pipeline ._raw : `SimpleTokenizer → CamelCaseSplitFilter → LowerCaser`
Enregistré sous le nom `"raw_code"` dans `configure_tokenizers()`.

### Segment writer (`src/indexer/segment_writer.rs`)

SfxCollectors créés pour tous les champs `FieldType::Str` indexés,
SAUF ceux dont le tokenizer contient "ngram" (offsets chevauchants
incompatibles avec GapMap).

### SuffixFstBuilder (`src/suffix_fst/builder.rs`)

- SI encoding : u16 (16 bits, max 65535) dans le u64 output
- Safety net : cap suffix depth à 256 bytes (MAX_CHUNK_BYTES)
- SfxCollector : skip tokens > MAX_TOKEN_LEN

### Merge .sfx (`src/indexer/merger.rs` + `src/indexer/merge_state.rs`)

Nouvelle phase `Sfx` dans MergeState entre FastFields et Close.
`IndexMerger::merge_sfx()` :
1. Collecte tokens uniques depuis les term dictionaries source
2. Rebuild SuffixFstBuilder
3. Copie GapMap par doc dans l'ordre du merge via `add_doc_raw()`
4. Écrit .sfx + manifeste

### drain_merges (`src/indexer/index_writer.rs`)

`IndexWriter::drain_merges()` : attend les merges pending sans consommer
le writer. Appelé dans le binding Python après commit.

## Ce qui est câblé sur SfxTermDictionary

### AutomatonPhraseQuery (FAIT — sans fallback)

`src/query/phrase_query/automaton_phrase_weight.rs` :
- `cascade_term_infos()` : exact get + fuzzy DFA → via SfxTermDictionary
- `prefix_term_infos()` : range scan + prefix fuzzy DFA → via SfxTermDictionary
- Utilise `SfxDfaWrapper` (lucivy_fst::Automaton) au lieu de `DfaWrapper` (tantivy_fst)
- `.sfx` obligatoire, erreur si absent
- `phrase_scorer()` et `single_token_scorer()` créent le SfxTermDictionary on-the-fly

### SuffixContainsQuery (FAIT)

`src/query/phrase_query/suffix_contains_query.rs` :
- Crée SfxFileReader directement, pas via SfxTermDictionary
- Fuzzy distance câblée (`with_fuzzy_distance()`)
- Routé via "sfx_contains" et "sfx_contains_split" dans build_query

## Ce qui reste à câbler (U2 suite)

### 1. TermQuery (`src/query/phrase_query/../../query/term_query.rs`)

Actuellement : `inverted_index.get_term_info(&term)` → TermDictionary standard.
À faire : si .sfx existe, utiliser `SfxTermDictionary.get(key)`.

### 2. FuzzyTermQuery (`src/query/fuzzy_query.rs`)

Actuellement : crée un DfaWrapper, délègue à AutomatonWeight qui fait
`terms().search(&dfa).into_stream()`.
À faire : dans AutomatonWeight.scorer(), si .sfx existe, utiliser
`SfxTermDictionary.search_automaton(&sfx_dfa)` au lieu du stream.

### 3. RegexQuery (`src/query/regex_query.rs`)

Actuellement : crée un regex automaton, délègue à AutomatonWeight.
Même pattern que FuzzyTermQuery — câbler via SfxTermDictionary.

### 4. AutomatonWeight (`src/query/automaton_weight.rs`)

Point central : FuzzyTermQuery et RegexQuery passent tous les deux par
AutomatonWeight. Si on câble SfxTermDictionary dans AutomatonWeight,
les deux queries en bénéficient automatiquement.

Pattern : dans `AutomatonWeight::scorer()`, ouvrir le .sfx si dispo,
créer SfxTermDictionary, utiliser `search_automaton()` pour résoudre
les termes au lieu de `terms().search(&dfa).into_stream()`.

Attention : AutomatonWeight utilise `tantivy_fst::Automaton` (DfaWrapper),
le .sfx utilise `lucivy_fst::Automaton` (SfxDfaWrapper). Il faut le même
adapter qu'on a fait dans automaton_phrase_weight.

### 5. Tests de parité

Pour chaque query type, écrire un test qui vérifie que le résultat via
SfxTermDictionary est identique au résultat via TermDictionary standard.
Utiliser un index avec .sfx et comparer les doc_ids + scores.

## Après U2

- U3 : Regex continuation DFA à travers GapMap (3 modes : contains, startsWith, strict)
- U4 : Supprimer ._ngram (code mort confirmé — le .sfx fait le multi-point)
- U5 : Supprimer ._raw FST (SfxTermDictionary le remplace complètement)

## Fichiers clés modifiés/créés cette session

```
src/suffix_fst/term_dictionary.rs      ← NOUVEAU (SfxTermDictionary)
src/suffix_fst/mod.rs                  ← export SfxTermDictionary
src/suffix_fst/file.rs                 ← expose fst(), parent_list_data()
src/suffix_fst/builder.rs             ← SI u16, cap 256 bytes
src/suffix_fst/collector.rs           ← guard MAX_TOKEN_LEN
src/suffix_fst/gapmap.rs              ← add_doc_raw(), doc_data() pub
src/tokenizer/camel_case_split.rs     ← NOUVEAU (CamelCaseSplitFilter)
src/tokenizer/mod.rs                  ← export CamelCaseSplitFilter
src/indexer/segment_writer.rs         ← sfx pour Str indexés, skip ngram
src/indexer/merger.rs                 ← merge_sfx()
src/indexer/merge_state.rs            ← phase Sfx
src/indexer/index_writer.rs           ← drain_merges()
src/query/mod.rs                      ← export SuffixContainsQuery
src/query/phrase_query/suffix_contains.rs       ← pivot most-selective
src/query/phrase_query/suffix_contains_query.rs ← fuzzy distance
src/query/phrase_query/automaton_phrase_weight.rs ← SfxTermDictionary, no fallback
lucivy_core/src/query.rs              ← sfx_contains, sfx_contains_split, build_sfx_contains_query
lucivy_core/src/handle.rs             ← raw_code tokenizer, drain_merges in commit
bindings/python/src/lib.rs            ← drain_merges after commit
bindings/emscripten/src/lib.rs        ← (inchangé, sfx routing via build_query)
playground/index.html                 ← sfx contains modes, dynamic dataset label
```

## État des tests

- **ld-lucivy** : 1197 tests, 0 échec, 7 ignored
- **lucivy-fst** : 141 tests, 0 échec
- **lucivy_core** : 73 tests, 0 échec
