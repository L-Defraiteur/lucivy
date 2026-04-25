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

### 2. Regex multi-mot : l'espace déclenche un split

`"programming languag"` ne match pas en regex, mais `"anguag"` (un seul
mot) match sans problème. Le problème c'est l'espace : la query est
splittée en deux tokens, et `"languag"` est cherché comme token complet
dans le term dict (pas en substring).

Le contains exact n'a pas ce problème car le SFX walk fait du substring
matching cross-token nativement.

**Cause** : le regex multi-token split sur espace et chaque mot passe par
le term dict regex (pas SFX). Un mot tronqué ne match aucun terme exact.

**Fix possible** : pour le regex multi-token, utiliser SFX + regex au lieu
du term dict regex. Ou ne pas splitter sur espace et traiter la query
comme un seul pattern regex cross-token.

**Sévérité** : moyenne. L'utilisateur doit écrire `"programming language"`
(complet) ou `"programming.*languag"` (regex explicit).

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
