# Knowledge dump — baselines, A/B, tests, outils, pour la session suivante

Tout ce qu'il faut pour reprendre la branche `v4` sans l'historique.
Remplace [`../04-09-2026/08-…`](../04-09-2026/08-knowledge-dump-baselines-tests-outils.md)
(état du matin) ; complète `docs/28-08-2026/09-knowledge-dump-tests-benchs-publication.md`
(publication, corpus, pièges de vérification), toujours valable.

Toujours : `export PATH="$HOME/.cargo/bin:$PATH"` ; sortie dans un fichier
puis `grep`, jamais `| tail` ; le shell est `zsh` (`echo ====` y est une
expansion `=cmd` : mettre des guillemets) ; pas de `/usr/bin/time` ni de
`perf` sur la machine (mesurer la RAM par `VmHWM` de `/proc/self/status`).

---

## 1. État du dépôt et des branches

- **`v4`** = la branche de travail, **poussée sur `origin/v4`**. Commits du
  5 septembre : `4023f2d` plan + `Alts::Prefix` + galop · `0d39507` noyau
  entier + variantes de tests · `57e453e` `.gmap` GMP2 · `8c4f580` option
  `shared_dictionary` + vérité du noyau · puis les trois docs de ce dossier.
- `main` = `origin/main` = `8301b55`, trois commits après `v3.0.8`.
  `wip/publication-3.0.0` = `main` + 3 commits du 28 août **non poussés**,
  à fusionner un jour ; `v4` est déjà par-dessus.
- Ne jamais lancer `gh` sans l'accord de Lucie (compte de travail) ;
  `git push origin v4` est autorisé ; jamais de `cargo publish` sans son feu
  vert ; commits en français, sans trailer.

---

## 2. Les corpus

| chemin | contenu | usage |
|---|---|---|
| `/tmp/lucivy-cmp` | 10 000 fichiers du noyau, 65 Mo | la référence du protocole |
| `/tmp/lucivy-cmp-90k` | 93 983 fichiers, 898 Mo | 30 000 premiers pour les A/B de temps, entier pour le chiffre final |
| `/tmp/linux-bench` | clone du noyau | source |

Sous `/tmp` : à reconstruire s'ils ont disparu (dump du 28 août, §5).

---

## 3. Construire un index de référence et lancer le panel vérifié

Harnais : `lucivy_core/tests/test_sfx_v3_ground_truth.rs`, test ignoré
`v3_ground_truth_demo`. Il indexe en RAM, copie dans `V3_INDEX_DIR`, écrit
`.v3_shape` et **réutilise** l'index si corpus, nombre de fichiers,
`V3_COMMIT_EVERY`, fusion, politique et `V3_SFX_VERSION` n'ont pas changé —
changer le binaire et relancer = **rouvrir** l'index, le test de compatibilité.

```bash
T="cargo test --release -p lucivy-core --test test_sfx_v3_ground_truth v3_ground_truth_demo -- --ignored --nocapture"
# référence 10 000 (160 segments, ~8 s v3 / ~19 s dictionnaire)
V3_CORPUS=/tmp/lucivy-cmp V3_INDEX_DIR=/chemin/idx $T > out.txt 2>&1
# 30 000 (120 segments, ~20 s / ~50 s) — le corpus des A/B de temps
V3_CORPUS=/tmp/lucivy-cmp-90k V3_MAX_DOCS=30000 V3_COMMIT_EVERY=2000 V3_INDEX_DIR=/chemin/idx30k $T
# le noyau entier (253 segments, ~65 s / ~255 s, 7,3 Go v3 / 5,6 Go dictionnaire)
V3_CORPUS=/tmp/lucivy-cmp-90k V3_MAX_DOCS=100000 V3_COMMIT_EVERY=10000 V3_INDEX_DIR=/chemin/idx90k $T
# en mode dictionnaire : V3_SFX_VERSION=4 devant, et un autre V3_INDEX_DIR
```

