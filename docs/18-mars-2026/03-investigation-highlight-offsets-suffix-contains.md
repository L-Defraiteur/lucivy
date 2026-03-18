# Investigation — Highlight offsets parasites dans SuffixContainsQuery

Date : 18 mars 2026
Status : bug identifié, fix en cours
Branche : `experiment/decouple-sfx` (aussi présent sur `feature/sfx-unified`)

## Le bug

`contains 'function'` sur le texte `"the function foo() calls disjunction bar()"` retourne :
```
@4..12 = "function"     ← CORRECT
@5..13 = "unction "     ← PARASITE (suffixe SI=1 de "function")
@28..36 = "junction"    ← FAUX MATCH (suffixe de "disjunction", pas "function")
@29..37 = "unction "    ← PARASITE (suffixe SI=1 de "disjunction")
```

## Bug pré-existant (pas lié au pre-tokenized pipeline)

Reproduit sur 1-shard LucivyHandle directe, sans sharding, sans pre-tokenize.
Test : `test_single_handle_highlights` dans `lucivy_core/tests/test_cold_scheduler.rs`.

## Cause identifiée

### Fichier : `src/query/phrase_query/suffix_contains.rs`, lignes 66-112

La fonction `suffix_contains_single_token_inner` :

1. Appelle `sfx_reader.prefix_walk("function")` qui retourne TOUTES les entrées
   FST dont la clé commence par "function" (dans les deux partitions \x00 et \x01)

2. Pour chaque entrée, itère TOUS les parents (tokens qui ont ce suffixe)

3. Pour chaque parent, résout les postings et crée un match avec les offsets

### Le prefix_walk retourne :

- `\x00function` → parents: [{raw_ordinal: X, si: 0}] (token "function")
- `\x01function` → parents: [{raw_ordinal: X, si: 1}] (token "function" à SI=1)
  + possiblement d'autres parents si d'autres tokens ont "function" comme suffixe

Mais `prefix_walk` est **correct** — il retourne bien les clés qui commencent par
"function". Le problème est en aval.

### Le calcul d'offset original (avant fix tenté) :

```rust
byte_from: entry.byte_from as usize + parent.si as usize,
byte_to: entry.byte_from as usize + parent.si as usize + query_len,
```

Ceci décale l'offset par `parent.si`. Pour SI=0, c'est `byte_from + 0 = 4` (correct).
Pour SI=1, c'est `byte_from + 1 = 5` (pointe vers "unction", pas "function").

### Fix tenté :

```rust
byte_from: entry.byte_from as usize,
byte_to: entry.byte_to as usize,
```

Utiliser les offsets du token parent directement. Mais ça ne résout pas le faux match
"disjunction" car "disjunction" a un sfxpost avec `byte_from=24, byte_to=36` qui est
un vrai posting — c'est bien la position de "disjunction" dans le texte.

### Le vrai problème : pourquoi "disjunction" matche "function" ?

`prefix_walk("function")` cherche les clés FST ≥ "\x00function" et < "\x00functioo".
Ceci NE devrait PAS matcher "junction" ou "unction" car ces clés commencent
respectivement par "\x00junction" et "\x01unction", pas par "\x00function".

MAIS le `prefix_walk` fait un merge des deux partitions :
```rust
pub fn prefix_walk(&self, prefix: &str) -> Vec<(String, Vec<ParentEntry>)> {
    let mut merged: HashMap<String, Vec<ParentEntry>> = HashMap::new();
    for (key, parents) in self.prefix_walk_with_byte(SI0_PREFIX, prefix) {
        merged.entry(key).or_default().extend(parents);
    }
    for (key, parents) in self.prefix_walk_with_byte(SI_REST_PREFIX, prefix) {
        merged.entry(key).or_default().extend(parents);
    }
    ...
}
```

Le merge par clé SANS le prefix byte fusionne les parents des deux partitions.
L'entrée "function" dans la map merged a les parents de DEUX partitions :
- \x00function → parent "function" SI=0
- \x01function → parent "function" SI=1 (c'est le suffixe SI=1 du token "function")

Mais aussi potentiellement un autre token qui a "function" comme suffixe à SI>0.

### Le faux match "disjunction" vient d'où ?

Si le token "disjunction" existe dans l'index, le suffix FST a :
- \x00disjunction (SI=0)
- \x01isjunction (SI=1)
- \x01sjunction (SI=2)
- \x01junction (SI=3)
- \x01unction (SI=4)
- \x01nction (SI=5)
- etc.

`prefix_walk("function")` cherche les clés commençant par "function".
"\x01unction" ne commence PAS par "function". "\x01function" pourrait exister
si un token finit par "function" (ex: "myfunction").

Les offsets parasites `@28..36 = "junction"` et `@29..37 = "unction"` ne devraient
PAS venir de prefix_walk("function") car aucune clé ne commence par "function"
et pointe vers "disjunction".

**Hypothèse à vérifier** : peut-être que prefix_walk retourne des résultats
incorrects, ou que le merge HashMap crée des collisions de clés qui fusionnent
des parents de tokens différents.

## Fichiers clés

| Fichier | Lignes | Rôle |
|---------|--------|------|
| `src/query/phrase_query/suffix_contains.rs` | 66-112 | `suffix_contains_single_token_inner` — le scorer |
| `src/suffix_fst/file.rs` | 202-241 | `prefix_walk` — le walk FST avec merge |
| `src/suffix_fst/collector.rs` | 114-126 | `add_token` — comment les offsets sont stockés |
| `src/query/phrase_query/scoring_utils.rs` | 29-80 | `HighlightSink` — comment les highlights sont collectés |

## Ce qui fonctionne

- Les queries trouvent les bons documents (scores corrects, doc_ids corrects)
- L'indexation 212K docs fonctionne (431s, pas de crash)
- Le deferred sfx merge fonctionne (pas d'explosion mémoire)

## Ce qui ne fonctionne pas

- Les highlights ont des offsets parasites (suffixes SI>0, faux matches)
- Le dedup `(doc_id, byte_from)` ne les attrape pas car les byte_from sont différents

## Fix proposé

1. **Dans `suffix_contains_single_token_inner`** : après le walk, filtrer les parents
   pour ne garder que ceux dont le suffixe matche réellement le query. Vérifier que
   `suffix_term[parent.si..].starts_with(query)` — si non, c'est un faux match.

2. **Ou dans `prefix_walk`** : ne pas merger les parents des deux partitions
   aveuglément. Retourner aussi le SI de chaque parent pour que le caller puisse
   filtrer.

3. **Dedup par token parent** : après résolution, dédupliquer par `(doc_id, raw_ordinal)`
   au lieu de `(doc_id, byte_from)`. Un token ne devrait produire qu'un seul highlight.

## Changements non commités sur la branche

- `src/query/phrase_query/suffix_contains.rs` : fix partiel (byte_from sans SI offset)
- `lucivy_core/benches/bench_sharding.rs` : sanity check avec snippets texte
- `docs/18-mars-2026/02-bug-highlight-offsets-pretokenized.md` : doc initial du bug
- `lucivy_core/tests/test_cold_scheduler.rs` : restauré depuis commit

## Pour la prochaine session

1. Ajouter un `eprintln` dans `prefix_walk` pour dumper les résultats sur le texte
   "the function foo() calls disjunction bar()" et vérifier l'hypothèse
2. Implémenter le fix (filtrage ou dedup par raw_ordinal)
3. Vérifier avec le test `test_single_handle_highlights`
4. Relancer le bench 212K pour confirmer que les highlights sont corrects
5. Commiter tout (fix highlights + deferred sfx + bench improvements)
