# Doublons de spans en mode relâché — défaut du moteur publié (trouvé le 11 septembre)

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

## Correctif proposé (non appliqué — décision de Lucie)

1. Dédupliquer par occurrence : clé `(doc_id, byte_from, byte_to)` au lieu de
   `(doc_id, position, byte_from)` (deux occurrences réelles dans un même morceau, `INIT2INIT`, gardent
   des octets différents).
2. Le harnais compte les doublons comme spans en trop (liste de l'index contre ensemble de la vérité).