Le panel : 10 requêtes (`mutex_lock` strict / relax, `spin_lock`, `sched`
term / strict, `printk` sw, `schdule` fz1, `regsiter` fz2,
`spin_lock_[a-z]+` rx, `schdule` jw1 non vérifié), comparées à un `grep`
du disque en **comptes et spans**.

```bash
grep -o '[0-9]* pass, [0-9]* fail' out.txt          # 9 pass, 0 fail attendu
sed -n '/^Query/,/^[0-9]* pass/p' out.txt            # le tableau, temps « search »
```

Variables : `V3_QUERIES="schdule:fz1,mutex_lock:strict"` (répéter une
requête = mémo chaude à la seconde), `V3_PROFILE=1`, `V3_PLAN=0` (sans
plan), `V3_DIAG_FUZZY=1`, `V3_FUZZY_MODE=pieces|pivot|auto`,
`V3_DEBUG_QUERY=<texte>`, `V3_RELAXED_CHUNK_CHAINS=1`,
`LUCIVY_DICT_MAX_GENERATIONS`.

**Les autres vérités du fichier** : `v3_ground_truth_contains` (15
requêtes), `v3_ground_truth_coherence` (21 littéraux longs, strict et
relâché) — `V3_MAX_DOCS` vaut pour elles aussi, elles réutilisent
`V3_INDEX_DIR` ; à lancer avec `<nom> -- --exact --nocapture`. Les
distribués (`v3_distributed_two_nodes`, `v3_distributed_coherence`,
`v3_sharded_filter_delete_delta`) à leur taille par défaut (3 000).
`test_fuzzy_ground_truth`, `test_regex_ground_truth` (le dépôt lui-même)
acceptent `V3_SFX_VERSION`.

**Piège RAM (5 septembre)** : ne jamais lancer **tout** le fichier avec
`V3_MAX_DOCS=100000` — `perf_shape_*` et les distribués reconstruisent
**chacun** un index de 94 000 fichiers en RAM ; la machine tombe, l'éditeur
avec. Un test à la fois, `free -g` avant, jamais deux constructions 90k en
parallèle. `fuzzy_finds_an_occurrence_that_straddles_tokens` **écrase**
`V3_INDEX_DIR` avec son petit index : pas de `V3_INDEX_DIR` pour lui.

---

## 4. Mesurer les tailles

```bash
python3 benches/scan_index_size.py /chemin/idx > scan.txt 2>&1
grep -A12 'TOTALS' scan.txt
```

Lit tous les layouts (conteneurs 3-8, tables par blocs, `TTX3` 1-3), les
`dict-*` comme des sidecars, compte les `.gmap`. Générations vivantes :
`grep -o '"generations":\[[^]]*\]' meta.json`. `du -sh` compte aussi les
fichiers tantivy.

Baselines de taille (format courant) : 10 000 fichiers **508 Mo** v3 /
**390** dictionnaire ; 30 000 : **1 659 / 1 327 Mo** ; noyau entier
**7,3 / 5,6 Go**. Le « 11,06 Go » d'avant était un v3 en conteneur 5.

---

## 5. Les A/B de temps

**Au même binaire, entre deux index**, 3 passes alternées, min : scripts
du scratchpad `run-ab-plan.sh` (30 000, `idx30k-v7` contre `idx30k-dict`,
sorties `abplan-*`), `run-ab-30k-gmp2.sh` (contre `idx30k-dict2`, `abplan3-*`),
`run-ab-90k.sh` (`idx90k-v8` contre `idx90k-dict`, `ab90k-*`). Extraire :

```python
re.match(r'^(\S+)\s+(strict|relax|term|sw|fz1|fz2|rx|jw1)\s+\S+\s+\S+\s+(?:OK|n/a) \(([0-9.]+)ms search', line)
```

