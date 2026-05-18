# Bugs découverts par les tests d'intégration v3

**Date** : 17 mai 2026  
**Tests** : 51 passent, 7 échouent

---

## Bug 1 — sep_len overflow (3 bits = max 7)

**Test** : `t7_only_seps` — texte `"________"` (8 underscores)  
**Erreur** : `sep_len overflow: 8` dans `encode_single_parent_v3`

**Cause** : sep_len encodé sur 3 bits dans le output u64 → max 7. Un chunk de 8 bytes de pur séparateur déborde.

**Cas réel** : un doc avec beaucoup de séparateurs (indentation, formatage). Avec MAX_TOKEN=8 et division égale, un segment de seps peut produire des chunks de 7-8 bytes.

**Fix** : élargir sep_len à **8 bits** (max 255). On peut prendre les bits sur own_len (passer de 15 à 14 bits → max 16383, largement suffisant).

**Nouvel encoding** :
```
[63]     multi_flag
[62]     is_word_start
[61..58] overlap_len    (4 bits, 0..15)
[57..50] sep_len        (8 bits, 0..255)  ← élargi de 3 à 8 bits
[49..36] own_len        (14 bits, max 16383)  ← réduit de 15 à 14 bits
[35..24] sti            (12 bits, max 4095)  ← réduit de 16 à 12 bits
[23..0]  token_ordinal  (24 bits)
```

STI max 4095 est suffisant : avec MAX_TOKEN=8 + overlap=2 = 10 bytes max par token, STI ne dépasse jamais 9.

---

## Bug 2 — falling walk chaîné ne traverse pas les pure-sep tokens (strict_sep=false)

**Tests** : `f8`, `s7`, `x11b`, `x11d`, `x12b`  
**Erreur** : query stripped "mutexlock" ne trouve pas "mutex________lock"

**Cause** : le `cross_token_chain_v3` fait un `falling_walk_v3` sur le remainder, mais la partition stripped ne contient que les suffixes de tokens avec `sep_len > 0`. Les tokens pure-sep (`content_len=0`) n'ont PAS d'entrées stripped (rien à stripper). Donc le falling walk sur la query stripped ne trouve rien dans les pure-sep tokens et la chaîne s'arrête.

**Le mécanisme décrit dans le doc 12** (sep-skip immédiat pour `content_len=0`) n'est pas encore implémenté dans `cross_token_chain_v3`. Le chain fait juste falling_walk → fst_candidates en boucle, sans logique de "passer à travers un token vide".

**Fix** : dans `cross_token_chain_v3`, quand un split ne trouve pas de continuation dans le FST, il faut essayer de "traverser" les tokens pure-sep. Ça nécessite de connaître le token suivant (TI+1), ce qui n'est pas possible au niveau FST seul — il faut les postings pour savoir quel est le prochain token dans un doc donné.

**Alternative** : le falling walk via partition stripped devrait naturellement trouver le match car les suffixes stripped de TI=0 ("mutex" + overlap) couvrent le début, et les suffixes de TI=N ("lock") couvrent la fin. Le problème est que le remainder "lock" après le split de "mutex" n'est pas relié au bon ordinal car on ne sait pas quels tokens intermédiaires traverser.

**Solution pragmatique** : dans `find_literal_v3`, combiner single-token (fst_candidates) + cross-token (chain). Si le chain échoue, tenter un **multi-token fallback** : splitter la query stripped en sous-tokens et vérifier l'adjacence via les postings avec des gaps tolérés.

---

## Bug 3 — sep long cross-chunk stripped ne fonctionne pas

**Test** : `t6_long_sep_split` — texte `"a________b"`, query `"ab"` strict_sep=false

**Cause** : même que Bug 2. La query stripped "ab" cherche une chaîne qui traverse les chunks de seps. Le "a" est trouvé dans TI=0 (stripped "a" + overlap), mais le remainder "b" n'est pas trouvé car les tokens pure-sep entre TI=0 et le token "b" ne sont pas traversés.

**Fix** : même que Bug 2.

---

## Priorité

1. **Bug 1** (sep_len overflow) : fix immédiat, change l'encoding u64
2. **Bug 2+3** (traversée pure-sep) : nécessite un mécanisme plus sophistiqué dans la chaîne cross-token pour strict_sep=false. C'est le challenge principal.
