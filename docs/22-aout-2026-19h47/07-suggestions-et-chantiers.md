# Suggestions et chantiers — tout ce que je corrigerais, du proche au lointain

Rédigé le 23 août en fin de journée. Classé par ce que j'attaquerais en premier, avec
pour chaque point : ce qui est **mesuré**, ce qui est **supposé**, et par où commencer.
Rien ici n'est une promesse de gain : les trois dernières fois que j'ai estimé un gain
sans mesurer, je me suis trompée.

---

## A. Correction — ce qui rend encore un résultat faux

### A1. Un segment fusionné n'est pas indiscernable d'un segment frais — FAIT le 23 août

**Le test** : `v3_merge_equals_fresh_by_spans` (`test_sfx_v3_pipeline.rs`) indexe le
même corpus deux fois — une fois sans fusion, une fois en 67 segments fusionnés en
deux niveaux (8 par 8, puis tout) — et compare les spans document par document, en
strict et en relaxed, plus un grep strict sur le texte. `V3_MERGE_DOCS` fixe la taille
(400 par défaut, 3 s ; 3 000 en 18 s), `V3_CORPUS` la source (kernel si présent).

**Ce qu'il a trouvé en 3 secondes** : c'était A3, pas un bug du merge. La cause
exacte est dans A3 ; la conséquence était que le « gagnant » d'une clé 0x02 changeait
avec l'ordre des segments, donc fusionné ≠ frais — et les deux pouvaient être faux
(`"in i-th"` pour `__init` relaxed était tronqué sur l'index **frais** selon le run).

**Deuxième trouvaille, même test sur les traductions chinoises** (`V3_CORPUS=…/zh_CN`,
3 s) : la même collision existe pour les **chunks**. `"spinlock"` est un chunk entier
(own_len 8, overlap 0) dans un document, et `spinlo` + overlap `ck` (own_len 6) dans
un autre : même texte étendu, un ordinal, métas du premier. Tout ce qui reconstruit du
texte depuis termtexts (`verify_literal`, la fenêtre relaxed) lit `own_len` et tombait
faux pour l'autre forme. Réduit à **3 fichiers** par `v3_merge_bisect` (delta-debugging,
`#[ignore]`, `V3_BISECT_TARGET`), reproduit en 30 ms par `v3_merge_repro_files`.
Correctif : internement chunk par (texte, own_len, sep_len, is_word_start), collector
et merge. Effet de bord : `include` sur zh_CN passe de 528 à 529 = grep.

**Reste de A1** (non vérifié, pas de symptôme après le correctif) : les `gap_len` de la
sibling table copiés verbatim au merge. Le test ci-dessus est maintenant le filet.

### A2. Occurrences manquantes en fin de fichier ou devant un caractère non-ASCII — FAIT le 23 août (la cause principale)

**Mesuré avant** : `rag3db` 144 / 15 128 spans manquants ; sur le kernel naturel,
`include` 3, `spin_lock` 1, `__init` 1, `__init` relax 161.

**Supposé hier** : l'overlap de 2 octets coupant un caractère UTF-8. **Faux.** Réduit par
`v3_merge_bisect` en mode grep (`V3_BISECT_GREP=1`) à un fichier, puis par
`v3_a2_probe` (le même texte coupé d'un caractère à la fois : échec tous les 3
caractères CJK) et `v3_a2_chunks` (dump du tokenizer) : **`equal_chunks` émettait un
chunk vide**. Il planifie N chunks de 7-8 octets, le snap aux frontières UTF-8 avance
chaque fin de 1-2 octets, et les derniers chunks planifiés commencent après la fin du
texte. Un chunk vide est une position sans texte : le chemin ancré sur le deuxième
token regarde `position - 1`, tombe dessus, et rejette un match réel.

**Correctif** : `equal_chunks` s'arrête quand le texte est consommé (+ test unitaire
`no_empty_chunk_on_multibyte_text`). Il change la numérotation des positions sur les
textes multi-octets : index à reconstruire (`v=6` dans la clé du cache du harnais).

**Résidu** : les quelques spans qui manquent encore sur zh_CN en strict sont
à remesurer après ce correctif (voir 06-progression pour les chiffres 50k).

### A3. La collision de clé 0x02 est structurelle — FAIT le 23 août

**Mesuré** : `"0ui"` = `"0"+"ui"` ou `"0u"+"i"` sous un seul ordinal ; de même `"init"`
= mot `init`, ou `in`+overlap `it`, ou `in`+overlap `i`+… Un ordinal portait les métas
(`own_len`, `overlap_len`) de la **première** occurrence internée, et la marche de
chaîne reprenait la requête au mauvais octet pour toutes les autres formes.

**Correctif** (deux parties) :
- L'internement 0x02 est clé par **(texte, content_len)**, dans le collector et dans
  `merge_segments_v3`. La fabrique FST acceptait déjà plusieurs parents sous une clé :
  chaque forme a son ordinal, ses métas, ses postings. Plus de gagnant.
- Le posting `word_sfxpost` porte la **fin de contenu** dans `byte_to` (format `WSP2`,
  avant : fin du dernier chunk, séparateurs compris). `resolve_single_word_v3` et les
  chaînes la lisent depuis le posting ; `word_content_end` (le contournement du 23 août
  via posmap + termtexts) est supprimé. Les entrées « tail » des mots > 264 octets ont
  maintenant un `byte_from` exact aussi.

**Test** : `v3_word_shapes_share_key_not_ordinal` — cinq documents, un par segment,
dans les deux ordres d'insertion ; échouait avant sur l'index frais déjà.

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

### B1. Le pipeline word sur gros segments — FAIT le 23 août, 17h

**Mesuré avant** : `uint64_t` relax 809 ms sur 32 segments fusionnés, 17,5 M de
lookups wordmap pour 62 736 survivants. **Après** (`resolve_word_chains_v3_wordmap_grouped`,
chaînes groupées par (tête, sti), un balayage avant par posting de tête, dispatch par
liste de queue distincte) : **214 ms**, 48 k lookups, mêmes survivants, spans
identiques à l'octet près sur naturel et fusionné.

Résidus observés en chemin, pré-existants (vérifiés avec l'ancienne fonction) :
`__init` relax perd un document sur l'index fusionné (A1), et manque 161 spans sur le
naturel (classe A2 probablement).

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