Baselines de temps (30 000, v3, min de 3 passes, ms) : mutex_lock strict
2,0 · relax 1,7 · spin_lock 1,7 · sched term 3,3 · sched strict 2,0 ·
printk 2,3 · fz1 11,5 · fz2 125,8 · rx 9,9 · jw1 14,5. Dictionnaire au
soir : 2,9 · 2,3 · 2,1 · 5,1 · 2,5 · 3,2 · 9,0 · 157,2 · 15,7 · 12,1.
Noyau entier v3 : 6,9 · 7,1 · 7,1 · 15,0 · 7,1 · 8,3 · 50,8 · 638,7 ·
116,4 · 78,5 ; dictionnaire (avant GMP2) : 12,3 · 11,5 · 11,4 · 19,2 ·
9,8 · 13,0 · 44,1 · 765,4 · 192,5 · 68,3.

Règles : le 10 000 ne discrimine pas la milliseconde, le 30 000 oui ;
**rien d'autre ne tourne** pendant un A/B (une compilation fausse tout —
et un script d'A/B qui appelle `cargo test` **recompile** si une source a
changé entre deux passes : ne pas éditer le code pendant) ; les walls
d'un seul passage varient de 20 % — décider sur 3 passes.

**Profil** (`V3_PROFILE=1`) : `[plan] contains "…": N waves, N cells
computed, N held, wall` puis par vague (mur, CPU, cellule la plus lente
`kind key`) ; `[prescan] … scatter DAG wall, per-segment CPU sum, max` (si
max ≈ mur, un segment calcule pendant que les autres attendent : ne doit
plus arriver) ; `[prescan] total prescan_segments_more`, `[weight] total` ;
la ligne `dictionary: memo lookups … | cut N items -> N kept in Nms |
sibling DFS … | anchored …` (où va le temps propre au mode dictionnaire) ;
`[cell] cand/02 "e": N entries, scan, sort` sous une cellule de plus de
2 ms ; `relaxed chunk walk: skipped=N walked=N` (doit ressembler à v3).

---

## 6. Les tests

```bash
cargo test --lib                                   # ld-lucivy : 1 456 verts, 21 ignorés (dont les bancs dictionnaire et postings)
cargo test --lib briques                           # les briques v3, 132 tests, 4 s
cargo test --lib gmap                              # le .gmap (galop, têtes, layout 1)
cargo test --release -p lucivy-core --no-fail-fast # 38 binaires, tout vert (~4 min, charge la machine)
cargo test --release -p lucivy-core --test test_dictionary_index   # v3 contre v4, 300 fichiers, 11 requêtes
cargo test --release -p lucivy-core --test test_federated_search --test test_filtered_search_truth --test test_luce_v3_roundtrip
```

Les trois derniers ont chacun une variante `sfx_version 4`
(`federated_dictionary_nodes_equal_one_v3_index`,
`…_on_dictionary_nodes`, `…_on_dictionary`, `luce_dictionary_sharded_roundtrip`).
Bindings : `bindings/python/tests/test_v3_api.py::TestSharedDictionary`,
`bindings/nodejs/tests/shared_dictionary.mjs` — pas de `maturin` ni `napi`
sur cette machine : ils tournent en CI (`cargo build -p lucivy-napi -p
lucivy-cpp -p lucivy-fts -p lucivy` compile les crates).

`luce_v3_sharded_roundtrip` peut tomber sous charge (ordre des ex æquo
entre shards) ; relancé seul il passe.

---

## 7. Le mode dictionnaire : mesurer, déboguer

- `V3_SFX_VERSION=4` devant le harnais ; `shared_dictionary: true` dans
  un `SchemaConfig` ; `test_dictionary_index::dictionary_pieces` imprime
  chaque pièce d'une requête (parents, `.gmap`, postings, splits,
  chaînes, résolution) sur trois documents.
