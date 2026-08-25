# Récap de progression et ce qu'il faut faire ensuite

25 août 2026, fin de journée. Branche `wip/publication-3.0.0`, forkée de
`v3-recovery` à `e8b5414` pour ne pas déranger la session rag3weaver qui est
sur ce HEAD. **On ne merge que des points complets.**

Document autonome : il doit suffire pour reprendre sans relire l'historique.

---

## 1. Où en est le produit

**Le navigateur indexe et sert 10 000 documents.** C'était l'objectif ; il est
atteint.

Chiffres du **soir** (après les corrections de `08-relecture-commits-journee.md`,
index reconstruit des deux côtés, navigateur **sans aucun paramètre d'URL**) :

| | natif | navigateur |
|---|---|---|
| index, 10 000 docs | 2 305 Mo (compacté) | 2 492 Mo (fusions plafonnées à 2 000 docs) |
| indexation | 25,7 s | ~6 min + fusions |
| requête, **moyenne** | 79 ms | **172 ms** |
| requête, **médiane** | 49 ms | **97 ms** |
| `contains strict kmalloc` | 35 ms | 104 ms |
| `contains relaxed kmalloc` | 44 ms | 106 ms |
| `fuzzy d1 kmallc` | 69 ms | 184 ms |
| `fuzzy d2 kmalloc` | 436 ms | 1 041 ms |
| `parse booléen` | 24-32 ms | 59-66 ms |

**Comptes identiques au natif sur les 21 requêtes du panel**, tous modes
confondus (contains strict/relax, split, startsWith, term, phrase, fuzzy d1/d2,
regex, parse simple et booléen, filtre, no-hit) — et pas même un ex æquo
différent.

Le ratio navigateur/natif est maintenant **plat, 2-3× sur toutes les
requêtes** (2,2× en moyenne, 2× en médiane).

### Comment on est passé de 551 à 172 ms — l'allocateur

Le profil de la soirée (`V3_PROFILE`) montrait le temps entièrement dans
`contains_v3`, zéro I/O, et surtout un écart **strict / relaxed de 14× sur le
même terme** (106 → 1 454 ms de CPU pour `kmalloc`) là où le natif fait
1,25×. Fuzzy 15×, parse booléen 20× — les chemins qui traversent les
frontières de tokens, ceux qui allouent. Et ajouter des threads dégradait.

Une seule cause explique les trois : **`dlmalloc`, l'allocateur par défaut
d'emscripten, prend un verrou global en pthreads**. `-sMALLOC=mimalloc`
(tas par thread), même index, même page :

| | dlmalloc | mimalloc | mimalloc, 8 threads |
|---|---|---|---|
| moyenne | 551 ms | 188 ms | **172 ms** |
| médiane | 244 ms | 107 ms | **97 ms** |
| `contains relaxed kmalloc` | 429 | 106 | |
| `fuzzy d1 kmallc` | 1 057 | 184 | |
| `parse booléen` | 498-526 | 59-66 | |

12 threads = 8 threads (172 / 99) : le plateau est à 8, c'est le nouveau
défaut (`min(cœurs, 8)`). mimalloc est le défaut du build.

Historique de la journée sur la même mesure : 893 (matin) → 567 (preload) →
551 (SFP3 corrigé, fusions plafonnées) → **172** (mimalloc + 8 threads).

**Condition de mesure** : ces chiffres navigateur ont été pris avec
`?rammax=3000`, parce que le défaut de `LUCIVY_RAM_INDEX_MAX` était **2 Go**
et l'index fait 2 600 Mo — sans le paramètre il était `Streaming`
(avertissement, preload sauté, recherches par lots). **Le défaut est passé
à 3 Go le soir** : la démo tient en RAM sans paramètre. L'argument du 2 Go
(indexer + servir dans une même page dépasse 4 Go) reste vrai, mais §3 dit
qu'une page qui vient d'indexer ne sert pas de toute façon.

## 2. Ce qui a été fait aujourd'hui

### Formats (index −22 %)

Trois sidecars réencodés en delta-varint, chacun mesuré avant d'être écrit,
chacun rétrocompatible — le lecteur accepte l'ancien **et** le nouveau, rien à
migrer :

| fichier | avant | après |
|---|---|---|
| `.word_sfxpost` (WSP3) | 738 Mo | 292 Mo |
| `.sibling_v3` (SIB2) | 251 Mo | 160 Mo |
| `.sfxpost` (SFP3) | 585 Mo | 311 Mo |
| **total 15 440 docs** | **4 339 Mo** | **3 392 Mo** |

Soit **220 Ko par document**.

**Corrigé le soir** (`08-relecture-commits-journee.md`) : le premier SFP3
n'écrivait pas la longueur des en-têtes, et chaque lookup les décodait tous
— O(n) par accès, ce qui était la vraie cause des « 12 % inhérents ». Le
bloc SFP3 porte maintenant `headers_len`. **Les index écrits en SFP3 dans la
journée ne se lisent plus** : la référence native est reconstruite
(`/tmp/lucivy_parity_native`, 2 305 Mo compacté), l'index OPFS du
navigateur reste à refaire. Remesuré en natif, même protocole, 21 comptes
identiques : **93 → 79 ms/requête, médiane 59 → 49, total −14 %**.
Même commit : `validate_sfxpost` acceptait `SFP2` seulement, donc **tout
merge d'un index v2 échouait** depuis 14h50 — test de merge v2 ajouté.

### Deux défauts, dont un corrompait les index

- **Rien ne bornait un segment v3.** `mem_usage()` ne comptait pas les
  collecteurs SFX, qui sont pourtant ce que le constructeur de FST consomme.
  Ça tenait par accident tant que les positions/offsets remplissaient le budget
  les premières. Budget SFX dédié désormais (`LUCIVY_SFX_HEAP`).
- **`pending_finalize` ne gardait qu'une tâche.** Une deuxième finalisation
  écrasait le récepteur de la première, donc un commit n'attendait que le
  dernier segment : **1 551 documents sur 2 000**. Silencieux, et antérieur à
  la journée. C'est une file maintenant.

Plus trois petits : débordement `usize` 32 bits sur la somme des tailles,
fichiers fantômes dans `list_files()` (composants v2 nommés sur des segments
v3), et 28 % de poids mort dans les snapshots (segments périmés).

### Architecture : servir un LUCE sans l'extraire

`read_manifest` + `SnapshotDirectory` + `ShardedHandle::open_snapshot` :
le blob **est** l'index, les fichiers sont des tranches dedans. Vérifié sur
3 000 fichiers kernel, 9 requêtes sur 9 identiques. **Natif uniquement.**

### Chargement anticipé (le seul levier de vitesse qui a payé)

`ShardedHandle::preload()` : **893 → 567 ms par requête (1,57×)**, stable sur
trois passages. Bat un cache déjà chaud, ce que les octets seuls n'expliquent
pas — hypothèse non vérifiée : la disposition mémoire (2,6 Go alloués d'un
trait contre des allocations entrelacées avec le travail de requête).

## 3. Le fait structurel à ne pas oublier

**Une session qui vient d'indexer ne peut pas servir.** Mesuré : après avoir
indexé 10 000 documents, le premier `search` échoue sur une allocation de
10 Mo — 2 727 Mo d'index plus ce que l'indexation laisse dépassent les 4 Go
adressables. La même page rechargée ouvre le même index et répond.
Reconfirmé le soir, index reconstruit : le preload passe (2 185 Mo), la
première recherche échoue sur 4 Mo ; la page rechargée avec `?open=`
sert à 551 ms.

**Et une fusion peut tourner encore après « indexation terminée ».** Un
commit attend les fusions qu'il trouve en cours, puis la politique en
planifie d'autres sur ce qu'il vient de publier. Le soir, le preload a
chargé 2,4 Go pendant qu'une fusion construisait son FST : elle est morte
sur un realloc de 2 Mo. `wait_merges_quiet()` est maintenant appelé par
`preload()` et par `drainMerges`. Et les fusions sont **plafonnées à
2 000 documents en wasm** (`LUCIVY_MAX_MERGED_DOCS`) : la fusion de niveau 2
(~10 000 docs) meurt sur 603 Mo, et n'a jamais abouti dans un navigateur.

Ce n'est pas une préférence de conception. **Indexer et servir doivent être
deux espaces d'adressage.**

## 4. Le parallélisme ne paie pas en WASM

Quatre essais, tous mesurés :

| levier | résultat |
|---|---|
| pool de pthreads 8 → 16 | 0,99× — la concurrence restait 4 |
| threads du planificateur 4 → 8 → 12 | 1,00× → 0,93× |
| threads d'écriture 1 → 2 (navigateur) | 0,87× (le natif gagnait 1,6×) |
| `-O3` au lieu de `-O2` | 0,97×, dans le bruit |
| **chargement anticipé** | **1,57×** |

Deux chemins qui n'ont rien en commun — requêtes et indexation — dégradent
tous deux quand on ajoute des threads. **Ce moteur n'attend pas du CPU
disponible en WASM.** Les défauts (4 threads de planificateur, 1 thread
d'écriture) étaient déjà les bons réglages, pour d'autres raisons que celles
écrites dans les commentaires.

**Infirmé le soir** (§1) : ce que ces requêtes attendaient, c'était le verrou
global de `dlmalloc`. Avec mimalloc, 4 → 8 threads gagne 8 % et 8 → 12 ne
gagne plus rien. La conclusion « le parallélisme ne paie pas » était vraie
*à cause de l'allocateur*, pas du moteur. Les threads d'écriture n'ont pas
été remesurés avec mimalloc.

---

## 5. À faire dans l'immédiat

Par ordre de valeur, avec ce que chacun demande.

### 5.0 ✅ Indexation navigateur rejouée (soir)

Fait : index 10 k reconstruit dans le navigateur avec les formats du soir,
fusions de fond plafonnées, panel rejoué (§1). Deux échecs en route, tous
deux dans une fusion de fond, tous deux corrigés (§3). **Non fait** : un
`?compact=N` explicite après chargement — les fusions plafonnées à 2 000
donnent déjà ~35 segments ; compacter plus haut est précisément ce qui ne
tient pas.

### 5.1 ✅ L'allocateur (soir) — et ce qui reste : un 2× plat

Fait, voir §1 : mimalloc, 551 → 172 ms. Le facteur restant est **plat, 2-3×
sur toutes les requêtes**, ce qui est la signature d'un coût uniforme (code
généré, bounds checks, pas de SIMD) et non d'un chemin pathologique. Les
prochains essais, dans l'ordre, chacun avec le panel :

- `-C target-feature=+simd128` côté Rust et `-msimd128` côté emcc ;
- `-O3` à remesurer **avec mimalloc** (il avait été jugé dans le bruit avec
  dlmalloc, qui écrasait tout) ;
- threads d'écriture 1 → 2 à remesurer avec mimalloc (même raison) ;
- profil Chrome de `contains strict kmalloc` (104 ms contre 35) sur le build
  `LUCIVY_WASM_DEBUG=1` si les deux premiers ne rendent rien.

### 5.2 Exporteur LUCE en flux ⭐ maillon manquant de l'architecture

`export_to_snapshot` construit tout le blob en RAM
(`Vec<(String, Vec<u8>)>` puis concaténation). Empaqueter un index de 2,3 Go
dans le navigateur doublerait donc la mémoire — exactement ce qu'on évite
en phase 3. Il faut écrire dans un fichier OPFS au fil de l'eau.

Le format est séquentiel (`lucistore/src/snapshot.rs`), donc c'est mécanique :
en-tête, puis pour chaque fichier `nom`, `longueur`, contenu.

### 5.3 Exécuter le chemin « servir un LUCE » en WASM

Tout est écrit et testé **en natif seulement** : `SnapshotDirectory`,
`open_snapshot`, `read_manifest`. Rien n'a jamais tourné en WASM. À faire
avant de bâtir dessus.

### 5.4 Brancher la modale sur la taille du LUCE

`memory_warnings()` et `residency()` existent et sont exposés
(`lucivy_memory_status`). Il manque le moment : décider **avant** de charger,
depuis la taille du fichier LUCE, plutôt qu'après avoir ouvert l'index.

Décision prise le soir : **défaut à 3 Go**. Il ne vaut que pour une page
qui ne fait que servir — ce que la phase 3 garantit, et ce que la modale
doit dire quand un index entre 3 et 4 Go se présente.

### 5.4 bis Reportés, pas abandonnés

Les idées du matin (`02-design-bytes-pagine.md` §7) — **shards fins + file
d'admission** et **bloom de trigrammes par shard** — ne sont ni faites ni
contredites : le choix « tout en RAM quand ça tient » les rend inutiles pour
la démo et les garde pertinentes pour un index qui ne tient pas.

### 5.5 `.bytemap` — 396 Mo, 12 % de l'index

Pas du varint : c'est un bitmap 256 bits par ordinal, très creux (5-15 octets
distincts sur 256) et redondant entre ordinaux. Sparsité + déduplication, ou
suppression pure puisqu'il est marqué **dérivable** depuis `.termtexts`.
Gagnerait ~194 Ko/doc, soit ~1 940 Mo pour 10 000 documents.

