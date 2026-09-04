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

---

## 4. Étape 3a — l'ordinal passe de 24 à 28 bits

Le record `.sfx` n'impose plus rien à l'ordinal depuis 2a (varint), et
`.posmap` s'élargit seul (`PMP3` → `PMAP` au-delà de 24 bits). Restait
`.word_pos_map` : slot `ordinal 24 | span 8`, et au-delà **la carte n'était
pas écrite** (silencieusement — les lecteurs retombaient sur les postings).
Nouveau `WMP3` : `ordinal 28 | span 4`, un span ≥ 15 rapporté comme
`SPAN_OVERFLOW` (le lecteur normalise, les consommateurs ne changent pas) ;
`WMP2` encore lu. `SuffixFstBuilderV3::MAX_ORDINAL` = 2²⁸ − 1, erreur dure
du builder et de `merge_segments_v3` (message en nombre d'ordinaux).

268 M d'ordinaux par segment : seize fois le noyau entier (25,6 M de
textes distincts). La fusion du noyau vers 10 segments, que le harnais
refusait ce soir sur 17,9 M de termes, devient possible — non relancée
(11 Go, longue) ; la borne effective est désormais la mémoire de la
fusion (`LUCIVY_MAX_MERGED_DOCS`, commentaire de `handle.rs` mis à jour),
pas l'encodage. Suite `cargo test --lib` verte (1 447).

---

## 5. Étape 3b — les tables d'offsets par blocs

**Mesure qui l'a décidée** : le scan de l'index de référence après 2c —
quatre tables de `u32` par ordinal (`.sfxpost`, `.word_sfxpost`,
`.sibling_v3`, `.termtexts`), 4 × 20,9 Mo = 84 Mo, **15 % de l'index**, pour
des offsets qui avancent de quelques octets à la fois.

**Changement.** Module `block_offsets` : la table est découpée en blocs de
64 offsets ; un bloc stocke une base `u32` et ses offsets comme différence
à la base, dans la largeur que le bloc demande (0 à 4 octets) ; un petit
annuaire donne la position de chaque bloc. Une lecture = deux accès (annuaire,
puis offset). `OffsetTable` lit les deux formes, si bien que chaque lecteur
garde son `read_offset(i)` et ouvre les deux layouts. Nouveaux magics
`SFP4`, `WSP4` (offsets relatifs à la région des blocs), `SIB4` ;
`.termtexts` layout 3 (section `0x05` : offsets de texte par blocs, puis une
table de méta de 4 octets par ordinal — `meta()` et `has_content()` restent
une lecture — puis les textes). Tout ce qui précède reste lu ; le
validateur de fusion v2 accepte `SFP4`.

**Vérification.** Panel identique (9/9, comptes et spans), réouverture des
conteneurs 5 et 6 identique. Suite lib verte (1 449).

**Taille** (référence 10 000 fichiers) : tables 84 → 31,8 Mo (sfxpost 8,9,
sibling 6,5, termtexts 11,1, word_sfxpost 5,4) ; index **559 → 508 Mo
(−9,2 %)** ; 30 000 fichiers 1 806 → 1 659 Mo (−8,1 %). **Depuis le matin
(v3, 1 152 Mo) : −55,9 %.**

**Temps** (30 000 fichiers, même binaire vs conteneur 6, min, ms) :
mutex_lock strict 2,1 → 1,8 ; relax 1,5 → 1,7 ; spin_lock 1,8 → 1,7 ;
sched term 3,1 → 3,1 ; printk sw 2,2 → 2,3 ; fz1 13,5 → 11,2 ; fz2
140 → 121 ; regex 10,0 → 10,4 ; jw1 16,9 → 13,3. Rien au-delà de ×1,13.

