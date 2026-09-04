# Journal des étapes de réduction — mesures

Une section par étape du plan ([01](01-recap-findings-et-plan-d-action.md),
§3), écrite après la mesure, jamais avant. Protocole : §4 du même document.

**Référence** : 10 000 fichiers de `/tmp/lucivy-cmp` (65 Mo sur disque),
harnais `v3_ground_truth_demo` avec `V3_INDEX_DIR`, 160 segments de 64
documents, machine au repos (charge 0,8). Tailles par
`benches/scan_index_size.py`.

```bash
V3_CORPUS=/tmp/lucivy-cmp V3_INDEX_DIR=/chemin/idx cargo test --release -p lucivy-core \
    --test test_sfx_v3_ground_truth v3_ground_truth_demo -- --ignored --nocapture
python3 benches/scan_index_size.py /chemin/idx <uuid du plus gros segment>
```

Index de référence v3 : **1 152,4 Mo** scannés (`.store` non compté),
dont `.sfx` 635,9 Mo = FST 256,9 + parents 368,9.

---

## Étape 1 — table de parents compacte (4 septembre)

**Changement.** Un record multi-parents était `[u32 count]` + 11 octets par
parent. Il est maintenant `[varint count]` + le même `u64` packé que la
valeur inline de la FST (`encode_single_parent_v3`), 8 octets. Un seul
décodeur pour les deux formes. Octet de version du conteneur `.sfx` : 3 → 4,
magic inchangé (`SFX3`), le lecteur accepte les deux
(`SfxFileReaderV3::container_version()`). Le builder refuse désormais un
ordinal au-delà de 24 bits **dans tous les cas**, pas seulement pour un
parent unique — avant, un ordinal de record était écrit sur 32 bits.

Fichiers : `src/suffix_fst/builder_v3.rs`, `src/suffix_fst/file_v3.rs`,
`benches/scan_index_size.py`.

**Taille.**

| fichier | avant | après | delta |
|---|---|---|---|
| `.sfx` — parents | 368,9 Mo | 261,9 Mo | **−29,0 %** |
| `.sfx` — FST | 256,9 Mo | 256,9 Mo | 0 |
| `.sfx` total | 635,9 Mo | 525,8 Mo | −17,3 % |
| tous les autres fichiers | identiques | | 0 |
| **index** | **1 152,4 Mo** | **1 042,2 Mo** | **−9,6 %** |

L'audit prévoyait −8,7 % sur l'index de 93 605 fichiers ; ici −9,6 %, la
part des records étant un peu plus forte sur des petits segments.

**Justesse.** Le panel rend les **mêmes comptes et les mêmes spans** sur les
neuf requêtes vérifiées (16, 24, 48, 94, 286, 74, 228, 3 630, 23 documents ;
spans « exact » partout). Et l'index de référence v3, non reconstruit,
**rouvre avec le nouveau binaire** et passe le même panel : la compatibilité
de lecture est prouvée, pas supposée.

**Temps.** Dans le bruit, à la demi-milliseconde près, sur des requêtes de
2 à 48 ms :

| requête | avant | après |
|---|---|---|
| `regsiter` fz2 (la plus lourde) | 48,3 ms | 45,1 ms |
| `schdule` fz1 | 6,3 ms | 6,4 ms |
| `spin_lock_[a-z]+` rx | 4,8 ms | 4,3 ms |
| `mutex_lock` strict | 2,6 ms | 2,4 ms |

Une seule passe chacune : ce panel de 10 000 fichiers ne discrimine pas des
écarts de cette taille. La mesure qui compte se fera sur l'index de 93 605
fichiers à la fin des étapes 1 à 4.

**Tests.** `cargo test --lib` : 1 438 verts, 0 rouge (3 tests ajoutés :
record ancien contre record packé sur 300 parents, valeurs maximales avec
70 000 parents, fichier de version 3 relu par le lecteur).
`cargo test -p lucivy-core` : 184 verts, 0 rouge, 31 ignorés (les bancs).

---

## Étape 2 — plus de `.bytemap` en v3 (4 septembre)

