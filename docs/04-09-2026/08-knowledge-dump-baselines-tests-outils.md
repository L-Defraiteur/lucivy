# Knowledge dump — baselines, A/B, tests, outils, pour la session suivante

Tout ce qu'il faut pour reprendre le chantier v4 sans l'historique de la
conversation. Complète `docs/28-08-2026/09-knowledge-dump-tests-benchs-publication.md`
(publication, corpus, pièges de vérification), qui reste valable.

Toujours : `export PATH="$HOME/.cargo/bin:$PATH"` ; sortie dans un fichier
puis `grep`, jamais `| tail` ; `git` parle français sur cette machine
(« objet commit fantôme », pas « dangling commit »).

---

## 1. État du dépôt et des branches

- **`v4`** = `wip/publication-3.0.0` + audit + 8 commits de format + docs,
  **poussée sur `origin/v4`**. C'est la branche de travail.
- `main` = `origin/main` = `8301b55`, trois commits après le tag `v3.0.8`.
- `wip/publication-3.0.0` = `main` + 3 commits du 28 août (bancs de
  comparaison, rapports 06-09), **pas poussés**. À fusionner dans `main`
  un jour ; `v4` est déjà par-dessus.
- 26 commits orphelins vérifiés le 4 septembre : stashes et jumeaux de
  rebase, contenu intégré, rien à récupérer.
- Ne jamais lancer `gh` sans l'accord de Lucie (elle bascule de compte pour
  travailler) ; `git push` de `v4` est autorisé (« hésite pas à push v4 »).

Commits du jour, dans l'ordre (tous sur `v4`) : `30ad3da` parents 8 o ·
`d0bb7d3` plus de bytemap · `2b3f5a8` posmap 3 o · `ff013a4` sibling sans
gap · `84c9c6a` termtexts layout 2 · `3ab5ba6` plus de suffixe d'overlap ·
`03dc617` mesure des parents · `37d6d52` parents delta (conteneur 5).

---

## 2. Les corpus

| chemin | contenu | usage |
|---|---|---|
| `/tmp/lucivy-cmp` | 10 000 fichiers du noyau, 65 Mo | la **référence** du protocole |
| `/tmp/lucivy-cmp-90k` | 93 983 fichiers, 898 Mo, matérialisés | 30 000 premiers pour les A/B, entier pour le chiffre final |
| `/tmp/linux-bench` | clone du noyau | source |
| `~/lucivy_bench/lucivy_bench_sharding/{single,round_robin,token_aware}` | index **v3** de bench (93 605 fichiers, 10 / 42 / 32 segments, 11,56 / 14,09 / 13,71 Go) | l'audit a scanné `single` ; lisibles par le binaire v4 |

Ces chemins sont sous `/tmp` : à reconstruire s'ils ont disparu (voir le
knowledge dump du 28 août, §5, pour le filtre de matérialisation).

---

## 3. Construire un index de référence et lancer le panel vérifié

Le harnais : `lucivy_core/tests/test_sfx_v3_ground_truth.rs`, test ignoré
`v3_ground_truth_demo`. Il indexe en RAM, copie l'index dans `V3_INDEX_DIR`,
écrit un marqueur `.v3_shape` et **réutilise** l'index si les paramètres
(corpus, nombre de fichiers, `V3_COMMIT_EVERY`, fusion, politique)
n'ont pas changé — donc changer le binaire et relancer = **rouvrir** l'index
existant, ce qui est le test de compatibilité.

```bash
# référence 10 000 fichiers (160 segments de 64 docs, ~8 s, 735 Mo en v4)
V3_CORPUS=/tmp/lucivy-cmp V3_INDEX_DIR=/chemin/idx cargo test --release -p lucivy-core \
  --test test_sfx_v3_ground_truth v3_ground_truth_demo -- --ignored --nocapture > out.txt 2>&1

# 30 000 fichiers (120 segments, ~20 s, 2,3 Go en v4) — le corpus des A/B de temps
V3_CORPUS=/tmp/lucivy-cmp-90k V3_MAX_DOCS=30000 V3_COMMIT_EVERY=2000 V3_INDEX_DIR=/chemin/idx30k ...

# le noyau entier (253 segments, ~65 s, 11,06 Go en v4)
V3_CORPUS=/tmp/lucivy-cmp-90k V3_MAX_DOCS=100000 V3_COMMIT_EVERY=10000 V3_INDEX_DIR=/chemin/idx90k ...
```

Le panel : 10 requêtes (`mutex_lock` strict / relax, `spin_lock`, `sched`
term / strict, `printk` sw, `schdule` fz1, `regsiter` fz2,
`spin_lock_[a-z]+` rx, `schdule` jw1 non vérifié), chacune comparée à un
`grep` du disque en **comptes et spans**. Lire :

