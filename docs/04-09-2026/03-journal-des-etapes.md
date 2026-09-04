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
