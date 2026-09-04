# Journal du chantier « dictionnaire partagé » — 4 septembre 2026, soirée

Suite de [06](06-chantier-dictionnaire-partage-rapport.md) (le plan) et de
[08](08-knowledge-dump-baselines-tests-outils.md) (le protocole). Une entrée
par étape : la mesure, la décision, le changement, la vérification. Tout est
sur la branche `v4`.

---

## 0. Les mesures du §4 du plan, faites avant de coder

**Répétition du dictionnaire sur le noyau entier** (`idx90k-v4`, 253
segments × 2 champs, script `dict_repeat.py` du scratchpad : haché 64 bits
de texte + forme, 2 min) :

| | ordinaux (somme) | distincts | répétition |
|---|---|---|---|
| tout | 67 339 487 | 25 574 178 | **×2,63** |
| chunks | 39 818 893 | 14 697 670 | ×2,71 |
| mots dépouillés | 27 520 594 | 10 876 508 | ×2,53 |

Un dictionnaire partagé tiendrait dans **38 %** de l'actuel. Textes
distincts par nombre de segments qui les portent : 1 segment 17,9 M ; 2-3
4,8 M ; 4-10 2,1 M ; 11-50 731 k ; 51-150 102 k ; plus de 150 : 18 k. Les
ordinaux, eux, sont à 27 % dans des textes présents dans 1 segment, à 73 %
dans des textes répétés. **25,6 M de distincts dépassent 2²⁴** : l'ordinal
large est obligatoire, ce qui a fixé l'ordre des étapes ci-dessous.

**Coût d'une génération** (`V3_PROFILE=1`, `[fst]` par segment, release,
charge 2,3) : linéaire, ≈ 0,15 µs par entrée soit ≈ 0,4 µs par clé —
162 k entrées → 20 ms ; 682 k → 92 ms ; 2,0 M → 306 ms ; 3,2 M → 494 ms.
Une génération d'un million de clés coûte 0,4 s ; ce n'est pas la
granularité des commits qui posera problème.

**Posmap sur 4 octets** : +33 % de 4,7 % de l'index ≈ +1,6 %. Négligeable.

**Layouts du `.sfx`** (test ignoré `measure_sfx_layouts`, `SFX_FILE=…`, qui
ré-encode un segment réel sous chaque candidat et mesure FST + table) :

| segment | base | tous en table | (ord, sti) seuls | clés coupées | coupées + (ord, sti) |
|---|---|---|---|---|---|
| 30k `6d785185.2` | 25,8 Mo | −0,5 % | −21,6 % | **−25,3 %** (858 819 → 314 393 clés) | −34,7 % |
| 10k `dfcc233c.2` | 10,4 Mo | −1,4 % | −35,4 % | **−41,1 %** (487 045 → 91 791 clés) | −55,4 % |
| 90k `0a7c18e4.2` | 34,5 Mo | +0,1 % | −24,0 % | **−27,7 %** (1 266 403 → 430 849 clés) | −38,4 % |

Lecture : couper les clés à la frontière du token retire **63 à 81 % des
clés**, pas seulement les marqueurs — deux chunks au même texte propre et à
overlap différent (deux ordinaux) partagent désormais une clé. « (ord, sti)
seuls » = un record sans `own_len`/`sep_len`/flags (lus dans `.termtexts`
pour les survivants) ; gardé pour plus tard (2c), le temps d'abord.

---

## 1. Étape 2a — conteneur `.sfx` version 6 : tous les parents en table

**Changement.** Un parent unique était packé dans les 63 bits de la valeur
FST (ordinal 24 bits) ; les records de la table ne servaient qu'aux clés à
plusieurs parents. Chaque clé pointe maintenant sur un record delta-varint
(un parent comme cent) : les offsets croissent avec les clés, la FST les
partage le long de ses chemins, et l'ordinal est un varint. Le lecteur
ouvre encore 3, 4, 5 ; une version plus récente que la sienne est refusée
(`SfxV3Error::UnsupportedVersion`) au lieu d'être rabattue. La borne des
24 bits reste une erreur dure du builder tant que `.word_pos_map` (slot
`ordinal 24 | span 8`, overflow silencieux → la carte n'est pas écrite) et
`.posmap` (`PMP3`) ne sont pas élargis.

**Vérification.** Panel 10 000 fichiers identique (comptes et spans) en
nouveau layout et en réouverture de `idx-v8` (conteneur 5). Taille :
735 → 729 Mo (10k), `.sfx` 30k 1 210 → 1 208 Mo. A/B au même binaire sur
30 000 fichiers, 3 passes, min :

| requête | v5 | v6 | ratio |
|---|---|---|---|
| mutex_lock strict | 2,8 | 2,1 | 0,75 |
| mutex_lock relax | 1,8 | 1,6 | 0,89 |
| sched term | 3,9 | 3,5 | 0,90 |
| printk sw | 2,6 | 2,2 | 0,85 |
| schdule fz1 | 13,6 | 13,9 | 1,02 |
| regsiter fz2 | 127,9 | 130,2 | 1,02 |
| spin_lock_[a-z]+ rx | 9,7 | 9,5 | 0,98 |

Le déréférencement ne coûte rien ; la FST plus compacte fait gagner les
requêtes exactes. Commit `bc58001`.

---

## 2. Étape 2b — version 7 : clés coupées à la frontière, overlap dans le record

**Changement.** `add_token` n'émet plus qu'une clé par suffixe, arrêtée à
`own_len` (`add_word_stripped` : au contenu du mot), sans marqueur ; les
octets d'overlap suivent la clé dans `key_buf` et entrent dans le record.
`ParentEntryV3` et `FstCandidateV3` portent `overlap: [u8; 4]`
(`MAX_OVERLAP_BYTES`, erreur dure au-delà ; le collecteur en prend 2).

**Le record v7, en trois essais mesurés.**

1. *Plat, l'overlap après chaque parent* (`.sfx` 30k 1 208 → 746 Mo,
   −38 %) : la marche décodait à chaque frontière **tous** les parents de
   la clé — tous les overlaps — avant d'en garder un ou deux ; `sched term`
   ×1,2, `mutex_lock relax` ×1,4, le profil montrait la marche des mots
   doublée (15 → 31 ms cumulés).
