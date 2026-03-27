# 07 — Design : regex contains via falling_walk + extraction de littéraux

## Problème

`RegexContinuationQuery` utilise `search_continuation(automaton, start_state)` pour Walk 1.
Cette fonction fait un scan DFA × FST sur **l'intégralité** du SFX FST (toutes les suffixes de tous les tokens).

Résultat : 16 secondes pour `shard[a-z]+` sur 862 documents en WASM.

Le fuzzy contains est rapide (~15ms exact, ~291ms d=1 sur 5k docs) parce qu'il utilise `falling_walk` — un lookup ciblé O(query_len) qui ne touche qu'un chemin dans le FST.

## Insight

On n'a pas besoin de scanner le FST entier. Un regex contient presque toujours des **fragments littéraux**. On peut les extraire, utiliser `falling_walk` pour trouver les candidats en O(fragment_len), puis valider avec le DFA regex.

C'est exactement ce que fait ripgrep : extraction de littéraux → filtre rapide → validation regex.

## Extraction de littéraux

Depuis le pattern regex, on extrait tous les fragments composés de caractères littéraux (pas de métacaractères).

| Pattern | Littéraux extraits | Meilleur candidat |
|---|---|---|
| `shard[a-z]+` | `"shard"` | `"shard"` (5 bytes, préfixe) |
| `get.*Element` | `"get"`, `"element"` | `"element"` (7 bytes, plus long) |
| `[a-z]+ment` | `"ment"` | `"ment"` (4 bytes, suffixe) |
| `.*getElementById` | `"getelementbyid"` | `"getelementbyid"` (15 bytes) |
| `foo(bar\|baz)qux` | `"foo"`, `"bar"`, `"baz"`, `"qux"` | `"foo"` ou `"qux"` (3 bytes) |
| `[0-9]{4}-[0-9]{2}` | `"-"` | `"-"` (1 byte — fallback vers scan) |

Heuristique : prendre le fragment le plus long. Si tous les fragments font ≤ 2 bytes, fallback vers `search_continuation` (trop peu sélectif).

Note : les littéraux sont lowercasés avant le falling_walk (le SFX FST est lowercase).

## Algorithme

```
regex_contains_via_literal(sfx_reader, pattern, resolver, ord_to_term):

  // 1. Compiler le regex DFA
  regex = Regex::new(pattern)

  // 2. Extraire les littéraux du pattern
  literals = extract_literals(pattern)  // Vec<String>
  best = literals.max_by_key(|l| l.len())

  if best.len() <= 2:
    // Trop court, pas sélectif → fallback scan complet
    return continuation_score_sibling(regex, ...)

  // 3. falling_walk sur le meilleur littéral
  candidates = sfx_reader.falling_walk(best.to_lowercase())

  // 4. Pour chaque candidat : récupérer le texte complet, valider via regex
  results = []
  for cand in candidates:
    token_text = ord_to_term(cand.parent.raw_ordinal)
    // Le texte visible commence à SI dans le token
    visible_text = token_text[cand.parent.si..]

    // Valider : est-ce que le regex matche une sous-chaîne du texte visible ?
    if regex.is_match(visible_text):
      // Match single-token
      postings = resolver.resolve(cand.parent.raw_ordinal)
      → émettre les résultats

    // Si le regex peut encore matcher (DFA alive en fin de token) :
    // → cross-token via sibling links + GapMap + DFA feed
    // (exactement comme continuation_score_sibling, mais en partant
    //  d'un ensemble de candidats ciblé au lieu du scan complet)

  return results
```

## Détail : validation single-token

Le `falling_walk(literal)` nous dit que le literal existe dans le SFX. Mais le regex peut être plus restrictif ou plus large que le literal seul.

Exemple : pattern `shard[0-9]+`, literal "shard". `falling_walk("shard")` matche "sharded". Mais "sharded" ne matche PAS `shard[0-9]+` (les lettres "ed" ne sont pas des chiffres). Il faut valider.

