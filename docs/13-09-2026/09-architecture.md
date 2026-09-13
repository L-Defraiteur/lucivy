# Architecture au 13 septembre 2026, soir (4.2.0)

*Autonome. Reprend `04-architecture.md` (matin) et y ajoute ce que la journée a
changé : les résultats, l'indexation, le dictionnaire à l'écriture. Les chiffres sont
mesurés sur le noyau épinglé (Linux v7.2, `8d3ae59288f1`, 101 141 fichiers, 899 Mo).*

## 1. Les couches

| crate | rôle |
|---|---|
| `ld-lucivy` | le moteur : segments (fork de tantivy 0.22), moteur SFX, requêtes, BM25, fusions, document store |
| `lucivy-core` | `ShardedHandle`, constructeur de requêtes JSON, tokenizers, snapshots LUCE / deltas LUCIDS, blob store |
| `luciole` | acteurs et DAG, sans `thread::spawn` (compatible WASM) |
| `lucistore` | persistance partagée, `BlobStore` |
| `sparse-vector` | index sparse (postings + WAND) |
| `lucivy-fst` | la fourche de `fst` : `MapBuilder::with_registry`, `OutputTable`, l'union en flux — **0.1.1**, publiée avant `ld-lucivy` |
| bindings | Python (PyO3), Node (napi), C++ (cxx), navigateur (emscripten), pont rag3db |

Tout le workspace est en **4.2.0** ; `lucivy-fst` garde son numéro propre.

## 2. Quatre dispositions, une seule vérité

| disposition | index | × texte | indexation 4.2 | ce qu'elle change |
|---|---|---|---|---|
| `sfx_version` 3 | 6 886 Mo | ×7,7 | 51 s | une FST de suffixes par segment |
| dictionnaire partagé (défaut) | 5 064 Mo | ×5,6 | 47 s (**35 s** sur les branches empilées de `v4.3`, `07` § 5 sexies-octies) | une FST par shard, en générations |
| + `derived_in_ram` | 3 408 Mo | ×3,8 | 46 s | les trois dérivés rebâtis à l'ouverture |
| + `positions: false` (4.1) | **2 491 Mo** | **×2,8** | 47 s | documents + fréquences, chaque match vérifié sur le texte stocké |

Les quatre passent le même panel de vérité terrain, 10/10 spans exacts. Le
dictionnaire ne coûte plus rien à l'indexation (il coûtait ×1,5 en 4.0).

## 3. Les fichiers d'un segment, par champ texte

