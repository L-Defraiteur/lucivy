# Architecture de lucivy — état au 11 septembre 2026 (4.0.2 publiée, branche `v4.1`)

Autonome ; le détail des formats 4.0 est dans `docs/05-09-2026/07-architecture.md`,
`11-architecture.md` et `docs/06-09-2026/04-architecture.md`.

## 1. Les couches

| crate | rôle |
|---|---|
| `ld-lucivy` | moteur : segments (fork de tantivy 0.22), moteur SFX (suffix FST), requêtes, BM25, fusions |
| `lucivy-core` | `ShardedHandle`, constructeur de requêtes JSON, tokenizers, snapshots LUCE / deltas LUCID(S), blob store |
| `luciole` | acteurs et DAG, sans `thread::spawn` (compatible WASM) |
| `lucistore` | persistance partagée, `BlobStore` |
| `sparse-vector` | index sparse (postings + WAND) |
| bindings | Python (PyO3), Node (napi), C++ (cxx), navigateur (emscripten), pont rag3db |

Tout le workspace porte le même numéro (4.0.2).

## 2. Un segment, un champ texte

Tokenisation en *chunks* (contenu + séparateurs + 2 octets de recouvrement du
suivant) ; des entrées « mot » sans séparateurs (partition `0x02`) pour le mode
relâché. Trois partitions dans la FST : `0x00` début de jeton, `0x01` suffixe
interne, `0x02` mot sans séparateurs.

Fichiers par segment et par champ (layout par défaut) :

| fichier | contenu |
|---|---|
| `.sfx` (v3) ou `dict-<g>.sfx` partagé par shard (v4, défaut) | FST de tous les suffixes, table de parents |
| `.termtexts` (ou `dict-<g>.termtexts`) | texte et méta de chaque jeton (`own_len`, `sep_len`, recouvrement) |
| `.gmap` (v4) | ordinaux locaux → ids globaux du dictionnaire |
| `.sfxpost` (`SFP5`) | positions de chaque jeton (doc, index de jeton) |
| `.word_sfxpost` (`WSP5`) | positions de chaque mot (premier et dernier chunk, `tail_off`) |
| `.posmap` (`PMP4`) | (doc, position) → jeton, points de contrôle d'octet toutes les 16 positions |
| `.word_pos_map`, `.sibling_v3` | dérivés : position → mot ; jeton → jetons qui le suivent |
| `store` | documents stockés (compressés) |

`derived_in_ram` : les trois dérivés rebâtis en RAM à l'ouverture au lieu d'être
écrits. **`positions: false` (4.1)** : `SFP6` / `WSP6` (documents + fréquences),
aucun dérivé ni écrit ni calculé.

## 3. Une requête

1. **Phase FST** (sans position) : candidats d'un seul jeton
   (`fst_candidates_v3`), marche descendante et chaînes de jetons à travers les
   frontières (`falling_walk_*`, `cross_*_chain_*`, complément par la table des
   voisins), plan par shard qui mémoïse les marches (`briques/plan.rs`).
2. **Résolution** (layout par défaut) : postings → positions, adjacence des
   chaînes vérifiée par `.posmap`, fenêtres de texte rebâties depuis `.posmap` +
   `.termtexts`, positions en octets par `place_spans`. Fuzzy : pigeonhole
   (pièces ou n-grammes) puis `fuzzy_spans` / `jaro_spans` sur fenêtres ;
   regex : littéraux requis puis `find_iter` sur fenêtres.
3. **Sans positions** (`briques/stored.rs`) : candidats = documents des jetons
   et des chaînes (listes de documents), puis vérification sur le texte stocké
   avec les prédicats mêmes de la vérité terrain (repli Unicode, séparateurs
   ôtés en relâché, occurrences chevauchantes, bornes, `fuzzy_spans_long` en
   Myers, `jaro_spans_windowed`, `find_iter`). Mêmes documents, spans, scores.
4. **Score** : BM25 (tf = occurrences vérifiées) ; fuzzy par paliers (distance
   vérifiée, ou similarité Jaro-Winkler) ; stats globales pour la fédération.
5. **Bornes** : `LUCIVY_MAX_MATCHES_PER_SEGMENT`, `LUCIVY_HIGHLIGHT_SPAN_CAP`
   (relance restreinte au top-k), troncature signalée (`last_search_truncated`).

## 4. Écriture et fusions

Collecteur par segment (`SfxCollectorV3`, `without_positions` pour l'option) →
DAG de construction (`sfx_dag_v3.rs`) : FST ou mintage dans le dictionnaire,
postings, dérivés. Dictionnaire partagé : chaque commit nomme les textes
neufs ; une tâche de fond les replie en générations, compactées au-delà de 8 ;
la recherche attend le repli (`dictionary_wait`). Fusions : v3 réinterne les
textes ; v4 remappe les `.gmap` ; sans positions, les fréquences sont portées
par des positions fictives recomptées à l'écriture.

## 5. Persistance, sharding, bindings

`StdFsDirectory` (I/O différée, WASM/OPFS), `RamDirectory`, `BlobDirectory`
(ACID dans le store de l'application, verrous jamais envoyés au store depuis
4.0.2). `ShardedHandle` : N shards, routage token-aware (0,2 par défaut), BM25
exact entre shards, recherche filtrée par pré-filtre réel, fédération par stats
exportées. Snapshots servis en place. Navigateur : un worker, pthreads sur
SharedArrayBuffer, OPFS, un seul index en mémoire dans la page.

## 6. Garanties et outils

Vérité terrain : panel comparé à un balayage des fichiers (comptes et spans),
10/10 sur le noyau dans les deux layouts. Contrat de format : 4.0 ouvre 3.0.x
(`test_compat_308`), un index `positions: false` n'est pas cherchable en 4.0.x (il s'ouvre, puis chaque recherche échoue : `sfxpost: invalid V2 format`).
Banc comparatif rejouable : `benches/compare_engines.sh`. Publication par tag
`v*` sur CI verte uniquement (job `checks` bloquant).