**Changement.** Sur tout le chemin v3, le bitmap de 256 bits par ordinal ne
répondait qu'à une question : « ce chunk contient-il un octet de contenu ? »
(quatre sites dans `resolve.rs` et `orchestrator.rs`, tous sur les mêmes
`CONTENT_RANGES`). La section META de `.termtexts` y répond par
`own_len > sep_len` : `TermTextsReaderV3::has_content`, trois octets lus à
`6 × ordinal`, contre 32 dans un autre fichier. Les octets de contenu sont
exactement ceux qu'accepte `is_content_char` du tokenizer, donc les deux
réponses coïncident — et c'est **prouvé par un test**
(`bytemap_and_meta_agree_on_content`) qui compare le bitmap et META sur chaque
ordinal d'un corpus à séparateurs de tête, UTF-8, emoji, chunks purement
séparateurs.

`ByteMapIndex::written_for(v)` vaut `v < 3` : le registre ne l'écrit plus pour
v3, `components_for` ne le liste plus, `BriquesContext` n'a plus de champ
`bytemap`, `has_word_pipeline` exige `.termtexts` à la place. Le chemin v2
(`literal_resolve`, `regex_continuation_query`) garde le sien.

Au passage, la construction de `.termtexts` depuis le collecteur, dupliquée
dans le nœud d'assemblage et trois helpers de test, est une seule fonction
(`TermTextsWriterV3::from_collector_v3`).

Fichiers : `termtexts_v3.rs`, `bytemap.rs`, `index_registry.rs`,
`briques/{context,resolve,composite,orchestrator,dag_nodes}.rs`,
`query/{contains,fuzzy,regex}_query_v3.rs`, `indexer/sfx_dag_v3.rs`, tests.

**Taille.**

| fichier | v3 | étape 1 | étape 2 |
|---|---|---|---|
| `.bytemap` | 166,9 Mo | 166,9 Mo | **0** |
| tous les autres | | identiques | identiques |
| **index** | **1 152,4 Mo** | 1 042,2 Mo | **875,3 Mo** |

**−16,0 %** sur l'étape 1, **−24,0 %** cumulé depuis v3 (l'audit prévoyait
−11 % puis −9 %, soit −19 % ; les petits segments de la référence pèsent plus
en dictionnaire).

**Justesse.** Mêmes comptes et mêmes spans sur les neuf requêtes vérifiées.
L'index de référence v3, qui porte encore ses 320 fichiers `.bytemap`, rouvre
avec le nouveau binaire et passe le panel : ils sont simplement ignorés.

**Temps.** Dans le bruit : `regsiter` fz2 45,8 ms (45,1 à l'étape 1, 48,3 en
v3), `schdule` fz1 5,5 ms (6,4 / 6,3), `spin_lock_[a-z]+` 4,1 ms (4,3 / 4,8).

**Tests.** `cargo test --lib` : 1 439 verts (1 ajouté, l'équivalence).
`cargo test -p lucivy-core` : 184 verts, 0 rouge, 31 ignorés.

---

## Étapes 3 et 4 — `.posmap` sur 3 octets, `.sibling_v3` sans `gap_len` (4 septembre)

**Étape 3.** Un slot de `.posmap` était un `u32` par position pour des ordinaux
que le builder borne à 24 bits (`ORDINAL_MASK`). Layout `PMP3` : 3 octets par
slot, marqueur vide `0xFFFFFF`, choisi par le writer quand tout ordinal tient
(toujours en v3 ; un segment v2 retombe sur `PMAP`). Le lecteur lit les deux,
et 0xFFFFFF reste un ordinal valide : le writer bascule sur 4 octets s'il le
rencontre.

**Étape 4.** Le `gap_len` de `SIB2` ne contenait plus un écart depuis le v3 :
le collecteur y mettait la **longueur de contenu de la destination**, lue par
un seul consommateur, la DFS de fratrie (`sibling_chain_dfs`), qui lisait déjà
`own_len` dans META pour le mode strict. Elle lit maintenant META dans les
deux modes (`own_len − sep_len` en relâché), et retombe sur `gap_len` pour un
fichier sans META. Layout `SIB3` : un varint de delta par lien, rien d'autre,
émis par le writer dès qu'aucun lien ne porte de gap (le v2 garde `SIB2` tout
seul). Le collecteur ne calcule ni ne garde plus cette longueur (2 octets par
paire de RAM de moins), la fusion la jette.

Fichiers : `posmap.rs`, `sibling_table.rs`, `collector_v3.rs`,
`indexer/sfx_dag_v3.rs`, `briques/fst_walk.rs`, `benches/scan_index_size.py`
(qui s'arrête maintenant à la fin réelle des données : les fichiers portent un
pied, d'où un « max_ordinal » absurde dans l'audit).

