# Knowledge dump — baselines, tests, outils, pour la session suivante (nuit du 5 au 6 septembre)

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
| indexation natif, index neufs, machine au repos | v3 **56 s** (6 629 Mo) · dictionnaire **131 s** (4 937) · + `derived_in_ram` **134 s** (3 344) · 3.0.8 : 122 s |
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
`LUCIVY_HIGHLIGHT_SPAN_CAP=0`).

## 5. Le scratchpad de la nuit

`compare/` (le banc : logs, JSON, `compare_engines.md`, liens `dict`,
`dict-ram` → index du noyau), `idx90k-dict-fresh`, `idx90k-dict-ram-fresh`
(rebâtis pour le temps), `idx30k-{v3-t,dict-t,dict-nocompact,dict-c10k}`,
`idx26-dict`, `idx26-v3`, `corpus-linux-2.6.0/` (extrait), `browser-ram.md`
(toutes les mesures navigateur, panels compris), `run-*.sh` et leurs `.out`.
Rien d'irremplaçable ; `compare_engines.md` est copié dans
`docs/compare-engines-2026-09-05.md`.

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
- Une accroche ou un chiffre changé dans le README principal doit être
  reporté le même jour dans les cinq autres README (bindings, core) et sur la
  page : c'est la règle posée cette nuit.
