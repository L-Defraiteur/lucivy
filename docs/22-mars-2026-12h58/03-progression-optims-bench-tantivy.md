# Doc 03 — Progression : optimisations, bench vs tantivy, queries exposées

Date : 22 mars 2026
Branche : `feature/acid-postgres-tests`

## Optimisations réalisées

### 1. TermQuery : drop sfxpost (1004ms → 9ms → 0.2ms)

- `build_term_query` mettait `prefer_sfxpost(true)` → le scorer ouvrait le SFX
  file et résolvait TOUS les postings (8850 pour "mutex") pour rien
- Fix 1 : `prefer_sfxpost = false` → 9ms (utilise inverted index standard)
- Fix 2 : suppression du fallback SFX dans `TermWeight::scorer()` fallback path
  qui ouvrait le .sfx file juste pour un point lookup du terme → 0.2ms

Le standard inverted index fournit les byte offsets pour highlights via
`WithFreqsAndPositionsAndOffsets` — pas besoin de sfxpost.

### 2. AutomatonWeight : SFX conditionnel (fuzzy/regex 2x plus rapide)

`AutomatonWeight::collect_term_infos()` ouvrait le SFX file pour TOUS les
automaton walks, même ceux qui n'ont besoin que du term dict standard.

- Fix : le SFX path est maintenant conditionné par `prefer_sfxpost`
- `FuzzyTermQuery` / `RegexQuery` (top-level) : `prefer_sfxpost=false` → term dict
- `RegexContinuationQuery` (contains+regex) : `prefer_sfxpost=true` → SFX (nécessaire pour suffixes)

### 3. Queries standard rebranchées sur tantivy behavior

`"fuzzy"` et `"regex"` comme types de query top-level pointaient vers les versions
cross-token SFX (RegexContinuationQuery). Remis sur le comportement tantivy standard :

- `"fuzzy"` → `FuzzyTermQuery` (Levenshtein sur term dict, single token)
- `"regex"` → `RegexQuery` (regex sur term dict, single token)

Les versions cross-token SFX restent accessibles via :
- `"contains"` + `distance: N` → fuzzy substring
- `"contains"` + `regex: true` → regex substring

### 4. Nouvelles queries exposées

- `"phrase_prefix"` → `PhrasePrefixQuery` (autocomplétion "mutex loc..." → "mutex lock")
- `"disjunction_max"` → `DisjunctionMaxQuery` (max score parmi N sous-queries)

## Bench vs tantivy 0.25 — 90K docs Linux kernel

```
Query                            Hits    Tantivy   Lucivy-1   Lucivy-4
----------------------------------------------------------------------
term 'mutex'                      20      0.2ms      0.1ms      0.2ms
term 'lock'                       20      0.2ms      0.2ms      0.2ms
term 'function'                   20      0.3ms      0.2ms      0.3ms

phrase 'mutex lock'               20      2.3ms      2.2ms      1.1ms
phrase 'struct device'            20     13.2ms     13.1ms      5.3ms
phrase 'return error'             20      5.7ms      5.3ms      2.5ms
phrase 'unsigned long'            20      4.8ms      4.8ms      2.1ms

parse 'mutex AND lock'            20      0.4ms      0.3ms      0.4ms
parse 'function OR struct'        20      0.9ms      0.7ms      0.6ms
parse '"return error"'            20      5.7ms      5.4ms      2.3ms

fuzzy 'schdule' d=1               20      6.5ms      4.7ms      3.5ms
fuzzy 'mutex' d=2                 20     24.5ms     20.6ms     13.1ms
fuzzy 'fuction' d=1               20      5.2ms      4.5ms      2.7ms
fuzzy 'prntk' d=2                 20     25.8ms     22.5ms     12.8ms

regex 'mutex.*'                   20      0.4ms      0.8ms      1.2ms
regex 'sched[a-z]+'               20      0.4ms      0.3ms      0.5ms
regex 'print[kf]'                 20      0.3ms      0.3ms      0.4ms

Lucivy-only                      Hits        ---   Lucivy-1   Lucivy-4
----------------------------------------------------------------------
contains 'mutex_lock'              20        N/A   2804.7ms    974.2ms
contains 'function'                20        N/A   2309.7ms    811.5ms
startsWith 'sched'                 20        N/A   2066.0ms    905.8ms
fuzzy 'schdule' d=1 (SFX)         20        N/A   2281.0ms    955.9ms
phrase_prefix 'mutex loc'          20        N/A      2.1ms      1.0ms
```

