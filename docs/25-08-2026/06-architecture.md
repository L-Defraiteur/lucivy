# Architecture de lucivy — état au 25 août 2026

Document autonome. Ce que je sais du système, y compris ce que la journée a
corrigé ou infirmé.

---

## 1. Les crates

| crate | rôle |
|---|---|
| `ld-lucivy` (racine) | moteur : index, requêtes, scoring, fusion, segments, moteur SFX |
| `lucivy_core` | handle unifié (`ShardedHandle`), construction de requêtes, tokenizers, snapshot/delta, répertoires |
| `luciole` | framework acteurs/DAG, WASM-safe, crate séparée |
| `lucistore` | persistance partagée : BlobStore, ShardStorage, snapshot LUCE, delta, sync |
| `sparse_vector` | index sparse (postings + WAND), crate amie, code original |

**Bindings** (5) : CXX rag3db (`lucivy_fts/rust/src/bridge.rs`), WASM
emscripten (`bindings/emscripten/`), Node napi, Python PyO3, C++ standalone.

---

## 2. Le moteur SFX v3 : comment une recherche de sous-chaîne fonctionne

C'est le cœur, et c'est ce qui distingue lucivy.

### Le principe

Un **Suffix FST** indexe non seulement les tokens mais leurs **suffixes**, ce
qui permet de trouver `malloc` dans `kmalloc` sans balayage. Le FST est
partitionné :

- **SI = 0** — début de token, pour `anchor_start` / `startsWith`
- **SI > 0** — suffixes, pour `contains` n'importe où
- **partition 0x02** — entrées « mot dépouillé » (word-stripped), qui
  agrègent un mot entier par-delà ses morceaux

### Le franchissement de frontière de token

Un token long est découpé en **chunks** avec **recouvrement**. Pour matcher à
travers deux chunks ou deux mots, la marche utilise `falling_walk` plus la
**table de fratrie** (`.sibling_v3`) : « ce token est suivi de celui-là, avec
N octets entre ». `contiguous_siblings` (gap = 0) est le chemin chaud.

### Fuzzy et regex

- **Fuzzy** : pigeonhole par trigrammes, via `RegexContinuationQuery`. Les
  scores sont étagés par nombre de « miss » (`miss_penalty * 1000 + bm25`) —
  les scores négatifs sont voulus.
- **Regex** : extraction des littéraux, marche sur les candidats, validation
  de la regex ensuite.

### Les fichiers d'un segment, par champ

| fichier | contenu | poids typique | touché par une requête |
|---|---|---|---|
| `.sfx` | le Suffix FST | 44 % | 18-20 % |
| `.sfxpost` | postings au niveau chunk (SFP3) | 9 % | 23 % |
| `.bytemap` | bitmap 256 bits des octets présents par ordinal | 12 % | — |
| `.word_sfxpost` | postings au niveau mot (WSP3) | 9 % | 72 % |
| `.termtexts` | textes des termes | 6 % | 1,5 % |
| `.sibling_v3` | table de fratrie (SIB2) | 5 % | 56 % |
| `.posmap` | position → ordinal, dense | 5 % | 47 % |
| `.word_pos_map` | inverse de `word_sfxpost` | 5 % | 37 % |

Les pourcentages « touché » viennent de `test_touched_bytes` (mmap +
`mincore`) sur une requête `kmalloc`.

**v2 seulement** : `.gapmap`, `.sepmap`, `.sibling`. **v3 seulement** :
`.word_sfxpost`, `.word_pos_map`, `.sibling_v3`. Le registre le déclare
(`SfxIndexFile::written_for(version)`), et `SegmentMeta::list_files_for`
l'honore — sinon un segment v3 nomme neuf fichiers qui n'existent pas.

### Les encodages (depuis le 25 août)

Trois fichiers stockaient des `u32` fixes pour des valeurs qui ne font que
croître. Ils sont maintenant en **delta + varint**, décodés pendant la marche
qui décodait déjà champ par champ — donc **pas de passe de décompression**.

- **WSP3** (`.word_sfxpost`) : `d_doc`, `d_first`, `last-first`, `d_from`,
  `to-from`. Points de reprise tous les **32** documents (16 octets) parce que
  `entry_at` est une recherche binaire.
