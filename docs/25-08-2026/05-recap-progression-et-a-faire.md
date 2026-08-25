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
| index, 10 000 docs | 2 305 Mo (compacté) | 2 879 Mo, 48 segments (pas de fusion de fond) |
| indexation | 25,7 s | **55 s** |
| requête, **moyenne** | 79 ms | **124-133 ms** (3 passages) |
| requête, **médiane** | 49 ms | **69-92 ms** |
| `contains strict kmalloc` | 35 ms | 45 ms |
| `contains relaxed kmalloc` | 44 ms | 42 ms |
| `fuzzy d1 kmallc` | 69 ms | 117 ms |
| `fuzzy d2 kmalloc` | 436 ms | 651 ms |
| `parse booléen` | 24-32 ms | 26-45 ms |

**Comptes identiques au natif sur les 21 requêtes du panel**, tous modes
confondus (contains strict/relax, split, startsWith, term, phrase, fuzzy d1/d2,
regex, parse simple et booléen, filtre, no-hit).

Le ratio navigateur/natif est **de 1,0× à 2,2× selon la requête** (1,6× en
moyenne). Deux valeurs hors ratio : `path contains ethernet/intel` (2 hits)
97 ms contre 1, et `no hit` 14 ms contre 0 — un **coût fixe par requête**
de l'ordre de 15-100 ms, sans doute proportionnel aux 48 segments × champs
ouverts. C'est le prochain point à décomposer (§5.1).

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

### Puis la taille des segments — le chemin critique d'une requête

À 8 threads et 19 segments, le profil montrait `wall ≈ plus gros segment`
(40,7 ms de wall pour 39,7 ms sur le segment de 2 000 docs) : un thread
marchait, sept attendaient. Fusions plafonnées à **800** docs en wasm — ce
qui, avec des segments de ~200 docs, ne laisse rien à fusionner à la
politique (groupes de 3 < minimum de 8) : **48 segments** au lieu de 19, le
wall devient CPU/8, et le panel passe de 172 / 97 à **124-133 / 69-92**.
Sous dlmalloc, le même index à 48 segments faisait 603 / 238 : sans
l'allocateur, la taille des segments ne changeait rien.

### Et l'indexation : 55 secondes

Le verrou de dlmalloc sérialisait aussi les quatre indexeurs. Mais mimalloc
garde les pages libérées dans le tas du thread qui les a libérées : quatre
builds de FST simultanés (un par shard au commit) sont morts trois fois sur
des allocations de 170 Mo là où dlmalloc les tenait. **Deux builds en vol**
(`LUCIVY_MAX_PENDING_FINALIZE`, permis coopératifs sur le modèle des
fusions) + **512 documents maximum en file** entre l'API et les indexeurs :
10 000 fichiers en **55 s** (dlmalloc : 5,5 min à 4 threads, 7 min 20 à 8).

Historique de la journée sur la même mesure : 893 (matin) → 567 (preload) →
551 (SFP3 corrigé, fusions plafonnées) → 172 (mimalloc + 8 threads) →
**124-133** (48 segments). Indexation : ~25 min → 6 min → **55 s**.

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

### 5.0 ✅ Indexation navigateur rejouée (soir), plusieurs fois

Fait : index 10 k reconstruit dans le navigateur, sans paramètre d'URL, en
55 s, panel rejoué trois fois (§1). Cinq échecs en route, tous en mémoire,
tous compris : deux fusions de fond (plafond 800 + `wait_merges_quiet`),
une attente bloquante dans un handler (interdite par luciole), puis deux
fois quatre builds simultanés sous mimalloc (permis à 2 + 512 docs en
file). **Ne pas compacter** en navigateur : les petits segments sont ce qui
fait la vitesse des requêtes (§1), et une fusion de 10 000 docs ne tient
pas.

### 5.1 ✅ L'allocateur (soir) — et ce qui reste : un 2× plat

Fait, voir §1 : mimalloc puis segments plus petits, 551 → 124-133 ms. Ce
qui reste : un ratio de 1,0 à 2,2× par requête, et surtout un **coût fixe par
requête** (97 ms pour 2 hits sur `path`, 14 ms pour zéro hit) qui n'existe
pas en natif. Les prochains essais, dans l'ordre, chacun avec le panel :

- **le coût fixe, cerné mais pas résolu** (25 au soir, 20 min dessus) :
  `no hit` = 7 ms moteur + 7 ms d'aller-retour worker/JSON — rien à faire.
  `path contains ethernet/intel` = 61 ms moteur dont **328 ms de CPU en
  « sidecar loads » et 61 en resolver** sur 48 segments, soit ~1 ms par
  `read_bytes()` de données **déjà en cache** (0 `[fs] load` après le
  preload) ; en natif la même requête fait 0,5 ms avec sidecars 0. Seul le
  champ `path` est touché (`content` : 5-7 ms au total). Deux hypothèses à
  départager avec un compteur dans `LazyFsHandle::read_bytes` (chemin pris,
  temps sous le mutex) : contention futex sur `FileCache::global().lock()`
  quand 48 tâches lisent en même temps sans marche FST pour les étaler ;
  ou des sous-tranches de fichier composite < 64 Ko qui prennent le chemin
  `read_direct` sans passer par le cache. Compte pour ~50 ms sur les
  requêtes de champ court, rien sur `content` ;
- `-C target-feature=+simd128` côté Rust et `-msimd128` côté emcc ;
- `-O3` à remesurer **avec mimalloc** (jugé dans le bruit avec dlmalloc, qui
  écrasait tout) ;
