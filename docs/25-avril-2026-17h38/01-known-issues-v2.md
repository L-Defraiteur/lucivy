# Issues connues — v2 unified ShardedHandle

## Bugs mineurs (non bloquants pour release)

### 1. Highlights fuzzy parfois imprécis

Les offsets des highlights en mode fuzzy (d=1) pointent parfois vers
des zones légèrement décalées ou matchent un morceau pertinent mais
pas exactement le terme attendu. Les documents retournés sont corrects,
c'est juste l'offset du highlight qui peut être approximatif.

**Cause probable** : le miss-count boundary-aware utilise des fenêtres
accumulatives, l'offset retourné est celui du meilleur match dans la
fenêtre mais pas forcément aligné sur les frontières de mots.

**Sévérité** : cosmétique. Les résultats de recherche sont bons.

### 2. Regex ne trouve pas les sous-chaînes tronquées

`"programming languag"` (sans le 'e' final) ne match pas en mode regex,
alors que `"programming language"` match. Le mode contains exact trouve
les deux sans problème.

**Cause probable** : le regex prescan utilise un DFA qui attend le pattern
complet. Le mode contains utilise le SFX walk (substring matching) qui
n'a pas cette contrainte.

**Sévérité** : faible. Le regex a un comportement attendu (match le pattern
exact), c'est le contains qui est plus permissif. Mais pourrait surprendre
les utilisateurs qui s'attendent à du substring matching en regex.

### 3. Logs de diagnostic dans la console

Beaucoup de logs `[contains-diag]`, `[cross-token-diag]`, `[build_fst]`
dans la console browser. Purement cosmétique mais bruyant.

**Fix** : rendre ces logs conditionnels (env var ou flag) ou les supprimer.

## Bugs corrigés dans cette session

- **Score ordering inversé** : double-reverse dans MergeResultsNode (fixé)
- **Contains_split highlights** : prescan cache keyed par SegmentId seul,
  deuxième mot écrasait le premier (fixé — keyed par (query_text, SegmentId))
- **Lock file OPFS** : au rechargement, le lock persistait dans OPFS (fixé —
  clean avant import)