- Où va le temps, dans l'ordre où ça a été trouvé : le calcul sur un
  thread (→ le plan), une liste de 533 000 entrées pour un reste d'un
  octet (→ `Alts::Prefix`), la coupe qui parcourait tout le `.gmap`
  (→ galop, têtes de blocs), la statistique « mots longs » du shard
  (→ par segment dans le `.gmap`), les vagues du plan (→ une seule),
  le compte des restes courts (→ présumés). Fausses pistes mesurées :
  découpe en sous-plages (le tri reste), verrou de la mémo, `.termtexts`
  multi-générations.
- **Mesurer une compaction** sans reconstruire l'index : le test ignoré
  `dictionary_compact::compaction_of_an_index_on_disk` lie en dur les
  `dict-*` d'un index dans un répertoire de travail et y compacte toutes
  les générations vivantes, chronométré, avec le pic de RAM anonyme
  (`RssAnon` échantillonné) et `VmHWM` (qui compte les fichiers mappés).

  ```bash
  LUCIVY_DICT_BENCH_DIR=/chemin/idx90k-dict2 LUCIVY_DICT_BENCH_OUT=/chemin/compact \
    LUCIVY_DICT_BENCH_MODE=stream V3_PROFILE=1 \
    cargo test --release --lib -- compaction_of_an_index_on_disk --ignored --nocapture > out.txt 2>&1
  # MODE=naive : la reconstruction d'avant (12,8 Go sur le noyau : free -g avant, jamais en parallèle)
  # MODE=compare : les deux puis les fichiers octet pour octet (pic RAM = le naïf)
  ```

  Baselines (champ contenu) : 30 000 fichiers, 7 générations — flux 7,2 s
  contre naïf 13,0 s ; noyau, 2 générations (902 + 21 Mo) — flux **18,9 s,
  229 Mo anonymes**, naïf **48,0 s, 12,8 Go**. Le profil (`V3_PROFILE=1`)
  imprime `[dict] compaction gen … : parts -> keys (merged), texts | fst ms
  | texts ms` à chaque compaction, y compris pendant une construction.
- **Deux mesures sur les postings** (`src/suffix_fst/postings_measure.rs`,
  tests ignorés, `LUCIVY_POSTINGS_DIR=<index d'un shard>`,
  `LUCIVY_POSTINGS_MAX_FILES=N` pour un échantillon) :
  `postings_without_byte_spans` ré-encode chaque `.sfxpost` /
  `.word_sfxpost` dans son layout et dans le layout à positions seules
  (`SFP5` / `WSP5`) — sur un index d'avant le 5 septembre au soir, la
  différence est ce que les spans pesaient (noyau : 842 Mo, 37 % des
  postings, 15 % de l'index) ; sur un index d'après, les deux tailles se
  confondent et l'écrivain est montré reproduire le fichier.
  `byte_spans_are_derivable` recalcule l'offset de chaque position par
  somme cumulée indépendante et vérifie : le `byte_at` du `PMP4` à chaque
  position (`posmap_bad`), **chaque posting de mot contre les chunks sous
  lui** — le texte de son ordinal, mot ou queue, doit être ce que les
  chunks tiennent à partir de `tail_off` (`word_text_bad`, c'est ce qui
  contrôle les entrées de queue sans span stocké) — et, sur un segment
  encore à spans, les spans stockés contre la somme et la méta comme avant
  (c'est ce test qui a trouvé les trois queues de mots chinois décalées,
  [04](04-progression-et-a-faire.md) §2). 0 partout attendu.
  Lancer : `cargo test --release --lib <nom> -- --ignored --nocapture`.
- **Les fichiers dérivés** (`src/suffix_fst/derived.rs`) :
  `rebuild_matches_the_collector` (unitaire, corpus synthétique) et le test
  ignoré `derived_files_match_the_index` — `LUCIVY_DERIVED_DIR=<index d'un
  shard> cargo test --release --lib derived_files_match_the_index --
  --ignored --nocapture` : chaque segment rebâti et comparé octet pour
  octet aux trois fichiers (le fichier sur disque moins le pied du
  répertoire, `Footer::extract_footer`). `lucivy_core/tests/
  test_derived_in_ram.rs` : l'option de bout en bout. Le harnais construit
  un index avec l'option sous `V3_DERIVED_IN_RAM=1` (entre dans
  `.v3_shape`) ; `LUCIVY_VERBOSE=1` trace chaque rebâti (`[derived] segment …
  rebuilt … in N ms`).