- **SIB2** (`.sibling_v3`) : un varint `(écart << 1) | (gap ≠ 0)`, puis `gap`
  seulement s'il est non nul. **Pas de points de reprise** : tous les lecteurs
  parcourent l'ordinal du début à la fin.
- **SFP3** (`.sfxpost`) : `num_docs`, **`headers_len`**, points de reprise,
  en-têtes par document en varints, payloads delta-encodés dans le document.
  Points de reprise tous les **8** documents (12 octets) — `find_doc` est une
  recherche binaire et `entry_at` est appelé une fois par match émis.
  `headers_len` a manqué au premier SFP3 (après-midi du 25 août) : sans lui
  le lecteur décodait tous les en-têtes à chaque lookup, O(n). Les fichiers
  de cette version-là ne se lisent plus ; aucun n'a été publié.

**La règle apprise** : avant de convertir un format, vérifier **qui le lit et
s'il saute dedans**. Séquentiel → gratuit. Accès aléatoire → il faut des
points de reprise, et leur pas se règle sur la fréquence des accès. Et la
seconde, apprise le soir : **une régression de performance mesurée n'est
« inhérente » qu'une fois la complexité de chaque accès vérifiée** — 12 % à
chaud était un O(n), pas un prix.

**Le merge v2 valide ce qu'il écrit** (`validate_sfxpost`) : depuis que le
writer émet `SFP3` pour les deux pipelines, cette validation doit accepter
les deux magics. Un test merge maintenant un index v2 et un index v3.

**Compatibilité** : chaque lecteur accepte l'ancien et le nouveau. Le
discriminant est le magic (`WSP2`/`WSP3`, `SFP2`/`SFP3`) sauf pour
`.sibling_v3` qui n'en avait pas — un sentinelle `0xFFFFFFFF` puis `"SIB2"`,
impossible dans un fichier v1 dont le premier mot est `num_ordinals`.

---

## 3. Sharding et recherche distribuée

`ShardedHandle` gère N shards. Routage configurable :
`balance_weight = 1.0` (round-robin, indexation rapide) ou `0.2` (token-aware,
co-localise les documents similaires).

BM25 correct entre shards : `ExportableStats` sérialisable, `merge()`,
`search_with_global_stats()` — la même abstraction sert au local multi-shard
et au distribué.

### Le DAG de recherche

```
drain → flush → [prescan par segment ...] → merge_prescan → build_weight
                                                                 ↓
                                       [search_shard ...] → merge → output
```

Le point important : le **prescan crée un nœud par segment**, pas par shard.
Sur 50 segments, 50 nœuds. C'est là qu'est le parallélisme des requêtes ; les
acteurs de shard (un par shard) n'interviennent qu'à la phase de recherche,
qui est devenue négligeable (2-18 ms).

### Recherche par lots (quand l'index ne tient pas)

Deux passes, comme le distribué :
1. **prescan par lot** de shards, avec libération des fichiers entre les lots,
   puis fusion des statistiques ;
2. **un seul poids compilé**, puis **recherche par lot**.

Les scores sont identiques quel que soit le découpage : les statistiques BM25
viennent de tous les shards et l'ordre de `ScoredEntry` est déterministe
(inversé sur le score, puis (shard, segment, doc) croissant).

**Piège rencontré** : `ScoredEntry::cmp` est déjà inversé (min-tas), donc un
`sort_by(|a,b| b.cmp(a))` par-dessus rendait les **pires** résultats.
`into_sorted_vec()` est la bonne fusion.

---

## 4. Mémoire : le sujet central en WASM

WebAssembly adresse **4 Go** (mesuré : 4 068 Mo). Tout le reste en découle.

### La décision de résidence

`ShardedHandle::residency()` mesure l'index et tranche :

- **`InMemory`** — un seul lot, cache relevé à la taille de l'index, rien
  d'évincé. C'est ce que fait tout natif.
- **`Streaming`** — les shards passent par lots, chacun lu puis libéré.
  Correct, mais borné par les lectures.

Seuil : `LUCIVY_RAM_INDEX_MAX`, **3 Go** en wasm32 (2 Go jusqu'au soir du
25 août : l'index 10 k fait 2 600 Mo et une page qui ne fait que servir l'a
tenu sur trois passages ; une page qui vient d'indexer ne sert de toute
façon pas, voir §5), illimité ailleurs.

