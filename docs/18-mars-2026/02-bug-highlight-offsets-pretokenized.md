# Bug — Highlight offsets décalés avec pre-tokenized pipeline

Date : 18 mars 2026

## Symptôme

Le sanity check du bench affiche des snippets qui ne contiennent pas le mot recherché :

```
=== Sanity check: 'function' on RR ===
  shard=3 doc=27 score=4.687 → ...
  shard=1 doc=10 score=4.917 → ...ATASET CSV tinysnb
  shard=2 doc=29 score=5.222 → ...CSV tck
```

Le mot "function" devrait apparaître entre les `«»` mais les offsets pointent
vers du texte sans rapport.

## Contexte

Le pre-tokenized pipeline (`tokenize_for_pipeline` dans sharded_handle.rs)
produit des `PreTokenizedString` avec :
- `text` = le texte original du document
- `tokens[i].text` = le texte du token **après** le tokenizer (lowercase, camelCase split)
- `tokens[i].offset_from/to` = les offsets **dans le texte original**

Le SegmentWriter fast path alimente le SfxCollector directement depuis ces tokens :
```rust
// segment_writer.rs fast path
collector.begin_value(&pre_tok.text);
for tok in &pre_tok.tokens {
    collector.add_token(&tok.text, tok.offset_from, tok.offset_to);
}
collector.end_value();
```

## Hypothèses

### H1 : Offsets du mauvais tokenizer
Le `tokenize_for_pipeline` utilise le tokenizer du champ (raw_code = SimpleTokenizer
→ CamelCaseSplit → LowerCaser). Ce tokenizer produit des offsets relatifs au texte
original. MAIS le SfxCollector s'attend peut-être à des offsets d'un tokenizer
différent (le raw tokenizer ?).

Dans le normal path (sans pre-tokenize), le SfxCollector est alimenté soit par :
- Le raw_analyzer (tokenizer séparé) avec ses propres offsets
- Le SfxTokenInterceptor qui capture les tokens du main tokenizer

Si le main tokenizer et le raw_analyzer produisent des offsets différents
(ce qui est possible si CamelCaseSplit change les boundaries), les highlights
seront décalés.

### H2 : Pre-tokenized tokens ont des offsets transformés
Le `std::mem::take(&mut token.text)` dans `tokenize_for_pipeline` prend le texte
du token APRÈS les filtres du tokenizer. Mais les offsets (`offset_from/to`)
réfèrent au texte ORIGINAL (avant les filtres). C'est le comportement standard
des tokenizers — les offsets sont toujours relatifs au texte source.

Si c'est correct, les offsets devraient être bons. Le bug serait ailleurs.

### H3 : Mauvais champ
Le highlight pourrait pointer vers le champ `path` au lieu de `content`.
Le HighlightSink stocke par `(segment_id, doc_id)` sans distinguer les champs
au niveau de la clé. Si le path et le content produisent des highlights, ils
sont mélangés.

En fait le HighlightSink stocke `Vec<(field_name, start, end)>`, donc le champ
est distingué. Mais le sanity check prend `h.iter().next()` qui pourrait
retourner le mauvais champ.

### H4 : Le field dans le HighlightSink ne matche pas le field du document
Le HighlightSink utilise `field_name` (String) comme clé secondaire. Si le
pre-tokenized path utilise un nom de champ différent (ex: le champ interne
vs le champ utilisateur), les highlights ne matchent pas.

## À investiguer

1. Vérifier les offsets : pour un doc donné, afficher le texte original +
   les offsets du highlight + le texte extrait aux offsets
2. Vérifier quel champ le highlight référence (path vs content)
3. Comparer les offsets du pre-tokenized path vs le normal path (1-shard)
   sur le même document
4. Vérifier si le bug existe aussi sans le pre-tokenized pipeline
   (bypass le pipeline, utiliser add_document_with_hashes ou 1-shard)

## Diagnostic (18 mars après-midi)

Debug avec texte réel aux offsets :

```
highlight field='content' offsets=[[29, 37], [30, 38], [62, 70], [63, 71]]
  @29..37 = "junction"     ← FAUX, pas "function"
  @30..38 = "unction "     ← suffixe SI=1
  @124..132 = "function"   ← CORRECT
  @125..133 = "unction "   ← suffixe SI=1 décalé de 1
```