- **La compatibilité 3.0.8** : `cargo test --release -p lucivy-core --test
  test_compat_308` sur la fixture `lucivy_core/tests/fixtures/index-3.0.8/`
  (construite par le wheel PyPI : `build.py`, venv `uv venv v && uv pip
  install --python v/bin/python lucivy==3.0.8`, `pip` n'est pas installé
  sur la machine). Ne pas la rebâtir sans raison : `panel-3.0.8.json` est
  la référence, et une fixture 3.0.8 pèse ×45 le texte (18 documents →
  7 Mo).
- **Les vérités `contains` et `coherence`** ne sont **pas** ignorées :
  `cargo test --release -p lucivy-core --test test_sfx_v3_ground_truth
  v3_ground_truth_contains -- --exact --nocapture` (sans `--ignored`, qui
  les filtre : « running 0 tests », vu le 5 septembre au soir).
- **Une construction qui compacte beaucoup**, pour la vérité de bout en
  bout : `V3_SFX_VERSION=4 V3_COMMIT_EVERY=500 LUCIVY_DICT_MAX_GENERATIONS=3`
  devant le harnais sur le corpus de référence (5 000 fichiers sans
  `V3_MAX_DOCS` : le harnais n'en prend pas 10 000 par défaut, mettre
  `V3_MAX_DOCS=10000` pour la référence), `V3_INDEX_DIR` neuf — six
  compactions, puis le panel, `contains` et `coherence` sur le même
  index (9/9, 15/15, 31/31 le 5 septembre, [01](01-journal-session-5-septembre.md) §13 bis).
- **Dans le navigateur** (`bash bindings/emscripten/build.sh`, puis
  `cd playground && node serve.mjs`, port 9877) : `?dict` crée l'index
  avec `shared_dictionary: true`, `?commit=N` commit tous les N fichiers
  (2 000 sinon ; à 1 000 sur `?corpus=corpus-kernel-16k.tar.gz` on passe
  par deux compactions), `?merges=N` fusions de fond à la fois (2 par
  défaut pour un index à dictionnaire, 1 en v3), `?verbose` pour les
  traces du moteur dans `diag.log` (`[merge] N segments: waited … ran …`,
  `[preload] waited for merges`), la page journalise le pic de mémoire
  WASM après l'indexation (`memoryStatus().heap_bytes`), `?corpus=<archive>` indexe une `.tar.gz` servie
  à côté de la page, `?nodemo` saute la démo. **Un seul onglet à la
  fois** : deux onglets qui indexent écrivent le même répertoire OPFS
  `user_index` et échouent tous les deux au premier commit (`I/O error`,
  code 29). Les logs : le panneau « Logs » de la page (le ring buffer)
  et `playground/diag.log` côté serveur. Le 5 septembre : 15 440 fichiers
  du noyau en mode dictionnaire, 16 commits, deux compactions, mêmes
  comptes que v3 sur la démo ([04](04-progression-et-a-faire.md) §1).
- Ce qui reste ([01](01-journal-session-5-septembre.md) §11) : la regex à
  ×1,6, la DFS de fratrie, `index_bytes` / `preload` / `residency` sans
  les `dict-*`.

---

## 7 bis. Le navigateur : construire, mesurer, rejouer

```bash
bash bindings/emscripten/build.sh            # ~1 min ; recopie pkg/ et js/ dans playground/, estampille index.html
cd playground && node serve.mjs 9877         # COOP/COEP, no-store, journal moteur dans playground/diag.log
```

