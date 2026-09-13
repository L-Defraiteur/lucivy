# Knowledge dump — 13 septembre 2026, soir

*Autonome. Reprend `05-knowledge-dump.md` (matin) et ajoute ce que la journée a
apporté : le profil de l'indexation, les bancs du store et de la compaction, le
chemin de republication. Règles de maison inchangées : tutoiement, docs en français,
code et commentaires en anglais, jamais de nom d'assistant dans le dépôt ; sortie des
commandes dans un fichier puis `grep`, jamais `| tail` ; `export
PATH="$HOME/.cargo/bin:$PATH"` ; `free -g` avant un gros run ; jamais deux builds
90k en parallèle ; un seul onglet qui indexe ; publier seulement sur le mot de Lucie.*

## 1. Tests

```bash
cargo test --release --lib                              # 1 471 (22 ignorés)
cargo test --release --lib --no-default-features        # 1 437
cargo test --release -p lucivy-core                     # 45-46 lots, tout vert
cargo test --release -p lucivy-cpp                      # 19
cargo test --release -p lucivy-fst                      # 141 (la fourche FST)
cargo clippy -p ld-lucivy -p lucivy-core -p lucivy-cpp -p lucivy-napi   # 0 erreur ; --tests a des erreurs antérieures
(cd bindings/python && source .venv/bin/activate && bash build.sh && python -m pytest tests -q)   # 113
(cd bindings/nodejs && npm run build && node test.mjs && for t in tests/*.mjs; do node "$t"; done)
#   smoke_warnings.mjs prend le chemin absolu de lucivy.node ; test.mjs n'imprime pas « ok »
bash bindings/emscripten/build.sh                       # WASM, ~1 min incrémental
```

Nouveaux tests du jour : `test_fetch_docs` (fetch parallèle : ordre, champs,
suppressions puis fusions), `bench_docstore_fetch` (ignoré), `bench_dict_compaction`
(ignoré). Un message de fond sans conséquence dans Python et Node : un repli qui
trouve son répertoire temporaire déjà supprimé en fin de test.

## 2. Vérité terrain