- threads d'écriture 1 → 2 à remesurer avec mimalloc et 2 builds ;
- `LUCIVY_MAX_INFLIGHT_DOCS` / builds : 2 est ce qui a passé, pas un
  optimum mesuré — 3 vaut un essai si l'indexation devient le sujet.

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

### 5.5 bis ✅ Fuzzy : Jaro-Winkler en option (fait le 25 au soir)

`src/suffix_fst/briques/jaro_winkler.rs` + branchement dans
`verify_candidates` (`composite.rs`). Dans le JSON :
`{"type":"fuzzy","value":"kmalloc","fuzzy_metric":"jaro_winkler","min_similarity":0.9}`.

- Les candidats restent ceux du pigeonhole à distance `d` (2 par défaut
  pour cette métrique) ; JW décide parmi eux — il ne peut que resserrer,
  coût borné.
- JW n'est pas semi-global : le needle glisse sur la fenêtre reconstruite
  en sous-chaînes de longueur `n ± d` caractères, la meilleure gagne
  (`best_window`), alignée sur les caractères UTF-8 ; le span est le sien.
- Score : `-(1 − similarité) × 10` dans l'étage du scorer (`× 1000 + bm25`),
  donc un typo en fin de mot passe devant un typo en début — ce que
  Levenshtein ne distingue pas. Test : `test_fuzzy_jaro_winkler`.
- Métrique inconnue → erreur explicite, pas d'ignorance silencieuse.

### 5.5 ter ✅ Playground prêt pour la vitrine (fait le 25 au soir)

Design retenu (Lucie) : **pas de dataset embarqué**. La page d'accueil clone
`github.com/L-Defraiteur/lucivy@main` par le proxy CORS et l'indexe dans la
page — 983 fichiers en 3 s, 213 Mo — puis le garde en OPFS (`/lucivy_source`,
ouverture instantanée aux visites suivantes, ↻ pour recloner). Le mode
« importer un dépôt » propose `postgres/postgres` comme exemple : 4 373
fichiers en 13 s, 972 Mo, servi dans la même session. `dataset.luce` (67 Mo,
v2) est sorti du dépôt ; le LUCE reste l'export/import de *son* index.

L'UI expose maintenant tout ce que le moteur sait faire : substring,
multi-mots, préfixe, mot exact, phrase, fuzzy (Levenshtein **ou
Jaro-Winkler** avec seuil), regex, syntaxe booléenne, **séparateurs relaxed /
strict** avec explication, filtre d'extension, surlignage. Une ligne d'aide
décrit le mode choisi.

Garde-fou : au-delà de 2 Go d'index, la page qui a indexé se recharge sur
`?open=` pour servir (une session qui indexe ne sert pas au-delà, mesuré).

Attention : tant que `main` n'est pas poussé sur GitHub, la démo indexe
l'ancien code — les exemples du placeholder (`wait_merges_quiet`,
`mimaloc`) n'y sont pas encore.

### 5.5 quater ✅ Bindings natifs revus, stockage blob ACID exposé (soir)

Relecture de l'API des trois bindings natifs contre le cœur : ils n'avaient
reçu que `query_warnings` depuis la 2.0.x. Ajoutés partout : `compact`,
`wait_merges_quiet`, `index_bytes`, `drop_index`, `open_snapshot(_from)`.
Puis le **blob store** : l'utilisateur fournit l'objet (`load` / `save` /
`delete` / `exists` / `list`, plus `blob_len` / `load_range` pour le lazy)
et lucivy tourne dessus — Python (`create_with_blob_store`, GIL relâché sur
tout appel, ce qu'aucune méthode ne faisait), Node (`BlobIndex` async),
C++ (`BlobBackend` abstrait, backend mémoire d'exemple, esquisse Postgres).
Tests : Python 108 / 4 skip, Node 55 + 40 vérifications, C++ 16.

Trois défauts du cœur trouvés par ce travail, corrigés : un snapshot servi
acceptait les écritures puis échouait au commit (refus explicite maintenant,
et `close()` ne commite plus dedans) ; un interblocage en mode lazy quand le
store ne sait pas répondre `blob_len` (`MutexGuard` gardé pendant un `if
let`) ; le message d'un `save` refusé pendant une finalisation de fond
perdu derrière « background finalize failed ».

### 5.6 ✅ Publication 3.0.0 (25 août, 21h50-22h)

Dans l'ordre, chacun vérifié sur le registre : `main` poussé (`069055b`),
crates.io `luciole` → `lucistore` → `ld-lucivy` → `lucivy-core` →
`sparse-vector`, PyPI (wheel abi3 manylinux_2_28 + sdist), npm (OTP donné
en direct). Tag `v3.0.0` sur `main`.

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
  (2 en wasm, illimité natif) — des **permis coopératifs** pris par la tâche
  de build, jamais une attente dans un handler (luciole panique, vérifié).
  Et `LUCIVY_MAX_INFLIGHT_DOCS` (512 wasm) : l'API attend sur le thread
  appelant quand trop de documents sont en file, sinon un commit démarre un
  build par shard quoi qu'il arrive.
- **Ne jamais remettre les fusions de fond à 2 000 en wasm** sans remesurer
  les requêtes : 19 segments = un thread qui marche, sept qui attendent.
- **Export LUCE** : `meta.json` est lu une fois et tout en dérive ; un fichier
  qui disparaît sous l'export (fusion + ramasse-miettes) relance depuis le
  nouveau `meta.json`, trois fois, puis échoue — il n'est plus ignoré.
- **`available_parallelism()` n'est pas consulté en WASM** : une ligne posait
  `LUCIVY_SCHEDULER_THREADS = "4"` en dur avant la lecture des drapeaux. C'est
  maintenant un défaut mesuré et affiché, mais toujours un défaut fixe.
