# Rapport du 25 août 2026

Branche `wip/publication-3.0.0`, de `fb7e2af` à `d661588`.

## 1. Où on en est

**Le navigateur indexe et sert 10 000 documents.** C'était l'objectif de la
journée et il est atteint, avec une réserve importante (§4).

| | natif | navigateur | ratio |
|---|---|---|---|
| index compacté, 10 000 docs | 2 273 Mo | 2 600 Mo (non compacté) | |
| indexation | 27,6 s | ~25 min | |
| panel 21 requêtes, **moyenne** | 85 ms | **893 ms** | 10,5× |
| panel, **médiane** | — | **614 ms** | — |
| `contains kmalloc` | 37 ms | **293 ms** | 7,8× |
| `no hit zzqqxx` | 0 ms | 5 ms | |
| `fuzzy d2 kmalloc` | 515 ms | 5 290 ms | 10,3× |

**Les 21 requêtes rendent des comptes identiques au natif.**

La moyenne est tirée par deux requêtes (`fuzzy d2` à 5,3 s, `split` à 2,9 s).
Une démonstration qui montre du `contains` tourne à **~300 ms**.

## 2. Ce que l'index pèse maintenant

Trois formats de sidecar réencodés en delta-varint, chacun mesuré sur de
vraies données avant d'être écrit, chacun rétrocompatible (le lecteur accepte
l'ancien et le nouveau, rien à migrer) :

| fichier | avant | après | facteur |
|---|---|---|---|
| `.word_sfxpost` (WSP3) | 738 Mo | 292 Mo | 2,53× |
| `.sibling_v3` (SIB2) | 251 Mo | 160 Mo | 1,57× |
| `.sfxpost` (SFP3) | 585 Mo | 311 Mo | 1,88× |
| **index total (15 440 docs)** | **4 339 Mo** | **3 392 Mo** | **−22 %** |

Soit **220 Ko par document**, contre 283 ce matin.

Le principe, et la raison pour laquelle ça ne coûte pas de décompression :
ces fichiers stockaient des `u32` fixes pour des valeurs qui ne font que
croître dans un document ou un ordinal. On écrit l'écart, en varint, et il est
décodé pendant la marche qui décodait déjà champ par champ. La compression par
blocs (zstd/lz4) a été écartée sur mesure : il faudrait décompresser plus vite
que les 1,15 Go/s d'OPFS qu'elle économise, et en natif où mmap ne charge que
les pages utiles c'est du CPU pur.

**Ce que ça coûte, dit franchement** : SFP3 rend ~12 % du temps de requête
natif à chaud (panel 2 405 → 2 684 ms) contre 247 Mo. Un accès aléatoire y
décode une série de varints là où v2 lisait trois cases. Trois tentatives de
récupération sont dans le code parce que chacune a été mesurée ; ce qui reste
est inhérent. C'est un échange RAM contre CPU, favorable au navigateur,
défavorable au natif à chaud. Réversible par un `git revert`.

## 3. Deux défauts trouvés, dont un corrompait les index

### 3.1 Rien ne bornait un segment v3

`SegmentWriter::mem_usage` — ce qui décide de vider — comptait les postings,
les fieldnorms, les fast fields et le sérialiseur, **jamais les collecteurs
SFX**, qui sont pourtant ce que le constructeur de FST consommera. Ça tenait
par accident : les positions et offsets remplissaient le budget les premiers
et coupaient tôt. Mesure sur les mêmes 2 000 fichiers kernel, même tas de
15 Mo :

| | segments | docs/segment |
|---|---|---|
| avec positions+offsets | ~56 | ~36 |
| après `ba48e60` (ce matin) | 4 | 500 |
| **avec le budget SFX** | **12** | **~166** |

Les collecteurs ont maintenant leur propre budget (`LUCIVY_SFX_HEAP`, 128 Mo
en wasm32, 1 Go ailleurs), **séparé** de celui des postings : les verser dans
le même donnait 629 segments et 117 s pour indexer 2 000 documents là où 4 s
suffisaient — erreur mesurée avant d'être corrigée.

### 3.2 La file de finalisation n'en gardait qu'une

`pending_finalize` était un emplacement unique. Une deuxième finalisation
soumise avant que la première soit consultée **écrasait son récepteur**, donc
le drain qu'attend un commit n'attendait que le dernier segment. Invisible
tant qu'un segment absorbait tout le lot d'un commit — il n'y avait jamais de
deuxième à perdre. Avec les segments recoupés par la mémoire : **1 551
documents sur 2 000**. C'est une file désormais, et le compte est juste.

