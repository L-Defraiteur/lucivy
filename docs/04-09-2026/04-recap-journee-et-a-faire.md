# 4 septembre 2026 — récap de la journée et ce qui reste

Écrit pour être lu seul. Le contexte : la promotion est en pause depuis le
28 août parce que l'index fait ×21 le texte ([28-08/07](../28-08-2026/07-rapport-progression-et-taille-index.md)).
Aujourd'hui, un audit du format ([02](02-audit-taille-index-sfx-v3.md)),
un plan ([01](01-recap-findings-et-plan-d-action.md)) et sept étapes
mesurées une à une ([03](03-journal-des-etapes.md)), sur la branche **`v4`**.

---

## 1. Le résultat

| corpus | v3 | v4 ce soir | delta |
|---|---|---|---|
| 10 000 fichiers du noyau (référence, 160 segments) | 1 152 Mo | **735 Mo** | **−36,2 %** |
| 30 000 fichiers (120 segments) | 3,4 Go | **2,3 Go** | −33 % |

Sur les 93 983 fichiers de la comparaison du 28 août, ×21 devient environ
**×13,5**. Loin du ×5 discuté, mais obtenu **sans toucher au modèle de
recherche** : chaque étape est de l'encodage, et une seule (5a) change ce
que le builder enregistre, pour des clés qui étaient des doublons.

**Exactitude** : à chaque étape, le panel `v3_ground_truth_demo` (comptes
**et** spans d'octets comparés au disque) rend les mêmes lignes, sur les
deux corpus. **Compatibilité** : les index écrits par chaque version
antérieure du jour (conteneur `.sfx` 3, 4, `.posmap` 4 octets, `SIB2`,
`.termtexts` layout 1, avec ou sans `.bytemap`) rouvrent avec le binaire du
soir et passent le panel.

**Temps** (30 000 fichiers, A/B par commit sur son propre index, puis au même
binaire entre formats, trois à cinq passes) : les requêtes exactes sont
**plus rapides qu'en v3** (`mutex_lock` strict 3,3 → 2,0 ms), le fuzzy lourd
et Jaro-Winkler inchangés, le fuzzy relâché et la regex à ±1 ms. Règle posée
par Lucie et écrite dans le plan : la taille d'abord, l'exactitude est ce
qu'on vend, le temps acceptable tant qu'on est loin de ×1,5.

---

## 2. Ce qui a été appris, au-delà des octets

- **Le `.sfx` était aux trois quarts une table de parents**, pas une FST :
  une entrée de 11 octets par (suffixe, chunk), répétant cinq champs de méta
  stockés par ailleurs. C'est là que la journée a gagné le plus.
- **Le panel de 10 000 fichiers ne discrimine pas la milliseconde.** Le
  checkpoint après les étapes 1 à 4 l'a montré : sur 30 000 fichiers, par
  commit, le fuzzy relâché avait perdu 3 ms par petits paliers, parce que
  deux étapes remplaçaient une lecture dans un fichier dédié par une
  lecture de META dans une autre région de `.termtexts`. L'étape 6 a mis la
  méta à côté de l'offset et les a rendus.
- **Un A/B au même binaire ne mesure que le format** ; pour le code il faut
  le binaire de chaque commit (worktree + `CARGO_TARGET_DIR`). Les deux
  sont dans le journal avec leurs commandes.
- **Les listes de parents géantes sont sur les clés courtes** — `_` porte
  54 747 parents sur un segment — et elles sont denses dans l'espace des
  ordinaux, donc le delta-varint les écrase (étape 8) et retirer les
  marqueurs (5b) les ferait décoder à chaque frontière : renoncé, chiffres
  dans le plan.
- Deux défauts préexistants trouvés en route : `V3_DIAG_FUZZY` paniquait en
  tronquant une fenêtre au milieu d'un caractère multi-octets (corrigé) ;
  `luce_v3_sharded_roundtrip` compare un ordre de top-10 entre documents à
  score égal, qui dépend de l'ordre de réponse des shards (**à corriger** :
  tri stable par id entre ex æquo).

---

## 3. État du dépôt

- **`v4`** : `wip/publication-3.0.0` + l'audit + 8 commits de format. Pas
  poussée. `main` = `origin/main`. Les trois commits du 28 août sont sur
  `wip/publication-3.0.0`, pas poussés non plus.
- Un binaire 3.0.x **ne lira pas** un index v4 (conteneur `.sfx` 5, `PMP3`,
  `SIB3`, `.termtexts` layout 2, pas de `.bytemap`). Le workspace passera à
  4.0.0 à la publication. La question de garder ou non la pile v2
  (`gapmap`, `sepmap`, `file.rs`, `sfx_dag.rs`…) est ouverte : elle ne pèse
  rien sur disque, c'est une décision de compatibilité.
- Le harnais de mesure : `benches/scan_index_size.py` lit tous les layouts
  du jour ; `measure_parents_by_key_length` (test ignoré) donne les listes
  de parents par longueur de clé.
- Le scratchpad de la session garde le worktree `wt-v3` (commit `1c263f3`,
  le binaire v3) et les index de référence ; ils se reconstruisent avec les
  commandes du journal.

---

## 4. À faire

1. **Étapes 7 et 9** du plan, environ −4 % : `.sfxpost` sans `bt − bf`,
   `.word_sfxpost` sans `to − from` (hypothèse à tester d'abord), deux
   espaces d'ordinaux.
2. **Tri stable des ex æquo** dans le merge des shards, puis
   `luce_v3_sharded_roundtrip` sous charge.
3. **Le prochain palier de taille : le dictionnaire partagé par shard**,
   piste retenue par Lucie le soir même — [05](05-piste-dictionnaire-partage-par-shard.md).
   Mesuré : le dictionnaire est répété ×2,2 d'un segment à l'autre, et il
   fait 71 % de l'index ; le partager vaut −35 à −40 % de plus, réduit les
   marches de FST par requête (une par génération de dictionnaire au lieu
   d'une par segment) et lève la borne des 24 bits d'ordinal en passant
   tous les parents en table, ce qui règle aussi l'overlap dans la clé. Les
   deux autres pistes (overlap dans la valeur, octets des mots à la
   demande) sont dans le même document, en second rang.
4. L'index complet des 93 983 fichiers en v4 est mesuré : **11,06 Go,
   ×12,3** (253 segments, panel identique au 28 août). La compaction vers
   10 segments par le harnais **échoue** sur la borne des 24 bits
   (`merge_segments_v3` : 17,9 M de termes sur 4 segments) — l'index de
   bench du 28 août à 10 segments avait été fusionné par un autre chemin ;
   à élucider, et [28-08/06](../28-08-2026/06-comparaison-moteurs-mesures.md)
   reste à mettre à jour (×3,6 Elasticsearch, ×0,8 tantivy, nous ×12,3).
5. Décider de la pile v2, de la version 4.0.0, et de ce que devient
   `wip/publication-3.0.0` (fusion dans `main` puis `v4` par-dessus).