### 5.5 bis Fuzzy : Jaro-Winkler en option (noté le 25 au soir)

Le chemin fuzzy v3 (`src/suffix_fst/briques/composite.rs`) fait déjà trois
étapes : pigeonhole par trigrammes (chaînes gardant ≥ n − d trigrammes),
**validation Levenshtein** sur la fenêtre source reconstruite
(`within_edit_distance`, DP semi-globale, deux lignes réutilisées), puis
`fuzzy_spans` pour le span exact. Jaro-Winkler se branche à l'étape 2 :

- `enum FuzzyMetric { Levenshtein, JaroWinkler { threshold } }` dans
  `QueryConfig`, propagé jusqu'à la boucle de validation ; JW sans
  allocation, ~40 lignes ;
- JW n'est pas semi-global : comparer le needle au **token porteur** de la
  chaîne (`back[]` donne la correspondance fenêtre → tokens), pas à la
  fenêtre entière ;
- le rappel reste celui du pigeonhole à distance `d` — JW ne peut que
  resserrer parmi ces candidats, ce qui est voulu (coût borné) ;
- score : la similarité JW elle-même plutôt que l'étage par trigrammes
  manqués ; span : celui du token comparé.

### 5.6 Publication crates.io 3.0.0

`cargo publish --dry-run` vert sur les 5 crates. **En attente du feu vert de
Lucie**, jamais publié sans.