Ce défaut **corrompait silencieusement l'index** dès qu'un commit produisait
plusieurs segments, c'est-à-dire dans le cas normal. Il précédait la journée.

### 3.3 Trois autres, plus petits

- **Débordement 32 bits** : sur wasm32 `usize` fait 32 bits, et la somme des
  tailles de shards (5 695 795 643 octets) rendait 1 400 828 347. Un index de
  plus de 4 Go paraissait petit — précisément le cas où « ça tient en RAM »
  fait tomber l'onglet. En release ça déborde en silence.
- **Fichiers fantômes** : `list_files()` nommait `.gapmap`, `.sepmap` et
  `.sibling` (v2) pour chaque champ de chaque segment v3 — neuf ouvertures
  vouées à l'échec par segment, à chaque passe du ramasse-miettes et à chaque
  mesure. Et ça noyait le seul signal utile : un fichier illisible parce
  qu'OPFS n'est pas prêt ne se distinguait plus d'un composant inexistant.
  Le registre déclare maintenant `written_for(version)` : 62 ouvertures sur
  62, contre 62 sur 82.
- **Le snapshot portait 28 % de poids mort** : l'export copiait le répertoire
  entier, donc tous les segments qu'une fusion avait remplacés. 75 fichiers
  sur un blob mesuré juste après indexation. L'export ne prend plus que les
  fichiers vivants : 69 % → 95 % de vivant.

## 4. La contrainte structurelle : indexer et servir ne partagent pas un espace

**Mesuré** : la session qui vient d'indexer 10 000 documents échoue au premier
`search` sur une allocation de 10 Mo — 2 727 Mo d'index plus ce que
l'indexation laisse derrière dépassent les 4 Go adressables. La même page
rechargée ouvre le même index et répond.

Ce n'est donc pas une préférence de conception : **une démonstration doit
indexer d'un côté et servir de l'autre.** D'où l'architecture en trois phases
et le travail déjà fait dessus :

| phase | où | état |
|---|---|---|
| indexer | OPFS, segments bornés | ✅ fait (§3.1) |
| empaqueter | un LUCE | ⚠️ l'export construit tout en RAM — il doit streamer |
| servir | lire le LUCE, servir des tranches | ✅ fait, natif seulement |

`SnapshotDirectory` + `ShardedHandle::open_snapshot` servent un LUCE **sans
l'extraire** : le blob est l'index, les fichiers sont des tranches. Vérifié
sur 3 000 fichiers kernel, 1 091,8 Mo, 9 requêtes sur 9 identiques, et
`index_bytes()` mesure 100 % du blob — la décision de résidence marche telle
quelle. **Jamais exécuté en WASM.**

Le piège évité : `import_from_snapshot` extrait chaque fichier, donc le blob
et les fichiers coexistent — 4,6 Go pour ouvrir un index de 2,3 Go.

## 5. Ce qui a été mesuré et qui oriente la suite

- **Le coût d'une requête est du CPU, pas de l'I/O.** Le ratio navigateur/natif
  est plat (4× à 19× selon la requête, sans lien avec le volume lu), et la
  chauffe ne vaut que 12 % (1 013 → 893 ms entre deux passages). **Tout le
  travail sur les octets fait *tenir* l'index, pas aller vite.**
- **Les octets qu'une requête touche vraiment** (mmap + `mincore`, éviction
  entre deux) : `kmalloc` 853 Mo sur 3 557 (24 %), `zzqqxxwwvv` 70 Mo (2 %).
  Ce qui donne l'ordre de grandeur d'une pagination fine : 3× sur un terme
  fréquent, ~50× sur un terme rare.
- **Débit OPFS** (23 105 chargements) : ~3 ms de fixe par ouverture, ~1,15 Go/s
  asymptotique. 72 % des chargements font moins de 4 Mo — d'où l'intérêt d'un
  fichier unique.
- **Le lazy n'économise rien pour une requête** : une seule `contains` charge
  ~930 fichiers, soit à peu près tous les sidecars de tous les segments. En
  navigateur (granularité = fichier entier), le chargement anticipé est donc
  meilleur ; en natif (granularité = page de 4 Ko), le lazy reste 4× meilleur.

## 6. Les threads : on utilise 4 cœurs sur 24

**Mesure** : `navigator.hardwareConcurrency` = **24** sur cette machine ;
concurrence observée pendant le prescan : **4**.

