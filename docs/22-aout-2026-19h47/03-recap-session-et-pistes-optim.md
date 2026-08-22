# Récapitulatif de session — 22 août 2026

> Branche `v3-recovery`. Reprise après changement de machine.
> Point de départ : toolchain absent, corpus absent, dernière mesure datant du 2 juin.

---

## 1. Ce qui a été fait, dans l'ordre

### Remise en route

Rust n'était pas installé sur la machine (`~/.cargo` absent, rien dans pacman), et le
corpus `rag3db` non plus. Installé rustup 1.98.0, recloné le corpus. **Le WIP du 2 juin
n'avait jamais été compilé** — il l'est, et sans erreur : les seuls échecs du workspace
sont `bindings/python` (PyO3 0.24.2 plafonne à Python 3.13, la machine a 3.14) et un
bench `bitpacker` qui exige nightly.

### Mesures d'entrée, qui contredisaient les docs

| | Docs de mai | Mesure réelle |
|---|---|---|
| Contains | 13/15, « 2 fails restants » | **15/15** |
| Fuzzy | 0/6 | 2/6 |
| Regex | jamais mesuré | 2/5 |

Le rapport de session 7 disait 13/15, le findings du 30 mai décrivait 2 fails, le doc
fuzzy du 1er juin affirmait « validée à 15/15 ». Les trois ne pouvaient pas être vrais
ensemble. **On a raisonné trois mois sur un score périmé.**

### Les dix vérités dichotomiques (doc 02) — toutes traitées

| # | Correction | Commit |
|---|---|---|
| 1 | Garde de merge `sfx_version >= 3` | `36d7cae` |
| 2 | `has_word_pipeline()` exige `num_ordinals > 0` | `36d7cae` |
| 3 | `merge_segments_v3` renommée et documentée honnêtement | `36d7cae` |
| 4 | `query_content_len` en octets et non en chars | `fcf21c0` |
| 6 | `byte_to` = fin du match, `token_end` = fin du conteneur | `fcf21c0` |
| 10 | Doc-comments menteurs | `fcf21c0` |
| 7 | Unité de `span` unifiée, post-filtre mort retiré | `8aeb093` |
| 8 | `resolve_single_word_v3` câblé dans `find_literal_v3` | `8aeb093` |
| 9 | Bloc explain mort supprimé (174 lignes) | `c464ddc` |
| 5 | Tag de partition persisté dans `TTX3` | `35bc2d6` |

### Perf du contains

| Étape | Effet |
|---|---|
| `prescan_segments` parallélisé via luciole (`665a516`) | `include` 1273 → 191 ms |
| Fan-out aplati par (shard, segment) (`3bc978f`) | `include` 191 → 166 ms, `function` 36 ms |

Avant : `uint64_t` relax à 1717 ms. Après : 110 ms. Sans sharding.

### Trois angles morts de couverture

- **v3 + sharding n'avait jamais été câblé** (`1281d70`). Les deux benchs shardés ne
  fixent pas `sfx_version`, donc tournent en v2. Le DAG shardé ouvrait tout `.sfx` avec
  le reader v1/v2 : échec immédiat sur `invalid .sfx magic bytes`.
- **v3 + distribué n'avait jamais tourné** (`6bc89ef`). Seul `acid_postgres.rs` teste ce
  chemin, `#[ignore]` et en v2. `ContainsQueryV3::make_weight` ignorait le fournisseur
  de stats globales — le distribué v3 était donc cassé.
- **Le regex n'avait jamais été mesuré sur v3.** `test_regex_ground_truth.rs` construit
  son index avec `SchemaConfig { ..Default::default() }`, donc v2.

### Correction : la même réponse structurelle trois fois

Regex, fuzzy et contains avaient **la même racine** : le pipeline accepte sans preuve.

