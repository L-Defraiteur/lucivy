# Ce qui a débloqué le navigateur — soirée du 25 août 2026

Document court, à lire avant le reste : la journée avait conclu « le WASM est
6× plus lent, le parallélisme ne paie pas, il faut profiler ». La soirée a
donné une autre réponse, en cinq pas mesurés. Même corpus (10 000 fichiers
du kernel), même panel de 21 requêtes, même machine.

| étape | requête (moy. / méd.) | indexation | ce qui a changé |
|---|---|---|---|
| après-midi | 567 / 281 ms | ~6 min | `?rammax=3000` obligatoire |
| SFP3 `headers_len` | 551 / 244 | — | un scan O(n) par lookup, pas « inhérent » |
| **mimalloc** | **188 / 107** | mort | `-sMALLOC=mimalloc`, un drapeau |
| 8 threads | 172 / 97 | mort | le verrou parti, les threads paient |
| **48 segments** | **124-133 / 69-92** | — | fusions à 800 : le chemin critique raccourcit |
| 2 builds en vol | idem | **55 s** | permis coopératifs, 512 docs en file |

Natif sur le même index : 79 / 49 ms, indexation 25,7 s.

## 1. L'allocateur — le seul vrai facteur

Le profil (`V3_PROFILE`) montrait le temps entièrement dans `contains_v3`,
zéro I/O, et un écart **strict / relaxed de 14× sur le même terme** (106 →
1 454 ms de CPU) là où le natif fait 1,25×. Fuzzy 15×, parse booléen 20×.
Les chemins qui traversent les frontières de tokens sont ceux qui allouent.

`dlmalloc`, l'allocateur par défaut d'emscripten, prend **un verrou global
en pthreads**. Quatre threads qui allouent se sérialisent dessus ; un
cinquième aggrave ; un tas fragmenté par du lazy loading coûte plus qu'un
tas rempli d'un trait. Ça expliquait les trois « faits » de l'après-midi :
le facteur 6×, le parallélisme qui dégrade, le preload qui bat un cache
chaud.

`-sMALLOC=mimalloc` : 551 → 188 ms. `relaxed kmalloc` 429 → 106, `fuzzy d1`
1 057 → 184, `parse` booléen 498 → 59. Le ratio au natif, de « 2× à 20×
selon la requête », devient plat.

**Leçon** : avant de profiler du code, vérifier ce que le runtime met sous
`malloc`. Un facteur *inégal* selon le type de requête désigne un coût
partagé (allocateur, verrou), pas le code de chaque chemin.

## 2. Les threads — vrais, une fois le verrou parti

4 → 8 threads : +8 %. 8 → 12 : rien. Défaut wasm : `min(cœurs, 8)`. La
conclusion de l'après-midi (« ce moteur n'attend pas du CPU ») était un
artefact du verrou.

## 3. Les segments — le chemin critique d'une requête

À 8 threads et 19 segments, `wall ≈ plus gros segment` (40,7 ms de wall
pour 39,7 ms sur le segment de 2 000 docs) : un thread marche, sept
attendent. Le prescan est **un nœud par segment** ; la vitesse d'une requête
est celle de son plus gros segment.

Fusions plafonnées à 800 docs en wasm → la politique de fusion ne trouve
plus rien à fusionner avec des segments de ~200 docs → 48 segments, wall =
CPU/8 : **172 → 124-133 ms**. CPU total identique (97 vs 94 ms cumulés).
Coût : +16 % de disque, 2,5× plus de fichiers à précharger.

**Leçon** : en WASM, ne pas fusionner. Les gros segments servent le natif
(mmap, page de 4 Ko) ; ici tout est en RAM et c'est le parallélisme qui
compte.

## 4. L'indexation — 55 secondes, après trois morts

Le verrou sérialisait aussi les quatre indexeurs : mimalloc a fait passer
l'indexation de 5,5 min à moins d'une minute. Mais mimalloc **garde les pages
libérées dans le tas du thread qui les a libérées** : quatre builds de FST
simultanés (un par shard, au commit) sont morts trois fois sur des
allocations de 170 Mo que dlmalloc tenait.

Deux bornes, par construction :
- **permis de build** (`LUCIVY_MAX_PENDING_FINALIZE`, 2 en wasm), pris par
  la tâche de build avec l'attente coopérative des fusions ;
- **documents en file** (`LUCIVY_MAX_INFLIGHT_DOCS`, 512 en wasm) : l'API
  attend sur le thread appelant.

Et une règle luciole vérifiée à la dure : **un handler d'acteur ne bloque
jamais**. Une attente va dans une tâche (coopérative) ou sur le thread de
l'appelant. Ma première version bloquait dans le handler : panique
immédiate, à raison.

## 5. Ce que ça change dans la façon de chercher

- Quand le ratio au natif varie de 2× à 20× **selon la requête**, le
  suspect est un coût partagé, pas le code de chaque requête.
- Quand `wall ≈ max_segment` dans le profil, ajouter des threads ne peut
  rien : réduire l'unité de travail.
- Une conclusion négative (« X ne paie pas ») ne vaut que dans la
  configuration où elle a été mesurée. Trois conclusions de l'après-midi
  sont tombées quand l'allocateur a changé.
- Le premier réflexe utile a été le profil déjà écrit (`V3_PROFILE`), pas un
  drapeau de plus.

## 6. Ce qui reste

Un **coût fixe par requête** qui n'existe pas en natif : `no hit` 14 ms
contre 0, `path contains ethernet/intel` 97 ms pour 2 hits. Voir
`05-recap-progression-et-a-faire.md` §5.1.