URL : `http://localhost:9877/?dict` (démo, dictionnaire), `?dict&ram`
(plus `derived_in_ram` : pas de `.posmap` / `.word_pos_map` / `.sibling_v3`
dans l'OPFS, rebâtis à l'ouverture),
`?corpus=corpus-kernel-16k.tar.gz&dict&commit=1000` (15 440 fichiers du
noyau, 16 commits, deux compactions), `&merges=N`, `&threads=N`,
`&wthreads=N`, `&verbose` (traces `[merge]`, `[preload]`, `[fs]` — inonde
`diag.log`), `?open=user_index` (rouvre l'index persistant sans réindexer :
la passe **froide**). **Un seul onglet qui indexe à la fois.** Lignes utiles
de `diag.log` : `[lucivy-wasm] merge concurrency N`, `[merge] N segments:
waited … ran …`, `[preload] waited for merges …` puis `[preload] N files …`,
`[search] done: search Xms …` (le temps du moteur, à comparer à ce que la
page affiche). La page journalise `[playground] indexed N files in Ns; wasm
memory high-water mark N MB` dans la console (et le panneau « Logs »).

**Rejouer des requêtes dans la page** par le serveur de debug (JS évalué
sur le thread principal ; un appel rend en quelques secondes au plus, donc
on lance une IIFE qui pose son résultat dans `window._x` et on l'interroge) :

```bash
# le panel de 21 requêtes (playground/parity_panel.json) : compte, top-10, ms
python3 -c 'import json;print(json.dumps({"js":open("playground/parity_run.js").read()}))' > /tmp/req.json
curl -s localhost:9877/eval/main -d @/tmp/req.json          # → "started"
sleep 3; curl -s localhost:9877/eval/main -d '{"js":"window._parityResult"}' > /tmp/parity.json
# une requête et le drapeau de troncature
curl -s localhost:9877/eval/main -d '{"js":"window._fz=null;(async()=>{const h=await window._playground.search({type:\"contains\",field:\"content\",value:\"kmalloc\",distance:2},{limit:100000});const st=await window._playground.memoryStatus();window._fz=JSON.stringify({count:h.length,truncated:st.last_search_truncated,heap_mb:st.heap_bytes>>20})})();1"}'
sleep 3; curl -s localhost:9877/eval/main -d '{"js":"window._fz"}'
# simuler la frappe (le chemin exact de la page, rendu compris) et lire l'en-tête
#   q=document.getElementById('query'); q.value='include'; q.dispatchEvent(new Event('input',{bubbles:true}));
#   … 1 s plus tard : document.getElementById('resultsHeader').textContent
```

**Les règles de `/eval/main`, apprises à la dure le 5 septembre** : (1) le
résultat revient **toujours en chaîne** (`{"result":"true"}`, `"2"`,
`"[…]"`) — comparer à `'true'`, `json.loads` pour un objet ; une attente
qui compare à un booléen boucle sans fin ; (2) chaque appel coûte ~1 s (la
page interroge le serveur toutes les secondes) et est coupé à 30 s (`error:
"timeout"`) — tout travail plus long part dans une IIFE `async` qui pose
son résultat dans `window._x`, qu'on interroge ensuite ; (3) le corps est
du JSON `{"js": …}` : construire avec `json.dumps` (Python, `urllib`) plutôt
qu'à la main dans le shell, où les guillemets se perdent ; (4) piloter le
terminal de la démo = poser `.term-input.value` et dispatcher un
`KeyboardEvent('keydown', {key: 'Enter', bubbles: true})`, puis relire
`[...document.querySelectorAll('.term-line')].map(l => l.textContent)`. Le
pilote du jour, `term-drive.py` (scratchpad) : `python3 -u term-drive.py
"index list" "sleep 5" "open mdn" "sleep 15" 'lines $ lucivy index list'`
— chaque argument est une commande du terminal, `sleep N`, ou `lines <préfixe>`
qui imprime le terminal depuis cette ligne ; il attend le prompt avant
chaque envoi (`!!document.querySelector('.term-input')` → `'true'`).

`window._playground` expose `search(query, opts)`, `memoryStatus()`,
`numDocs()`, `doSearch`, `buildQuery`. Le 5 septembre le panel a servi à
comparer dictionnaire et v3 sur 15 440 fichiers (mêmes comptes, même ordre
de temps, [04](04-progression-et-a-faire.md) §1 ter) et à trouver les pics
dus à `memoryStatus` (§1 quater). Les JSON des passes sont dans le
scratchpad (`parity-dict-cold.json`, `parity-v3-cold.json`, …), tabulés par
`parity-table.py` (un script de 15 lignes : charge chaque JSON, imprime
nom / compte / ms par colonne — à réécrire au besoin).

---

## 8. Le scratchpad de la session (à recréer sinon)

`/tmp/claude-1000/-home-lucied-git-workspaces-lucivy/de715c8d-…/scratchpad/` :
`idx-dict2` (10 000, dictionnaire, générations), `idx30k-v7` (v3 au
format courant, 1,6 Go), `idx30k-dict` (GMAP), `idx30k-dict2` (**GMP2**,
1,3 Go), `idx90k-v8` (v3 au format courant, 7,3 Go), `idx90k-dict`
(GMAP), `idx90k-dict2` (**GMP2**, 5,6 Go, générations 10 et 11 — le
banc de compaction lie ses `dict-*` en dur), `idx10k-v3-pmp4` /
`idx10k-dict-pmp4` (10 000, posmap `PMP4`, postings encore à spans —
l'étape 1), `idx10k-v3-sfp5` / `idx10k-dict-sfp5` (**la référence 10 000
du format courant** : `SFP5`, `WSP5`, `PMP4`), `idx30k-dict4` (PMP4 +
SFP4), `idx30k-dict5` et `idx30k-v3-sfp5` (**30 000 au format courant**,
les A/B de temps), `idx90k-dict-sfp5` (noyau au format courant),
`idx30k-dict-ram` et `idx90k-dict-ram` (les mêmes avec `derived_in_ram` :
sans `.posmap` / `.word_pos_map` / `.sibling_v3`),
`idx30k-dict3` (30 000,
dictionnaire, **collecteur corrigé** : la référence pour
`byte_spans_are_derivable`, 0 désaccord), `idx10k-dict-compact` (5 000
fichiers, commit tous les 500, six compactions), les scripts `run-ab-*.sh`,
`run-suite-and-ab.sh`, `run-compact-*.sh`, `run-derivable.sh`,
`run-parity.sh`, `parity-table.py`, les sorties `abplan*-*.txt`,
`ab90k-*.txt`, `prof30k-*.txt`, `gt90k-*.txt`, `postings*.txt`,
`derivable*.txt`, `compact30k-*.txt`, `compact90k-*.txt`, `parity-*.json`.
Index au format de `main` encore présents : `/tmp/lucivy-idx-90k` (18 Go,
non compacté) et `~/lucivy_bench/lucivy_bench_sharding/single` (11 Go,
10 segments). Rien d'irremplaçable.

---

## 9. Le protocole d'une étape

1. Le changement, avec le lecteur qui ouvre **encore** l'ancien layout.
2. `cargo test --lib`, `test_dictionary_index`.
3. Référence 10 000 : `9 pass`, scan, comparer au précédent.
4. Rouvrir l'index de référence **précédent** : `index reused` + `9 pass`.
5. 30 000 : A/B au même binaire, 3 passes, machine au repos.
6. `cargo test -p lucivy-core` (pas pendant les A/B).
7. Journal, un commit par étape, en français, sans trailer ; `push v4`.

---

## 10. Pièges rencontrés le 5 septembre

- Le tri d'une liste de 533 000 candidats (`sort_by_key` stable, clé
  tuple) coûtait plus que son scan ; trier par parties puis tri stable
  adaptatif divise par trois — et la bonne réponse était de ne pas la
  matérialiser.