| Moteur | Ce qui a été fait | Résultat |
|---|---|---|
| Regex | leftmost match (`031b498`), `ByteRangeCheck` implémenté + vérification du pattern réel (`6aa30bf`) | 2/5 → 5/10 exacts, 535 FP éliminés |
| Fuzzy | vérification Levenshtein (`d21775f`), slack séparateur + chaînes multiples (`48576e9`) | 2/6 → **6/6 exact** |
| Contains | vérification du littéral (`fc79372`) | 6/10 → 9/10 sur 50k docs kernel |

Les trois vérifications utilisent `posmap` + `termtexts`, **sans jamais toucher au
docstore**.

### Passage à l'échelle

Corpus kernel Linux cloné (`/tmp/linux-bench`, 95 730 fichiers, 2,1 Go). Trois bugs que
5 000 documents ne montraient pas :

- **`TableFunction` strict** matchait `migra|table function|` — le supplément de chaînes
  par siblings tournait sans condition sur `strict_separators` (`c04ff19`).
- **`__init` strict** rendait 13 275 documents pour 4 742 — la chaîne enjambait un token
  intermédiaire entier. Réglé par la vérification.
- **Le plafond de 10 000 résultats** transformait toute requête à fort rappel en échec.

### Merge v3

Bloqué le matin (corruption silencieuse via le DAG v2), puis débloqué en deux temps :
ré-indexation (`26c80b0`), puis remap (`2a4c375`) parce que la ré-indexation coûtait
18 Go pour fusionner 50 000 documents.

---

## 2. État mesuré en fin de session

| Axe | Valeur | Corpus |
|---|---|---|
| Contains | 15/15 | rag3db 5k |
| Contains | 9/10 | kernel 50k, 800 segments |
| Fuzzy | 6/6 exact | rag3db 500 |
| Regex | 5/10 exacts | rag3db 2k |
| Baseline globale | 9/11 | rag3db 500 |
| `cargo test --lib` | 1426 passed, 3 failed, 16 ignored | |

Les 3 échecs unitaires : deux fixtures mortes depuis le 19 mai (elles passent `None`
pour les maps), une casse connue du WIP (`tokens` passé de `BTreeSet` à `Vec`).

---

## 3. La découverte qui inverse la prémisse

**Moins de segments rend les requêtes plus lentes.** Même corpus de 20 000 documents du
kernel, mêmes requêtes :

| Query | 320 segments | 1 segment | Écart |
|---|---|---|---|
| `spin_lock` strict | 159 ms | 7 376 ms | **46×** |
| `struct file` strict | 211 ms | 8 380 ms | **40×** |
| `net_device` strict | 161 ms | 7 942 ms | **49×** |
| `kmalloc` strict | 136 ms | 400 ms | 3× |
| `__init` strict | 9 264 ms | 28 114 ms | 3× |

Le segment est l'unité de parallélisme du prescan : 320 segments donnent un fan-out de
320 sur 24 cœurs, un seul n'en donne aucun. J'ai affirmé plusieurs fois dans la journée
que « les 800 segments dominent le coût et le merge va aider » — **c'était faux**.

Corollaire : le merge sert à **borner** le nombre de segments, pas à le minimiser.
L'optimum est vraisemblablement de l'ordre du nombre de cœurs.

Et un fait à connaître : **aucun merge ne se déclenche automatiquement.**
`segment_updater_actor.rs:135` diffère les merges à un `drain_merges()`/`start_merge()`
explicite « pour éviter la famine de threads pendant le commit », et `drain_merges` se
contente d'attendre ceux déjà en vol. Un index construit via `LucivyHandle` ne fusionne
jamais tout seul.

---

## 4. Pistes d'optimisation pour la prochaine session

### 4.1 Trouver le bon nombre de segments — le plus rentable

On a les deux extrêmes (320 et 1), il manque la courbe. Faire varier le nombre de
segments à corpus constant et tracer la latence. Hypothèse : un plateau autour de
`n_cœurs` à `4 × n_cœurs`, puis dégradation lente quand le coût fixe par segment domine.

