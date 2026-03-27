# Doc 05 — Design : fuzzy cross-token via split graph

Date : 26 mars 2026
Branche : `feature/cross-token-search`

## Contexte

Le cross-token exact utilise maintenant un **split graph** qui explore les remainders
uniques (pas les chemins). Chaque remainder n'est exploré qu'une fois → O(unique_remainders)
au lieu de O(paths). Résultat : "getElementById" passe de 8.8s à 10ms.

Question : peut-on réutiliser ce graph pour le fuzzy cross-token ?

## Rappel du problème fuzzy

Avec distance=D (1 à 3), la query peut avoir jusqu'à D erreurs (substitutions,
insertions, suppressions) **n'importe où**. Les erreurs peuvent tomber :
- Entièrement dans un seul token (single-token fuzzy, déjà géré)
- À cheval sur un split : partie gauche et/ou partie droite
- Réparties sur 2+ splits dans une chaîne multi-token

## Idée : fuzzy sur le graph, pas sur le worklist

### Phase 1 : Build du graph avec budget fuzzy

Le graph exact explore chaque remainder via :
- `falling_walk(remainder)` → split candidates
- `prefix_walk_si0(remainder)` → terminal match

Pour le fuzzy, on remplace par :
- `falling_walk(remainder)` → split candidates (exact, inchangé)
- `fuzzy_walk_si0(remainder, remaining_budget)` → terminal match fuzzy

Le point clé : **le falling_walk reste exact**. La frontière de token est une
contrainte structurelle (si + depth == token_len), pas fuzzy. C'est le terminal
(la dernière portion de la query) qui absorbe le budget fuzzy.

### Budget tracking dans le graph

Chaque noeud du graph a un **budget restant** :

```
explore_queue: Vec<(remainder, depth, remaining_distance)>
graph key: (remainder, remaining_distance)
```

Pour le seed : `(query, 0, D)` avec D = distance totale.

À chaque split, le budget ne change pas (le falling_walk est exact, il ne consomme
pas de distance). Seul le terminal consomme le budget :

```
terminal = fuzzy_walk_si0(remainder, remaining_distance)
```

Mais attention : si D=3, on fait `fuzzy_walk_si0(remainder, 3)` partout.
C'est correct mais potentiellement large. Le FST walk avec d=3 est plus lent
qu'avec d=1, mais c'est par remainder unique, pas par chemin → borné.

### Cas : erreurs dans la partie gauche d'un split

Problème : `falling_walk` est exact. Si la query a une erreur AVANT le split
(e.g. "rak3weaver" → typo 'k' dans la partie "rag3"), le falling_walk ne
trouvera pas le split "rag3"|"weaver".

Solutions possibles :

#### Option A : fuzzy_falling_walk pour le premier niveau seulement

Le premier `falling_walk(query)` est remplacé par `fuzzy_falling_walk(query, D)`.
Les niveaux suivants restent exact. Le budget restant pour le terminal est
`D - edits_used_by_first_split`.

Problème : on ne connaît pas `edits_used_by_first_split` exactement.
Approximation : `|fst_depth - prefix_len_query|` (borne inférieure).

Ou plus simple : pour le terminal après un fuzzy first split, on utilise
`remaining_budget = D` (on accepte de potentiellement dépasser D au total).
En pratique c'est rare que l'utilisateur tape D erreurs ET que ça traverse
un token boundary.

#### Option B : générer les variantes de la query

Pour D=1 : générer les 26*L + L + L-1 variantes (substitutions + deletions +
insertions) de la query, et faire un falling_walk exact sur chaque variante.
Les remainders produits sont ensuite explorés normalement.

Pour D=1 et query de 10 chars : ~300 variantes. Chacune fait un falling_walk
O(L). Total : ~3000 opérations FST, probablement < 1ms.

Pour D=2 : ~300² = 90K variantes. Trop cher.
Pour D=3 : inutilisable.

Conclusion : option B ne marche que pour D=1.

#### Option C : fuzzy_falling_walk avec fst_depth (déjà implémenté)

On a déjà `fuzzy_falling_walk` qui utilise un Levenshtein DFA et `fst_depth`
comme split point. Le problème précédent (explosion) était dû au WORKLIST,
pas au falling_walk lui-même. Avec le graph, on explore chaque remainder une
seule fois.

Le fuzzy_falling_walk retourne des candidates avec `prefix_len = fst_depth`.
Le remainder est `query[fst_depth..]`. Plusieurs candidates peuvent avoir
le même remainder → le graph les deduplique.

L'enjeu : le fuzzy_falling_walk peut retourner BEAUCOUP de candidates
(le DFA explore plus de chemins dans le FST). Mais les remainders uniques
sont bornés par la query length → le graph reste petit.

**C'est probablement la meilleure option.** Le graph absorbe naturellement
l'explosion de candidates en ne gardant que les remainders uniques.

#### Option D : ne pas gérer les erreurs gauche en cross-token

Accepter la limitation : le fuzzy cross-token ne gère que les erreurs dans
la partie terminale (droite). Pour les erreurs gauche, l'utilisateur doit
corriger la typo ou le single-token fuzzy la trouve si elle est dans un seul token.

C'est l'option la plus simple et couvre 90%+ des cas utiles.

### Recommandation

**Court terme** : Option D — fuzzy seulement sur le terminal (droite).
Le graph exact fait les splits, `fuzzy_walk_si0(remainder, D)` sur le terminal.
Simple, rapide, pas d'explosion.

**Moyen terme** : Option C — `fuzzy_falling_walk` pour le premier niveau,
intégré dans le graph. Le graph deduplique les remainders → pas d'explosion.
Budget tracking approximatif (remaining_budget = D partout).

**Long terme** : Budget tracking exact. Stocker `edits_consumed` dans chaque
noeud du graph. Le terminal utilise `D - edits_consumed`. Nécessite d'exposer
l'edit distance depuis le DFA state (déjà préparé : `edit_distance_at_prefix`
dans le State struct du Levenshtein DFA).

## Impact mémoire

Le graph a au plus O(L²) noeuds (chaque sous-chaîne de la query est un
remainder possible, L = query length). Pour L=20 : max 200 noeuds.
Chaque noeud stocke ses falling_walk candidates + terminal.
Avec budget tracking : O(L² × D) noeuds max. Pour L=20, D=3 : 600 noeuds.

C'est négligeable comparé aux 265K+ itérations du worklist précédent.
