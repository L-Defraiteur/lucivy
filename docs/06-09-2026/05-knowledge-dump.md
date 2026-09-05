# Knowledge dump — baselines, tests, outils, pièges (6 septembre 2026, pour la suite)

Complète [`../05-09-2026/12-knowledge-dump-baselines-tests-outils.md`](../05-09-2026/12-knowledge-dump-baselines-tests-outils.md)
et `08` (harnais, tailles, A/B, fixture 3.0.8, scratchpad), toujours valables.
Toujours : `export PATH="$HOME/.cargo/bin:$PATH"`, sortie dans un fichier puis
`grep`, jamais `| tail`, `free -g` avant un gros run, une construction 90k à
la fois, un seul onglet qui indexe.

## 1. État

`v4` = `main` = dernier commit ; **4.0.0 et 4.0.1 sur PyPI, npm (7 paquets),
crates.io (5)** ; release GitHub `v4.0.1` avec 12 artefacts ; page déployée.
`gh` actif : L-Defraiteur (Lucie rebascule lundi sur le compte pro). Le
conteneur `lucivy-es` tourne encore ; le serveur du playground sur 9877.

## 2. Baselines du 6

| quoi | valeur |
|---|---|
| 30 000 fichiers, index neuf (commit tous les 2 000) | v3 **15,2-15,4 s** · dictionnaire **23,0-23,6 s** (32,2 la veille) ; index 1 445 / 1 125 Mo |
| noyau 93 983, commit tous les 10 000 | dictionnaire **106,8 s** (131), 4 928 Mo · + `derived_in_ram` **110,9 s** (134), 3 334 Mo · v3 56 s |
| pic RSS du harnais, 30 000 | v3 6 006 Mo ; dictionnaire 6 044-6 419 selon l'étape (bruit) |
| chemin par jeton, cumul sur les fils | 14,97 M `lookup_or_mint` ; 46 → 35 s ; FST 32 → 28 (20,6 avec le filtre, mais frappés 7 M × 2,9 µs) ; verrou 6,7 → 4,9 |
| commit, cumul 15 commits | nommage 2,0 → ~0,5 s (ids sans décoder) ; écriture 8,8 → 0 (fond) ; compaction 3,4 → fond ; attente finale 2,6 s |
| fusion en flux | 1,2 µs la clé FST ; passe textes = un quart ; 950 000 textes en 0,75-0,9 s |
| filtre de Bloom | 6,46 M des 6,6 M marches pour rien sautées ; 10 bits/clé ; mur natif égal ; Chrome 2.6.0 40 s / 2 023 Mo, Godot 30 / 1 766 |
| panel 30 000 dictionnaire | exactes 2,6-4,6 ms, fz1 9,9, fz2 159, rx 20,2, **jw1 11,4** (v3 : 2,2-4,1, 13,0, 125, 12,6) |
| panel noyau dictionnaire (froid) | fz1 98 ms, fz2 772, rx 242, **jw1 72,7** ; v3 (banc, chaud) fz1 46, fz2 573, rx 112 |
| Jaro-Winkler vérifié | 10 000 : 228 docs / 876 spans ; 30 000 : 707 / 2 284 ; noyau : 5 196 / 18 824 (fz1 : 18 825) ; balayage 3,4 / 12 / 66 s |
| Chrome, pages fraîches, commits 8 Mo | 2.6.0 : **2 023 Mo / 42 s** (FST par segment + fond : 2 279 / 42 ; repli synchrone seul : 2 279 / 44) ; Godot **1 766 / 31** (1 894 / 36 avec) ; noyau 15 440 : 1 902 Mo / 75 s |
| compat main | index 3.0.8 de 10 000 fichiers : 160 segments, `.bytemap`, 1 133 Mo, 11,6 s ; rouvert par v4 : 10/10, 0,0 s |
| OPFS | Chrome 60 % du disque par origine, Firefox min(10 % disque, 10 Gio) par site, Safari 60 % et effacement à 7 jours ; budget de la page = min(8 Gio, quota/2) → 5,9 Go sur la machine de Lucie |

## 3. Tests

