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