**Taille.**

| fichier | étape 2 | étapes 3+4 | delta |
|---|---|---|---|
| `.posmap` | 32,5 Mo | 24,4 Mo | −25 % |
| `.sibling_v3` | 47,0 Mo | 37,5 Mo | −20 % |
| **index** | 875,3 Mo | **857,7 Mo** | −2,0 % |

**−25,6 %** cumulé depuis v3 (1 152,4 → 857,7 Mo).

**Justesse.** Mêmes comptes et mêmes spans sur les neuf requêtes vérifiées.
L'index v3 de référence rouvre et passe le panel.

**Temps — un vrai A/B.** Le même binaire lit les deux layouts, donc l'index de
l'étape 2 (`PMAP`/`SIB2`) et celui-ci (`PMP3`/`SIB3`) ont été interrogés en
alternance, deux passes chacun, dans les mêmes conditions (charge 5, la queue
de la suite de tests ; le premier essai, pris pendant la suite à charge 14,
a été jeté). Meilleur des deux :

| requête | étape 2 | étapes 3+4 |
|---|---|---|
| `regsiter` fz2 | 53,8 ms | **49,4 ms** |
| `schdule` fz1 | 6,3 ms | **5,6 ms** |
| `spin_lock_[a-z]+` rx | 4,2 ms | **3,9 ms** |
| `schdule` jw1 | 8,4 ms | 8,0 ms |
| `sched` strict | 2,5 ms | 2,8 ms |
| les cinq autres | 2,1 à 3,1 ms | identiques à 0,1 ms près |

Moins d'octets à faulter, pas de lecture non alignée qui coûte. Le seul recul
(0,3 ms sur `sched`) est sous le bruit d'une passe à l'autre.

**Tests.** `cargo test --lib` : 1 441 verts (2 ajoutés : les deux largeurs de
`.posmap` lues pareil, `SIB3` contre `SIB2`). `cargo test -p lucivy-core` :
184 verts, 0 rouge.

---

## Checkpoint après les étapes 1 à 4 — le vrai A/B, sur 30 000 fichiers

Le panel de 10 000 fichiers ne discrimine pas la milliseconde, et l'A/B des
étapes 3 et 4 ne mesurait que le format (même binaire des deux côtés). Ici :
**le binaire de chaque commit sur l'index qu'il écrit**, ce qu'un utilisateur
ressent. Corpus `/tmp/lucivy-cmp-90k` plafonné à 30 000 fichiers,
`V3_COMMIT_EVERY=2000`, 120 segments ; l'ancien binaire compilé depuis
`1c263f3` dans un worktree ; trois passes alternées par commit, machine sans
autre charge que les panels (charge 3 à 6, c'est le prescan lui-même).

```bash
git worktree add /chemin/wt-v3 1c263f3
cd /chemin/wt-v3 && CARGO_TARGET_DIR=/chemin/wt-target V3_CORPUS=/tmp/lucivy-cmp-90k \
  V3_MAX_DOCS=30000 V3_COMMIT_EVERY=2000 V3_INDEX_DIR=/chemin/idx30k-v3 \
  cargo test --release -p lucivy-core --test test_sfx_v3_ground_truth v3_ground_truth_demo -- --ignored --nocapture
```

**Taille** : 3,4 Go (v3) → 3,1 (ét. 1) → 2,6 (ét. 2) → 2,6 Go (ét. 4), `du`.

**Temps, minimum de trois passes (ms)** :

| requête | docs | v3 | ét. 1 | ét. 2 | ét. 3 | ét. 4 |
|---|---|---|---|---|---|---|
| `mutex_lock` strict | 233 | 2,9 | 2,6 | 2,5 | 2,5 | 3,4 |
| `mutex_lock` relax | 246 | 1,7 | 1,7 | 1,7 | 1,6 | 1,8 |
| `spin_lock` strict | 588 | 2,4 | 2,1 | 2,3 | 2,2 | 2,6 |
| `sched` term | 1 406 | 3,4 | 3,1 | 3,2 | 3,7 | 3,9 |
| `sched` strict | 1 998 | 2,1 | 2,3 | 2,3 | 2,5 | 2,2 |
| `printk` sw | 1 252 | 2,5 | 2,5 | 2,4 | 2,4 | 2,6 |
| `schdule` fz1 | 707 | **12,2** | 12,8 | 13,9 | 14,4 | **15,2** |
| `regsiter` fz2 | 8 623 | 142,0 | 143,5 | 141,6 | 143,3 | 143,8 |
| `spin_lock_[a-z]+` rx | 439 | 10,1 | 10,8 | 10,5 | 11,2 | 10,4 |
| `schdule` jw1 | 773 | 16,5 | 17,3 | 15,8 | 17,2 | 16,0 |

**Lecture.** Les requêtes exactes bougent de quelques dixièmes dans les deux
sens : bruit. Le fuzzy lourd et la regex ne bougent pas. **Une seule ligne
recule vraiment : le fuzzy relâché, +3 ms sur 12 (+25 %)**, et pas d'un coup
— par paliers de 0,5 à 1,1 ms à chaque étape, les deux plus gros aux étapes 2
(`has_content` lit META au lieu du bitmap) et 4 (la DFS lit META au lieu de
`gap_len`). Le point commun : deux lectures aléatoires de `.termtexts` par
pas là où il y en avait une, parce que la table d'offsets (pour `text()`) et
la section META sont dans deux régions du fichier. C'est ce que l'étape 6 peut
rendre : mettre la méta **dans** la table d'offsets, 8 octets par ordinal,
et `meta()` devient gratuit après `text()`.

**Décision (règle du 4 septembre, [01](01-recap-findings-et-plan-d-action.md)
§4)** : rien n'approche ×1,5, l'index a perdu 24 % ; on garde les quatre
étapes, on écrit la perte, et l'étape 6 est la prochaine parce qu'elle réduit
et accélère à la fois.

---

## Étape 6 — la méta dans la table d'offsets de `.termtexts` (4 septembre)

Faite avant l'étape 5 à cause du checkpoint : les 3 ms perdus par le fuzzy
relâché venaient de deux lectures aléatoires de `.termtexts` par pas là où il
y en avait une, la table d'offsets (pour `text()`) et la section META (pour
`meta()` et `has_content()`) étant dans deux régions du fichier.