Deux plafonds distincts, et la réponse à « c'est une règle de compilation ? »
n'est pas la même pour les deux :

1. **Le pool de pthreads emscripten** : `-sPTHREAD_POOL_SIZE=8` dans
   `build.sh`. C'est bien un drapeau de compilation, **mais sa valeur peut
   être une expression JavaScript évaluée au démarrage** — emscripten accepte
   `-sPTHREAD_POOL_SIZE='navigator.hardwareConcurrency'`. **Donc pas besoin de
   plusieurs binaires WASM** : un seul, dimensionné à l'ouverture selon la
   machine. À noter, `PTHREAD_POOL_SIZE_STRICT=0` est déjà posé, donc les
   threads au-delà du pool sont créés à la demande — mais leur création passe
   par le thread principal, ce qui est la source classique d'interblocage si
   le thread demandeur attend.
2. **Le planificateur luciole** se dimensionne déjà tout seul :
   `available_parallelism()`, qui sur emscripten rend `hardwareConcurrency`.
   Il demande donc 24 et n'en obtient que 8, dont une partie est retenue par
   les acteurs (shards, indexeurs, lecteurs) — d'où les 4 observés.

**La réserve à ne pas oublier** : plus de threads est gratuit pour les
**requêtes**, et dangereux pour l'**indexation**. C'est le parallélisme des
fusions qui avait épuisé l'espace d'adressage la nuit dernière
(`merge_permits` = 1 en wasm), et c'est la taille des segments qui l'a fait
aujourd'hui. Augmenter le pool sans rendre les budgets mémoire conscients du
nombre de threads recasserait l'indexation. L'ordre est donc : monter le pool,
mesurer les requêtes, et **ne toucher au parallélisme d'indexation qu'après**.

## 7. Prochains leviers, par rapport gain/risque

1. **Pool de threads depuis `hardwareConcurrency`** (§6). Un drapeau, pas de
   binaire supplémentaire. Le gain porte sur les requêtes, qui sont
   CPU-limitées : 4 → 8 threads utiles devrait se voir directement.
2. **`-O3` et `-msimd128`** : le build est en `-O2` sans SIMD. Deux essais
   gratuits avant toute optimisation de fond. Le facteur 10× navigateur/natif
   est mesuré **de bout en bout et jamais décomposé** — un profil doit venir
   avant d'optimiser quoi que ce soit.
3. **Chargement anticipé** sur le chemin `InMemory` : ouvrir, mesurer, charger,
   puis déclarer prêt. Rend le pic prévisible et donne à la modale le bon
   moment pour parler.
4. **Exporteur LUCE en flux** : le seul maillon manquant de la chaîne
   indexer → empaqueter → servir.