Répartition de l'index de référence après 3b (508 Mo) : `.sfx` 210 Mo
(41 % — parents 180, FST 30), `.sfxpost` 72, `.termtexts` 71, `.word_sfxpost`
47, `.word_pos_map` 32, `.posmap` 24, `.sibling_v3` 23. Le dictionnaire
(sfx + termtexts + sibling) fait 60 % : c'est ce que le partage par shard
divise par 2,6.

---

## 6. Le dictionnaire partagé par shard — plan d'implémentation (v1)

Décidé après 3b, avec la carte du code faite par les trois explorations du
soir (persistance, ordinaux/segments, `.sfx`). Le principe de [06](06-chantier-dictionnaire-partage-rapport.md)
tient ; ce qui suit est la forme minimale qui le prouve et mesure le gain.

**Les segments gardent leurs ordinaux locaux et tous leurs formats.** Ce
qui change : un segment ne porte plus `.sfx` ni `.termtexts` ; il porte un
`.gmap` — la liste **triée** des identifiants globaux de ses ordinaux
(index = ordinal local, `u32` par ordinal, global → local par recherche
binaire) — et un `.newtexts` (texte + méta des identifiants qu'il a
frappés le premier). `.sfxpost`, `.word_sfxpost`, `.posmap`,
`.word_pos_map`, `.sibling_v3` restent locaux et inchangés.

**Le dictionnaire du shard** : `dict.sfx` (FST + parents, ordinaux =
identifiants globaux) + `dict.termtexts` (global → texte + méta), enregistré
dans `meta.json` (`IndexMeta`, un champ `dictionary { generation, files }`)
pour que `list_files` (GC), le snapshot LUCE, le delta et `index_bytes` le
voient — la liste des dangers de l'exploration persistance (un fichier de
shard non enregistré est supprimé au premier GC).

**Frappe des identifiants** (collecteur, `intern_extended`) : chaque
indexeur cherche le texte dans la génération courante (clé `[0x00] +
minuscule(texte propre)`, parent à `sti == 0` et même forme, texte exact
confirmé dans `dict.termtexts` — la casse ne se voit pas dans la clé) ; s'il
ne l'y trouve pas, il frappe un identifiant sur un compteur atomique du
shard. Deux indexeurs concurrents peuvent frapper deux identifiants pour
un même texte nouveau : toléré (le FST porte les deux parents, une seule
requête les trouve tous les deux), dédoublonné pour l'avenir à la
génération suivante. Les ordinaux locaux sont attribués **dans l'ordre
des identifiants globaux**, ce qui trie `.gmap` gratuitement.

**Le commit** rebâtit la génération : fusion des textes de la génération
précédente et des `.newtexts` des nouveaux segments (identifiants stables,
append-only), FST + parents + termtexts réécrits en entier — « simple,
lent, juste » ; les générations incrémentales viennent après, une fois la
justesse prouvée par le panel. Écriture atomique puis `meta.json`.

**La requête** : une marche de FST par shard (plus une par segment) → des
candidats à identifiants globaux ; par segment, `gmap.local(global)` puis
les postings, posmap, fratrie locaux comme aujourd'hui. Les endroits qui
vont de local à global (posmap → termtexts, fratrie → texte) passent par
`gmap.global(local)`. `BriquesContext` porte le lecteur du dictionnaire et
le `.gmap` du segment.

**La fusion de segments** : union triée des `.gmap`, remappage des locaux,
concaténation des postings — plus de réinternement, plus de FST à rebâtir
(`sparse_vector::merge_segments`). Elle ne touche pas au dictionnaire.

**Ordre** : D1a l'infrastructure (lecteur du dictionnaire, champ de meta,
enregistrement des fichiers, chargement dans `Index` / `LucivyHandle` —
sans dictionnaire, rien ne change) ; D1b la frappe et la génération au
commit ; D1c la requête ; D1d la fusion ; D1e le protocole (panel, taille,
temps) ; puis les générations et la compaction.

---

## 7. Le dictionnaire partagé, v1 — ce qui est fait (nuit du 4 au 5 septembre)

