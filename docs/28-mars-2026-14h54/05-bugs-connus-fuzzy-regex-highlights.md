# 05 — Bugs connus : fuzzy, regex, highlights

Date : 28 mars 2026

## Bug 1 : Fuzzy "rak3weaver" d=1 ne trouve pas "rag3weaver" (cross-token)

### Symptôme
Query "rak3weaver" d=1 en mode contains. Le texte indexé contient "rag3weaver"
(un seul token) ou "rag3" + "weaver" (deux tokens). Le fuzzy ne retourne que
les docs qui contiennent littéralement "rak3weaver" dans leur texte (mentions
dans les docs de test), pas ceux qui contiennent "rag3weaver".

### Cause
Le filtre trigram passe correctement (chain_len=5, trigrammes 3-7, threshold=4).
Le bug est dans la **validation DFA Levenshtein** qui suit :

1. Le premier trigramme matché est "3we" (index 3), qui est **cross-token**
   ("3" fin du token "rag3", "we" début du token "weaver")
2. Le code cherche la position du premier trigramme dans `all_matches` pour
   trouver `first_pos` (la position du token)
3. Il prend le token à `first_pos` et le feed au DFA depuis l'offset du trigramme
4. Mais "3we" chevauche deux tokens — le token à `first_pos` est "rag3" (position 0
   dans le doc), et l'offset de "3we" dans "rag3" est byte 3
5. Le DFA reçoit "3" (fin de "rag3") puis devrait recevoir le gap + "weaver"
   via `validate_path`, mais le DFA pour "rak3weaver" attend "rak3w..." dès le
   début, pas "3..."

### Fix proposé
Pour les trigrammes cross-token, il faut reculer au début du match complet,
pas au début du premier trigramme. Le DFA doit être feedé depuis le byte
qui correspond à la position 0 de la query dans le texte, c'est-à-dire
`first_bf - query_position_of_first_matched_trigram`.

Alternative : ne pas valider par DFA les candidats cross-token. Utiliser
uniquement le filtre trigram + byte span pour ces cas. Le risque de faux
positif est minime quand 5+ trigrammes matchent dans le bon ordre.

### Workaround actuel
Le fuzzy fonctionne pour les tokens non-cross (single token). "rak3db" d=1
→ "rag3db" fonctionne car les bigrammes matchent dans un seul token et le
DFA validation n'a pas de problème cross-token.

## Bug 2 : Highlights fuzzy décalés

### Symptôme
Fuzzy "rag3weavr" d=1 → highlight `[1273, 1281]` = "rag3weav" au lieu de
"rag3weaver" (10 chars). Le highlight est trop court (8 chars au lieu de 10)
et commence parfois au mauvais endroit.

### Cause
`intersect_trigrams_with_threshold` retourne `(first_bf, last_bt)` du premier
et dernier trigramme matché. Pour "rag3weavr" d=1 :
- Premier trigramme matché = "rag" à byte_from=0
- Dernier trigramme matché = "avr" qui ne matche pas, donc "eav" à byte 5,
  byte_to = 8

Le highlight devrait être `[0, 10]` (toute la chaîne "rag3weaver") mais
on a `[0, 8]` (seulement jusqu'au dernier trigramme matché).

### Fix proposé
Étendre le highlight : `first_bf` reste identique, mais `last_bt` devrait
être `first_bf + query_text.len() + distance` (la longueur maximale du
match dans le texte).

## Bug 3 : Regex "sched[a-z]+" retourne 0 résultats

### Symptôme
Le texte contient "schedule" et "scheduler". Le regex `sched[a-z]+` devrait
matcher ces tokens. Retourne 0 résultats dans le test natif (5 docs).

### Cause
Non investiguée. Le `find_literal("sched")` trouve 2 matches dans un segment
mais le DFA validation échoue. Probablement un problème de DFA state après
le feed du littéral — le DFA `sched[a-z]+` après "sched" attend `[a-z]+`
mais le `is_match` n'est vrai que quand au moins 1 char `[a-z]` a été feedé.
Le early return dans `validate_path` pourrait ne pas se déclencher si le token
est "schedule" et que le DFA n'est pas en état match après "sched" seul.

### Note
Ce bug existait peut-être avant nos changements — le regex path n'a pas
été modifié pour ce pattern. A investiguer.

## Bug 4 : Regex "sched[a-z]+" résultats inconsistants cross-segment

### Symptôme
`find_literal("sched")` retourne 2 matches dans un segment, 0 dans les autres.
Mais le texte "schedule" est dans doc 4 qui peut être dans n'importe quel segment.

### Cause
L'index a 3 segments (commit crée un segment). Les docs ne sont pas forcément
dans le même segment. Le `find_literal` est per-segment. Si "schedule" est dans
le segment 2 et que `find_literal` retourne 0 dans les segments 0 et 1, c'est
normal — le mot n'est pas dans ces segments.

Le vrai problème est que quand `find_literal("sched")` TROUVE le token, le
DFA validation échoue quand même (bug 3).

## État des tests

### Tests Rust natifs (ld-lucivy) : 1181/1181 passent
Tous les tests unitaires passent, y compris les 21 tests fuzzy/regex.
Les tests utilisent un index en RAM avec 3 docs. Le fuzzy fonctionne
correctement dans ce contexte.

### Test natif sur .luce (lucivy_core) : partiellement
- Contains exact : 3/3 ✓
- Fuzzy : 4/4 trouvent des résultats (mais bug 1 = matches littéraux pas fuzzy vrais)
- Regex : 1/2 (bug 3 sur "sched[a-z]+")
- Highlights : incorrects pour fuzzy (bug 2)

### Test Python binding : fonctionne
Nécessite de passer un **dict** à `idx.search()`, pas un `json.dumps(string)`.
Si on passe un string, le binding le traite comme une query texte
(contains_split sur chaque mot du JSON).