5. **`.bytemap`** (396 Mo, 12 % de l'index) : pas du varint — un bitmap
   256 bits par ordinal, très creux (5-15 octets distincts sur 256) et
   redondant. Sparsité et déduplication, ou suppression puisqu'il est marqué
   dérivable.
6. **Pagination fine du `.sfx`** (design `02-design-bytes-pagine.md`) : le gros
   morceau, 1 508 Mo dont 18 % touchés. À faire après le profil, parce que
   §5 dit que le sujet actuel est le CPU.

## 8. Ce qui n'est pas testé

À dire clairement, parce que la journée a produit beaucoup de chiffres natifs :

- `SnapshotDirectory`, `open_snapshot`, `read_manifest`, l'export restreint aux
  fichiers vivants : **natif uniquement**.
- L'export LUCE d'un index de 2,3 Go dans le navigateur : jamais tenté, et il
  échouerait probablement (construction en RAM).
- Le compactage navigateur avec les nouveaux formats : pas rejoué depuis
  cette nuit.
- La publication crates.io 3.0.0 : `--dry-run` vert, en attente du feu vert.

## 9. Addendum : les threads, mesurés — l'hypothèse était fausse

Suite du §6, où j'annonçais qu'on utilisait 4 cœurs sur 24 et que le pool de
pthreads était le plafond. **C'était faux, et voici les trois mesures.**

Le pool a d'abord été dimensionné à l'exécution
(`-sPTHREAD_POOL_SIZE='Math.min(navigator.hardwareConcurrency || 4, 16)'`,
soit 16 ici) : **aucun gain, 0,99×**, et la concurrence observée restait
**exactement 4**. Le pool n'était donc pas le plafond.

Le vrai plafond était une ligne : `LUCIVY_SCHEDULER_THREADS = "4"` **en dur**
au début de `__main_argc_argv`, avant même la lecture des drapeaux. Le
planificateur luciole demande bien `available_parallelism()` — il ne l'a
jamais vu, la variable était déjà posée.

Rendu réglable (`--scheduler-threads=N`, `?threads=N`) et rendu visible
(`[scheduler] starting with N threads (source)`), le panel sur l'index de
10 000 documents donne :

| threads | moyenne | médiane | `fuzzy d2` | `contains kmalloc` |
|---|---|---|---|---|
| **4** (défaut) | **893 ms** | 614 ms | **5 290 ms** | 293 ms |
| 8 | 960 ms | 633 ms | 5 993 ms | 241 ms |
| 12 | 949 ms | 587 ms | 6 262 ms | 276 ms |

La concurrence suit bien (12 threads → « peak concurrency 12 »), mais **le
temps monte**. Les requêtes lourdes se dégradent régulièrement — `fuzzy d2`
perd 18 % entre 4 et 12 threads — pendant que les légères gagnent à peine.
Ces requêtes ne parallélisent donc pas : elles attendent autre chose que du
CPU disponible, et douze threads qui traversent des centaines de mégaoctets
dans un même tas WASM se gênent (bande passante mémoire, allocateur).

`-O3` a été essayé dans la foulée : **918 ms contre 893, soit 0,97×** — dans
le bruit, médiane un peu meilleure (558 contre 614), rien qui justifie le
changement. Le build reste en `-O2`.

**Ce qui est conservé** : le pool s'adapte vers le bas
(`min(hardwareConcurrency, 8)`) pour une petite machine, le nombre de threads
est réglable et affiché, et le défaut de 4 est désormais un choix mesuré et
non un chiffre en dur oublié.

**Ce que ça redirige** : le facteur 10× entre navigateur et natif ne vient ni
du parallélisme ni du niveau d'optimisation. Il faut un **profil réel** — le
profileur de Chrome sur le module, ou une instrumentation du chemin chaud —
avant d'essayer un drapeau de plus. Les deux essais d'aujourd'hui étaient
gratuits ; le suivant ne doit plus être à l'aveugle.

## 10. Chargement anticipé : le seul levier qui a payé

Le raisonnement du §5 disait que le lazy n'économise rien en navigateur —
une seule requête `contains` ouvre ~930 fichiers, soit à peu près tous les
sidecars de tous les segments. Il ne supprime donc pas le travail, il le cache
dans la première requête.

`ShardedHandle::preload()` lit tout l'index dans le cache quand la résidence
le permet, et ne fait rien quand l'index est streamé (là, tout tenir est
précisément ce qui ne rentre pas). La page l'appelle avant de se déclarer
prête et affiche ce qu'elle charge.

**Mesure sur l'index de 10 000 documents (2 600 Mo) :**

```
[preload] 837 fichiers, 2600 Mo en 2516 ms      (~1,03 Go/s, conforme au débit OPFS mesuré)
```

| panel | ms/requête | médiane |
|---|---|---|
| 1er passage, sans preload (froid) | 1 013 ms | — |
| 2e passage, sans preload (cache chaud) | 893 ms | 614 ms |
| **1er passage après preload** | **567 ms** | **281 ms** |
| 2e passage après preload | 559 ms | 302 ms |
| 3e passage après preload | 572 ms | 317 ms |

**567 ms contre 893 : 1,57× plus rapide, et stable sur trois passages.**
Comptes identiques.

Le résultat surprend : le chargement anticipé bat un cache *déjà chaud*. Les
octets sont pourtant les mêmes. L'explication la plus plausible est la
disposition mémoire — le préchargement alloue 2,6 Go d'un trait, en séquence,
tandis que le chargement paresseux entrelace ces allocations avec le travail
de requête et fragmente le tas linéaire WASM. **Non vérifiée** : c'est une
hypothèse, pas une mesure.

Ce sont les requêtes lourdes qui gagnent le plus (`split` 1,80×, `regex`
1,27×, `fuzzy d2` 1,22×) — celles-là mêmes qui se dégradaient quand on
ajoutait des threads. Les deux observations pointent dans la même direction :
ce que ces requêtes attendent, c'est la mémoire, pas le CPU.

**Bilan de la journée sur la vitesse navigateur : 893 → 567 ms/requête en
moyenne, 614 → 281 ms en médiane.** Une démonstration `contains` est
maintenant à **~250-300 ms**.
