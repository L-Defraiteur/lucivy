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

**Après** : 50k naturel et fusionné 9/9 exacts, rag3db 15/15, zh_CN fusionné = frais =
grep. Plus un seul span manquant ou en trop connu.

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

### A4. Deux falaises d'encodage silencieuses en release — FAIT le 23 août

**Mesuré avant de corriger** : fusionner les 50k docs kernel vers un segment faisait
monter le compteur de parents d'une clé à 63 242, puis 64 461 — la limite `u16` est
65 535. Le merge suivant aurait tronqué la liste sans un mot. Le garde posé juste
avant l'a refusé proprement.

**Correctifs** : erreurs franches dans `build()` (ordinal > 24 bits, parents > limite),
garde en amont dans `merge_segments_v3` (avant de calculer les index dérivés), en-tête
de liste de parents passé en **u32** (2 octets de plus par clé multi-parent, format
`v=7`), message d'erreur réel remonté (la conversion `lucivy_fst::Error` l'écrasait
en « I/O error »). Métriques `[fst]` : nombre de clés, `max_parents`, marge d'ordinaux.

**Ce que la fusion complète a ensuite révélé** : 50 000 docs dans un segment =
138 M d'entrées, FST de 427 Mo, 30,8 M de clés, une clé à 3 248 834 parents, et
**82,7 % des 16,7 M d'ordinaux adressables**. Résultats exacts, mais `include` y coûte
718 ms contre 55 sur 800 segments. Un chunk de 8 octets + overlap est quasi unique :
les ordinaux croissent avec le texte, pas le vocabulaire. Conclusion pour B2 et B4.

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

### B2. Un gros segment = un thread — reformulé le 23 août

**Mesuré** : `include` 55 ms sur 800 segments, 410 sur 32, **718 sur 1**. Sur le 32,
un seul segment prend 407 ms des 435 de CPU (concurrence de pointe 24 : le pool fait
son travail, c'est la répartition qui est mauvaise).

**Ce que j'avais écrit** : paralléliser dans le segment. **Ce que A4 a montré** : les
gros segments sont mauvais sur tous les axes — FST, temps de merge, falaises, requête.
Le naturel à 800 segments est le meilleur index mesuré, pas un accident. Le chantier
n'est pas d'accélérer les gros segments, c'est de ne pas en fabriquer : voir B4. La
parallélisation intra-segment reste possible (posmap par doc, chaînes par plage de
docs) mais n'est plus prioritaire.

### B2 bis. Le contains sur littéraux courts en relaxed

**Mesuré** : `inc` relax = 120 ms CPU sur rag3db, 26 000 chaînes chunk à travers les
séparateurs, pour 20 677 spans ; c'est ce qui borne le mode `pieces` du fuzzy.
**Essayé** : désactiver les chaînes chunk en relaxed (le pipeline word couvre le
reste) — faux : les entrées word n'indexent que 256 octets de suffixes + une queue,
`deepmark` au fond d'un identifiant de 400 octets perd 10 occurrences sur 20
(`v3_relaxed_sku_corpus_matches_grep`). Voie possible : un drapeau par segment
« aucun mot plus long que le plafond », posé à l'indexation, qui autorise le saut.

### B3. Le chemin ancré sur le deuxième token coûte parfois plus qu'il n'économise

**Mesuré** : `net_device` chunk walk 245 → 67 ms CPU et DFS 246 → 6 ms, mais l'ancrage
en coûte 765 : les restes courts (`e`, `ce`…) ont des milliers de candidats. Gardé
parce que le mural n'a pas bougé et que le plancher dominait ; **à remesurer**
maintenant que le plancher a disparu, et probablement à borner : n'ancrer que si le
reste fait ≥ 4 octets, laisser la marche avant pour le reste.

### B4. Le merge progressif du harnais est une mauvaise politique — FAIT le 23 août

**Avant** : `LogMergePolicy` était la policy par défaut et **jamais consultée** ;
chaque merge venait d'un `start_merge()` explicite, un writer laissé seul produisait
un segment par commit pour toujours, et le harnais simulait une policy à la main.

**Correctif** : `handle_commit` consulte la policy après chaque commit et lance les
candidats par le chemin non bloquant (`handle_start_merges`). Segments en vol suivis
dans l'acteur (`merging`) ; un merge explicite qui recouvre une fusion en cours est
**refusé** (avant : deux fusions sur un segment, 400 docs → 269, mesuré le jour même).
`LogMergePolicy::set_max_merged_docs` : plafond de **sortie** (les niveaux sont
empaquetés dessous), en plus du plafond d'entrée hérité de tantivy. `LucivyHandle`
pose les deux à 10 000 docs.

Harnais : `NoMergePolicy` dès qu'il pilote lui-même (`V3_MERGE`), `V3_POLICY=1` pour
laisser faire la policy et mesurer l'index « réel ».

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

### C2. La vérité terrain par spans comme test de CI — FAIT le 23 août

Les spans sont assertés dans `test_sfx_v3_ground_truth` (15/15 exacts sur rag3db,
9/9 sur 50k naturel et fusionné). `V3_SPANS_REPORT_ONLY=1` revient au critère
documents pour diagnostiquer.

### C3. Une requête vide dans chaque panel

29 ms aujourd'hui. Si ce chiffre bouge, c'est le plancher qui bouge, et aucun
compteur interne ne le dira.

### C4. Le fuzzy et le regex n'ont pas eu la journée — fuzzy FAIT le 23 août au soir

Fuzzy : vérité terrain par spans depuis le disque (`V3_QUERIES=…:fz1`), définition
partagée moteur/harnais (`briques/fuzzy_spans.rs`), rag3db 11/11 exacts, kernel 50k
9/9 documents et 8/9 spans (1 sur 1,8 M). Voir 06-progression. **Reste** : la perf à
grande échelle (`__init` fz1 11 s sur 50k — profiler `rebuild_window_mapped` et la
résolution des bigrammes fréquents), le test fusionné = frais en fuzzy, et le regex,
qui n'a toujours rien eu : même traitement (spans depuis le disque, panel kernel,
requête vide).

**À trancher** : `sfx_version` vaut 2 par défaut (`index_meta.rs:293`). Deux anciens
tests fuzzy mesuraient v2 sans le savoir.

### C5. Emscripten n'a jamais été compilé depuis le début du v3

`bash bindings/emscripten/build.sh` n'a pas tourné. Le merge parallèle et les
`Instant::now()` du profiling sont gardés par `cfg`, mais personne ne l'a vérifié.

---

## C bis. Avertissements honnêtes à la requête

Quand la couverture aura été poussée autant que possible, ce qui reste comme
limitation **connue** doit remonter à l'utilisateur au moment de la recherche, pas
rester un silence : regex sans littéral exploitable (`MIN_LITERAL_LEN`) ou dont les
gaps dépassent ce que l'index sait vérifier, fuzzy dont la distance avale la
requête (`__init` relaxed → `init` d=1 admet `int`, `unit`, `inet` : 44 579 docs sur
50 000), mot plus long que le plafond de suffixes des entrées word, requête vide
après retrait des séparateurs, plafond de résultats atteint. Forme : un champ
`warnings: Vec<String>` dans le résultat de recherche, propagé par les bindings.
À faire après la couverture, pas à la place.

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
