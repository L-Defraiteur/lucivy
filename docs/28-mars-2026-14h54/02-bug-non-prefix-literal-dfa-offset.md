# 02 — Bug : regex avec littéral non-préfixe du token

Date : 28 mars 2026

## Symptôme

`ag3.*ver` retourne 0 résultats. `rag3.*ver` fonctionne correctement.

## Cause

1. `extract_all_literals("ag3.*ver")` → ["ag3", "ver"]
2. `find_literal("ag3")` trouve "ag3" comme substring de "rag3db" via le suffix FST → match correct, byte_from=1, byte_to=4 dans "rag3db"
3. DFA validation : le DFA du pattern `ag3.*ver` est feedé le texte **complet** du token "rag3db" depuis le byte 0
4. Le DFA attend "a" comme premier byte, reçoit "r" → **meurt immédiatement**

Le fix existant (doc 13, point 16) feed le texte complet pour les non-prefix literals quand le pattern commence par `.*` (ex: `.*weaver`), car le `.*` au début du DFA accepte n'importe quoi. Mais `ag3.*ver` n'a PAS de `.*` au début — le DFA attend strictement "ag3" dès le premier byte.

## Le vrai problème

Le DFA est feedé depuis l'offset 0 du token, mais le littéral matche à l'offset 1 ("ag3" dans "**r**ag3db"). Il faudrait feeder le DFA à partir de `byte_from` du match littéral dans le token, pas depuis le début.

## Piste de fix

### Option A : feeder le DFA à partir de byte_from

Pour un single-literal match avec `is_prefix = false` :
```rust
// Au lieu de feeder le token complet depuis byte 0 :
let text = ord_to_term(ordinal);
feed_dfa(automaton, text.as_bytes());

// Feeder depuis byte_from du match littéral :
let text = ord_to_term(ordinal);
feed_dfa(automaton, &text.as_bytes()[byte_from..]);
```

Mais il y a un cas subtil : le pattern `ag3.*ver` attend que "ag3" soit au DÉBUT du match, pas au milieu d'un token. Si on feed depuis byte_from=1, le DFA voit "ag3dbXXX" et accepte — mais est-ce que "ag3" au milieu de "rag3db" est un match valide pour le pattern `ag3.*ver` ?

Réponse : **oui**, car on fait du **contains** (le regex peut matcher n'importe où dans le texte). `ag3.*ver` en mode contains signifie "il existe une sous-chaîne qui matche `ag3.*ver`". La sous-chaîne "ag3db is cool version" matche.

### Option B : wrapper le pattern dans `.*(...)`

Transformer automatiquement `ag3.*ver` en `.*ag3.*ver` avant la compilation DFA. Le `.*` initial permet au DFA d'avancer dans le texte jusqu'à trouver le début du match.

Avantage : pas besoin de gérer byte_from dans le feed.
Inconvénient : le DFA explore plus d'états (le `.*` initial ne prune jamais).

### Option C : feeder le DFA token par token depuis la position du littéral

Utiliser le PosMap pour lire les tokens à partir de la position du littéral match, feeder le DFA séquentiellement. C'est ce que `validate_path` fait déjà pour le cross-token.

Pour le single-token case, feeder seulement `&text[byte_from..]`.

## Recommandation

**Option A** pour le single-token case (simple, précis), combiné avec **Option C** qui est déjà implémenté pour le cross-token case.

Le fix est petit : dans `regex_contains_via_literal`, quand `is_prefix = false`, feeder le DFA depuis `byte_from` du match littéral au lieu de byte 0.
