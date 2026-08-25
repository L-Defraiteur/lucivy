# Récap de progression et ce qu'il faut faire ensuite

25 août 2026, fin de journée. Branche `wip/publication-3.0.0`, forkée de
`v3-recovery` à `e8b5414` pour ne pas déranger la session rag3weaver qui est
sur ce HEAD. **On ne merge que des points complets.**

Document autonome : il doit suffire pour reprendre sans relire l'historique.

---

## 1. Où en est le produit

**Le navigateur indexe et sert 10 000 documents.** C'était l'objectif ; il est
atteint.

| | natif | navigateur |
|---|---|---|
| index compacté, 10 000 docs | 2 273 Mo | 2 600 Mo (non compacté) |
| indexation | 27,6 s | ~25 min |
| requête, **moyenne** | 85 ms | **567 ms** |
| requête, **médiane** | 49 ms | **281 ms** |
| `contains kmalloc` | 37 ms | ~290 ms |

**Comptes identiques au natif sur les 21 requêtes du panel**, tous modes
confondus (contains strict/relax, split, startsWith, term, phrase, fuzzy d1/d2,
regex, parse simple et booléen, filtre, no-hit).

Le ratio navigateur/natif est de **6,7× sur la moyenne, 5,6× en médiane**.
C'est ce que le playground annonce maintenant, avec un encadré et un
pictogramme d'avertissement.

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

---

## 5. À faire dans l'immédiat

Par ordre de valeur, avec ce que chacun demande.

### 5.1 Profiler le module WASM ⭐ le vrai prochain pas

Le facteur ~6× est mesuré **de bout en bout et jamais décomposé**. On a
éliminé aujourd'hui le parallélisme, le niveau d'optimisation et le
chargement. Ce qui reste est dans le code.

**Ne plus tenter de drapeau à l'aveugle.** Profileur de Chrome sur le module
(les symboles s'obtiennent avec `LUCIVY_WASM_DEBUG=1 bash
bindings/emscripten/build.sh`), ou instrumentation du chemin chaud
(`contains_v3` dans `src/query/contains_query_v3.rs`, les briques dans
`src/suffix_fst/briques/`).

Piste ouverte par le résultat du preload : si la disposition mémoire compte
autant, regarder l'allocateur (`dlmalloc` par défaut en emscripten ;
`-sMALLOC=mimalloc` existe) avant le code métier.

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

### 5.5 `.bytemap` — 396 Mo, 12 % de l'index

Pas du varint : c'est un bitmap 256 bits par ordinal, très creux (5-15 octets
distincts sur 256) et redondant entre ordinaux. Sparsité + déduplication, ou
suppression pure puisqu'il est marqué **dérivable** depuis `.termtexts`.
Gagnerait ~194 Ko/doc, soit ~1 940 Mo pour 10 000 documents.

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
- **SFP3 est un échange RAM contre CPU** : −247 Mo, mais ~12 % de temps de
  requête natif à chaud en plus. Favorable au navigateur, défavorable au natif
  à chaud. Réversible par `git revert` si le natif devient prioritaire.
- **`available_parallelism()` n'est pas consulté en WASM** : une ligne posait
  `LUCIVY_SCHEDULER_THREADS = "4"` en dur avant la lecture des drapeaux. C'est
  maintenant un défaut mesuré et affiché, mais toujours un défaut fixe.