Le plan du §6, tel quel, en quatre commits (`c1d5880` l'infrastructure,
`dc00639` le reste). Ce qui a changé en route :

- **`.newtexts` n'est pas un fichier du registre.** Listé, il aurait été
  emballé, transporté et gardé par le GC, alors qu'il est mort dès que le
  commit l'a replié. Il survit jusqu'au commit parce que le GC garde tout
  fichier géré qui porte l'identifiant d'un segment vivant ; le commit
  suivant le supprime.
- **Un compteur d'identifiants par champ**, pas un par shard : les textes
  du dictionnaire sont indexés par identifiant, et un identifiant frappé
  pour un autre champ y faisait un trou de cinq octets — le
  `dict.termtexts` était plus gros que la somme des `.termtexts` qu'il
  remplaçait.
- **Compteurs et textes en attente partagés entre générations**, et le
  collecteur lit la génération *courante* à chaque recherche (le slot de
  l'`Index`, pas un `Arc` pris à sa création) : un commit remplace la
  génération pendant que des segments s'écrivent, et un writer parti avant
  aurait sinon frappé sur l'ancien compteur ou re-frappé les textes que
  la nouvelle génération venait d'absorber. Deux indexeurs concurrents qui
  voient le même texte nouveau ne frappent plus deux identifiants (table
  des textes en attente sous verrou, prise seulement après un échec dans
  la FST).
- **Le double mappage** : `SfxPostReaderV2::entries` délègue à
  `entries_filtered`, qui traduit déjà ; traduire deux fois cherchait un
  ordinal local comme un identifiant global et ne trouvait rien — le
  strict rendait zéro document quand le relâché rendait les bons. Trouvé
  brique par brique (`dictionary_pieces`).

**Vérité** : `lucivy_core/tests/test_dictionary_index.rs` — 300 fichiers
du noyau, huit commits (chaque commit replie et retrouve), les fusions de la
politique, une réouverture ; onze requêtes (strict, relâché, terme,
préfixe, fuzzy 1 et 2, regex, casse mixte) : documents et spans identiques
à un index v3. Fichiers : aucun `.sfx` ni `.termtexts` par segment, une
génération par champ (`dict-8.1.*`, `dict-8.2.*`), un `.gmap` par segment.
Suite lib 1 450 verte.

**Taille sur ces 300 fichiers** (5 segments après fusion) : −1,2 % —
`dict.sfx` 11,4 Mo pour 12,5 de `.sfx` par segment, `dict.termtexts` 3,7
pour 4,3, plus 1,2 Mo de `.gmap`. Cinq segments ne se répètent guère.

**Taille sur la référence de 10 000 fichiers** (160 segments, 20
commits → génération 20, `V3_SFX_VERSION=4`, scratchpad `idx-dict`) :
**508 → 387 Mo (−23,7 %)** ; `.sfx` 210 → 99,6 Mo (FST 8,3 + parents
91,3), `.termtexts` 70,6 → 33,0 (2,39 M de textes distincts pour 5,22 M
d'ordinaux, ×2,19), `.gmap` 20,9 Mo (5,22 M × 4), `.sibling_v3` 23 → 25
(ordonné par identifiant global, deltas un peu plus grands). **Depuis le
matin : 1 152 → 387 Mo, −66,4 %.** Panel 9/9 identique (comptes et spans).

**Ce qui reste de v1** : la génération est réécrite en entier à chaque
commit (« simple, lent, juste ») ; la marche de FST reste faite par
segment (le `.sfx` du dictionnaire est servi à chaque segment, la
requête ne gagne rien encore) ; `index_bytes` / `preload` ne comptent pas
les fichiers du dictionnaire ; l'import WASM les route dans `shard_0/`
sans les connaître ; un segment abandonné entre deux commits laisse ses
identifiants sans texte (inoffensif : rien ne les cite).

---

## 8. Le dictionnaire partagé, v1 — la requête (nuit du 4 au 5)

**Le problème, mesuré.** Avec le dictionnaire tel que le §6 l'a décrit, chaque
segment marchait toute la FST du shard : 160 marches de 2,4 M de textes
au lieu de 160 marches de 15 000. Sur la référence 10 000 : `sched term`
202 ms (3), `mutex_lock relax` 37 ms (3), fz1 619 ms (5,6), fz2 788 ms (42).
Le §6 disait « une marche par shard au lieu d'une par segment » : rien dans
le code ne le faisait, les briques prenaient le lecteur du segment.

**Ce qui a été fait** (commit `2b6359d`) :

1. `FstMemo` dans `SfxFileReaderV3` : les résultats de `fst_candidates_v3`,
   `falling_walk_chunks`, `falling_walk_words` par (fonction, requête,
   drapeaux), une cellule `OnceLock` par clé — le premier segment calcule,
   les autres attendent. Le lecteur du `DictionaryField` est ouvert avec
   une mémo et `SegmentReader::sfx_dictionary_field` le tend aux trois
   chargeurs (contains, fuzzy, regex) au lieu d'un `open_owned` par segment.
2. Une **vue par segment** (`for_segment(gmap)`) : les listes mémoïsées sont
   triées par identifiant, et coupées à ce que le segment a par une
   **marche fusionnée** avec le `.gmap` (`keep_in_segment`, O(C + G)). La
   première version filtrait par recherche binaire par candidat : sur un
   fuzzy c'était ×5 de plus (1 616 ms de CPU cumulé sur 160 segments pour
   les seuls filtres).
3. Les **chaînes** se construisent par segment, à partir des splits filtrés,
   les marches par reste étant mémoïsées : une première version mémoïsait
   les chaînes du shard entier (`cross_word_chain_v3` sur 1 237 splits →
   24 ms sur un thread, tous les autres attendant) — c'est ce qui rendait
   `sched term` à 25 ms.
4. `resolve_all_trigrams` fait partir chaque segment d'un n-gramme
   différent (graine = premier identifiant du `.gmap`) — sans effet
   mesurable, gardé parce que sans coût.

**Où on en est** (référence 10 000, ms, index v3 3b entre parenthèses) :
strict 3,3 (3,2), relax 2,9 (3,1), spin_lock 3,0 (2,7), term 7,0 (3,2),
sw 8,5 (2,7), strict sched 3,8 (2,6), fz1 50,8 (5,6), fz2 139 (42), rx 5,3
(4,0), jw1 13,6 (7,7). **La même requête relancée** (mémo chaude) : fz1
12,0 puis 10,6 ; term 3,7. Donc : le coût résiduel est le calcul **froid**
au niveau du shard — la marche est faite une fois, mais sur un seul thread
(la mémo sérialise ce qu'un segment demande le premier), là où l'index v3
répartissait le même CPU sur 24 threads. `max` par segment ≈ mur.

**À faire (prochain pas de la requête)** : paralléliser le calcul froid —
un nœud « prescan du dictionnaire » par shard dans le DAG de recherche,
avec une tâche par pièce ou n-gramme (le fuzzy en demande dix à trente),
avant les nœuds par segment ; ou, plus simplement, une pré-passe dans
`fuzzy_v3` qui soumet les `fst_candidates_v3` des pièces au scheduler.
Objectif : fz1 sous 10 ms, term sous 4.

**Ce que ça corrige dans les documents antérieurs** : [06](06-chantier-dictionnaire-partage-rapport.md)
§2.1 (« une requête : prescan sur chaque génération vivante ») décrivait
l'intention ; la réalité v1 est « les segments partagent le lecteur et sa
mémo, le premier demandeur calcule ». [07](07-architecture.md) §3 (« le
prescan crée un nœud par segment ... c'est ce qu'un dictionnaire par shard
réduira ») : le nœud par segment demeure, et c'est bien.