- Une mémo partagée sérialise ce que le premier segment demande ; les
  attentes coopératives imbriquées sous un producteur se pompent entre
  elles — ne jamais attendre sous une cellule.
- Une marche fusionnée liste × `.gmap` coûte le `.gmap` entier par liste :
  galoper depuis le côté le plus petit.
- `V3_MAX_DOCS` s'applique à **chaque** test d'un fichier de vérité.
- `cargo test --test X a b` : un seul filtre par appel ; deux noms = erreur.
- `grep --include` échoue dans ce shell ; `echo ====` aussi.
- Un `python3 - <<'EOF'` qui échoue à mi-chemin sur un `assert` a déjà
  écrit les fichiers d'avant : vérifier avec `grep -c` ce qui est passé.
- Un test qui persiste son index dans `V3_INDEX_DIR` (le fuzzy à cheval)
  écrase l'index de référence.
- Le harnais sans `V3_MAX_DOCS` prend **5 000** fichiers, pas 10 000
  (`.v3_shape` le dit).
- Deux onglets du playground qui indexent en même temps échouent tous
  les deux au premier commit (même répertoire OPFS).
- `lucivy.js` filtre les options d'initialisation : une option ajoutée au
  worker n'arrive pas au moteur tant qu'elle n'est pas dans sa liste ;
  chercher la ligne `[lucivy-wasm] …` dans `diag.log` pour prouver qu'un
  drapeau est passé.