**Un compte incomplet est un plancher, pas une mesure** : si un fichier ne
peut être ouvert, on ne conclut pas que l'index tient. Se tromper dans ce sens
épuise l'espace d'adressage ; dans l'autre, c'est une recherche lente qui se
corrige au coup suivant.

**Piège 32 bits** : `usize` fait 32 bits en wasm32. La somme des tailles doit
être un `u64`, sinon un index de plus de 4 Go paraît petit — précisément le
cas où « ça tient » est fatal. En release ça déborde en silence.

### Les répertoires

| type | usage | I/O |
|---|---|---|
| `StdFsDirectory` | natif + WASM/OPFS | lecture paresseuse, cache de fichiers entiers borné |
| `RamDirectory` | tests | pure RAM |
| `BlobDirectory` | ACID (mmap + blob DB) | extensible |
| `SnapshotDirectory` | **nouveau** — servir un LUCE | tranches du blob, lecture seule |

Le cache (`LUCIVY_FILE_CACHE_BYTES`) est un LRU de fichiers entiers. Les
petites lectures (≤ 64 Ko) passent directement, et les 4 premiers Ko d'un
fichier sont gardés sur son handle — les en-têtes sont relus sans cesse.

**Lazy ou pas ?** La granularité décide :
- **natif** : la page de 4 Ko, mmap ne charge que le touché — `kmalloc` faute
  853 Mo sur 3 392. Le lazy y est 4× meilleur.
- **navigateur** : le fichier entier, et une requête `contains` en ouvre ~930,
  soit tous les sidecars de tous les segments. Le lazy n'économise rien, il
  déplace le coût dans la première requête. **Le chargement anticipé
  (`preload()`) gagne 1,57×.**

### L'indexation

- **Tas de l'écrivain** : 200 Mo natif, 15 Mo WASM. C'est un **total** réparti
  entre les threads, avec un plancher de 15 Mo par part.
- **Budget SFX** (`LUCIVY_SFX_HEAP`, 128 Mo wasm) : ce que les collecteurs
  peuvent tenir avant de couper un segment. **Global, divisé par le nombre de
  threads** — sinon le pic réel se multiplie par le nombre de threads.
- **Fusions** : `LUCIVY_MERGE_CONCURRENCY`, 1 en wasm.
- **Taille des fusions** : `LUCIVY_MAX_MERGED_DOCS`, 10 000 natif, **800 en
  wasm** — borne ce qu'une fusion produit *et* ce qu'elle reprend. À 800 et
  des segments de ~200 docs, la politique ne trouve jamais 8 segments à
  fusionner : le navigateur garde ~48 segments par 10 000 docs, et c'est
  voulu — le wall d'une requête est le temps de son plus gros segment
  (1 nœud de prescan par segment), et 48 petits segments remplissent 8
  threads là où 19 gros en occupaient un (172 → 124-133 ms/requête). Une
  fusion de ~10 000 docs meurt de toute façon sur 603 Mo.
- **Builds en vol** : `LUCIVY_MAX_PENDING_FINALIZE`, **2 en wasm**, illimité
  natif — permis coopératifs (`merge_permits::acquire_build`), comme les
  fusions. Quatre builds simultanés meurent sous mimalloc (rétention des
  pages libérées par thread) là où dlmalloc les tenait.
- **Documents en file** : `LUCIVY_MAX_INFLIGHT_DOCS`, 512 en wasm — l'API
  bloque le thread appelant ; jamais un acteur.
- **Index calme** : `ShardedHandle::wait_merges_quiet()` — un commit
  n'implique pas « rien ne fusionne » (la politique replanifie après). Appelé
  par `preload()` et `drainMerges` avant de réclamer l'espace d'adressage.