`lucivy_core/tests/test_sfx_v3_ground_truth.rs`, test `v3_ground_truth_demo` ;
`V3_CORPUS`, `V3_MAX_DOCS`, `V3_COMMIT_EVERY`, `V3_SFX_VERSION` (3 par défaut, 4 =
dictionnaire), `V3_POSITIONS=0`, `V3_DERIVED_IN_RAM=1`, `V3_QUERIES=
'a:strict,b:relax,c:fz1,d:fz2,e:rx'`, `V3_INDEX_DIR` (réutilisé si la forme est la
même : **supprimer le dossier pour remesurer l'indexation**), `LUCIVY_HIGHLIGHT_SPAN_CAP=0`.
Panel 10k dans les trois dispositions : `V3_MAX_DOCS=10000 V3_COMMIT_EVERY=2000`
+ `V3_SFX_VERSION=4` / `V3_POSITIONS=0` / `V3_SFX_VERSION=3` ;
`LUCIVY_DICT_MAX_GENERATIONS=2` force une compaction à chaque repli.
Le fetch des hits par le harnais lit le fast field (plus de document relu).
Corpus : `~/lucivy_bench/linux-7.2` (épinglé, `benches/compare_engines.sh` le clone),
`~/lucivy_bench/linux-2.6.0/linux` (extrait de `playground/corpus-linux-2.6.0.tar.gz`).

## 3. Profiler l'indexation

- `samply` est installé mais demande `perf_event_paranoid` ≤ 1 (root, non persistant).
- **Sans root** : `benches/gdb_sample.sh samples.txt 0.25 400 -- <binaire> <args>`
  (gdb + `SIGALRM` externe toutes les 250 ms, piles de tous les fils) puis
  `python3 benches/gdb_top.py samples.txt [--inclusive] [--threads] [--from S --to S]`.
  Binaire à tables de lignes dans un `CARGO_TARGET_DIR` à part (un seul binaire, pas
  de périmé) : `CARGO_TARGET_DIR=~/lucivy_bench/target-prof
  CARGO_PROFILE_RELEASE_DEBUG=line-tables-only cargo test --release -p lucivy-core
  --test test_sfx_v3_ground_truth --no-run`.
- **Compteurs** : `LUCIVY_VERBOSE=1` (par commit : lookups du dictionnaire — cache,
  génération, attente, mintés, filtrés, `fst`, `parents decoding`, `lock` — replis,
  compactions, finalisations), `V3_PROFILE=1` (étapes du pipeline de compaction,
  tailles FST / parents, `[fst]` par segment). Horodater : `2>&1 | python3 -c 'import
  sys,time; t=time.time(); [sys.stdout.write("%7.2f %s" % (time.time()-t, l)) for l in sys.stdin]'`.
- **A/B honnête** : ancien binaire rebâti (`git stash push -- <fichiers suivis>` —
  `Cargo.lock` n'est pas suivi, le nommer fait échouer la remise), même état de
  machine, deux runs chacun. Une compilation de fond a inventé un gain de 21 %.
- Fils : `LUCIVY_WRITER_THREADS`, `LUCIVY_WRITER_HEAP` (total), `LUCIVY_SFX_HEAP`
  (total) ; les défauts sont par fil (25 Mo, 128 Mo) sur `min(cœurs, 16)`. Relever le
  nombre de segments et rejouer le panel sur tout changement de forme.

## 4. Bancs du jour

```bash
# document store : phases d'un fetch sur un index existant
V3_INDEX_DIR=~/lucivy_bench/compare-4.1/dict-nopos BENCH_QUERY=mutex_lock \
  cargo test --release -p lucivy-core --test bench_docstore_fetch -- --ignored --nocapture
# fetch_docs via un dossier shardé (lien symbolique shard_0 + _shard_config.json)
SHARDED_DIR=~/lucivy_bench/scratch-positions/sharded-nopos cargo test --release -p lucivy-core \
  --test bench_docstore_fetch bench_sharded -- --ignored --nocapture
# compaction seule, sur des liens vers les dict-* d'un index ; DICT_KEEP=1 pour comparer par sha256sum
DICT_DIR=~/lucivy_bench/scratch-positions/compact-bench DICT_GENS=2,4,6,11 DICT_FIELD=2 V3_PROFILE=1 \
  cargo test --release -p lucivy-core --test bench_dict_compaction -- --ignored --nocapture
# à froid : benches/cold_cache.py (fadvise DONTNEED, sans root)
```

Références : noyau 48,2 s (16 fils), 30 000 fichiers 14,2-14,8 s, 10 000 4,8 s ;
compaction 4 générations 2,6 s ; fetch 5 202 hits 15 ms.

## 5. Comparatif

`benches/compare_engines.sh kernel ~/lucivy_bench/compare-4.1` : corpus épinglé
(clone/checkout automatiques), quatre dispositions lucivy (**supprimer `dict`,
`dict-ram`, `dict-nopos`, `v3` du dossier pour remesurer l'indexation**), tantivy
rebâti, Elasticsearch (conteneur `lucivy-es`, index réutilisé si l'id de corpus est le
même : ses temps d'indexation viennent alors d'un run antérieur), rapport
`compare_engines.md` (générateur `compare_engines_report.py`, étiquettes « lucivy
4.2 »), copié dans `docs/compare-engines-2026-09-13.md`. Le README, l'article
(`docs/07-09-2026/06-…md` et `playground/blog/*.html`, barres à largeurs) et la page
reprennent ses chiffres à la main.

## 6. Playground et WASM

`cd playground && node serve.mjs` (port 9877). `?corpus=corpus-kernel-10k.tar.gz`
(+ `&nopos`, `&commitmb=2` pour forcer des compactions, `&verbose`), `index linux` au
prompt du terminal (Linux 2.6.0, 35 s, 1 089 Mo). Depuis la console :
`window._playground.search(query, { limit, highlights, fields })` et
`memoryStatus()` ; contrôle à l'octet des spans par `TextEncoder`. **Règle** : toute
parallélisation ou changement du chemin d'indexation se vérifie sur 10 000 fichiers
dans Chrome. Observé : la toute première recherche après une indexation à commits
très rapprochés attend les fusions de fond (27 s puis 260 ms).

## 7. Publication

Chemin : branche de travail `v4.x` → PR vers `main` → fusion (GitHub **rebase** :
nouveaux SHA) → CI verte du commit exact de `main` → tag `v4.x.y` → `release.yml`
(ci + build, puis PyPI, npm ×6, wasm, crates.io **en dernier**). Numéro partout :
`Cargo.toml` (×9, dépendances internes comprises), `package.json` (×7), `pyproject`,
en-têtes des README, bannière et `BUILD` de la page. **Tout crate modifié change de
numéro**, `lucivy-fst` compris (publié en premier, version lue par `cargo metadata`).
Trusted publishers crates.io sur les six crates (`release.yml`, environnement
`release`). **Republier une partie** : `gh workflow run release.yml --ref main -f
publish=true` — chaque job saute une version déjà présente (PyPI `skip-existing`,
npm `npm view`, crates.io par l'API) ; `main` est cible de déploiement autorisée de
l'environnement à côté des tags `v*`. Vérifier ensuite registre par registre
(`curl crates.io/api/v1/crates/<c>`, `pypi.org/pypi/lucivy/json`, `npm view`).
Surveiller un run : `gh api repos/L-Defraiteur/lucivy/actions/runs?head_sha=<sha>`
et `…/runs/<id>/jobs` ; un job échoué « en 1 s sans étape » = refus de l'environnement.
`gh` sur le compte personnel (`gh auth status`).

## 8. Pièges

zsh ne découpe ni `$VAR` ni `set -- $spec` (scripts bash) ; `sed` avec `&` dans le
remplacement ; `/tmp` vidé à 10 jours ; `ls deps/x-* | head -1` prend un périmé ;
`cancel-in-progress` avale les verdicts des pushes rapprochés ; le sampler gdb :
`handle SIGALRM stop print nopass` (`noprint` implique `nostop`), viser l'enfant de gdb
(`pgrep -P`) ; le `git stash` avec un fichier non suivi ; une mesure sur machine
chargée mesure la charge.