```bash
grep -o '[0-9]* pass, [0-9]* fail' out.txt          # 9 pass, 0 fail attendu
sed -n '/^Query/,/^[0-9]* pass/p' out.txt            # le tableau, temps « search » par ligne
```

Variables utiles : `V3_QUERIES="schdule:fz1,mutex_lock:strict"` (ne change
pas le marqueur : l'index est réutilisé), `V3_PROFILE=1` (profil par phase :
word walk, sibling DFS, resolve…), `V3_DIAG_FUZZY=1` + `V3_DIAG_FUZZY_MAX=0`
(tous les rejets), `V3_FUZZY_MODE=pieces|pivot|auto`, `V3_DEBUG_QUERY=<texte>`.

**Fusion par le harnais** : `V3_MERGE=1 V3_MERGE_TARGET=n V3_MERGE_GROUP=k`
(progressive ; `V3_MERGE_AT_END=1` pour tout à la fin). Sur le noyau entier
elle **échoue** au-delà de ~16,7 M de termes par fusion (24 bits) — groupes
de 8 comme de 4 ont buté ; à élucider avant de s'en servir.

---

## 4. Mesurer les tailles

```bash
python3 benches/scan_index_size.py /chemin/idx > scan.txt 2>&1          # par fichier, tous segments
python3 benches/scan_index_size.py /chemin/idx <uuid du plus gros segment> # + décodage profond d'un segment
grep -A12 'TOTALS' scan.txt
```

Il lit tous les layouts du jour (conteneur 3/4/5, `PMAP`/`PMP3`,
`SIB2`/`SIB3`, `TTX3` layout 1/2, bytemap absent ou présent) et s'arrête à
la fin réelle des données (les fichiers portent un pied). Le plus gros
segment : `ls -S idx/*.sfx | head -1`. Le script ne compte que les fichiers
SFX ; `du -sb` compte aussi les fichiers tantivy (~50 Mo sur 10 000 fichiers).

Comparer deux scans : le petit script Python du journal (§ de chaque étape)
qui lit les lignes après `=== TOTALS` en `ast.literal_eval`.

Listes de parents par longueur de clé, sur un `.sfx` réel :

```bash
SFX_FILE=/chemin/idx/<seg>.2.sfx cargo test --release --lib measure_parents_by_key_length -- --ignored --nocapture
```

Répétition du dictionnaire entre segments : le script de
[05](05-piste-dictionnaire-partage-par-shard.md) §1 (textes distincts de
`.termtexts` contre somme des ordinaux ; sur 67 M d'ordinaux, hacher plutôt
que garder les chaînes).

---

## 5. Les deux A/B de temps, et lequel choisir