2. *Groupé par overlap, tous les records* (`decode_parents_where` ne lit
   que le groupe qui continue la requête) : les temps reviennent (fuzzy
   −20 %), mais 746 → 897 Mo : l'en-tête de groupe coûtait deux octets sur
   chaque clé à un parent, et les ordinaux d'un groupe, plus clairsemés,
   un octet de delta de plus. Un en-tête compact d'un octet n'a rendu que
   23 Mo.
3. **Retenu** : *plat jusqu'à 32 parents* (`FLAT_RECORD_MAX_PARENTS`,
   l'overlap après chaque parent, en-tête = le compte), *groupé au-delà*
   (bit 7 ; par groupe `[u8 len][overlap][varint n][zigzag Δpremier
   ordinal vs groupe précédent][varint byte_len sauf le dernier]`, deltas
   dans le groupe). Simulé par `measure_sfx_layouts` sur les segments
   réels : −22,5 % (30k) et −36,3 % (10k) contre −25,3 / −41,1 pour le plat
   idéal ; renuméroter les ordinaux par overlap (variante mesurée avec
   `TERMTEXTS_FILE`) n'ajoute qu'un point, abandonné.
   Sur un segment de 30 000 fichiers : 310 501 records plats, 3 892 groupés.

Côté requête (`fst_walk.rs`), deux chemins selon
`SfxFileReaderV3::keys_cut_at_boundary()` :
- `split_at_boundary` : le nœud final **est** la frontière ; le split
  exige que l'overlap du record soit ce que la requête dit ensuite et que
  la requête continue au-delà — les conditions sous lesquelles la marche
  sur les anciennes clés longues atteignait la clé complète ;
- `fst_candidates_v3` sonde, en plus du scan de plage, les clés préfixes
  propres de la requête (jusqu'à 4 octets plus courtes) et garde les
  parents dont l'overlap complète la requête — ce que la clé longue
  faisait trouver au scan ;
- `overlap_lookahead` n'est plus appelé (aucune clé ne dépasse le
  frontière) ; il reste pour les fichiers 3 à 6, avec l'ancien `check_split`.

Découverte en route : **les marqueurs ne produisaient jamais de split**
(`check_split` les rejetait à `prefix_len == split_byte`) ; leur seul rôle
était de rendre le nœud final et de gonfler les listes.

**Vérification.** Panel 10 000 fichiers : les 9 requêtes vérifiées
identiques (comptes et spans), en nouveau layout et en réouverture de
`idx-v6` (conteneur 6) et `idx-v8` (conteneur 5).

**Taille** (encodeur final) :

| index | conteneur 6 | conteneur 7 | `.sfx` |
|---|---|---|---|
| 10 000 fichiers | 756 Mo | **615 Mo** (−18,7 %) | 407 → 266 Mo (−34,7 %) |
| 30 000 fichiers | 2 374 Mo | **1 963 Mo** (−17,3 %) | 1 208 → 798 Mo (−34,0 %) |

Depuis le matin (v3, 1 152 Mo sur la référence) : **−46,6 %**.

**Temps** (30 000 fichiers, même binaire, 3 passes, min, ms) :

| requête | conteneur 6 | conteneur 7 | ratio |
|---|---|---|---|
| mutex_lock strict | 2,1 | 1,7 | 0,81 |
| mutex_lock relax | 1,4 | 1,6 | 1,14 |
| spin_lock strict | 1,7 | 1,7 | 1,00 |
| sched term | 3,2 | 3,6 | 1,12 |
| sched strict | 2,1 | 1,9 | 0,90 |
| printk sw | 2,1 | 2,4 | 1,14 |
| schdule fz1 | 12,6 | 10,8 | 0,86 |
| regsiter fz2 | 131,9 | 117,8 | 0,89 |
| spin_lock_[a-z]+ rx | 10,1 | 9,3 | 0,92 |
| schdule jw1 | 15,9 | 14,1 | 0,89 |