`cargo test --lib` 1 464 verts ; `cargo test -p lucivy-core --no-fail-fast`
**40 suites** ; `cargo test --lib --no-default-features` 1 430 (le job
`test-minimal`) ; `cargo clippy --lib -- -D warnings` et sur les quatre
crates ; Node `npm run build` puis `node test.mjs` et `tests/*.mjs`
(`smoke_warnings.mjs` prend `../index.js` en argument) ; Python `bash
build.sh` puis `pytest tests` (112 verts, 4 sautés, 10 min à machine
chargée, 1 min au repos) ; `cargo test -p lucivy-cpp`. Nouveaux :
`deferred_fold_settles`, `reopened_writer_mints_no_duplicate_ids`, tests
`jaro_spans_*`, `dictionary_bloom` ; le panel `v3_ground_truth_demo` fait
**10/10** (jw1 vérifiée). Les tests de bindings créent leur index de
référence avec `shared_dictionary: false` ; les tests `lazy` aussi.

## 4. Outils

```bash
LUCIVY_VERBOSE=1 … v3_ground_truth_demo           # [dictionary] commit: … / fold: … ; sum-dict.py <log> additionne
V3_PROFILE=1 …                                     # [dict] compaction … fst … ms | texts … ms
run-rss.sh <tag> <dict|v3>                         # 30k à neuf + pic RSS (VmHWM ; pas de /usr/bin/time ici)
run-main-compat.sh                                 # worktree main, harnais à part, index 10k, puis .v3_shape += " sfx=3"
LUCIVY_DICT_WAIT=0 / LUCIVY_DICT_SYNC_FOLD=1 / LUCIVY_DICT_MAX_PENDING=N / LUCIVY_DICT_MAX_GENERATIONS=N
gh run view <id> --json status,conclusion,jobs     # suivre un run ; gh variable list ; gh api …/environments/release
```

Chrome : `javascript_tool` n'attend pas une promesse (renvoyer du synchrone,
attendre avec `computer wait` ≤ 10 s, un `browser_batch` ≤ ~45 s au total) ;
lire le terminal par `#termLines`.children (innerText aplatit) ; le
`[playground] indexed … high-water` dans la console ; la page relit
`meta.json` d'un index OPFS via `navigator.storage.getDirectory()`.

## 5. Scratchpad

`instr-30k-dict-{1..10}.txt` (compteurs par étape), `rss-*.txt`,
`ab-fold-*.txt` (3 A/B), `index-time-dict{,-ram}-fold.txt`, `idx90k-dict-fold`
(index noyau à neuf, réutilisable), `idx10k-from-main` (l'index 3.0.8),
`target-main/` (cibles du worktree, supprimable), `panel-jw-*.txt`,
`browser-ram.md` (toutes les mesures navigateur), `compat-main-v4.txt`,
`final*-*.txt` (chaînes de tests), `ci-failed-clean.log`.

## 6. Pièges du 6

- **Le cumul sur les fils n'est pas le mur** : 46 s cumulées pour 30 s de
  mur ; le mur était le commit. Chronométrer côté mur avant d'optimiser.
- Une éviction « au budget » qui rebalaye la table à chaque insertion est
  quadratique (110 s). Évincer en bloc.
- **Dans WASM, un travail parallèle de plus se paie en pic, pas en temps** ;
  tester une variable à la fois (repli synchrone seul : rien ; FST par segment :
  tout).
- Un filtre de Bloom se clé sur ce qui rend l'entrée unique (texte + forme),
  pas sur la clé FST partagée par toutes les casses et formes.
- Le harnais prend `handle.reader.searcher()` en direct : il ne passe pas par
  l'attente du repli ; c'est `wait_merging_threads` puis `reload` qui lui donne
  le bon état.
- **La course du repli** : lecteur qui relit `meta.json` entre permutation et
  réécriture → dictionnaire sans textes. Garder le dictionnaire tenu s'il est
  en avance (`next_generation`).
- Les ex æquo revenaient dans l'ordre du gestionnaire de segments (hash) :
  tri déterministe dans `save_metas`, et les tests comparent triés.
- L'ouverture paresseuse d'un blob store lit les `dict-*` entiers.
- **Ne jamais tagger sur une CI rouge** : la 4.0.0 est partie ainsi ;
  `release.yml` a maintenant `checks`, mais on regarde `gh run list` avant.
- `#[cfg(feature = "mmap")]` sur tout test qui ouvre un `MmapDirectory`.
- Une relecture extérieure trouve les formulations trop fortes (« does not
  leak », « inexpressible », « identical counts ») : dire exactement ce qui
  est prouvé, dans quelle configuration.
- Le corpus noyau du banc est **Linux v7.2** (`Makefile` 7.2.0, copié le
  28 août), pas un instantané de septembre.
- Il n'y a jamais eu de 3.0.9 ; le job WASM de `release.yml` date de la 3.0.8.
