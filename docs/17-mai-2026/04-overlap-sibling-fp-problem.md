# Problème : FP causés par le mélange d'overlaps dans les content-prefix ordinals

## Résumé

Les content-prefix ordinals regroupent tous les tokens avec le même contenu
(leading content chars) sous un seul ordinal. Cela inclut des tokens avec
des **overlaps différents**. Le falling walk matche les bytes d'une variante
d'overlap, mais le resolve prend le posting d'une autre variante dans un
autre doc — les bytes à cette position sont différents → faux positif.

## Exemple diagnostiqué

Query "struct" (6 bytes), fichier `stats_info.test` (doc 10).

```
pos=42: actual_ord=7184  text="exist.\n-ST"
pos=43: actual_ord=6136  text="RETURN x;"
```

Le token au pos 42 a content prefix "exist" (5 bytes). À sti=3 :
- text[3]='s', text[4]='t', text[5]='.' (sep)
- Query byte 2: 'r' ≠ '.' → le walk aurait dû ÉCHOUER

Mais le walk a matché sur un AUTRE token avec le même ordinal 7184 — un token
avec content "exist" + overlap "ru" (au lieu de overlap ".\n"). Dans CE token,
text[3..7] = "stru" → walk OK. Mais le posting au (doc 10, pos 42) est pour
le token avec overlap ".\n", pas "ru".

## Cause racine

Content-prefix ordinals : `extract_content_prefix(text)` = leading alphanum chars.

- Token A : "exist.\n-ST" → content prefix "exist" → ordinal X
- Token B : "existru"     → content prefix "exist" → ordinal X (même !)

L'ordinal X a des postings dans des docs pour les DEUX tokens. Le FST a deux
clés : "\x00exist.\n-ST" et "\x00existru". Le falling walk matche sur la clé
"existru" (qui a "stru" à sti=3). Mais le posting résolu pour (doc 10, pos 42)
vient du token "exist.\n-" (dont les bytes à sti=3 sont "st.", pas "stru").

## Pourquoi la v2 n'avait pas ce problème

En v2, les ordinals étaient basés sur `text[..own_len]` (content + sep, sans
overlap). "exist.\n-" et "exist " ont des ordinals différents (seps différents).
Les overlaps n'étaient pas dans le texte interné → pas de mélange.

## Ce qu'on a essayé et pourquoi ça n'a pas marché

1. **Content-prefix ordinals (text sans sep)** : fixe les FN cross-token
   (même ordinal pour seps différents) mais crée des FP (mélange d'overlaps)

2. **Word map global** : dit "ordinal X peut être chunk N de mot Y" mais
   c'est vrai pour TOUTES les variantes → ne filtre pas

3. **Word pos map per-doc** : dit "pos P et pos P+1 sont dans le même mot"
   → intra-word = toujours valide → ne filtre pas

4. **TermTexts verification** : le TermTexts stockait le texte word-stripped
   au lieu du chunk (bug trouvé et fixé). Mais même avec le bon texte,
   TermTexts ne stocke qu'UN texte par ordinal → peut pas distinguer les
   variantes d'overlap

## Options pour résoudre

### Option A : Séparer les ordinals par overlap (revenir vers v2)
- Ordinal = `text[..own_len]` (content + sep, comme v2)
- Avantage : pas de mélange d'overlaps, pas de mélange de seps
- Inconvénient : les cross-token chains ont besoin du forking (différents
  seps → différents ordinals → le chain doit tester tous les ordinals)
- C'est ce qui causait les FN au début de la session

### Option B : Garder content-prefix ordinals + post-filtre byte-exact
- Après le resolve, lire les bytes du stored field et comparer avec la query
- 100% exact, aucune structure supplémentaire
- Coût : I/O par match candidat (mais typiquement <200 chain matches)
- Le plus simple à implémenter

### Option C : Ordinals hybrides
- Pour les single-token matches : content-prefix ordinals (pas de FP car le
  content est identique pour toutes les variantes)
- Pour les chains : utiliser l'ordinal own_len-based du PREMIER token (qui
  distingue les seps). Le dernier token utilise content-prefix (trouvé via
  fst_candidates)
- Complexe mais élimine les FP à la source

### Option D : Overlap-aware content ordinals
- Content key = content_prefix + hash des overlap bytes
- Sépare les variantes d'overlap tout en gardant les seps fusionnées
- Mais les overlaps dépendent du contexte (next token) → le même chunk
  dans des contextes différents aurait des ordinals différents
- Revient presque à l'ordinal text[..own_len+overlap_len] = le texte complet

### Option E : Falling walk overlap filtering
- Le falling walk valide N bytes d'overlap (overlap_validated)
- Au resolve, pour chaque posting du premier token, vérifier que le
  byte_to - byte_from == own_len attendu du split's parent entry
- Si own_len diffère → la variante d'overlap est différente → rejecter
- Déjà discuté : pas 100% fiable (deux variantes peuvent avoir le même
  own_len avec des seps/overlaps différents)

### Option F : Dual posting — content ord + specific ord
- Chaque posting stocke DEUX ordinals : le content-prefix ord (pour search)
  et le specific ord (pour verification)
- Au resolve, on cherche par content ord (large), puis on vérifie le
  specific ord correspond à ce que le falling walk attendait
- Plus d'espace mais exact

## Recommandation

Option B (post-filtre byte-exact) est la plus pragmatique : simple à
implémenter, 100% exact, et le coût I/O est négligeable pour le nombre
de chain matches typique.

Option A (retour aux ordinals own_len-based) est la plus propre
architecturalement, mais nécessite de résoudre le problème de forking
des chains (ce qui nous avait amené aux content-prefix ordinals).

Les deux ne sont pas exclusives : on peut faire Option B maintenant
(quick win) et Option A plus tard (clean architecture).

## Bugs trouvés et fixés cette session

1. **Word-stripped postings en doublon** : les word-stripped entries ajoutaient
   des postings au même (doc, pos) que les chunks normaux, sous un ordinal
   différent. Fix : mapper les word-stripped vers le même content ordinal
   que le premier chunk dans into_data().

2. **TermTexts écrasé par word-stripped** : la boucle TermTexts dans
   AssembleV3Node ne filtrait pas is_word_stripped, laissant le texte
   word-stripped écraser le texte chunk. Fix : ajouter
   `if meta.is_word_stripped { continue; }` dans la boucle TermTexts.
