# Suggestions et chantiers — tout ce que je corrigerais, du proche au lointain

Rédigé le 23 août en fin de journée. Classé par ce que j'attaquerais en premier, avec
pour chaque point : ce qui est **mesuré**, ce qui est **supposé**, et par où commencer.
Rien ici n'est une promesse de gain : les trois dernières fois que j'ai estimé un gain
sans mesurer, je me suis trompée.

---

## A. Correction — ce qui rend encore un résultat faux

### A1. Un segment fusionné n'est pas indiscernable d'un segment frais

**Mesuré** : `include` sur l'index fusionné à 32 segments manque 11 spans, contre 3 sur
l'index naturel du même corpus. Les tests de merge (`sfx_dag_v3.rs:769-877`) ne
vérifient que la cohérence interne de la sortie.

**Causes probables** (audit des contrats, P3/P4) : l'internement « premier gagnant »
au merge ne désigne pas le même gagnant qu'à l'indexation — même texte étendu, métas
(`own_len`, `sep_len`, `overlap_len`, `is_word_start`) différentes selon l'ordre des
segments — et les `gap_len` de la sibling table sont copiés verbatim alors qu'ils
dérivent des métas de la destination.

**Par où commencer** : le test qui manque. Indexer A∪B en un segment, fusionner A et B,
comparer l'ensemble des clés FST, des textes termtexts, et le résultat de N requêtes
**avec spans**. Ce test aurait trouvé A1 hier.

### A2. Occurrences manquantes en fin de fichier ou devant un caractère non-ASCII

**Mesuré** : `rag3db` 144 / 15 128 spans manquants, tous suivis de `\n` en fin de
fichier ou de `→`, `│`, `─`. `function`, `return`, `struct` : 1-2 chacun, même profil.

**Supposé** : l'overlap de 2 octets coupe un caractère UTF-8 de 3 octets ; le snap à la
frontière de caractère rend un overlap vide ou différent de ce que la marche attend.

**Par où commencer** : un test minimal `"namespace rag3db\n"` et `"rag3db →"` dans
`test_sfx_v3_pipeline.rs`, avec `V3_DIAG_LITERAL` pour voir les clés produites.

### A3. La collision de clé 0x02 est structurelle

**Mesuré** : `"0ui"` = `"0"+"ui"` ou `"0u"+"i"` sous un seul ordinal ; les postings des
deux lectures sont mélangés. Contourné le 23 août (fin de contenu lue depuis les
chunks), pas résolu.

**Ce que je ferais** : inclure la longueur de l'overlap dans la clé 0x02, ou stocker
`content_len` dans l'entrée `word_sfxpost` (elle a 20 octets fixes, un u16 de plus ne
change pas l'ordre). La seconde est plus simple et rend le contournement inutile.

### A4. Deux falaises d'encodage silencieuses en release