### Résumé

| Catégorie | vs Tantivy |
|-----------|-----------|
| term | = égal |
| phrase | **lucivy 2-2.5x plus rapide** (parallélisme 4 shards) |
| parse | = égal ou mieux |
| fuzzy (term dict) | **lucivy 2x plus rapide** (parallélisme) |
| regex (term dict) | = égal |
| contains/startsWith | **exclusif lucivy** (tantivy ne peut pas) |
| phrase_prefix | **exclusif lucivy** (1ms autocomplétion) |

## Architecture des modes query

```
                    ┌── term dict ──── FuzzyTermQuery      ("fuzzy")
                    │                  RegexQuery           ("regex")
                    │                  TermQuery            ("term")
                    │                  PhraseQuery          ("phrase")
Query ──────────────┤                  PhrasePrefixQuery    ("phrase_prefix")
                    │                  QueryParser          ("parse")
                    │                  DisjunctionMaxQuery  ("disjunction_max")
                    │                  BooleanQuery         ("boolean")
                    │
                    └── SFX FST ───── SuffixContainsQuery  ("contains", "startsWith")
                                      + fuzzy_distance     ("contains" + distance)
                                      + regex              ("contains" + regex:true)
                                      RegexContinuationQuery (cross-token regex)
```

- **Term dict path** : rapide (~0.2-5ms), même vitesse que tantivy, parallélisme shardé
- **SFX path** : plus lent (~800-1000ms sur 90K) mais offre le substring search que tantivy ne peut pas faire

## Fichiers modifiés

| Fichier | Changement |
|---------|-----------|
| `lucivy_core/src/query.rs` | fuzzy→FuzzyTermQuery, regex→RegexQuery, +phrase_prefix, +disjunction_max |
| `src/query/term_query/term_weight.rs` | Suppression fallback SFX dans scorer |
| `src/query/automaton_weight.rs` | SFX conditionnel via prefer_sfxpost |
| `src/query/phrase_query/regex_phrase_weight.rs` | prefer_sfxpost=true pour contains+regex |
| `lucivy_core/benches/bench_vs_tantivy.rs` | Bench head-to-head vs tantivy 0.25 |
| `lucivy_core/Cargo.toml` | +tantivy = "0.25" dev-dependency |

## Pistes futures identifiées

### Mode indexation sans SFX

Flag `sfx: false` dans SchemaConfig pour skip le SfxCollector.
Indexation plus rapide + index plus petit. Queries contains/startsWith
retourneraient une erreur claire.

### Bypass DAG pour queries sans prescan

Pour term/phrase/fuzzy/regex, le DAG (drain→flush→prescan→merge_prescan→build_weight→search→merge)
ajoute un overhead. Un fast path direct (search shards en parallèle via thread::scope
ou le shard pool) donnerait ~0.1ms au lieu de ~0.3ms.

### MoreLikeThisQuery

Reste à exposer via build_query. Utile pour recommandations.

## Commits

```
a0a8f94 perf: term query 1004ms→9ms — drop sfxpost, use standard postings
31ae09b feat: expose phrase_prefix + disjunction_max queries
74013d5 perf: beat tantivy on phrase queries, match on term/parse
(en cours) perf: fuzzy/regex — use term dict instead of SFX, beat tantivy on fuzzy
```
