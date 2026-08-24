# Knowledge dump — tests, benchs, scripts, points critiques (24 août 2026)

Tout ce qu'il faut pour vérifier, mesurer et diagnostiquer. Autonome ;
`docs/BENCHMARKS.md` reste le mode d'emploi détaillé du harnais 50k.

## 0. Réflexes

```bash
export PATH="$HOME/.cargo/bin:$PATH"          # toujours
cargo test --release ... > /tmp/x.txt 2>&1     # toujours vers un fichier, jamais | tail
grep -E "test result|FAILED|panicked" /tmp/x.txt
cargo test --release -p lucivy-core --no-fail-fast   # sinon la suite s'arrête à bench_sharding
```

Release pour tout ce qui mesure ; **debug** pour les asserts de contrat
(`CachedPrescan::new` trie, `SliceCursor::new` trié, `debug_assert` de
monotonie) : `cargo test -p lucivy-core --test test_sfx_v3_pipeline`.

## 1. Les suites, et ce que chacune garde

### ld-lucivy (`cargo test --release --lib`, 1415 tests, ~5 s)
Le moteur : index, segments, merges, requêtes, SFX v3 unitaires
(`src/suffix_fst/briques/*` ont leurs tests), union/boolean. Les tests du
moteur v2 utilisent `Index::create_in_ram_sfx2`.

### lucivy-core (`cargo test --release -p lucivy-core --no-fail-fast`)
| binaire | garde | durée |
|---|---|---|
| lib (102) | query compat, warnings, blob_directory, snapshot, sync | 7 s |
| `test_sfx_v3_pipeline` (40 + 4 ignorés) | **le filet principal** : merge = frais (spans), policy, chaînes strict (`v3_strict_sep_head_three_chunks`), mots sans séparateur final et ponctuation multi-octets (`v3_contains/fuzzy_*_beside_multibyte_punctuation`), case fold (Kelvin, İ), `parse` (deux formes, highlights, syntaxe booléenne), `_node_id` estampillé, config stricte, migration v2→v3, fuzzy/regex sur tous les shards, **union fuzzy triée** (`v3_fuzzy_union_docsets_are_sorted`), handle fermé, **filtre routé** (`v3_sharded_filter_routes_to_holding_shards`) | 5 s |
| `test_sfx_v3_ground_truth` (10) | spans à l'octet contre le disque sur rag3db : `v3_ground_truth_contains` (15 requêtes strict/relax/fz/rx), `v3_ground_truth_coherence` (32 requêtes RAG : longs littéraux à séparateurs, sw/term, typos, accents, CJK, emoji/ZWJ), `v3_distributed_coherence` (19 × 3 formes : 1 shard / N shards / 2 nœuds simulés), `v3_sharded_filter_delete_delta` | 45 s |
| `test_acid_blob_v3` (4) | blobs = vérité (cache effacé, réouverture ailleurs), lazy = mêmes réponses et ouverture < moitié du store, `drop_index` ne laisse rien, **aucun appel au store après close** (store-sentinelle) | 4 s |
| `test_commit_floor` (4, `--ignored`) | chronos de commit RAM/blob/réouverture et **comptage des appels au store par phase** (`CountingStore`) — le harnais qui a trouvé le plancher de 733 ms et les 480 appels | 1 s |
| `test_fuzzy_ground_truth`, `test_fuzzy_monotonicity`, `test_regex_ground_truth`, `test_merge_contains` | fuzzy/regex vs définitions partagées ; d croissant ⊇ | 5-8 s |
| `test_luce_roundtrip` | import du snapshot v2 du playground (migration) | 0,1 s |
| `test_query_warnings`, `test_two_fields`, `test_diagnostics`, `test_store_fallback`, `test_cold_scheduler` | divers | s |
| `benches/bench_sharding` | **t01 (clone réseau) et t04 (sfx:false disparu) échouent depuis toujours** — ignorer | |
| `acid_postgres` (6, `--ignored`) | vrai Postgres, v2 | service requis |

### luciole (`cargo test --release -p luciole --lib`, 169)
Scheduler, pools (`request_dropped_reply_is_an_error`,
`drain_and_shutdown_after_workers_left_do_not_panic`), DAG
(`panicking_node_is_a_dag_error_not_a_double_free` — abortait le process
avant le 24 août), pipe/collect, wait graph.