C'est la mesure la moins chère et la plus directement exploitable : elle donne un
réglage de politique de merge, pas un chantier de code.

### 4.2 `__init` — pathologie de requête

9,3 s à 320 segments contre 160 ms pour les autres requêtes du même corpus. La requête
commence par deux séparateurs, ce qui fait exploser la construction de chaînes : le diag
montrait 308 puis 504 chaînes candidates par segment, contre une poignée ailleurs.

Piste : une requête dont le préfixe est un séparateur ne devrait pas ancrer sur des
tokens pure-séparateur, qui sont légion. Instrumenter `V3_DIAG_LITERAL=__init` et
regarder d'où viennent les splits.

### 4.3 Le pipeline word — voir §5

### 4.4 Le prescan charge les fichiers par segment et par requête

`prescan_segment_v3` fait `load("posmap")`, `load("bytemap")`, `load("word_sfxpost")`,
`load("sibling_v3")`, `load("termtexts")` — cinq lectures **avec `.to_vec()`**, donc
cinq copies, à chaque segment et à chaque requête. Sur 800 segments c'est 4 000 copies
par requête.

Piste : mmap ou cache par segment. C'est probablement significatif sur le relax, qui
charge les cinq, contre trois pour le strict.

### 4.5 Le seuil pigeonhole peut être baissé

Passé de `.max(2)` à `.max(1)`, sans effet mesuré parce que `ngrams.len() - n·d` vaut
déjà 3+ sur les requêtes testées. Il ne joue que sur les requêtes très courtes. Depuis
que la vérification est exacte, baisser le seuil ne peut que gagner du rappel.

### 4.6 Le O(n²) de `build_trigram_chains`

Double boucle `start`/`j` par document, et on émet maintenant jusqu'à
`MAX_CHAINS_PER_DOC = 8` chaînes au lieu d'une. Coût mesuré : +40 % sur `inclde` et
`retrun`. Une DP à balayage unique donnerait `O(H·T)` et serait *optimale*, donc
supprimerait aussi le FN dû au premier-arrivé.

Trois gaspillages purs identifiés et non corrigés :
- `fst_candidates_v3` appelé **deux fois par n-gramme** (sélectivité puis résolution)
- hits chunk et word en doublon dans la même `Vec` (×4 sur le quadratique)
- le tri par sélectivité ne sert plus à rien depuis que le `doc_filter` a disparu

### 4.7 `best_bt` — highlights fuzzy faux

`composite.rs` prend la première occurrence globale du dernier trigramme après
`best_bf`, pas celle de la chaîne retenue. `test_fuzzy_ground_truth` est rouge avant
comme après cette session (vérifié par `git stash` sur HEAD).

### 4.8 Le remap de merge charge un segment entier à la fois

Acceptable pour des paliers de 8, à revoir si on fusionne plus large. La première version
chargeait **tous** les fichiers de **tous** les segments avant de commencer : 30 Go.

---

## 5. Le pipeline word — pourquoi il est lent

Mesuré : le relax coûte **~15× le strict**. Sur 50k docs / 800 segments,
`spin_lock relax` 6,9 s contre 0,5 s en strict ; `ku_dynamic_cast` 605 ms contre 45 ms
sur rag3db. Soit ~10 ms par segment contre 0,7 ms.

Aucune de ces hypothèses n'est mesurée — elles sont classées par suspicion décroissante,
et la première chose à faire est d'**instrumenter le temps par étage** dans
`find_literal_v3`, comme `V3_DIAG_LITERAL` le fait déjà pour les comptages.

### H1 — Le relax exécute les deux pipelines, pas un seul

`find_literal_v3` fait toujours les chaînes chunk, **puis** ajoute les chaînes word si
`!strict_separators`. Le relax ne remplace donc pas le strict, il s'y ajoute. À lui seul
ça expliquerait un facteur 2, pas 15.

### H2 — `intermediates_are_pure_sep` est un balayage par position