**Changement.** Layout 2 du `.termtexts` (octet de version 2, magic `TTX3`
inchangé) : une seule section ENTRIES, `num + 1` entrées de **8 octets** —
`[u32 offset][u16 own_len][u8 sep_len][u8 flags]`, les flags portant
`overlap_len` (4 bits), `is_word_start`, `is_word_stripped` — puis les
textes. `meta()` et `has_content()` lisent la même ligne de cache que
`text()`. 8 octets par ordinal au lieu de 4 + 6. Le lecteur ouvre encore le
layout 1 (TEXTS + META), prouvé par `layout_1_and_layout_2_read_alike`.

**Taille (30 000 fichiers).** `.termtexts` 261,1 → 231,1 Mo (−11,5 %), la
méta 89,8 → 59,9 Mo ; index 2 637,7 → 2 607,7 Mo (−1,1 %).

**Temps — A/B à trois binaires**, chacun sur l'index qu'il écrit, trois
passes alternées, minimum (ms) :

| requête | v3 | étape 4 | **étape 6** |
|---|---|---|---|
| `mutex_lock` strict | 3,3 | 3,3 | **2,5** |
| `spin_lock` strict | 2,7 | 2,5 | **2,1** |
| `sched` term | 3,4 | 4,3 | 3,9 |
| `printk` sw | 2,6 | 2,7 | **2,3** |
| `schdule` fz1 | 14,2 | 14,4 | **12,0** |
| `regsiter` fz2 | 132,3 | 132,0 | 146,0 |
| `spin_lock_[a-z]+` rx | 11,3 | 10,5 | **9,9** |
| `schdule` jw1 | 15,8 | 16,0 | 15,5 |

La ligne `regsiter` fz2 a été reprise à part, cinq passes alternées
étape 4 / étape 6 : **150,5 / 158,9** (min / médiane) contre
**146,2 / 147,0**. C'était du bruit de session ; l'étape 6 est plus rapide
et plus stable là aussi. Sur `schdule` fz1, cinq passes : 12,4 / 13,4 contre
12,6 / 13,4 — égal.

**Bilan du checkpoint** : les 3 ms du fuzzy relâché sont rendus, et les
requêtes exactes sont **plus rapides qu'en v3** (2,5 ms contre 3,3 sur
`mutex_lock` strict). L'index de 30 000 fichiers fait 2,6 Go contre 3,4 :
−24 %.