Les trois requêtes exactes courtes paient 0,2 à 0,4 ms (les sondes sur
les clés préfixes, dont les listes d'un et deux octets) ; le fuzzy et la
regex gagnent 8 à 14 %. Rien n'approche ×1,5.

### 2.1 Jaro-Winkler : 243 → 245 documents, et pourquoi ce n'est pas 2b

La requête `schdule` jw1 (distance 1, similarité ≥ 0,9) n'a pas de
référence dans le panel. Avec `V3_DUMP_DOCS` (ajouté ce soir au harnais :
une ligne JSON par requête, documents et spans) : **884 spans dans les deux
layouts**, 11 spans propres à chacun, 243 documents contre 245. Les
documents propres à chaque côté sont de vrais résultats (fenêtre `schedul`,
similarité 0,933, vérifiée hors moteur par `jw_check.py`) : *les deux
layouts manquent des fenêtres valides, différentes*. Le chemin Jaro-Winkler
tire ses candidats des n-grammes les plus rares (`keep_rarest`,
`composite.rs:568`), choisis d'après le nombre de candidats par n-gramme —
un compte que le layout change (plus de doublons de marqueurs, sondes). Ce
n'est pas une régression de 2b, c'est **un défaut de rappel préexistant du
chemin Jaro-Winkler**, invisible parce que non vérifié — le même genre que
le fuzzy de 3.0.2-3.0.6. À faire : une vérité terrain Jaro-Winkler dans le
panel, puis revoir la génération de candidats de ce chemin. Le fuzzy
Levenshtein (fz1, fz2) est exact dans les deux layouts.

---

## 3. Étape 2c — version 8 : le parent ne répète plus ce que la clé dit

**Changement.** Dans un record, un parent portait `own_len` (varint) et
`sep_len` (octet). Or la clé est `extended[sti..own_len]` depuis 2b : sa
longueur **est** `own_len − sti` (pour un mot, `content_len − sti`). Le
record v8 ne stocke donc plus `own_len` — le décodeur le dérive de la clé,
qu'il reçoit désormais (`decode_parents(value, key)`) — sauf quand la mise
en minuscules a changé une longueur d'octets (bit « own_len explicite » +
varint) ; et `sep_len` tient dans trois bits des flags (7 = varint). Un
parent plat = Δordinal + sti + flags + overlap : 3 à 7 octets contre 5 à 9.

Le conteneur 7 (une heure d'existence, jamais publié) est refusé avec un
message clair (`IntermediateVersion`) plutôt que lu de travers ; 3 à 6
restent lus.

Au passage, un `debug_assert` de 2b a fait tomber `test_fuzzy_ground_truth`
en debug : `to_lowercase` peut changer une longueur d'octets (`İ` → `i̇`),
donc couper le texte minuscule à `own_len` mettait des octets propres dans
l'overlap. Les octets propres et l'overlap sont maintenant passés en
minuscules chacun de leur côté ; `own_len` et `sti` restent ceux que le
collecteur a mesurés sur l'original (test
`lowercase_that_changes_length_keeps_key_and_overlap_apart`). En release
(sans `debug_assert`) le défaut était silencieux et antérieur : la clé
d'un tel token était fausse d'un ou deux octets depuis toujours.

**Vérification.** Panel 10 000 fichiers : les 9 requêtes vérifiées
identiques (comptes et spans), en conteneur 8 et en réouverture des
conteneurs 6 et 5 ; jw1 à 245 comme en 2b (§2.1). Suite `cargo test --lib`
verte (1 446).

**Taille** :

| index | conteneur 6 | conteneur 8 | `.sfx` | depuis v3 (matin) |
|---|---|---|---|---|
| 10 000 fichiers | 756 Mo | **559 Mo** (−26,0 %) | 407 → 210 Mo (−48 %) | 1 152 → 559 Mo : **−51,5 %** |
| 30 000 fichiers | 2 374 Mo | **1 806 Mo** (−23,9 %) | 1 208 → 640 Mo (−47 %) | 3,4 → 1,8 Go |

Le `.sfx`, 49 % de l'index ce matin, en fait 38 % ce soir.

**Temps** (30 000 fichiers, même binaire, 3 passes, min, ms) :

| requête | conteneur 6 | conteneur 8 | ratio |
|---|---|---|---|
| mutex_lock strict | 2,2 | 1,9 | 0,86 |
| mutex_lock relax | 1,5 | 1,6 | 1,07 |
| spin_lock strict | 1,8 | 1,7 | 0,94 |
| sched term | 3,3 | 3,6 | 1,09 |
| sched strict | 2,2 | 2,2 | 1,00 |
| printk sw | 2,3 | 2,5 | 1,09 |
| schdule fz1 | 13,5 | 11,4 | 0,84 |
| regsiter fz2 | 130,8 | 123,0 | 0,94 |
| spin_lock_[a-z]+ rx | 11,6 | 9,9 | 0,85 |
| schdule jw1 | 15,4 | 15,7 | 1,02 |

Les index de référence du scratchpad `idx-v7` et `idx30k-v7` contiennent
des fichiers **conteneur 8** (le script de protocole a gardé le nom).
