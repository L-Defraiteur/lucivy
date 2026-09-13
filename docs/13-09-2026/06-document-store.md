# Le document store, mesuré — 13 septembre 2026

Point de départ : sur `mutex_lock` au noyau, le harnais annonçait 16 ms de recherche
et **123 ms pour relire les documents**, à chaud comme à froid. Deux hypothèses avaient
été notées (`04-architecture.md` §5) : le cache de 4 blocs du chemin sans positions,
et la relecture du document entier pour un seul champ. **Les deux sont fausses.**
Voici ce que la mesure dit, et ce qui en découle.

## 1. Le banc

`lucivy_core/tests/bench_docstore_fetch.rs` (`#[ignore]`), sur un index existant :

```bash
V3_INDEX_DIR=~/lucivy_bench/compare-4.1/dict-nopos BENCH_QUERY=mutex_lock \
cargo test --release -p lucivy-core --test bench_docstore_fetch -- --ignored --nocapture
```

Même liste de hits, chaque phase chronométrée séparément ; trois tours dans le même
processus pour séparer la première touche du régime établi. `BENCH_THREADS`,
`BENCH_ROUNDS`, `BENCH_RELAX` en options.

## 2. Ce que ça donne (`mutex_lock`, strict, 5 202 hits, index sans positions)

| phase | tour 0 (processus neuf) | tours 1-2 | ce qu'elle mesure |
|---|---|---|---|
| recherche seule | 27,5 ms | 21-22 ms | le moteur (dictionnaire seul : 17,5 puis 9,5 ms) |
| `_node_id` par fast field | 1,6 ms | 1,6 ms | zéro accès au store |
| octets du document (`get_document_bytes`) | 87 ms | 48 ms | seek + **décompression LZ4** |
| document complet (`searcher.doc`), ordre des scores | **114 ms** | 13,8 ms | octets + désérialisation ; tours 1-2 = tout en cache LRU |
| document complet, hits triés par (segment, doc) | 13,2 ms | 13,4 ms | lecteurs déjà chauds, pas une mesure de localité |
| document complet, **8 fils** par groupe de segments | 11,2 ms | 10,6 ms | le plafond d'un fetch parallèle |
| « comme la vérification » : ids triés par segment, LRU 1 / 4 / 100 blocs | 52,5 / 52,4 / 53,6 ms | 52 / 52 / 52 ms | la forme de `verify_stored` |

Les mêmes chiffres sur l'index à dictionnaire (272 segments) : identiques à 2 ms près.

**Le volume d'abord** : ces 5 202 documents pèsent **147 Mo de texte** (28 Ko en
moyenne — les fichiers qui prennent un mutex sont de gros fichiers). Tout le reste
en découle.

## 3. Ce que les chiffres disent

1. **Les 123 ms n'étaient pas le store, c'était le protocole.** Le harnais relit
   **tous** les hits, en entier, séquentiellement, dans un processus neuf. Ça se
   décompose en : ~40-65 ms de défauts de page à la première touche du `mmap` (87 → 48
   ms entre le tour 0 et le tour 1 sur les octets seuls), **48 ms de décompression
   LZ4** (147 Mo à ~3 Go/s, incompressible tant qu'on lit ce volume), et 3 à 14 ms de
   désérialisation. « À chaud comme à froid » parce que le harnais n'est jamais chaud :
   chaque run est un processus, donc un LRU vide et un mapping neuf.
2. **Le cache de 4 blocs n'est pas en cause.** Les candidats de `verify_stored`
   arrivent triés par document (ordre des postings) ; LRU de 1, 4 ou 100 blocs donnent
   le même temps. Rien à changer là.
3. **Relire « tout le document pour un champ » ne coûte rien ici.** La compression est
   par bloc, et un document plus grand que le bloc (16 384 octets) **est** son bloc :
   pour le noyau, `content` est le document. Sauter le champ à la désérialisation
   économiserait la copie (quelques ms sur 147 Mo), pas la décompression. Ce point
   redevient vrai pour des **petits documents** (1 Ko → 16 par bloc, on décompresse 16
   Ko pour en lire 1) : à mesurer sur un corpus de petits documents, pas sur le noyau.
4. **La vérification sans positions est bien parallèle** — confirmé dans le code
   (`prescan_segments_more` scatter chaque segment sur le scheduler luciole, et
   `verify_stored` tourne dans `prescan_one`) et dans les chiffres : recherche sans
   positions 21 ms contre 9,5 ms avec, soit +11 ms, ce que coûte justement la lecture
   de 147 Mo sur 8 fils (10,6 ms). En natif, c'est le prix exact du choix « pas de
   positions » sur cette requête : décompresser le texte des candidats.
5. **Le fetch des résultats, lui, n'est pas parallèle — et il lit trop.** Dans les
   quatre bindings (Python `collect_sharded_results`, Node, C++ `collect_results` et
   `collect_results_with_highlights`, emscripten) et dans `ShardedHandle::search_with_docs`,
   la boucle est séquentielle et **relit le document entier même quand l'appelant n'a
   pas demandé les champs**, uniquement pour lire `_node_id` — qui est un fast field
   (`node_ids_of` existe déjà et le lit sans le store). Sur 5 202 hits : 1,6 ms au lieu
   de 114. Sur un top-200 de gros fichiers : ~4 ms gaspillés par requête, du même
   ordre que la recherche elle-même.
6. **Le LRU du `Searcher` retient beaucoup** : 100 blocs décompressés **par segment**,
   donc 272 × 100 blocs sur cet index — c'est pourquoi le tour 1 trouve tout en cache
   (4 719 blocs distincts). Jusqu'à plusieurs centaines de Mo si un client relit
   beaucoup de gros documents ; ça ne se remplit qu'au fil des fetchs, et le chemin de
   vérification ouvre le sien (4 blocs, jeté après). À garder en tête pour WASM.

## 4. Ce qui en découle

Par ordre de rendement :

1. **`_node_id` par fast field dans les cinq sites**, document relu seulement quand
   `fields` est demandé (Python, Node, emscripten) ; C++ et `search_with_docs` :
   idem selon la signature. Gain : le coût du fetch disparaît pour l'appel par défaut.
   Petit patch, contrat inchangé.
2. **Fetch parallèle par segment via le scheduler** quand les champs sont demandés et
   que les hits sont nombreux (au-delà de quelques dizaines) : ×5 mesuré, en
   réutilisant les lecteurs de store du `Searcher` (thread-safe, `get(&self)`). Rien à
   gagner sur un top-10.
3. **Rien à changer dans le store lui-même** pour ce corpus : LZ4 est le décompresseur
   le plus rapide qu'on ait, et le bloc vaut le document.
4. **Plus tard, à mesurer sur des petits documents** : taille de bloc plus petite
   (moins de décompression par document, ratio moins bon) ou un store par champ
   (`content` séparé des petits champs). Note pour l'index à la carte
   (`01-index-a-la-carte.md`) : la forme du store est aussi une réponse à « quelles
   questions on pose » — si l'appelant ne relit jamais `content`, il n'a pas à le
   décompresser.
5. **Dans le harnais et le banc comparatif**, le temps de fetch reste hors du temps
   annoncé (c'est déjà le cas) ; le passer au fast field le ramènerait de 123 à 2 ms
   sans rien changer aux vérités.
