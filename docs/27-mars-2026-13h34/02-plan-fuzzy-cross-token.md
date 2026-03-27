# Doc 02 — Plan : fuzzy cross-token via sibling links

Date : 27 mars 2026
Branche : `feature/cross-token-search`

## Principe

Le fuzzy s'applique uniquement au **terminal** de chaque chaîne sibling.
Les splits structurels (falling_walk) et les intermédiaires (chaîne sibling)
restent exacts. La distance fuzzy est consommée par le dernier match.

## Changements

### 1. `cross_token_resolve_for_multi` — fuzzy terminal

Ajouter `fuzzy_distance: u8` en paramètre (déjà passé dans `_impl` mais ignoré).

**is_last + fuzzy_distance > 0** :
- `prefix_walk_si0` → `fuzzy_walk_si0(query, fuzzy_distance)` pour les single-token matches
- Dans la sibling chain, le check terminal `next_text.starts_with(rem)` →
  `levenshtein_prefix_match(rem, next_text, fuzzy_distance)` :
  accepter si le remainder est à distance ≤ d d'un **préfixe** du token suivant

**is_last + fuzzy_distance == 0** : inchangé (exact)

**!is_last** : toujours exact. Les tokens intermédiaires doivent matcher
exactement (on ne veut pas de drift cumulatif avec le fuzzy).

### 2. `cross_token_search_with_terms` — fuzzy terminal

Même logique : le sibling chain terminal check devient fuzzy quand
`fuzzy_distance > 0`. Le falling_walk reste exact.

### 3. `levenshtein_prefix_match(query, text, max_distance)` — nouvelle fonction

Retourne true si `query` est à distance ≤ max_distance d'un préfixe de `text`.

Implémentation : matrice Levenshtein standard, mais au lieu de vérifier
`dp[query.len()][text.len()] <= d`, vérifier `min(dp[query.len()][0..=text.len()]) <= d`.

C'est-à-dire : le minimum de la dernière ligne de la matrice DP. Si ce minimum
est ≤ d, alors query matche un préfixe de text (le préfixe optimal est celui
qui donne le minimum).

Coût : O(query.len() × text.len()). Pour query ~10 chars et text ~10 chars,
c'est 100 opérations. Négligeable.

### 4. Pas de fuzzy falling_walk

Le falling_walk reste exact. Le split structurel (si + prefix_len == token_len)
est une contrainte géométrique, pas fuzzy. Si la query a une typo AVANT le
split point (e.g. "rak3weaver"), le falling_walk ne trouvera pas "rag3".

C'est une limitation acceptée :
- Le single-token fuzzy gère les typos qui ne traversent pas un split
- Le cross-token fuzzy gère les typos dans la partie terminale (après le dernier split)
- Les typos qui tombent exactement sur un split point sont rares

### 5. Pas de fuzzy intermédiaires

Les tokens intermédiaires dans une sibling chain doivent matcher exactement.
Raisons :
- Le fuzzy sur chaque step accumulerait la distance (d×N au lieu de d)
- Le token text doit être entièrement consommé (`rem.starts_with(&next_text)`)
  — un fuzzy ici changerait la longueur consommée, décalant tout le reste
- En pratique, les intermédiaires sont des tokens courts (2-6 chars) issus de
  CamelCaseSplit — une typo sur eux est très rare

## Exemples

| Query | Distance | Comportement |
|-------|----------|-------------|
| rag3weaver | 0 | exact chain: rag3 → weaver ✓ |
| rag3weavr | 1 | exact split rag3, fuzzy terminal "weavr" ≈ "weaver" ✓ |
| rag3weavrr | 2 | exact split rag3, fuzzy terminal "weavrr" ≈ "weaver" ✓ |
| rak3weaver | 1 | falling_walk ne trouve pas "rag3" → single-token fuzzy ✗ |
| getElementById | 0 | chain: get → element → by → id ✓ |
| getElmentById | 1 | chain: get → fuzzy terminal? Non — "elmentbyid" n'est pas le terminal. En fait la chaîne fait "get" exact → "elmentbyid" ne matche pas "element" exact → ✗ |

Note : "getElmentById" avec typo dans un intermédiaire ne fonctionnera pas.
C'est acceptable — le single-token fuzzy "getelmentbyid" d=1 trouvera
"getelementbyid" si c'est un token unique indexé.

## Impact code

| Fichier | Changement |
|---------|-----------|
| `suffix_contains.rs` | `levenshtein_prefix_match()` nouvelle fonction |
| `suffix_contains.rs` | `cross_token_resolve_for_multi` : fuzzy prefix_walk + fuzzy terminal sibling |
| `suffix_contains.rs` | `cross_token_search_with_terms` : fuzzy terminal sibling |

Pas de changement dans :
- sibling_table.rs (structure inchangée)
- collector.rs (indexation inchangée)
- suffix_contains_query.rs (fuzzy_distance déjà propagé)
- sfx_merge.rs (merger inchangé)

## Conclusion

### Regex cross-token — à traiter ensuite

Les sibling links ouvrent la porte au regex cross-token :
1. Regex walk sur le premier token (via automaton sur le SFX)
2. Quand le regex DFA atteint la fin du token → sibling link → continuer le DFA
3. Comme `RegexContinuationQuery` mais avec O(1) sibling lookup au lieu de FST search

Le `RegexContinuationQuery` existant fait déjà un DFA continu via gap bytes.
Avec les sibling links, on pourrait accélérer l'étape "trouver le token suivant"
(sibling O(1) vs FST search O(N)). Mais le regex DFA doit traverser les gap bytes
(les sibling links ne stockent pas le contenu du gap) → il faudrait combiner
les sibling links avec le GapMap pour les gap bytes.

À explorer dans une session dédiée.
