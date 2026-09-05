# Knowledge dump — baselines, tests, outils, pour la session suivante (nuit du 5 au 6 septembre, complété le 6 au matin)

Complète [08](08-knowledge-dump-baselines-tests-outils.md) (harnais,
tailles, A/B, tests, fixture 3.0.8, scratchpad, pièges), toujours valable ;
ici ce qui s'est ajouté dans la nuit. Toujours : `export PATH="$HOME/.cargo/bin:$PATH"`,
sortie dans un fichier puis `grep`, jamais `| tail`, le shell de l'outil est
`fish`, pas de `pip` (`uv` oui).

---

## 1. État

`v4` poussé, 4.0.0 non publié, `main` = `8301b55`. Tag `stable-avant-fuzzy-fenetres`.
Jamais de tag `v*` sans le feu vert de Lucie (`release.yml`, `PUBLISH_ENABLED`).
Le conteneur Docker `lucivy-es` (Elasticsearch 8.19, port 9200, 8 Go) tourne
encore : `docker rm -f lucivy-es`. Pas de `gh` sans son accord.

## 2. Baselines de la nuit (noyau moderne, 93 983 fichiers, 857 Mo, sauf mention)

| quoi | valeur |
|---|---|
| indexation natif, index neufs, machine au repos | v3 **56 s** (6 629 Mo) · dictionnaire **131 s** (4 937) le 5, **106,8 s** (4 928) le 6 avec le repli différé · + `derived_in_ram` **134 s** (3 344) le 5, **110,9 s** (3 334) le 6 · 3.0.8 : 122 s |
| 30 000 fichiers | v3 15,4 s · dict 31,3 · dict sans compaction 29,4 · dict 3 commits 26,8 |
| Elasticsearch 8.19 | standard 781 Mo / 28 s · trigrammes + `wildcard` 3 082 Mo / 123 s |
| tantivy 0.25 | défaut 612 Mo / 1,3 s · trigrammes 680 Mo / 4,9 s |
| sous-chaîne pure (docs égaux) | ES 3-8 ms · lucivy 12-15 · tantivy vérifié 107-151 |
| où les questions diffèrent | relâché 9 552 vs 6 577 / 6 601 · `spinlokc` d2 10 034 vs 3 549 / 6 557 · regex 5 510 vs 5 440 / 0 · `de` 93 009 vs 0 / 0 · phrase floue 14 449 vs 14 446 |
| positions (`mutex_lock`, 5 145 docs) | lucivy 20 797 spans 15 ms · ES 200 docs 179 ms · tantivy 200 docs 96 ms |
| poids des fichiers SFX (dict, 4 259 Mo) | sfx 23 % · sfxpost 18 · word_pos_map 15 · word_sfxpost 15 · posmap 12 · sibling_v3 10 · termtexts 7 |
| navigateur, 2.6.0 (14 032 fichiers) | 28 s / 1 087 Mo (commits 2 000) · 41 s / 2 023 Mo de pic (commits 8 Mo) · natif 23 s / 905 Mo |
| navigateur, les douze corpus | [04](04-progression-et-a-faire.md) §2 bis (TypeScript 39 044 en 33 s, MDN 14 s, Go 19, Godot 19→30, PostgreSQL 10, CPython 10, Git 5, curl 3, Redis 2, SQLite 2, nginx 1) |
| `?ram` navigateur | noyau : OPFS 1 571 → 1 159 Mo, pic indexation 3 335 → **3 859**, repos 2 803 → 3 055 ; MDN 478 → 369, pic 1 646 → 1 906 |

## 2 bis. Le chantier indexation du 6 au matin (repli différé) — baselines

Voir `04` §2 sexies pour le récit et `11` §2 pour le mécanisme.

