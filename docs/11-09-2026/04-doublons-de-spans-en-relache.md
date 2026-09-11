# Doublons de spans en mode relâché — défaut du moteur publié (trouvé et corrigé le 11 septembre)

Trouvé en testant `positions: false` dans le navigateur. Le défaut n'est **pas** dans l'option : il est
dans le layout par défaut, déjà dans **4.0.2 publiée**.

## Comment

Panel de parité du playground (`playground/parity_run.js`, 21 requêtes) sur 2 000 fichiers du noyau
(`?corpus=corpus-kernel-2k.tar.gz`), une fois avec positions, une fois `?nopos`, rapports comparés par
`playground/parity_diff.py`. Une seule vraie différence : `contains_split "spin lock init"`, document
31 (`fs/dcache.c`), **846 spans avec positions, 838 sans**. La vérité terrain relâchée (définition de
`fold_into` : minuscules, tout caractère non alphanumérique ôté, occurrences chevauchantes) donne
**838** : `spin` 156, `lock` 627, `init` 55 — l'index sans positions est exact, span pour span.
L'index par défaut rend 635 spans pour `lock`, dont 627 distincts : **8 doublons**, tous `lock` dans
`superblock`.

## Cas minimal

Un document `superblock`, `contains` relâché `lock` : `[[6,10],[6,10]]`, score 0,3956 au lieu de
0,2877 pour une occurrence. Même chose en v3 (`shared_dictionary: false`) et avec le paquet npm
`lucivy@4.0.2`. Strict, fuzzy et sans positions : un seul span.

Observé : le mot est découpé en morceaux (`superblock` → `super` + `block`, au-delà de 8 octets ;
`aaaaalock` double, `aaaalock` non) et l'aiguille se termine avec le mot, dans le dernier morceau
(dans `superblock` : `k` … `block` doublent, `rblock` et plus longs non ; `lockaaaaa` non).

## Pourquoi

Trace `V3_DIAG_LITERAL=lock` : la même occurrence sort deux fois de la phase littérale —

    pos=0 span=2 byte=[6..10] head="superblock" head_is_ws=Some(true)   (entrée mot sans séparateurs)
    pos=1 span=1 byte=[6..10] head="block"      head_is_ws=Some(false)  (le morceau)

La déduplication de `briques/orchestrator.rs` (après la phase littérale et après la vérification) a
pour clé `(doc_id, position, byte_from)` : positions différentes, les deux restent. Pour `aaaalock`
(non découpé) les deux entrées portent la position 0 et la déduplication les fond.

## Ampleur

Sur 10 000 fichiers du noyau (`corpus-kernel-10k`, navigateur), la seule requête
`contains_split "spin lock init"` : **585 documents portent des spans en double, 3 050 doublons**. Pour
chaque document du top-10 qui diffère, les spans *distincts* de l'index par défaut sont exactement ceux de
l'index sans positions (665 sur 703, 224 sur 229, 295 sur 299, 414 sur 458).

## Conséquences

Spans en double dans les highlights ; tf gonflé, donc score BM25 et ordre faussés pour ces documents.
Le compte de documents est juste. Touche toute requête relâchée dont l'aiguille termine un mot long
(`lock` dans `superblock`, `spinlock`, `unlock`…).

## Pourquoi la vérité terrain ne l'a jamais vu

`test_sfx_v3_ground_truth.rs` compare les spans comme des ensembles (`HashSet`, calcul de
`missing` / `extra`) : un doublon y disparaît. Le panel de parité compte les spans (longueur des
listes), c'est lui qui l'a montré.

## Correctif (appliqué le 11 au soir, décision de Lucie : sur v4.1)

1. `orchestrator::dedup_occurrences`, aux deux endroits : clé `(doc_id, byte_from, byte_to)` pour un
   match placé, sa position s'il ne l'est pas (`place_spans` laisse `0..0` à ce qu'il ne peut placer) ;
   l'ordre `(doc_id, position, byte_from)` est rendu ensuite. Deux occurrences réelles dans un même
   morceau (`initinit`) gardent des octets différents et restent deux.
2. Le harnais compte les doublons comme spans en trop (`duplicates = highlights.len() − ensemble`), dans
   le panel et dans la comparaison des formes (1 shard, 4 shards, 2 nœuds).

Rouge puis vert, sur l'index de 10 000 fichiers (dictionnaire, positions) : avant, `lock` relâché
`extra=542`, `init` relâché `extra=68` ; après, exacts. Test `test_relaxed_duplicate_spans` (échoue sur
l'ancien code : `[(6, 10), (6, 10)]`) : un span par occurrence en v3 et dictionnaire, avec et sans
positions, et l'index par défaut score comme l'index sans positions. Temps (trois passes, 10 000
fichiers) : `de` strict 25,6-26,7 → 27,2-27,8 ms (+4 %, le tri de plus), `lock` relâché 5,8-6,2 →
5,7-6,5 ms.

Panel de vérité terrain complet sur 10 000 fichiers, doublons comptés : **10/10 en dictionnaire avec
positions, 10/10 sans positions, 10/10 en v3** ; `cargo test --lib` 1 471 verts, `lucivy-core` vert,
clippy propre, Node `positions.mjs` vert, WASM rebâti.

Navigateur, WASM rebâti avec le correctif, 10 000 fichiers : le panel de parité donne **20 requêtes sur
21 identiques** entre l'index par défaut et l'index sans positions, `split spin lock init` compris
(comptes, top-10, scores, spans) ; la 21ᵉ est un ex æquo à la coupure (`kmalloc AND NOT kfree`, 10ᵉ
document, même score 3,497132). Tailles inchangées : 1 051 Mo contre 637.

Texte non ASCII (question de Lucie : « risque rien sur les smileys ou non ASCII ? ») : la clé est faite
d'octets du texte source, deux occurrences distinctes ne peuvent pas y avoir les mêmes. Le test couvre des
mots longs découpés en morceaux avec accents (`déjàsuperblock`), repli de casse sur deux octets
(`ÉTÉsuperblock`, `été`), un emoji séparateur (`superblock🙂superblock`) et 36 octets de CJK d'un seul mot
(`理解` trois fois) : un span par occurrence, aux bons octets, et mêmes scores qu'avec l'index sans
positions, en v3 et en dictionnaire.