- Un temps affiché par la page peut être un temps d'attente derrière un
  autre message du worker (le `memoryStatus` d'après la recherche
  précédente) : toujours comparer au `[search] done: search Xms` du
  moteur avant d'accuser une requête.
- Le JSON rendu par `/eval/main` est une chaîne échappée : `grep '"count"'`
  ne la trouve pas, `grep count` oui. Et `pkill -f <script>` tue aussi le
  shell qui porte le nom du script dans sa ligne de commande.
- `?verbose` inonde `diag.log` de lignes `[fs]` : filtrer par `grep`.
- `for p in $(pgrep -f motif); do kill $p; done` tue aussi le shell qui
  porte `motif` dans sa ligne de commande (deux fois le 5 septembre, code
  144) : filtrer sur le nom du programme, ou ne pas tuer.
- Pour un dernier jeton qui est un **mot**, son texte commence à son
  **premier** chunk : `last_position` (fin du span, l'adjacence) n'est pas
  le début de ses octets. Le placement des spans l'a appris par la vérité
  (`mutex lock` relâché : `[5441..5456]` pour `[5441..5451]`), pas par les
  1 460 tests unitaires — la vérité terrain sur corpus reste le juge.
- Un seuil converti d'octets en positions garde la même valeur pour rester
  un sur-ensemble (une position ≥ 1 octet), mais un **jeu** de séparateurs
  en octets ne se convertit pas ainsi : 32 octets de séparateurs tiennent
  dans 5 positions, et 32 positions regroupaient quatre fois trop de texte
  (fuzzy d2 : DP 515 → 903 ms).
- `--ignored` filtre les tests non ignorés : `v3_ground_truth_contains` et
  `_coherence` se lancent sans.
- Le terminal de la démo accepte au prompt `index mdn` / `index kernel`
  (corpus servis à côté de la page, `playground/corpus-*.tar.gz`, ignorés
  par git), `index github owner/repo[@branch]` (par le proxy, refusé
  au-delà de ~220 Mo de texte), `index list`, `open <nom>`, `drop <nom>` ;
  l'index ouvert vit en RAM, les autres en OPFS (`lucivy/<chemin>`),
  registre `localStorage.lucivy_corpora`.