### lucistore (`cargo test --release -p lucistore`, 41)
Delta/snapshot/sync, `ShardRouter` (déplacé ici : `.filter(|(_, count)| **count …)`
à cause de l'édition 2024).

### sparse-vector (`cargo test --release -p sparse-vector`, 62)
13 handle (fs, blob, legacy `sparse.bin`), 15 index, 29 wand (vérité
terrain brute k ∈ {1,5,10,200} élagage on/off, poids négatifs, filtres,
seek, plafonds sous 2 000 mutations, adaptateur mmap), 5 sharded (égalité
avec un handle unique, filtre sur 6 tailles et deux chemins, remove +
réouverture + refus après close, blob store = vérité, config).
Bench : `cargo test --release -p sparse-vector --test bench_wand_compare -- --ignored --nocapture`
(50k records, 2 000 dims, Zipf ; `BENCH_SKEW=1`).

### Bindings
`bindings/python/tests/smoke_warnings.py <dir du .so renommé lucivy.so>`,
`bindings/nodejs/tests/smoke_warnings.mjs <chemin du .so napi>` (build : voir
l'en-tête de chaque script). `bash bindings/emscripten/build.sh` (emsdk
6.0.8 + nightly + `-Z build-std`, EXPORTED_FUNCTIONS dans le script).

## 2. Les benchs de référence (kernel 50k)

```bash
git clone --depth=1 https://github.com/torvalds/linux /tmp/linux-bench     # 95 730 fichiers
git clone --depth=1 https://github.com/L-Defraiteur/rag3db.git /tmp/rag3db-bench
V3_INDEX_DIR=/tmp/v3idx_50k_nat V3_CORPUS=/tmp/linux-bench V3_MAX_DOCS=50000 V3_COMMIT_EVERY=500 V3_PROFILE=1 \
V3_QUERIES='zzqqxxyyww:strict,kmalloc:strict,include:strict,__init:strict,uint64_t:relax,__init:relax,kmallc:fz1,kmalloc:fz2,/\*[^*]*\*/:rx' \
cargo test --release -p lucivy-core --test test_sfx_v3_ground_truth v3_ground_truth_contains -- --nocapture > /tmp/gt50k.txt 2>&1
grep -E "^\s*\S+\s+(strict|relax|fz[0-9]|rx)\s+[0-9]+\s+[0-9]+\s+(OK|MISMATCH)" /tmp/gt50k.txt
```

Modes de `V3_QUERIES` : `strict`, `relax`, `fz1..3`, `rx`, `sw`/`sws`,
`term`/`terms` (le mode = ce qui suit le dernier `:`). Toujours une requête
vide (`zzqqxxyyww`) en tête : c'est le plancher. Cache d'index : clé
`v=10` dans `index_shape_key` — **à incrémenter à tout changement de
format**. Construction 50k ~65-85 s, réutilisation 0 s. `V3_POLICY=1` pour
l'index « réel » (policy de merge).

Chiffres du 24 août soir (24 cœurs, spans exacts, index naturel 800
segments) : plancher 25-27 ms · `kmalloc`/`spin_lock`/`__init` strict 28-30 ·
`include` strict (36 824 docs, 214 692 spans) 46 · `kmalloc` relax 24 ·
`uint64_t` relax 33-45 · `__init` relax 49 · fz1 43-47 · fz2 171 · regex
`/\*[^*]*\*/` 197-220 (421 036 spans) — grep sur la même tâche : 1,6-1,9 s
(5 s pour le fuzzy). Le `+fetch` des documents (hors moteur) coûte souvent
plus que la recherche (`include` : 245 ms).

Commit (MemBlobStore, `test_commit_floor`) : 2 shards 9 docs 5,6 ms, 4 shards
900 docs 18,7 ms, réouverture puis commit 7-10 ms, ~91 appels au store.

Sparse (`bench_wand_compare`) : 137 µs RAM, 127 µs mmap par requête top-10,
insertion 50k en 139 ms.

## 3. Variables de diagnostic

| variable | effet |
|---|---|
| `V3_PROFILE=1` | compteurs par étage (contains / fuzzy / verify_literal / prescan / `relaxed chunk walk: skipped/walked`), lignes `[prescan]`, `[fst]`, `[merge]` |
| `V3_DIAG_LITERAL=mot` (+ `V3_DIAG_BYTE=n`) | chaînes et matchs du contains pour ce littéral (`[lit]`, `[match]`) |
| `V3_DEBUG_QUERY=mot` | trace `[anch]` du chemin « second token anchored » |
| `V3_DIAG_FUZZY=1`, `V3_DIAG_REGEX=1`, `V3_DIAG_RESOLVE=1` | fuzzy (pièces, fenêtres, rejets), regex, résolution posmap |
| `V3_SPANS_REPORT_ONLY=1` | le harnais compare des ensembles de documents au lieu d'asserter les spans |
| `V3_RELAXED_CHUNK_CHAINS=1` | force la marche des chaînes de chunks en relaxed (A/B de B2 bis) |
| `V3_FUZZY_MODE=auto|pieces|pivot|ngram` | générateur de candidats fuzzy |
| `LUCIVY_VERBOSE=1` | commits, policy, finalize |
| `LUCIVY_BLOB_DEBUG=1` (+ `LUCIVY_BLOB_TRACE=suffixe`) | matérialisations lazy (+ backtrace) |
| `LUCIOLE_REPLY_TRACE=1` | backtrace à chaque `Reply` lâché sans réponse |
| `LUCIVY_WAIT_WARN_SECS` | seuil du warning d'attente (défaut 10 s) |
| outils `#[ignore]` du pipeline | `v3_merge_bisect` (`V3_BISECT_*`), `v3_merge_repro_files`, `v3_a2_probe`, `v3_a2_chunks` (dump du tokenizer depuis `/tmp/a2_line.txt`) |

## 4. Où regarder quand ça casse

| symptôme | premier endroit |
|---|---|
| un span manque / en trop | `V3_DIAG_LITERAL` ; `v3_merge_bisect` pour réduire à 1-3 fichiers ; puis `briques/fst_walk.rs` (`build_chains_from_splits`, `falling_walk_chunks`), `composite.rs` (`find_literal_v3`, `second_token_anchored_v3`), `orchestrator.rs` (`verify_literal`, `verify_boundaries`) |
| relaxed ne trouve pas un mot | est-il dans la partition 0x02 ? (`builder_v3::add_word_stripped`, STATS version) ; B2 bis saute les chaînes si pas de mot long |
| `attempt to subtract with overflow` dans `buffered_union` | un scorer non monotone : `doc_tf` non trié (prescans fuzzy/regex/contains → `CachedPrescan::new`) |
| double free / SIGSEGV au teardown | acteurs vivants après `close()` ? `Reply` lâché (`LUCIOLE_REPLY_TRACE=1`) ? valgrind avant `MALLOC_CHECK_` (qui ne voit pas un double free sur chunk réalloué) |
| `wait(...) blocked 10s` avec tous les threads idle | un `Reply` lâché sous un pipe/collect ; le warning stderr nomme le chemin |
| commit lent | `test_commit_floor` (`commit_floor_store_calls`) : fsync ? nombre d'appels au store ? nombre de segments (routage collant) ? |
| 0 résultat silencieux | `query_warnings(json)` d'abord (`parse` branche, `fields` ignoré, regex sans littéral, v2) |
| perf qui bouge | requête vide = plancher ; `V3_PROFILE=1` : `peak concurrency` ≈ cœurs, `derive_miss` = 0, `verify_literal` 40-70 % attendu sur gros volumes |
| lazy qui charge tout | `LUCIVY_BLOB_DEBUG=1` : les lectures de footer doivent passer par `load_range`, matérialisation au 4e accès |

## 5. Scripts et outils divers

- `playground/build_dataset.py` (Python binding requis) reconstruit
  `dataset.luce` — l'actuel est **v2**, volontairement non régénéré (67 Mo
  dans git) ; `playground/serve.mjs` sert la démo wasm.
- `bindings/emscripten/build.sh` ; `bash` obligatoire (emsdk activé dedans).
- Audit de licence du sparse (script inline utilisé le 24 août) : comparer
  les lignes non triviales de `sparse_vector/src/**` à un clone `--sparse`
  de qdrant (`lib/sparse`, `lib/common/common`) ; référence : ≤ 10 % partout.
- Identité git : `git config user.email` local (perso) sur lucivy, rag3db,
  luciole, lucistore ; `~/.ssh/config` `Host github.com` → clé perso,
  `Host github-sairen` → clé pro. Réécriture d'historique : branche de
  sauvegarde, `--force-with-lease`, `git diff` vide entre sauvegarde et HEAD.
