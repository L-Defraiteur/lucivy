# Rapport de progression — 24 août 2026

Suite du 23 août (`docs/22-aout-2026-19h47/08-rapport-23-aout.md`). Journée
menée en tandem avec la session rag3weaver qui migre son FTS vers le
`ShardedHandle` v3 — leurs retours (`rag3db/extension/rag3weaver/docs/
23-aout-2026-20h33/`, docs 04 à 10) ont piloté la moitié des correctifs.

## Vue d'ensemble

| Chantier | Commit | Effet mesuré / testé |
|---|---|---|
| Test ACID v3 sans service externe | `5a05e4e` | blobs = source de vérité, cache mmap jetable, réouverture exacte |
| Lazy loading **optionnel** des blobs | `4fa729e` | ouverture 3,6 Ko au lieu de 104 Ko ; matérialisation au 4e accès |
| `_node_id` estampillé + `add_document_json` | `ce03ac6` | plus de champ à deviner ; id contradictoire refusé |
| Config stricte + réouverture tolérante | `32ca1dc` | `"shard": 4` échoue en nommant les clés ; vieux `_config.json` rouvrent |
| `impl BlobStore for Arc<T>` | `f7dd5c2` | le pont `DynBlobStore` de rag3weaver supprimé |
| `parse` réparé (était du code mort) | `0d70904` | OU par mot×champ / QueryParser si syntaxe booléenne ; warnings |
| `drop_index()` | `a31dcf5` | destruction complète (shards + fichiers racine), 3 storages |
| `close()` arrête les acteurs | `6e6bd24` | poursuite du SIGSEGV rag3db ; test-sentinelle « aucun appel au store après close » |
| Emscripten : build OK | — | `lucivy.wasm` 8,5 Mo produit (emsdk 6.0.8 + nightly installés) ; exécution Node pend sur les ccall proxifiés — à reprendre |
| lucistore : imports morts | `b09667e` | leur `-D warnings` passe |
| Contrat highlights de `parse` pinné | `19f5133` | branche simple surligne, branche QueryParser laisse le sink vide (jamais faux) |
| Plancher de commit : fsync du cache jetable | `9a66fbf` | 9 docs / 2 shards : 733 → 9,6 ms ; 4 shards 900 docs : 17,2 s → 38 ms |
| `.managed.json` sauvé au point de commit | `e8ace07` | 135 → 2 sauvegardes au store par commit |
| Routage collant (64 docs par indexeur) | `8e2db07` | 9 docs = 1 segment/shard : 340 → 81 fichiers, 232 → 6 suppressions |
| Bug de chaîne « clé avalante » (v3) | `4d00531` | `<binder::Expression` retrouvé quand les chunkings diffèrent dans un segment ; panel 50k inchangé |
| luciole : `Reply` lâché sous un pipe avertit | `e6176f5` | plus de collect muet |
| **Mot sans séparateur final absent de la partition mots** (v3) | `36b1edd` | dernier mot d'une valeur introuvable en relaxed dès que la requête chevauchait ses chunks ; STATS versionné, anciens segments en repli chaînes ; clé de cache `v=10` |
| Test luce : snapshot v2 dit tel quel, affichage sûr | `a59e4a8` | plus de panic sur `→` |
| **Double free luciole** : `ptr::read` des nœuds de DAG remplacé par sentinelle + `catch_unwind` ; `request` rend `Err` sur `Reply` lâché ; tout `Reply` lâché avertit (`LUCIOLE_REPLY_TRACE=1` = backtrace) | `3675c3d` | valgrind rag3weaver (doc 26) ; test « nœud qui panique » abortait avant |
| `ShardedHandle` fermé refuse proprement (`closed`) | `3c282c7` | search/commit/add_document → « handle is closed » |
| `parse` booléen → composite de contains (fin du QueryParser) | `8f14edc` | AND/OR/NOT, +/-, guillemets, parenthèses ; highlights et sous-chaîne dans les deux formes ; refus des négations pures |

Harnais ajouté : `lucivy_core/tests/test_commit_floor.rs` (chronos et
comptage des appels au store, `--ignored`).

Suites à `6e6bd24` : **lib 1415/1415**, lucivy-core complet vert (t01/t04 de
bench_sharding pré-existants, hors sujet), luciole 166, bindings natifs
compilent, `cargo check` emscripten passe.

## Les trois découvertes de la journée

1. **`parse` était inatteignable** (trouvé par rag3weaver) : le dispatch
   envoyait toute valeur vers contains, et la branche QueryParser exigeait la
   valeur que sa garde excluait. « Rust safety » était devenu une sous-chaîne
   littérale. Réparé en deux sémantiques honnêtes choisies sur la valeur,
   annoncées par `query_warnings`.
2. **`close()` laissait les acteurs vivants** : merges drainés, writers
   libérés, mais les pools (shards, readers, routeur) gardaient des `Arc` du
   store — la fenêtre exacte du SIGSEGV de leur teardown C++. `close()` rend
   maintenant le handle inerte, et un store-sentinelle le prouve sous merges
   en vol.
3. **Le lazy naïf ne servait à rien** : `ManagedDirectory` lit le footer de
   chaque fichier à l'ouverture — deux petites lectures suffisaient à tout
   télécharger. D'où `BlobStore::load_range` (plages ≤ 64 Ko servies depuis le
   store) et la matérialisation au 4e accès distant.

## Hygiène

Trailers retirés des 35 commits (filter-branch + push --force-with-lease,
sauvegarde `backup/v3-recovery-avant-rewrite`) ; identité git verrouillée en
local sur les 4 repos (la globale reste sairen pour le taff) ; convention
nouvelle pour les dossiers docs : `JJ-MM-AAAA` (triable), ce dossier inaugure.

## Reste ouvert

1. Publication crates.io 2.1.0 (`lucivy-core`, `ld-lucivy`, `luciole`,
   `lucistore`) — rag3weaver vit sur `[patch.crates-io]` + chemin en attendant.
2. Emscripten : exécuter (playground navigateur, son vrai habitat ; sous Node
   le main proxifié sort et les ccall pendent).
3. SIGSEGV rag3db : attendre leur rejeu sur `6e6bd24` ; sinon gdb
   (`thread apply all bt`) et la durée de vie de la connexion C++ chez eux.
4. `verify_literal` = 40-70 % du CPU des requêtes à gros volume (piste perf).
5. Fusion post-suppressions : un gros segment fusionné repart entier dans un
   delta LUCIDS (à borner côté policy si ça gêne).
6. Blob : ~91 appels au store par commit de petit lot (un par fichier de
   segment). rag3weaver les regroupe en une requête (4,5 ms) : pas de blob
   composite chez nous tant que le volume ne le justifie pas.
7. `is_content_char` : tout non-ASCII est contenu (`→`, `«`, `—` sont des
   mots). Cohérent et sans perte, mais une ponctuation Unicode qui compte
   comme un mot se discute ; changement de format si on y touche.
8. Toujours lancer `cargo test -p lucivy-core --no-fail-fast` : sans, la
   suite s'arrête à `bench_sharding` et les binaires suivants ne tournent pas.
