# Architecture de lucivy — état au 13 septembre 2026 (4.1.0 publiée)

*Autonome. Le détail des formats est dans `docs/11-09-2026/03-architecture.md` et
`docs/05-09-2026/07-architecture.md` ; les commandes dans `05-knowledge-dump.md`.*

## 1. Les couches

| crate | rôle |
|---|---|
| `ld-lucivy` | le moteur : segments (fork de tantivy 0.22), moteur SFX (FST des suffixes), requêtes, BM25, fusions, document store |
| `lucivy-core` | `ShardedHandle`, constructeur de requêtes JSON, tokenizers, snapshots LUCE / deltas LUCID(S), blob store |
| `luciole` | acteurs et DAG, sans `thread::spawn` (compatible WASM) |
| `lucistore` | persistance partagée, `BlobStore` |
| `sparse-vector` | index sparse (postings + WAND) |
| bindings | Python (PyO3), Node (napi), C++ (cxx), navigateur (emscripten), pont rag3db |

Tout le workspace porte le même numéro : **4.1.0**.

## 2. Quatre dispositions, une seule vérité

La taille de l'index est un **choix fait à la création** ; les réponses, elles,
ne changent jamais. Mesuré sur le noyau épinglé (101 373 fichiers, 899 Mo) :

| disposition | index | × texte | ce qu'elle change |
|---|---|---|---|
| `sfx_version` 3 | 6 821 Mo | ×7,6 | une FST de suffixes par segment |
| dictionnaire partagé (défaut) | 5 044 Mo | ×5,6 | une FST par **shard**, en générations |
| + `derived_in_ram` | 3 392 Mo | ×3,8 | les trois dérivés reconstruits à l'ouverture, octet pour octet |
| + `positions: false` (4.1) | **2 478 Mo** | **×2,8** | postings documents + fréquences, chaque match vérifié sur le texte stocké |

Les quatre passent le même panel de vérité terrain, **10/10 sur le noyau entier**.

## 3. Les fichiers d'un segment, par champ texte

| fichier | contenu |
|---|---|
| `.sfx` / `dict-<g>.sfx` | la FST des suffixes (partitions `0x00` début de jeton, `0x01` suffixe interne, `0x02` mot sans séparateurs) |
| `.termtexts` | texte et méta de chaque jeton (`own_len`, `sep_len`, recouvrement) |
| `.gmap` | ordinaux locaux → ids globaux du dictionnaire |
| `.sfxpost` | `SFP5` : positions ; `SFP6` : documents et fréquences |
| `.word_sfxpost` | `WSP5` / `WSP6`, idem au niveau du mot |
| `.posmap`, `.word_pos_map`, `.sibling_v3` | dérivés : position → jeton, position → mot, voisins |
| `store` | **le document store** : les champs stockés, compressés par blocs |

Avec `positions: false`, les trois dérivés ne sont **ni écrits ni calculés**, et
rien de positionnel n'est produit à l'indexation ni aux fusions.

## 4. Le chemin d'une requête

1. **Phase FST**, sans aucune position : candidats d'un seul jeton, marche
   descendante, chaînes à travers les frontières, plan mémoïsé par shard.
2. **Avec positions** : les postings donnent les positions, `.posmap` rebâtit les
   fenêtres de texte, `place_spans` place les octets, puis `verify_literal`
   confirme l'occurrence sur le texte.
3. **Sans positions** (`briques::stored`) : les candidats sont des **documents**,
   et chaque candidat est **relu dans le document store** puis vérifié avec les
   prédicats mêmes de la vérité terrain (repli Unicode, séparateurs ôtés en
   relâché, occurrences chevauchantes, bornes, Myers, Jaro-Winkler fenêtré,
   `find_iter`).
4. **Déduplication par occurrence** (`orchestrator::dedup_occurrences`, 13
   septembre) : clé `(doc, byte_from, byte_to)` pour un match placé, sa position
   sinon. Avant, la clé contenait la position et une même occurrence trouvée par
   deux chemins comptait deux fois — dans les spans **et dans le tf**.
5. **Spans à la demande** (13 septembre) : le vecteur `(doc, début, fin)` n'est
   construit que si un collecteur l'attend. Le tf compte les matches, pas les
   spans, donc les comptes ne bougent pas. Gain jusqu'à −44 % sur une requête à
   millions de spans. Le fuzzy et la regex gardent les leurs : leur tf s'en déduit.

## 5. Le document store — la piste ouverte

C'est le prochain sujet, et voici ses portes d'entrée.

| fait | où |
|---|---|
| ouverture par segment | `SegmentReader::get_store_reader(cache_num_blocks)`, `src/index/segment_reader.rs:342` |
| le chemin sans positions l'ouvre avec **4 blocs** de cache | `src/suffix_fst/briques/stored.rs:611`, puis `store.get(doc)` par candidat |
| le `Searcher` en prend **100** (`DOCSTORE_CACHE_CAPACITY`) | `src/store/reader.rs:23` |
| bloc de **16 384 octets** par défaut | `default_docstore_blocksize()`, `src/index/index_meta.rs:365` |
| compression **LZ4** par défaut | `impl Default for Compressor`, `src/store/compressors.rs:133` |
| en WASM, pas de thread dédié à la compression | `docstore_compress_dedicated_thread: false` |

**Ce que la mesure dit** : sur `mutex_lock` au noyau, la recherche coûte 16 ms et
**aller chercher les documents 123 ms** — à chaud comme à froid, ce qui indique
que le store était déjà hors cache dans les deux cas. Deux pistes en découlent :
le cache de 4 blocs du chemin sans positions (chaque candidat relu peut coûter
une décompression complète), et le fait que la vérification lit **tout** le
document là où elle ne cherche qu'un champ.

## 6. Écriture, fusions, persistance

Collecteur par segment (`SfxCollectorV3`, `without_positions` pour l'option) →
DAG de construction : FST ou mintage dans le dictionnaire, postings, dérivés.
Dictionnaire partagé : chaque commit nomme ses textes neufs, une tâche de fond
les replie en génération, compactée au-delà de huit ; la recherche attend ce
repli par défaut. Fusions : v3 réinterne les textes, v4 remappe les `.gmap` ;
sans positions, les fréquences voyagent par des positions fictives.

`StdFsDirectory` (I/O différée, WASM/OPFS), `RamDirectory`, `BlobDirectory`
(ACID dans le store de l'application). `ShardedHandle` : N shards, routage
token-aware (0,2 par défaut), BM25 exact entre shards, pré-filtre réel par ids,
fédération par statistiques exportées.

## 7. Ce qui garantit tout cela

- **La vérité terrain** : un panel comparé à un balayage des fichiers, comptes
  **et** spans — et depuis le 13, un span rendu deux fois compte comme un span en
  trop. C'est elle qui a trouvé les doublons du moteur publié.
- **Le banc à trois moteurs**, sur un **corpus épinglé par son commit**, où
  chaque ligne porte les deux temps d'Elasticsearch (documents, puis highlights).
- **La CI en trois fichiers** : `ci.yml` (le code est juste), `build.yml` (ça se
  construit partout), `release.yml` qui **appelle** les deux avant de publier —
  le feu vert d'une publication est la CI de tous les jours, pas une copie.
