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
cargo test --lib                                   # ld-lucivy : 1 452 verts, 18 ignorés
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
- Ce qui reste ([01](01-journal-session-5-septembre.md) §11) : la
  compaction naïve (prochain chantier), la regex à ×1,6, la DFS de
  fratrie, `index_bytes` / `preload` / `residency` sans les `dict-*`.

---

## 8. Le scratchpad de la session (à recréer sinon)

`/tmp/claude-1000/-home-lucied-git-workspaces-lucivy/de715c8d-…/scratchpad/` :
`idx-dict2` (10 000, dictionnaire, générations), `idx30k-v7` (v3 au
format courant, 1,6 Go), `idx30k-dict` (GMAP), `idx30k-dict2` (**GMP2**,
1,3 Go), `idx90k-v8` (v3 au format courant, 7,3 Go), `idx90k-dict`
(GMAP), `idx90k-dict2` (**GMP2**, 5,6 Go), les scripts `run-ab-*.sh`,
`run-suite-and-ab.sh`, les sorties `abplan*-*.txt`, `ab90k-*.txt`,
`prof30k-*.txt`, `gt90k-*.txt`. Rien d'irremplaçable.

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
