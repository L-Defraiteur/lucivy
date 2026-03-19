# Bug — Segments sans sfx/sfxpost (gapmap panic)

Date : 19 mars 2026
Status : cause à investiguer

## Symptôme

Sur 90K docs, certains petits segments (47-140 docs) n'ont ni .sfx ni .sfxpost.
Quand merge_sfx les fusionne, il ajoute des gapmap entries vides pour ces docs.
Le gapmap résultant a une taille incorrecte → panic au search :

```
thread 'scheduler-0' panicked at src/suffix_fst/gapmap.rs:447:15:
index out of bounds: the len is 3 but the index is 3
```

## Diagnostic

```
[merge_sfx] WARNING: seg_ord=1 (140 docs) missing sfxpost (no_file), has_sfx=false
[merge_sfx] WARNING: seg_ord=3 (99 docs) missing sfxpost (no_file), has_sfx=false
[merge_sfx] WARNING: seg_ord=4 (80 docs) missing sfxpost (no_file), has_sfx=false
```

`has_sfx=false` → le SegmentReader n'a PAS de .sfx file pour ces segments.
Ce n'est PAS un problème de GC (les fichiers n'ont jamais été créés).

## Hypothèses

1. **Merge produit des segments sans sfx** : quand merge_sfx écrit le .sfx
   résultant mais un source segment n'a pas de .sfx, le gapmap est incorrect.
   Le merge devrait skip ces segments ou les traiter correctement.

2. **SfxCollector pas initialisé** : certains segments sont créés par un
   chemin de code qui ne passe pas par le SegmentWriter normal. Par exemple
   un segment produit par un merge précédent qui n'avait pas de sfx.

3. **Cascade de merges sans sfx** : si un merge échoue silencieusement à
   écrire le .sfx (collector.build() Err, line 169-171 de segment_writer.rs),
   le segment résultant n'a pas de sfx. Quand ce segment est re-mergé,
   le nouveau merge hérite du problème.

## Fix possible

Le merge_sfx gère déjà le cas `sfx_readers.get(seg_ord) = None` en
ajoutant des empty docs au gapmap. Mais le gapmap et le sfxpost doivent
être cohérents avec le nombre total de docs dans le segment mergé.

Le bug est que le gapmap du segment résultant a un nombre de docs
incorrect — certains docs des segments sans sfx ne sont pas comptés
ou comptés en trop.

## A investiguer

1. Vérifier si les segments sans sfx sont des segments mergés ou des
   segments initiaux (flushés)
2. Ajouter un log dans segment_writer.rs pour vérifier que TOUS les
   segments passent par le SfxCollector
3. Vérifier le calcul du nombre de docs dans le gapmap résultant
   de merge_sfx quand certains source segments n'ont pas de sfx