| quoi | valeur |
|---|---|
| 30 000 fichiers, dictionnaire, index neuf | 32,2 s la veille → **23-24 s** (v3 15,2-15,3 ; trois A/B alternés, `ab-fold-*.txt`) |
| pic RSS du harnais sur ces constructions | v3 6 006 Mo ; dictionnaire 6 044-6 419 selon l'étape (bruit ±3 %, jamais une marche) |
| chemin par jeton (`LUCIVY_VERBOSE`, cumul sur les fils) | 14,97 M `lookup_or_mint` : 46 s → 35 s ; FST 32 → 28 ; verrou 6,7 → 4,9 (16 tranches) |
| commit, cumul sur 15 commits | écriture des générations 8,8 s → **0** (paires nommées en 40-300 ms) ; compaction 3,4 → en fond ; réouverture 1,4 → en fond |
| fusion en flux d'un span | 1,2 µs la clé FST, passe textes = un quart ; 950 000 textes en 0,75-0,9 s |
| requêtes du panel après repli | inchangées (2,6-4,9 ms exactes, fz2 155, rx 19,6) ; **avec les paires visibles : 19-36 ms** — la fenêtre que la recherche n'expose jamais par défaut |
| filtre de Bloom (clé d'internement) | 6,46 M des 6,6 M marches sautées, FST 28,4 → 20,6 s cumulées, mur natif égal ; ids sans décoder les textes : 30 000 → **23,0 s** ; Chrome 2.6.0 **40 s / 2 023 Mo**, Godot 30 s / 1 766 |
| navigateur, build du matin, pages fraîches, commits 8 Mo | 2.6.0 : FST par segment + fond **2 279 Mo** / 42 s ; + repli synchrone 2 279 / 44 ; **sans FST par segment (chemin d'avant) 2 023 / 42** = la veille ; Godot 1 894 / 36 avec, **1 766 / 31 sans**, référence 1 778 / 30 ; noyau 15 440 : 75 s, 1 902 Mo |

Compteurs : `LUCIVY_VERBOSE=1` imprime `[dictionary] commit: …` (textes
neufs, paires nommées, temps, et depuis le dernier commit : appels, frappés
en génération, en attente, mintés, FST, ouverture, verrou) et
`[dictionary] fold: …` (génération, paires, textes, temps, compaction,
paires encore en attente) ; `V3_PROFILE=1` imprime `[dict] compaction …`
avec la passe FST et la passe textes. `sum-dict.py <log>` du scratchpad
additionne les lignes de commit.

## 3. Outils ajoutés

```bash
benches/compare_engines.sh /tmp/lucivy-cmp-90k /chemin/travail   # ~10 min si les index lucivy existent (liens symboliques OK)
python3 benches/compare_engines_report.py /chemin/travail > compare_engines.md
python3 playground/tools/build_corpus.py all|mdn linux …          # --dry-run compte ; cache ~/.cache/lucivy-corpora
CMP_CORPUS=… CMP_OUT=out.json cargo test --release -p lucivy-core --test compare_tantivy compare_tantivy -- --ignored --nocapture
ES_URL=http://localhost:9200 python3 benches/compare_elasticsearch.py /tmp/lucivy-cmp-90k   # /tmp/es_compare.json
```

Le playground : `?commitmb=M` (8 par défaut), `?ram`, `?dict`, `index list`
/ `open` / `drop`, `Lucivy.dropIndex(path)`. Piloter le terminal depuis
`javascript_tool` : [08](08-knowledge-dump-baselines-tests-outils.md) §7
(module, `.term-input`, sanitize la sortie, 45 s par appel).

## 4. Tests et scripts de vérité inchangés

`cargo test --lib` 1 461 verts ; `test_compat_308`, `test_derived_in_ram`,
`test_dictionary_index`, `test_federated_search` (union = index unique **et**
scores égaux — c'est la preuve du pilier 5), `test_filtered_search_truth`,
`test_luce_v3_roundtrip` ; le harnais `v3_ground_truth_demo` avec
`V3_QUERIES` pour les cas sur mesure (`retur\s-ENOMEM:fz1`, `de:strict` avec
`LUCIVY_HIGHLIGHT_SPAN_CAP=0`). **Depuis le 6 au soir le panel fait 10/10** :
la ligne `jw1` (Jaro-Winkler) est vérifiée par `grep_spans_jaro` avec la
définition partagée `jaro_spans` (10 000 : 228 documents, 876 spans ;
30 000 dictionnaire : 707, 2 284 ; noyau 94 000 dictionnaire : 5 196, 18 824, 72,7 ms) ; sa vérité coûte 3,4 s / 12 s / 66 s de balayage.
**Compat 3.0.8 avec un index de `main`** : `run-main-compat.sh` (worktree
`wt-main`, `CARGO_TARGET_DIR=target-main`, index `idx10k-from-main`, puis
ajouter ` sfx=3` avant ` v=10` dans `.v3_shape` pour que le harnais v4 rouvre
au lieu de rebâtir) : 10/10 le 6 au soir. Bindings : Node `test.mjs` + `tests/*.mjs` (`smoke_warnings.mjs` prend le
chemin du module en argument : `node tests/smoke_warnings.mjs ../index.js`),
Python `pytest tests` (10 min, 112 verts), `cargo test -p lucivy-cpp`.

## 5. Le scratchpad de la nuit

`compare/` (le banc : logs, JSON, `compare_engines.md`, liens `dict`,
`dict-ram` → index du noyau), `idx90k-dict-fresh`, `idx90k-dict-ram-fresh`
(rebâtis pour le temps), `idx30k-{v3-t,dict-t,dict-nocompact,dict-c10k}`,
`idx26-dict`, `idx26-v3`, `corpus-linux-2.6.0/` (extrait), `browser-ram.md`
(toutes les mesures navigateur, panels compris), `run-*.sh` et leurs `.out`.
Rien d'irremplaçable ; `compare_engines.md` est copié dans
`docs/compare-engines-2026-09-05.md`.

## 5 bis. Le scratchpad du matin

`run-rss.sh <tag> <dict|v3>` : une construction 30 000 à neuf, non
verbeuse, avec le pic RSS relevé par `VmHWM` (pas de `/usr/bin/time` sur la
machine) ; `rss-<tag>-<mode>.txt`. `instr-30k-dict-{1..8}.txt` : les
constructions verbeuses de chaque étape. `run-fold-measure.sh` : trois A/B
puis le noyau puis le build WASM ; `ab-fold-*.txt`, `index-time-dict-fold.txt`,
`idx90k-dict-fold/` (index noyau à neuf, réutilisable par le banc).

## 6. Pièges de la nuit

- **`pkill -f motif` tue le shell qui porte le motif** (exit 144) : `for p in
  $(pgrep -f 'moti[f]'); do kill $p; done`.
- L'outil Bash tue un `run_in_background` au bout de 600 s : lancer les longs
  bancs par `nohup … & disown` et surveiller le fichier de sortie
  (`Monitor` avec `tail -f | grep --line-buffered | sed -u '/fin/q'`).
- `javascript_tool` : un `await` > 45 s échoue ; la sortie qui ressemble à
  une query string ou un cookie est **bloquée** (`[BLOCKED]`) — ne renvoyer
  que du texte assaini `[A-Za-z0-9 .,:;()_-]`, jamais `innerText` brut.
- Une `PhraseQuery` tantivy d'un seul terme panique ; son `NgramTokenizer`
  met toutes les positions à 0 ; `Value` doit être importé pour `as_str()`.
- Elasticsearch : `took` d'une requête déjà vue peut être un hit de cache ;
  la `fuzziness` compte une transposition pour une édition (Levenshtein :
  deux) — `retrun` rend 0 des deux côtés, `retur` est la bonne faute.
- La page Python `.replace(",", " ")` du rapport : les nombres portent
  une espace fine insécable, voulue.
- Les temps d'indexation de référence vieillissent : « ~255 s » du 08 était
  d'avant la compaction en flux ; remesurer à neuf avant de publier un temps.
- **Le cumul sur les fils n'est pas le mur.** Les compteurs du chemin par
  jeton faisaient 46 s cumulées pour 30 s de mur : ce sont les fils des
  collecteurs, en parallèle du flux. Le mur était le commit (une seule
  génération par shard, en série). Toujours distinguer les deux avant
  d'optimiser.
- Un cache avec éviction « au budget » qui rebalaye toute la table à chaque
  insertion au-dessus du budget est quadratique : 110 s au lieu de 30
  (278 s de verrou). Évincer en bloc, amorti, ou pas du tout.
- Le harnais `v3_ground_truth_demo` prend `handle.reader.searcher()` en
  direct : il ne passe **pas** par `ShardedHandle::search`, donc pas par
  l'attente du repli. Il mesure ce que le lecteur voit ; ce qui lui donne le
  bon état, c'est `wait_merging_threads` (qui attend le repli et son
  `meta.json`) puis `reload`.
- `SfxDictionaryMeta::pair_files` nomme la paire pour **chaque** champ ;
  un segment n'écrit une paire que pour les champs où il a minté. Une
  lecture qui traite un fichier manquant comme une perte (le snapshot LUCE)
  doit tolérer `.newsfx`/`.newtexts` absents.
- **Dans WASM, un travail parallèle de plus se paie en pic, pas en temps** :
  huit constructeurs de FST vivants en même temps (un par segment en cours)
  ont coûté +256 Mo sur la 2.6.0 pour 0 s gagnée. Tout ce qui recouvre les
  constructions de segments doit être mesuré dans Chrome avant d'être gardé
  sur wasm32, et `cfg!(target_arch = "wasm32")` est le bon garde-fou.
- **Un gain en cumul de fils n'est un gain que si ces fils font le mur.**
  Le filtre a rendu 8 s de FST cumulées et 0 s de mur natif. Avant
  d'optimiser un chemin parallèle, vérifier qu'il borne quelque chose.
- Un filtre de Bloom se clé sur ce qui rend l'entrée unique (ici texte +
  forme), pas sur la clé de la structure qu'il protège quand celle-ci est
  partagée (la clé FST : 1,6 M sautées contre 6,46 M).
- Une hypothèse de pic mémoire se teste une variable à la fois : le repli
  synchrone seul n'a rien rendu (2 279), c'est la seconde variable (les FST
  par segment) qui portait tout.
- Rouvrir un index dont l'écrivain vit encore → `LockBusy` : fermer par
  `writer.take().wait_merging_threads()` d'abord (le test
  `deferred_fold_settles`).
- Une accroche ou un chiffre changé dans le README principal doit être
  reporté le même jour dans les cinq autres README (bindings, core) et sur la
  page : c'est la règle posée cette nuit.