`.sfx` / `dict-<g>.sfx` (la FST des suffixes, partitions `0x00` début de jeton,
`0x01` suffixe interne, `0x02` mot sans séparateurs ; **depuis le 13 au soir, un
`dict-<g>.<champ>.pidx` dérivé à côté de chaque génération** — un point de contrôle
par 16 groupes de parents des records de plus de 16 groupes, reconstruit en RAM
s'il manque, ignoré par un ancien lecteur, `dictionary_pidx.rs`), `.termtexts`, `.gmap`,
`.sfxpost` (`SFP5` positions / `SFP6` documents + fréquences), `.word_sfxpost`,
les dérivés `.posmap`, `.word_pos_map`, `.sibling_v3`, et le `store` (document
store, LZ4 par blocs de 16 384 octets ; un document plus grand que le bloc **est**
son bloc). Rien n'a changé sur disque en 4.2 : un index 4.1 s'ouvre, un index 4.2
s'ouvre avec 4.1.

## 4. Le chemin d'une requête

1. Phase FST sans position : candidats d'un seul jeton, marche descendante, chaînes à
   travers les frontières, plan mémoïsé par shard.
2. Avec positions : postings → `.posmap` → fenêtres → `place_spans` → `verify_literal`.
3. Sans positions (`briques::stored`) : candidats = documents, chacun relu dans le
   store et vérifié avec les prédicats mêmes de la vérité terrain ; **parallèle par
   segment** via le scatter DAG luciole (confirmé : +11 ms sur `mutex_lock`, ce que
   coûte la lecture de 147 Mo sur 8 fils).
4. Déduplication par occurrence `(doc, byte_from, byte_to)`.
5. Spans à la demande : construits seulement pour un collecteur ; le tf compte les
   matches.
6. **Résultats (4.2)** : l'id est le fast field `_node_id`, lu sans le store. Les
   documents ne sont relus que si l'appelant demande les champs
   (`ShardedHandle::fetch_docs`) : séquentiel sous 64 hits, une tâche luciole par
   (shard, segment) au-delà, lecteurs de store du `Searcher` réutilisés. 5 202 hits :
   114 → 15 ms ; sans champs 1,6 ms ; top-200 0,6 ms.

## 5. Le document store

Blocs LZ4 de 16 Ko, cache LRU de 100 blocs décompressés **par segment** sur le
`Searcher` (à garder en tête pour WASM : jusqu'à des centaines de Mo si un client relit
beaucoup). Le coût d'un fetch est le texte qu'il décompresse et rien d'autre : 147 Mo
à 3 Go/s = 48 ms ; le cache de 4 blocs de la vérification sans positions est sans
effet (candidats triés) ; sauter un champ n'économiserait que la copie. Pour des
**petits documents** (16 par bloc) la question se repose : blocs plus petits ou store
par champ, à mesurer sur un autre corpus.

## 6. L'écriture : segments, dictionnaire, fusions

```
document ─ tokenizer ─┬─ index inversé (postings, fréquences)
                      ├─ collecteur SFX v3 (jetons, postings de chunks et de mots)
                      ├─ fast fields · doc store · fieldnorms
                      └─ segment coupé à un budget ─ construction en fond :
                            FST (builder v3) + sidecars ─ publié au commit
```

- **Collecteur** (`collector_v3.rs`, `add_value`) : par valeur, les chunks du
  tokenizer, un jeton étendu (chunk + recouvrement) interné par `(forme, texte)`, une
  entrée de mot par ordinal mot-dépouillé (la première occurrence ; un mot suivi de
  séparateurs différents est un seul ordinal), postings par ordinal ; tampons
  réutilisés, rien d'alloué par occurrence depuis le 13 au soir (`07` § 5 octies).
- **Fils** : `min(cœurs, 16)` natifs, un sur WASM. Tas d'écriture 25 Mo et budget SFX
  128 Mo **par fil** : la forme des segments ne dépend pas du nombre de fils (noyau :
  308 segments à 16 fils, 263 à 8, requêtes égales). Moins de segments = moins de
  fils au prescan : toute forme nouvelle se rejoue côté requête.
- **Dictionnaire partagé à l'écriture** : chaque segment écrit ses textes neufs
  (`.newsfx` / `.newtexts`) ; le commit nomme ses paires en attente (jusqu'à
  `LUCIVY_DICT_MAX_PENDING` = 64, 16 avant — au-delà, repli synchrone sur le fil
  appelant, ce qui coûtait 25 s des 97 du noyau) et une tâche de fond les replie en
  génération. Le chemin par jeton : filtre de Bloom sur la clé d'internement, puis
  **cache partagé des ids trouvés** (`LookupCache` : table fixe de paires atomiques,
  deux slots par hash, sans verrou ; chaque hit vérifié sur `.termtexts` avant usage —
  le cache propose, le fichier décide), puis la marche des parties FST, puis la table
  des textes en attente (64 stripes ; **par époque de commit depuis le 13 au soir,
  tard** : clés en arène, table par époque, `prepare_commit` tourne l'époque avant de
  vider les écrivains et le commit lâche les époques antérieures entières une fois ses
  paires nommées — le `retain` à `String` était 6,7 s de chemin sériel sur le noyau)
  et le mintage (compteur atomique par champ). Sur le noyau : 71 % des marches
  évitées. Dans une marche, le groupe de parents voulu
  est atteint par le `.pidx` (dichotomie, au plus 16 en-têtes, arrêt au premier
  recouvrement dépassé) : décodage 52 → 23 s de CPU sur le noyau, mur 48,2 → 47,4.
  Un fichier de plus dans une génération se déclare dans
  `dictionary::GENERATION_EXTENSIONS`, la source unique de l'inventaire (GC,
  snapshots, restes, tailles).
- **Compaction et replis** (`dictionary_compact::merge_sfx`) : fusion en flux des
  FST d'entrée (union), records copiés verbatim quand une seule partie les tient,
  parents fusionnés et ré-encodés sinon (les parties ont des ids disjoints : pas de
  dédoublonnage, l'encodeur ordonne) ; **pipeline** union → deux encodeurs → écrivain
  (insertion FST, sérielle par nature) sur canaux bornés, une allocation par lot ;
  sortie identique à l'octet. WASM : mêmes étapes en séquence. 6 générations du noyau
  : 10,5 → 3,0 s. `LUCIVY_FST_REGISTRY` (10 000) dimensionne le registre de nœuds du
  constructeur : plus grand = FST plus petite (75 → 43 Mo à 4 M) mais passe ×1,6.
- Fusions de segments, `wait_merges_quiet`, permis de construction, LUCE/LUCIDS,
  blob store : inchangés (voir `04-architecture.md` §6-7).

## 7. Ce qui garantit tout cela

- Vérité terrain : `test_sfx_v3_ground_truth.rs`, comptes **et** spans contre un scan
  des fichiers, dans les trois dispositions, doublons comptés.
- `test_positions_off`, `test_relaxed_duplicate_spans`, `test_fetch_docs` (ordre,
  champs, suppressions puis fusions), `test_dictionary_index` (repli différé),
  `test_compat_308`, fédéré, filtré, LUCE.
- Bancs : `bench_docstore_fetch`, `bench_dict_compaction` (empreintes des sorties),
  `benches/compare_engines.sh` sur le corpus épinglé.
- Navigateur : toute parallélisation se vérifie sur 10 000 fichiers dans Chrome.

## 8. Ce qui reste ouvert

Le mintage sans `String` (le `.pidx` est fait, `07` § 5 sexies), le coût par
document des collecteurs, la finalisation d'un segment en deux tâches, l'arrêt du
monde au commit ; la vérification sans positions avec le harnais
des traversées ; l'index à la carte (`01-index-a-la-carte.md`).