Validation : on feed le texte complet du token (depuis SI) au DFA regex. Si le DFA accepte → match. Sinon → rejeté.

Pour les cas `Contains` (regex peut matcher au milieu du token), on feed les bytes à partir de chaque SI possible. Mais `falling_walk` nous donne déjà le bon SI (c'est la position dans le token où le literal commence).

## Détail : cross-token via sibling links

Si le DFA est alive mais pas accepting à la fin du token, on suit les sibling links :

```
for sib in sibling_table.siblings(current_ordinal):
  next_text = ord_to_term(sib.next_ordinal)
  gap_bytes = gapmap.read_separator(doc_id, pos, pos+1)

  // Feeder gap bytes au DFA
  state = feed(state, gap_bytes)
  // Feeder next token bytes au DFA
  state = feed(state, next_text)

  if accepting(state): émettre match
  if alive(state): continuer la chaîne (depth+1)
```

C'est exactement `continuation_score_sibling` mais en partant des candidats du falling_walk au lieu du scan FST.

## Détail : position du littéral dans le regex

Le falling_walk nous donne les tokens/suffixes contenant le littéral. Mais le regex peut avoir du contenu AVANT le littéral (ex: `.*getElementById`).

Deux cas :

**Littéral = préfixe du regex** (`shard[a-z]+`) :
- Le DFA start state → feed le texte → si le DFA matche, c'est bon
- Simple et direct

**Littéral ≠ préfixe** (`.*getElementById`, `[a-z]+ment`) :
- Le littéral est au milieu ou à la fin du regex
- Le falling_walk nous donne les tokens contenant le littéral
- Mais le regex peut commencer AVANT ce token (dans un token précédent)
- Solution : on valide le match en feedant le texte complet depuis le token courant au DFA.
  Pour `Contains` mode, le DFA du regex est wrappé en `.*regex.*` implicitement,
  donc feeder le texte du token suffit — le DFA gère le prefix `.*` automatiquement.

## Estimation de performance

| Étape | Complexité | Temps estimé (862 docs) |
|---|---|---|
| `extract_literals` | O(pattern_len) | ~0.001ms |
| `falling_walk(literal)` | O(literal_len) | ~0.01ms |
| `ord_to_term` par candidat | O(1) par lookup | ~0.01ms × N |
| Validation DFA par candidat | O(token_len) | ~0.001ms × N |
| Sibling chain + GapMap | O(chain_depth × siblings) | ~0.1ms |
| Resolve postings | O(df) par ordinal | ~1ms |
| **Total estimé** | | **~5-50ms** |

vs. 16 000ms actuellement.

## Edge cases

- **Pas de littéral extractible** (`[a-z]+`, `.*`) : fallback vers `continuation_score_sibling` (scan complet mais avec sibling links pour Walk 2+)
- **Littéral très court** (1-2 chars) : trop de candidats, fallback vers scan
- **Regex avec alternation** (`foo|bar`) : extraire les deux littéraux, faire falling_walk sur chacun, union des résultats
- **Littéral avec échappement** (`\[array\]`) : déséchapper avant falling_walk (`[array]`)
- **Regex case-insensitive** : le SFX FST est déjà lowercase, falling_walk sur literal.to_lowercase()

## Fichiers à toucher

1. **`suffix_contains.rs`** (ou nouveau module) — `regex_contains_via_literal()` : extraction littéraux + falling_walk + DFA validation + sibling chain
2. **`regex_continuation_query.rs`** — scorer route vers `regex_contains_via_literal` quand sibling table dispo
3. **`lucivy_core/src/query.rs`** — aucun changement (build_contains_regex construit déjà RegexContinuationQuery)

## Résumé

On remplace le scan DFA × FST entier par un lookup ciblé sur les littéraux du regex. Même pattern que ripgrep. Le gain devrait être ~100-1000x sur les regex avec littéraux longs, avec fallback transparent vers le scan complet pour les regex purement symboliques.