**Tests.** `cargo test --lib` : 1 442 verts. `cargo test -p lucivy-core` :
un rouge, `luce_v3_sharded_roundtrip`, **puis quatre fois vert relancé
seul**. Le rouge est tombé pendant que trois compilations chargeaient la
machine (charge 17), sur une assertion d'ordre du top-10 entre dix documents
**à score strictement égal** — l'ordre entre ex æquo dépend de l'ordre de
réponse des shards. Ce n'est pas cette étape, c'est une non-déterminisme
préexistant, à traiter à part : un tri stable par id entre ex æquo.

---

## Étape 5, première moitié — plus de suffixe qui commence dans l'overlap (4 septembre)

**Changement.** `add_token` n'enregistre plus les suffixes dont l'index de
départ est dans l'overlap (`sti ≥ own_len`). Pour `mutex_` + `lo`, les clés
`lo` et `o` pointant vers `mutex_` disparaissent : ce sont un ou deux octets
du token **suivant**, qui les porte lui-même sous son propre ordinal, à la
même position du texte, à sti 0 et 1. La marche les rejetait déjà
(`check_split`) et le scan de plage les résolvait en doublons des spans du
token suivant. Et c'étaient les clés d'un et deux octets, celles aux listes
de parents géantes. Le builder seul change ; aucun format, aucun lecteur.

Test : `no_suffix_starts_in_the_overlap`. Deux tests qui affirmaient
l'existence de la clé d'overlap (`test_builder_v3_basic`,
`test_builder_v3_multi_parent_overlap`) affirment maintenant son absence.

Au passage, un plantage préexistant du mode diagnostic `V3_DIAG_FUZZY` :
la fenêtre rejetée était tronquée à 80 octets au milieu d'un caractère
multi-octets (`用`), selon l'ordre des threads. Corrigé (coupe sur frontière
de caractère), prouvé avec `V3_DIAG_FUZZY_MAX=0` : 16 895 rejets affichés,
0 panique.

**Taille.**

| | étape 6 | 5a | delta |
|---|---|---|---|
| 10 000 fichiers, `.sfx` parents | 261,9 Mo | 216,4 Mo | −17 % |
| 10 000 fichiers, index | 857,7 Mo | **799,6 Mo** | **−6,8 %** |
| 30 000 fichiers, `.sfx` parents | 809,8 Mo | 675,7 Mo | −17 % |
| 30 000 fichiers, index | 2 607,7 Mo | **2 472,7 Mo** | **−5,2 %** |

La FST elle-même ne bouge pas (256,9 → 255,8 Mo) : les clés supprimées
existaient déjà sous le token suivant, seuls leurs parents en trop partent.
Cumul depuis v3 : **−30,6 %** sur 10 000 fichiers (1 152 → 800 Mo).

**Justesse.** Mêmes comptes et mêmes spans sur les neuf requêtes vérifiées,
sur les deux corpus. L'index v3 de référence rouvre.

**Temps — A/B au même binaire**, index de l'étape 6 contre celui-ci, 30 000
fichiers, cinq passes alternées, min / médiane (ms) :

| requête | étape 6 | 5a |
|---|---|---|
| `mutex_lock` strict | 1,8 / 1,9 | **1,4 / 1,5** |
| `spin_lock` strict (3 passes) | 2,0 / 2,0 | **1,7 / 1,9** |
| `sched` term (3 passes) | 3,8 / 3,9 | **3,1 / 3,1** |
| `spin_lock_[a-z]+` rx (3 passes) | 9,8 / 10,3 | **8,8 / 8,8** |
| `schdule` fz1 | 13,7 / 14,4 | 14,3 / 15,9 |
| `regsiter` fz2 | 131,3 / 140,2 | 134,3 / 147,6 |

Les requêtes exactes et la regex gagnent 10 à 20 % : plus de liste de
300 000 parents à décoder au premier octet de chaque marche. Le fuzzy
relâché perd environ **1 ms sur 14** ; le profil (`V3_PROFILE=1`) montre
le même travail sur les deux index — mêmes pièces, mêmes candidats par
segment — et « word walk » à 93 contre 96 ms cumulés sur les threads. Pas
attribué plus finement. Sous la règle du 4 septembre : gardé.

**Tests.** `cargo test --lib` : 1 443 verts. `cargo test -p lucivy-core` :
184 verts.