- **Règle luciole apprise à la dure** : un handler d'acteur ne bloque
  jamais (`wait` dedans → panique `[luciole] FATAL: cooperative wait inside
  actor handler`). Une attente va soit dans une *tâche* (attente coopérative,
  qui fait tourner d'autres travaux), soit sur le thread de l'appelant.

**Ce qui borne réellement un segment**, c'est le budget SFX, parce que le pic
du constructeur de FST est proportionnel aux tokens du segment. Le tas de
l'écrivain ne l'a jamais borné — ça ne se voyait pas tant que les
positions/offsets remplissaient le budget les premières.

---

## 5. LUCE : l'index comme un seul fichier

Format séquentiel (`lucistore/src/snapshot.rs`) : magic `LUCE`, version,
drapeau shardé, fichiers racine, puis par index son chemin et ses fichiers
(`nom`, `longueur`, contenu).

Deux façons de le lire :

- **`import_from_snapshot`** — extrait chaque fichier sur disque. Le blob et
  les fichiers coexistent : **4,6 Go pour ouvrir un index de 2,3 Go**.
- **`open_snapshot`** — `read_manifest` donne la table des matières sans les
  octets, `SnapshotDirectory` rend des `FileSlice` qui pointent **dans** le
  blob. `OwnedBytes` étant un `Arc`, N shards partagent une copie. Le verrou
  d'écriture est accordé sans fichier : les octets sont immuables.

L'export ne prend que les fichiers **vivants** (ceux que les segments
consultables nomment, plus `meta.json` et `.managed.json`) — sinon il embarque
tous les segments qu'une fusion a remplacés : 28 % mesurés.

### L'architecture cible en trois phases

| phase | où | mémoire | état |
|---|---|---|---|
| **indexer** | OPFS, segments bornés | bornée | ✅ |
| **empaqueter** | un LUCE sur OPFS | doit streamer | ⚠️ à faire |
| **servir** | lire le LUCE, servir des tranches | = taille du blob | ✅ natif seulement |

C'est **obligatoire**, pas préférable : une session qui vient d'indexer
10 000 documents ne peut pas les servir (mesuré, §3 du récap de progression).

---

## 6. luciole — acteurs et DAG

- **Acteur** : trait avec priorités (Idle → Critical), `GenericActor` avec
  handlers typés.
- **Scheduler** : pool de threads persistants, compatible WASM. Se dimensionne
  par `available_parallelism()` — en WASM, `min(cœurs, 8)` posé par le
  binding (mesuré le soir : plateau à 8 avec mimalloc), affiché au démarrage.
- **DAG** : construction et exécution topologique, undo, checkpoint.
- **StreamDag** : pipeline en flux avec drain topologique.
- **`pipe_to` / `collect_replies_to` / `task_pipe_to`** : requête-réponse non
  bloquante.
- **WaitGraph** : suivi des dépendances, dump mermaid/texte. **C'est l'outil
  qui a identifié les blocages** (`indexer_flush_finalize` bloqué 1 415 s).
- **`BranchNode` est une FONCTION, pas une struct** : `BranchNode(|| cond)`.

**Règles WASM** : jamais de `thread::spawn` — tout passe par le scheduler ;
pas d'I/O dans un handler d'acteur ; callbacks de watch en ligne.

---

## 7. Ce que la journée a infirmé

À garder, parce que ces croyances étaient écrites dans les commentaires :

- « 1 thread d'écriture en WASM **pour ne pas épuiser le pool de pthreads** » —
  le chiffre est bon, la raison est fausse. C'est qu'ajouter des threads
  dégrade (0,87×).
- « 4 threads de planificateur » n'était pas un choix : c'était une ligne en
  dur avant la lecture des drapeaux. Le mesurer donne raison au 4.
- Le pool de pthreads (8) n'a jamais été le plafond du parallélisme observé.
- **Le parallélisme n'est pas le levier en WASM.** Sur quatre essais, seul
  celui qui *réduit le travail mémoire* a payé. — **Réinfirmé le soir** : le
  levier était l'allocateur. `dlmalloc` sérialise les threads sur un verrou
  global ; avec `mimalloc` (défaut du build désormais) la même page passe de
  551 à 188 ms/requête, et 8 threads gagnent encore 8 %. Le ratio au natif
  est passé de « 2× à 20× selon la requête » à un **2-3× plat**.
- **« Le preload bat un cache chaud à cause de la disposition mémoire »** :
  cohérent avec un allocateur qui souffre de la fragmentation — le vrai
  coupable est le même.