**Au même binaire, entre deux index** — mesure le coût du **format** seul.
Valable quand le lecteur ouvre les deux layouts (c'est le cas de tout ce
qui a été écrit aujourd'hui). Alterner, 3 à 5 passes, prendre min et
médiane :

```bash
for r in 1 2 3; do for idx in idxA idxB; do
  V3_CORPUS=... V3_INDEX_DIR=/chemin/$idx cargo test --release ... > ab-$idx-$r.txt 2>&1
done; done
```

**Par commit, chaque binaire sur son index** — mesure ce qu'un utilisateur
ressent (format + code). Compiler l'ancien commit dans un worktree avec son
propre `CARGO_TARGET_DIR` (une compilation complète, ~5 min) :

```bash
git worktree add /chemin/wt-X <commit>
cd /chemin/wt-X && CARGO_TARGET_DIR=/chemin/target-X V3_CORPUS=... V3_INDEX_DIR=/chemin/idx-X cargo test --release ...
```

Extraire les temps : la regex du journal
(`^(\S+)\s+(\S+)\s+\S+\s+(\S+)\s+\S+ \(([\d.]+)ms search`).

**Ce que le jour a appris là-dessus** : le panel de 10 000 fichiers ne
discrimine pas la milliseconde ; 30 000 fichiers, oui. Une suite de tests
ou une compilation en parallèle **fausse tout** (charge 14 → +40 % sur le
fuzzy) : vérifier `uptime` avant et après, jeter la passe si la charge
n'est pas celle des panels eux-mêmes (3 à 6). L'A/B au même binaire a
manqué la perte de 3 ms des étapes 1-4 parce que le code était le même
des deux côtés ; le par-commit l'a vue. La règle de décision est dans
[01](01-recap-findings-et-plan-d-action.md) §4 : taille d'abord, exactitude
vendue, temps acceptable sous ×1,5.

---

## 6. Les tests

```bash
cargo test --lib                       # ld-lucivy : 1 444 verts, 17 ignorés (~20 s + compilation)
cargo test --lib suffix_fst            # le module seul, 302 tests, 4 s
cargo test -p lucivy-core              # 184 verts, 31 ignorés (~5 min) — charge la machine
cargo test -p lucivy-core --test test_luce_v3_roundtrip   # le test à l'ordre non déterministe
```

`luce_v3_sharded_roundtrip` peut tomber **sous charge** : il compare l'ordre
du top-10 entre documents à score strictement égal, ordre qui dépend de
l'ordre de réponse des shards. Relancé seul il passe. À corriger par un
tri stable par id entre ex æquo dans le merge des shards (à faire, pas
fait).

Tests ajoutés aujourd'hui, à connaître : `packed_and_legacy_records_decode_to_the_same_parents`,
`dense_lists_compress_to_two_bytes_per_parent`, `version_3_file_still_reads_its_parents`
(+ v4), `bytemap_and_meta_agree_on_content` (la preuve de l'étape 2),
`narrow_and_wide_layouts_read_alike` (posmap), `gapless_tables_take_sib3_and_read_alike`,
`layout_1_and_layout_2_read_alike` (termtexts), `no_suffix_starts_in_the_overlap`.

---

## 6 bis. Le mode dictionnaire : construire, mesurer, déboguer

- **Construire la référence en mode dictionnaire** : le même harnais avec
  `V3_SFX_VERSION=4` (la clé de forme `.v3_shape` en tient compte, donc un
  autre `V3_INDEX_DIR`) :
  ```bash
  V3_SFX_VERSION=4 V3_CORPUS=/tmp/lucivy-cmp V3_INDEX_DIR=/chemin/idx-dict cargo test --release -p lucivy-core \
    --test test_sfx_v3_ground_truth v3_ground_truth_demo -- --ignored --nocapture > out.txt 2>&1
  ```
  19 s au lieu de 8 (une génération par commit, une compaction toutes les
  huit — `LUCIVY_DICT_MAX_GENERATIONS`) ; 390 Mo ; panel 9/9. Le scan
  `benches/scan_index_size.py` lit les `dict-*` comme des sidecars (totaux
  `sfx`, `termtexts`) et compte les `.gmap`. Les générations vivantes :
  `grep -o '"generations":\[[^]]*\]' meta.json`.
- **La vérité de bout en bout** : `cargo test --release -p lucivy-core
  --test test_dictionary_index` — 300 fichiers du noyau (ou un corpus
  synthétique sans `/tmp/lucivy-cmp`), index v3 et v4 côte à côte, onze
  requêtes comparées documents et spans, réouverture ; et
  `dictionary_pieces`, qui imprime chaque pièce (parents du dictionnaire,
  `.gmap`, postings, splits, chaînes, résolution) sur trois documents —
  c'est ce qui a trouvé le double mappage.
- **Mémo froide / chaude** : `V3_QUERIES=schdule:fz1,schdule:fz1` relance
  la même requête dans le même processus ; la seconde a la mémo chaude.
  `V3_PROFILE=1` : la ligne `[plan] contains "…": N waves, N cells
  computed, N held, wall` puis par vague (mur, somme CPU, cellule la plus
  lente, `kind key`) ; `[prescan]` / `[fz prescan]` donne le mur du
  scatter, la somme par segment et le **max par segment** — quand max ≈
  mur, un segment calcule pendant que les autres attendent une cellule
  (ne doit plus arriver avec le plan) ; la ligne `dictionary: memo lookups
  … | cut N items -> N kept in Nms | sibling DFS … | anchored …` dit où va
  le temps par segment propre au mode dictionnaire (la coupe au `.gmap`
  d'abord). `[cell] cand/02 "e": N entries, scan, sort` sous une cellule
  de plus de 2 ms. `V3_PLAN=0` : sans plan (les segments calculent les
  cellules en ligne comme avant le 5 septembre) — l'A/B du plan lui-même.
- **A/B 30 000 dictionnaire contre v3** : `run-ab-dict.sh` du scratchpad
  (même binaire, `V3_SFX_VERSION=4` pour l'index dictionnaire, 3 passes ;
  `run-ab-plan.sh` / `run-suite-and-ab.sh` : les mêmes avec les sorties
  `abplan-*` / `abplan2-*`) ; résultat du 5 septembre au matin : ×2 à ×22
  à froid ([09](09-journal-chantier-dictionnaire.md) §11) ; au soir, avec
  le plan : [11](11-journal-chantier-plan-fst.md) §4 ; noyau entier
  (`run-ab-90k.sh`, sorties `ab90k-*`, référence v3 `idx90k-v8` au format
  courant, 7,3 Go) : [11](11-journal-chantier-plan-fst.md) §4 bis.
- **Ce qui n'est pas fait / à savoir** : le plan
  ([11](11-journal-chantier-plan-fst.md)) a ramené le 30 000 à froid de
  ×2-22 à ×0,8-1,9 ([11](11-journal-chantier-plan-fst.md) §4) ; ce qui
  reste par segment est la coupe des listes au `.gmap` et la DFS de
  fratrie (recherche binaire dans le `.gmap` par `siblings()`) ; le mode
  dictionnaire n'est pas le défaut tant que ×1,5 n'est pas tenu partout ;
  `index_bytes`, `preload`, `residency` ignorent les fichiers `dict-*` ;
  l'import WASM (`lucivy_import_file`) les route dans `shard_0/` sans les
  connaître, ce qui se trouve être juste ; un segment abandonné entre deux
  commits laisse des identifiants sans texte (inoffensif) ; la suite
  `cargo test -p lucivy-core` complète n'a pas été relancée après le
  dictionnaire (la lib et le test de bout en bout, si) ; pas d'A/B 30 000
  en mode dictionnaire.
- **Index du scratchpad** : `idx-dict` (référence 10 000 en une
  génération, v1), `idx-dict2` (générations incrémentales, 4 vivantes),
  `idx30k-dict` (1,33 Go), `idx90k-dict` (5,98 Go, le noyau entier),
  `prof-dict*.txt`, `abdict-*.txt` (l'A/B), `dict-e2e.txt`.

---

## 7. Le protocole d'une étape, en résumé

1. Écrire le changement avec le lecteur qui ouvre **encore** l'ancien layout.
2. `cargo test --lib`.
3. Construire la référence 10 000 (`idx-nouveau`) : `9 pass, 0 fail`, scan,
   comparer au scan précédent.
4. Rouvrir l'index de référence **précédent** avec le nouveau binaire :
   `index reused` + `9 pass`.
5. Construire le 30 000, A/B au même binaire contre l'index précédent (et
   par commit si le code de lecture a changé).
6. `cargo test -p lucivy-core` (pas pendant les A/B).
7. Journal ([03](03-journal-des-etapes.md)) : changement, taille, justesse,
   temps, tests ; un commit par étape, message en français, sans trailer.

---

## 8. Ce qui est dans le scratchpad de la session du 4 (à recréer sinon)

`/tmp/claude-1000/-home-lucied-git-workspaces-lucivy/de715c8d-…/scratchpad/` :
`idx-v3` (référence 10 k en v3, conteneur 3, avec bytemap), `idx-v4c`
(conteneur 4), `idx-v8` (état final 10 k), `idx30k-v3`, `idx30k-v6`,
`idx30k-v5a`, `idx30k-v8`, `idx90k-v4` (noyau entier, 253 segments),
`wt-v3` (worktree du commit `1c263f3`, binaire v3 dans `wt-target`), les
sorties `scan-*.txt`, `ab*.txt`, `demo-*.txt`. Depuis la nuit : `idx-v6`,
`idx-v7` et `idx30k-v6`, `idx30k-v7` (conteneurs 6 et **8** malgré le
nom, tables par blocs), `idx-dict`, `mesures-chantier-dict.txt`,
`explo-*.md` (les trois cartes du code faites par les agents). Environ
30 Go. Rien d'irremplaçable : tout se reconstruit avec §3 et §6 bis.

---

## 9. Pièges rencontrés aujourd'hui

- `git stash` puis `rebase` sur un fichier modifié des deux côtés → conflit
  résiduel au `pop` ; résoudre à la main, `stash drop`.
- Une lecture de fichier qui efface les lignes vides (`grep -v '^$'`)
  rend les remplacements textuels impossibles : relire avec `sed -n` avant
  d'éditer.
- `snap_to_char_boundary` est `pub(crate)` depuis ce soir ; un `&String`
  n'est pas un `&str` sans `&*`.
- Le scan Python lisait le pied des fichiers comme des slots : « max_ordinal
  2 067 407 470 » dans l'audit est un artefact, corrigé.
- Trois `cargo` en parallèle (debug + release + worktree) marchent, mais
  aucun chronométrage n'est valable pendant ce temps.
- `grep -c` sans résultat rend 1 en code de sortie : un `&&` derrière fait
  croire à un échec.
- Un `debug_assert` n'est vérifié que dans les suites debug : la
  différence de longueur du `to_lowercase` (`İ`) n'est apparue que dans
  `cargo test -p lucivy-core`, jamais dans le panel release.
- Un lecteur qui traduit global → local ne doit le faire qu'une fois :
  `entries()` déléguant à `entries_filtered()` traduisait deux fois et
  cherchait un ordinal local comme un identifiant global — zéro résultat en
  strict, les bons en relâché.
- Mémoïser au niveau du shard sérialise ce qu'un segment demande le
  premier ; filtrer par recherche binaire par candidat coûte ×5 sur un
  fuzzy : trier les listes mémoïsées et marcher avec le `.gmap`.
- `cargo build` à la racine du workspace ne compile pas forcément la
  crate modifiée (« Finished in 0.05 s ») : `-p ld-lucivy -p lucivy-core`.