### Constat
Les highlights arrivent **par paires** :
- `[124, 132]` → "function" (SI=0, mot complet) — correct
- `[125, 133]` → "unction " (SI=1, suffixe décalé de 1) — parasite

Et certains highlights pointent vers des mots qui ne contiennent PAS "function" :
- `[29, 37]` → "junction" — c'est un suffixe d'un autre mot (disjunction ?)

### Cause probable
Le SuffixContainsQuery renvoie les **offsets de TOUS les suffix matches** dans
le document, y compris :
1. Les suffixes SI>0 du même token (unction, nction, ction...)
2. Les suffixes d'autres tokens qui matchent le pattern

Le query matche correctement les documents (les docs contiennent bien "function"),
mais les highlight offsets incluent des matches parasites.

### Confirmé : bug pré-existant (pas le pre-tokenized pipeline)

Test sur 1-shard (LucivyHandle directe, pas de pre-tokenize) :
```
text="the function foo() calls disjunction bar()"
  @4..12 = "function"    ← correct
  @5..13 = "unction "    ← parasite SI=1
  @28..36 = "junction"   ← parasite : suffix de "disjunction"
  @29..37 = "unction "   ← parasite SI=1 de "disjunction"
```

Le bug est dans le **SuffixContainsQuery highlight resolution**, pas dans le
pre-tokenized pipeline.

### Cause : les highlights ne résolvent pas vers le parent token

Quand le suffix FST matche "\x01unction" (SI=1), il faut remonter au parent
token "function" (SI=0) et utiliser les offsets du parent (4..12), pas
l'offset du suffixe (5..13).

Quand "\x01unction" matche aussi "disjunction", l'offset 28..36 pointe vers
"junction" dans "disjunction" — mais "disjunction" ne contient PAS "function",
donc ce match devrait être filtré.

## Impact

- Les queries **trouvent** les bons documents (20 hits, scores corrects)
- Les **highlights ont des offsets parasites** (suffixes SI>0 + faux matches)
- L'indexation et le search fonctionnent
- Bug de présentation, pas de correctness (les bons docs sont trouvés)

## Résolution (18 mars soir)

### Cause racine : default fuzzy distance = 1

Le bug n'est **pas** dans `prefix_walk` ni dans `suffix_contains_single_token_inner`.

Dans `lucivy_core/src/query.rs`, `build_contains_query` :
```rust
let distance = config.distance.unwrap_or(1);  // ← LE BUG
```

Quand aucune `distance` n'est précisée dans la query config, le default est 1.
Avec fuzzy distance 1, le suffix FST fuzzy walk matche légitimement :
- `"junction"` → Levenshtein distance 1 de `"function"` (substitution f→j)
- `"unction"` → Levenshtein distance 1 de `"function"` (suppression de 'f')

Le code de résolution d'offsets (`byte_from + parent.si`) est **correct** — il positionne
le highlight sur le suffixe matché dans le texte original. Le problème était que le fuzzy
ramenait des matches non désirés.

### Fix

`lucivy_core/src/query.rs` : `unwrap_or(1)` → `unwrap_or(0)` pour les queries `contains`
(2 endroits : `build_contains_query` et `build_filter_clause` cas `"contains"`).

Le default pour `fuzzy` reste `unwrap_or(1)` — c'est le comportement voulu pour les
queries de type fuzzy.

### Vérification

Test E2E `test_single_handle_highlights` dans `lucivy_core/tests/test_cold_scheduler.rs` :
```
field='content' offsets=[[4, 12]]
  @4..12 = "function"
```
Un seul highlight, propre, pas de parasites.

Test unitaire `test_no_parasitic_matches_function_disjunction` dans
`src/query/phrase_query/suffix_contains.rs` : confirme que `prefix_walk("function")`
ne retourne pas de résultats liés à "disjunction" (le suffix FST est correct).

### Leçon

Le diagnostic initial (doc 03) cherchait le bug dans `prefix_walk` et le merge HashMap.
En réalité `prefix_walk` est correct — aucune clé FST commençant par `"function"` ne
pointe vers "disjunction". Les faux matches venaient du fuzzy walk (distance 1) qui
est un code path différent, activé par le default trop permissif.