### 5.7 Pagination fine du `.sfx`

Design écrit : `02-design-bytes-pagine.md`. 1 508 Mo dont 18 % touchés par une
requête. C'est le gros morceau, mais §4 dit que le sujet actuel est le CPU —
donc **après** le profil.

---

## 6. Points de vigilance

- **La régression d'indexation navigateur est passée inaperçue quinze heures**
  parce qu'aucune indexation navigateur n'a été relancée entre 00:05 et
  14:30. Tout ce qui a été mesuré entre-temps tournait sur un index déjà
  construit. **Relancer une indexation navigateur après tout changement qui
  touche l'écriture.** Voir `03-regression-indexation-navigateur.md`.
- **Ne pas augmenter les threads sans revoir les budgets mémoire.** Le tas de
  l'écrivain est un *total* réparti entre les threads, avec un plancher par
  part ; le budget SFX est global et divisé. Les deux sont liés au nombre de
  threads.
- **SFP3 n'est plus un échange RAM contre CPU connu** : les ~12 % mesurés à
  chaud venaient d'un scan O(n) des en-têtes à chaque lookup, corrigé le soir
  (`headers_len`). Le coût résiduel, s'il existe, est **à remesurer** sur le
  panel natif avant d'écrire quoi que ce soit dessus.
- **Contre-pression sur la finalisation** : `LUCIVY_MAX_PENDING_FINALIZE`
  (1 en wasm, 4 natif) borne le nombre de segments en construction. Avant, la
  file était sans limite et l'indexation 10 k tenait par le timing, pas par
  construction.
- **Export LUCE** : `meta.json` est lu une fois et tout en dérive ; un fichier
  qui disparaît sous l'export (fusion + ramasse-miettes) relance depuis le
  nouveau `meta.json`, trois fois, puis échoue — il n'est plus ignoré.
- **`available_parallelism()` n'est pas consulté en WASM** : une ligne posait
  `LUCIVY_SCHEDULER_THREADS = "4"` en dur avant la lecture des drapeaux. C'est
  maintenant un défaut mesuré et affiché, mais toujours un défaut fixe.