**Vérifié dans le code, non déclenché** : `ORDINAL_BITS = 24` (au-delà de 16,7 M
ordinaux, un terme sert les postings d'un autre) et le compteur de parents en `u16`
(au-delà de 65 535 parents pour une clé, les suivants sont perdus → faux négatifs).
Gardés par des `debug_assert!` seulement ; `Cargo.toml:117` désactive les assertions
en release. La fusion est exactement l'opération qui franchit ces seuils.

**Ce que je ferais** : une erreur franche au build FST, et un garde dans
`merge_segments_v3`. Coût nul.

### A5. `min_suffix_len` codé à 1 dans le merge

Le collector le lit dans `LUCIVY_MIN_SUFFIX_LEN` ; le merge ne peut pas le connaître,
il n'est persisté nulle part. Les résultats basculent au rythme des merges si la
valeur n'est pas 1. Trou de format : le persister dans le `.sfx`.

### A6. Le test `term` ignoré

`v3_term_is_whole_token_not_prefix` est `#[ignore]` depuis `8aeb093`. Tu as dit qu'on
s'en fichait, et c'est documenté. Je le laisse ici pour qu'il ne disparaisse pas.

---

## B. Perf — ce qui reste lent, mesuré

### B1. Le pipeline word sur gros segments

**Mesuré** : `uint64_t` relax 170 ms sur 800 segments naturels, **788 ms** sur 32
fusionnés. Même classe que les 264 M d'entrées du chunk hier : le pipeline word n'a
pas reçu les mêmes traitements (groupement par tête commune, élagage par docs actifs
à la position 0, `entry_at` à l'émission seulement).

**Par où commencer** : `V3_PROFILE=1` sur l'index fusionné, lire `wordmap resolve` et
`word_entries`.

### B2. Un gros segment = un thread

**Mesuré** : `include` 58 ms sur 800 segments, **434 ms** sur 32 fusionnés, avec un seul
thread occupé dans les dumps luciole. Le segment est l'unité de parallélisme du
prescan ; un index « sain » (quelques dizaines de gros segments) le perd.

**Ce que je ferais** : paralléliser **dans** le segment — par tranche de documents
(posmap est indexé par doc, les chaînes peuvent être résolues par plage de docs), ou
par groupe de chaînes. C'est le vrai chantier perf qui reste, et il conditionne
l'intérêt du merge.

### B3. Le chemin ancré sur le deuxième token coûte parfois plus qu'il n'économise

**Mesuré** : `net_device` chunk walk 245 → 67 ms CPU et DFS 246 → 6 ms, mais l'ancrage
en coûte 765 : les restes courts (`e`, `ce`…) ont des milliers de candidats. Gardé
parce que le mural n'a pas bougé et que le plancher dominait ; **à remesurer**
maintenant que le plancher a disparu, et probablement à borner : n'ancrer que si le
reste fait ≥ 4 octets, laisser la marche avant pour le reste.

### B4. Le merge progressif du harnais est une mauvaise politique

**Mesuré** : 660 s sur 886 s du run 50k fusionné. Ma politique « par taille de groupe »
re-fusionne sans cesse des segments moyens. `LogMergePolicy` existe, est la policy par
défaut du writer, et **n'est jamais consultée** (`handle_commit` diffère tout). Depuis
`2eb6426`, les merges ne bloquent plus l'acteur — la raison de les différer a disparu.

**Ce que je ferais** : brancher la policy au commit avec `merge_many`. Le harnais n'a
plus à simuler.

### B5. Le merge lui-même, ~700 ms par fusion de 8 segments

**Mesuré** : `merge_segments_v3` ~200 ms, FST ~100 ms ; le reste dans les index dérivés
relus depuis le `.sfxpost` qu'on vient d'écrire (`build_derived_indexes_v3`, 3-4 appels
dynamiques par posting) et les sérialisations non chronométrées. Et trois fichiers
morts (`chunk_word_map`, `next_word_map`, et le `word_map` interne au `.sfx`) sont
toujours construits, fusionnés, écrits. Les supprimer est gratuit.

### B6. 25 fsyncs par segment

**Mesuré** : sur btrfs+zstd, 65 ms chacun. Contourné pour le bench (cache sans fsync),
réel en production sur tout FS. `composite_file.rs` existe : un fichier composite par
champ SFX diviserait le nombre par dix. Changement de format.

### B7. `.freqmap`

Écrit par `build_derived_indexes_v3`, 8 octets par couple (ordinal, doc), non
compressé — de l'ordre du `.sfxpost`. **Aucun lecteur nulle part.** À supprimer.

---

## C. Structure — ce qui rendrait les prochains bugs impossibles

### C1. Faire passer le DAG de commit par `execute_dag_async`

Le chemin spécial que j'ai ajouté pour le merge parallèle est une instance d'un motif
général : fan-out par continuation, sûr en emscripten parce qu'aucun thread n'attend.
`DagExecutor` existe. Généraliser, et supprimer le chemin spécial. Voir §5 bis du recap.

### C2. La vérité terrain par spans comme test de CI, pas seulement de bench

`test_sfx_v3_ground_truth` tourne sur rag3db en 45 s. Les spans y sont **rapportés**,
pas assertés. Les asserter sur les requêtes aujourd'hui exactes (11 sur 15) transforme
une heure de diagnostic en un échec immédiat.

### C3. Une requête vide dans chaque panel

29 ms aujourd'hui. Si ce chiffre bouge, c'est le plancher qui bouge, et aucun
compteur interne ne le dira.

### C4. Le fuzzy et le regex n'ont pas eu la journée

Tout ce qui précède concerne `contains`. `fuzzy_v3` et `regex_v3` partagent le
prescan (donc le plancher tombé leur profite) mais pas les correctifs de résolution,
et leur vérité terrain (`baseline_fuzzy_regex`) compare des **documents**, pas des
spans, sur 500 fichiers. Même traitement à leur appliquer : spans depuis le disque,
panel kernel, requête vide.

### C5. Emscripten n'a jamais été compilé depuis le début du v3

`bash bindings/emscripten/build.sh` n'a pas tourné. Le merge parallèle et les
`Instant::now()` du profiling sont gardés par `cfg`, mais personne ne l'a vérifié.

---

## D. Hygiène — petit, sûr, sans urgence

- `resolve_doc` et `first_entries.iter().find(doc_id)` : ce dernier a été remplacé
  dans les chemins posmap, il reste dans `find_multi_token_v3`.
- `segment_reader.rs:162` : `sfx_index_file(id, field)` fait `id.to_string()` par
  lookup ; `load_sfx_files` instancie les 13 writers du registre par champ et par
  segment pour tester l'existence de fichiers.
- `builder_v3.rs:248,322` : `to_lowercase()` par token au merge, sur des textes déjà
  en minuscules.
- `sfx_dag_v3.rs:360` : `global_intern` alloue une `String` par lookup, y compris sur
  hit — `HashMap<(bool, &str), u32>` suffit.
- Le mode strict de `contains` charge `.bytemap` sans le lire. Coût nul, mais faux.
- Les trois échecs unitaires pré-existants (`diag_false_positive_uint64t`,
  `test_resolve_chain_sep_skip`, `test_into_data_sorted`) : deux fixtures mortes depuis
  mai, une casse connue du WIP. À réparer ou supprimer, pas à laisser rouges.

---

## E. Ce que je ne referais pas

- Estimer un coût à partir du nombre de fichiers chargés (§4.4 d'hier).
- Déduire une cause d'une corrélation de forme d'index sans isoler la variable (§3).
- Livrer une optimisation sans la requête vide et sans la vérité terrain par spans
  dans le même run.
- Lancer un run de 19 minutes avant d'avoir une reproduction à 5 secondes.