`resolve_chains_v3_relaxed` et `resolve_word_chains_v3` acceptent des trous entre
positions, et vérifient chaque position intermédiaire via `posmap.ordinal_at` puis
`bytemap`. Le coût est donc `O(taille du trou)` **par paire de candidats**, alors que le
strict exige `pos+1` et ne vérifie rien.

C'est mon suspect principal : le coût est quadratique en densité de candidats, et les
requêtes lentes sont précisément celles à fort rappel.

### H3 — Trois partitions scannées au lieu de deux

En relax, `fst_candidates_v3` scanne `[0x00, 0x01, 0x02]`. Un range scan de plus par
n-gramme et par requête, plus le décodage des parents associés.

### H4 — Deux fichiers de plus à charger par segment

Le strict n'a besoin ni de `posmap` ni de `bytemap` ni de `word_sfxpost`. Le relax charge
les cinq, avec `.to_vec()` (§4.4). Sur 800 segments ça se voit.

### H5 — Le DFS de siblings branche davantage en word

`falling_walk_words` + `splits_from_fst_candidates` produisent les splits, puis
`sibling_chain_dfs` explore. Les entrées word-stripped ont un overlap de contenu, donc
davantage de clés partagent un préfixe, donc plus de branches.

### H6 — La vérification a plus de matches à traiter

`verify_literal` reconstruit une fenêtre par match. Le relax produit mécaniquement plus
de matches que le strict, donc paie plus. Effet réel mais probablement du second ordre —
et c'est le prix de la correction, pas un défaut.

### Comment trancher

1. Chronométrer chaque étage de `find_literal_v3` (single / chunk chains / word chains /
   verify) et sortir la répartition sur `ku_dynamic_cast` strict vs relax. Une seule
   mesure suffit à éliminer trois hypothèses.
2. Si H2 se confirme, borner la taille des trous acceptés en relax : au-delà de N
   positions, la vérification finale tranchera de toute façon.
3. H4 se teste en mesurant le temps de `prescan_segment_v3` hors résolution.

---

## 6. Ce qu'il ne faut pas refaire

**Cinq hypothèses fausses dans cette session**, dont deux justes mais incomplètes :

| Hypothèse | Verdict |
|---|---|
| Double-feed d'overlap dans le DFA regex | testée **deux fois**, négative les deux fois |
| Un seul span testé par doc (regex) | réfutée par les compteurs |
| Classes de caractères comme cause racine | c'était un symptôme |
| Câblage des siblings dans le regex | mesure strictement neutre |
| Chaînes multiples par doc (fuzzy), seules | sans effet — n'a marché qu'avec le slack |

Ce qui a débloqué à chaque fois, c'est **l'instrumentation**, jamais le raisonnement :
`first_token_dfa=240 sur 347` a localisé le bug regex, et
`window="falseAllowShortFunctionsOnASingle"` a rendu le problème de casse évident en une
ligne. Deux paires de correctifs n'avaient d'effet que **combinées** (leftmost +
troncature côté regex, slack + chaînes multiples côté fuzzy) : tester isolément conduisait
à conclure « inutile » ou « nuisible ».

**Corollaire de méthode** : ne jamais conclure d'une analyse statique qu'un code est
« inoffensif ». L'analyse du matin avait vu le code des siblings en strict et conclu
exactement ça. Le corpus de 50 000 documents l'a réfutée.

---

## 7. Reste ouvert

- 1 FP sur `spin_lock relax` à 50k
- 2 patterns regex totalement cassés **en v2 comme en v3** : `#include\s*[<"]`,
  `[A-Z][a-z]+Error` — extraction de littéraux, pas validation
- `v3_term_is_whole_token_not_prefix` marqué `#[ignore]`, régression située à `8aeb093`
- 3 tests unitaires rouges (2 fixtures de mai, 1 casse connue)
- Build emscripten jamais lancé — la contrainte WASM est respectée par construction
  (tout passe par `build_scatter_dag`, aucun `thread::spawn`) mais non prouvée
