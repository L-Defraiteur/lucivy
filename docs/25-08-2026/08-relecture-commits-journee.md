# Relecture des commits du 25 août (08:59 → 16:35)

Relecture à froid des 25 commits de `03c09b3` à `2f9e6db` sur
`wip/publication-3.0.0`, plus les docs 02-07. Chaque point ci-dessous a été
vérifié dans le code ou par un test, pas déduit des messages de commit.

État des suites au moment de la relecture : `cargo test --lib` **1 428 / 0**,
`cargo test -p lucivy-core --no-fail-fast` tout vert sauf `bench_sharding`
t01/t04 (pré-existants), `lucistore` 43 / 0. **Les suites sont vertes et
pourtant deux des défauts ci-dessous sont graves** : elles ne couvrent ni le
merge d'un index v2 ni la complexité des accès SFP3.

---

## 1. À corriger avant tout merge dans `v3-recovery`

### 1.1 CRITIQUE — tout merge d'un index `sfx_version = 2` échoue (ee3f4a0)

`src/indexer/sfx_merge.rs:304` : `validate_sfxpost` exige le magic `SFP2`.
Depuis ee3f4a0, `SfxPostWriterV2::finish` émet `SFP3` **quel que soit** le
pipeline — le DAG de merge v2 (`sfx_dag.rs`, nœud `validate_sfxpost`, actif
sauf `LUCIVY_SKIP_VALIDATION=1`) rejette donc sa propre sortie.

Reproduit : `Index::create_in_ram_sfx2`, deux commits, `merge()` →
`node 'validate_sfxpost' failed: sfxpost: missing SFP2 magic`.

Concerné : tout index v2 existant sur lequel on écrit encore (rag3weaver,
`playground/dataset.luce` qui est v2). Aucun test ne merge un index v2, d'où
le vert.

Correctif : accepter `SFP2` **ou** `SFP3` dans `validate_sfxpost` (ou déléguer
à `SfxPostReaderV2::open_slice`), et **ajouter un test de merge sfx2**.

### 1.2 MAJEUR — les accès SFP3 sont O(num_docs) : « ce qui reste est inhérent » est faux

`src/suffix_fst/sfxpost_v2.rs:414-420` : `read_ordinal_header` (V3) décode
les `num_docs` triplets d'en-tête **à chaque appel** pour trouver le début des
payloads. Le commentaire dit « once, at open » ; c'est appelé par `has_doc`,
`entries_for_doc`, `entry_at`, `doc_freq` — donc à chaque lookup. La recherche
binaire sur checkpoints arrive *après* un scan complet.

Mesuré (debug, 2 000 `has_doc` sur un ordinal) : 1 k docs → 245 ms, 10 k →
2,5 s, 100 k → 25 s, 1 M → 249 s. Linéaire.

C'est très probablement **la** cause des 12 % de régression native attribués
à SFP3 (04 §2, 05 §6), et les trois « tentatives de récupération » n'ont pas
touché ce point. Sur un ordinal fréquent (`kmalloc` couvre des milliers de
docs) et une fuzzy qui appelle `entry_at` une fois par match, c'est aussi un
candidat sérieux pour une partie du facteur navigateur.

Correctif : écrire un varint `headers_len` après `num_docs` dans le writer
(ligne ~156, 1-5 octets par ordinal) et remplacer la boucle par ce saut. Puis
**remesurer** le panel natif à chaud avant de reconduire la conclusion « échange
RAM contre CPU ».

### 1.3 MAJEUR — un commit incomplet : la division du budget SFX n'est dans aucun commit

`src/indexer/indexer_actor.rs` est **modifié, non commité** (mtime 16:14, entre
a2716a9 et 24b741a) : `sfx_budget()` divisé par `LUCIVY_WRITER_THREADS`.
Le message de 24b741a (« the SFX budget is shared between threads »), 06 §4 et
07 §6 (« global, divisé par les threads ») décrivent ce code. Les mesures de
24b741a ont tourné avec. Le dépôt, lui, a toujours un budget **par thread**.

À faire : relire la modif (le `unwrap_or(if wasm {1} else {1})` est à
simplifier ; et le natif a `threads = auto` par défaut, donc la division par 1
ne borne pas le total natif — acceptable, mais le commentaire prétend le
contraire), puis commiter.

Un fichier `tests/zz_review_probe.rs` a existé pendant la relecture ; il est
supprimé. Le working tree ne contient que la modif ci-dessus.

---

## 2. Défauts réels, moins urgents

### 2.1 L'export LUCE peut manquer des fichiers (76a7e1e)

`read_live_files` lit d'abord les `SegmentMeta` (T0), puis `meta.json` (T1) et
les fichiers ; `export_to_snapshot` ne vérifie que `has_uncommitted()`, il
**n'attend pas les fusions**, qui tournent en arrière-plan après un commit.
Une fusion qui aboutit entre T0 et T1 donne un `meta.json` nommant un segment
dont les fichiers ne sont pas exportés. Et un `NotFound` (segment retiré et
ramassé entre le listing et la lecture) est **ignoré en silence** — ce qui
était bénin quand `list_files` était un sur-ensemble ne l'est plus depuis que
`list_files_for` est exact et que `test_touched_bytes` affirme qu'aucun
fichier listé n'est absent.

Correctif : lire `meta.json` **une fois**, dériver la liste des fichiers de ce
contenu-là, et traiter `NotFound` comme une erreur (ou recommencer sur un
nouveau `meta.json`). Idéalement drainer les fusions avant.

### 2.2 Le défaut de 2 Go ne tient pas la démonstration de 10 000 documents

`LUCIVY_RAM_INDEX_MAX` = 2 Go en wasm ; l'index 10 k fait 2 600 Mo. **Sans
`?rammax=`, il est `Streaming`** : avertissement rouge, preload sauté, pas de
cache relevé, recherches par lots. Toutes les mesures « tout en RAM, 567 ms »
(04 §10, 05 §1, 07 §8) ont été faites avec le bouton, et aucun doc ne le dit.

L'objectif énoncé était « 10 k docs en RAM sans contournement ». Il est
atteint techniquement, pas dans la configuration par défaut. Décision à
prendre : monter le défaut (3 Go ? — 2 727 Mo + résidu d'indexation
dépasse 4 Go, mais une page qui ne fait que servir a tenu 2 600 Mo sur trois
passages) ou assumer que la démo pose le paramètre.

### 2.3 Panic possible sur `num_docs` corrompu (SFP3)

`sfxpost_v2.rs:403-406` : `num_docs` lu en u64 puis `as usize`,
`checkpoints_for(num_docs) * 12` déborde (panic en debug, wrap en release,
troncature sur wasm32). WSP3 utilise `read_varint_u32`, SFP3 non. Borner par
`data.len()`. Même famille : `decode_vint` (lignes 698-709) shifte au-delà de
32 bits sur 6 octets de continuation — pré-existant, mais le commentaire
589-594 affirme depuis ee3f4a0 que ce chemin est sûr sur fichier corrompu.

### 2.4 `ensure_opfs_mounted(8)` bloque 7,2 s par point d'entrée quand OPFS est absent

Pauses 200 → 1 600 ms cumulées = 7,2 s **à chaque** `create` / `open` /
`open_begin` / `import_snapshot` tant que le montage échoue. Après un échec au
démarrage *et* un échec à la première entrée, `OPFS_DISABLED` devrait se poser.

### 2.5 Aucune contre-pression sur la file de finalisation (d661588)

`pending_finalize` est une file non bornée : avec des segments coupés tous
les ~166 docs, si une finalisation (construction de FST, ~400 Mo de pic) dure
plus que l'indexation de 166 docs, plusieurs tournent en même temps. Mesuré
OK sur 10 k docs, **pas borné par construction**. Un plafond (attendre quand
`pending_finalize.len() >= N`) ferme le trou.

### 2.6 Recherche par lots : les requêtes v2 perdent les lots précédents

`SuffixContainsQuery` et `RegexContinuationQuery` implémentent
`prescan_segments` (qui **remplace** le cache) mais pas `prescan_segments_more`
(défaut → `prescan_segments`). En mode `Streaming` sur un index v2 de plus
d'un lot, seuls les segments du dernier lot répondent. Latent (un index v2
de plus de 1 Go en navigateur n'existe pas aujourd'hui), mais faux.

### 2.7 `with_capacity(n)` non borné dans WSP3

`word_sfxpost.rs:402-403` : `n` vient du fichier, seule borne ≈ 2× la taille
du fichier → jusqu'à 40× en allocation. Sur wasm32 c'est un abort. Borner par
`(end - entries) / 5`.

### 2.8 bb8985d n'a fait que la moitié du chemin

Les scratch buffers sont corrects (vidés, aucune donnée périmée), mais
`SfxPostWriterV2` alloue encore `docs: Vec<(u32, Vec<…>)>` — un `Vec` par
document par ordinal. « Bounded whatever the index » est vrai pour
`WordSfxPostWriter`, pas pour lui.

---

## 3. Docs : ce qui ne colle pas avec le code ou entre eux

| doc | problème |
|---|---|
| 04 §10, 05 §1, 07 §8 | « tout en RAM, 2 600 Mo » sans dire que `?rammax` est indispensable (§2.2) |
| 06 §4, 07 §6 | « budget SFX divisé par les threads » décrit du code non commité (§1.3) |
| 04 §2, 05 §6 | « ce qui reste est inhérent » pour SFP3 — non, c'est un scan O(n) (§1.2) |
| 03 §3, commentaire de `sfx_budget()` | « 117 segments pour 15 440 docs = 132 docs/segment, la taille quand ça marchait » — c'est un compte **après compactage** (01-journal, 00:53). La conclusion (ba48e60 a grossi les segments) tient par la mesure directe de 04 §3.1 (56 → 4 segments), pas par ce chiffre |
| playground `wasm-note` | médiane 317 ms (3ᵉ passage) ; 05 et 07 disent 281 (1ᵉʳ). Choisir |
| `build.sh` l.56 | « more threads is free for queries, which are CPU-bound » — c'est l'hypothèse que 1e95043 infirme, laissée dans le commentaire |
| 02 §7 | « bloom de trigrammes par shard » et « file d'admission » notés comme décision du matin ; **absents de 05 §5**, sans mention qu'ils sont abandonnés ou reportés |
| 04 §8 | « compactage navigateur avec les nouveaux formats : pas rejoué » — point de vigilance **non repris dans 05 §6** ; et depuis §1.1 c'est plus qu'une vigilance |
| 04 §1 vs §10 | le tableau de tête dit 893 ms / 10,5×, corrigé en §10 — acceptable pour un journal, mais 05 est le doc de référence, pas 04 |
| CLAUDE.md | « 1415 passed » → 1 428 ; le tableau « Packages publiés » dit lucivy-core 0.1.1 alors que le workspace est à 3.0.0 |
| 7697d52 | le bump `2.1.0 → 3.0.0` (Cargo.toml, CHANGELOG) est enfoui dans un commit « résidence » ; il aurait dû être seul |

Ce qui est **juste et précieux** dans les docs et mérite d'être conservé tel
quel : 03 §1 (preuves horodatées), 04 §9-11 (hypothèses infirmées, gardées
avec leurs chiffres), 06 §7, 07 §9.

---

## 4. Ce qui a été relu et jugé correct

- **ba48e60** (`WithFreqs` en v3) : les seuls lecteurs de positions sont
  `contains_scorer`, `phrase_weight`, `phrase_prefix_weight` (v2) et
  `TermWeight` avec `highlight_sink` — aucun n'est construit par `build_query`
  sur un index v3 ; `more_like_this` lit en `Basic`. Un index créé avant garde
  son schéma (positions), un `meta.json` sans `sfx_version` vaut 2 : pas de
  risque de GC des sidecars v2 par `list_files_for`.
- **40ee55f** (lots) : statistiques BM25 globales, `into_sorted_vec()` correct,
  pass 1/pass 2 cohérents, `Subset` bien propagé. Le sur-coût (chaque lot lu
  deux fois) est assumé en mode `Streaming` seulement.
- **7697d52** : accumulateur u64, `written_for`, `.del` conditionnel — la
  race GC est équivalente à celle de tantivy, qui sérialise dans le segment
  updater.
- **4e762bb** : cache d'en-tête 4 Ko correct, `read_direct` pour le reste.
- **d661588** : `mem_usage()` délibérément non modifié ; file de finalisation
  correcte (hors §2.5).
- **a2716a9** : `preload` ne fait rien en `Streaming` ; budget relevé à la
  taille exacte de l'index, LRU cohérent.
- **24b741a** : tas × threads, `LUCIVY_WRITER_HEAP` explicite honoré.
- **Encodeurs** : discriminateurs non ambigus (magic WSP3/SFP3, sentinelle
  `u32::MAX` puis `SIB2`), symétrie lecteur/écrivain champ par champ, deltas
  sur entrées triées par les writers, `d_doc == 0` sans collision, checkpoints
  cohérents aux bornes (n = 1, 32, 33), fichiers tronqués refusés, chemin de
  merge v3 (`sfx_dag_v3.rs`) via les mêmes Reader/Writer.
- **b567506**, **b316520** (hors §2.4), **045e2ef**, **1e95043**, **5c712cf** :
  rien à signaler.
- Le `lucivy.wasm` commité (`playground/pkg` = `bindings/emscripten/pkg`,
  même md5) date de 24b741a, dernier commit touchant le code.

`cargo check --workspace` échoue sur `pyo3-ffi` (environnement Python de la
machine), pas sur le code du jour.

---

## 5. Ordre proposé

1. §1.1 — accepter SFP3 dans `validate_sfxpost` + test de merge sfx2.
2. §1.2 — `headers_len` dans SFP3, remesurer le panel natif, réécrire 04 §2 /
   05 §6.
3. §1.3 — relire et commiter `indexer_actor.rs`.
4. §2.1 — export LUCE sur un seul `meta.json`, `NotFound` fatal.
5. §2.2 — trancher le défaut de `LUCIVY_RAM_INDEX_MAX`, et le dire dans 05.
6. Rejouer une **indexation + compactage navigateur** (04 §8) — les formats
   ont changé et le merge v2 est cassé ; le merge v3 n'a été rejoué qu'en
   natif.
7. Le reste de §2 et les corrections de §3.

Rien de ceci ne remet en cause les résultats mesurés (comptes identiques,
1,57× du preload, parallélisme infirmé) : ce sont les conclusions tirées de
la régression SFP3 et l'état du dépôt qui sont à corriger.

---

## 6. Corrections appliquées le soir même

Décision : rattraper, pas revenir en arrière — les défauts sont localisés et
tout ce qui a été mesuré tient.

| point | fait |
|---|---|
| §1.1 | `validate_sfxpost` accepte `SFP2` et `SFP3` ; tests `merge_tests::a_v2_index_still_merges` / `a_v3_index_merges` |
| §1.2 | SFP3 porte `headers_len` après `num_docs` ; le lecteur saute aux payloads sans décoder. **Format changé sur place** (jamais publié) : les index SFP3 de l'après-midi sont à reconstruire. Remesuré en natif sur l'index 10 k reconstruit : 93 → 79 ms/requête, médiane 59 → 49, −14 %, 21 comptes identiques |
| §1.3 | `sfx_budget()` divisé par les threads d'écriture, aligné sur le vrai défaut natif (`min(available_parallelism, 8)`), commité |
| §2.1 | `read_live_files` lit `meta.json` une fois et en dérive tout ; `NotFound` relance depuis le nouveau `meta.json`, trois fois, puis erreur |
| §2.2 | défaut `LUCIVY_RAM_INDEX_MAX` monté à **3 Go** en wasm (décision de Lucie, soir) |
| §2.3 | `num_docs` et `headers_len` lus en `read_varint_u32` et bornés par le bloc ; `decode_vint` s'arrête à 5 octets ; test `v3_refuses_corrupt_block_counts` |
| §2.4 | `OPFS_DISABLED` posé après deux tours de tentatives échoués |
| §2.5 | `LUCIVY_MAX_PENDING_FINALIZE` (1 wasm / 4 natif), attente sur la plus ancienne |
| §2.6 | `prescan_segments_more` sur `SuffixContainsQuery` et `RegexContinuationQuery` (union au lieu du remplacement) |
| §2.7 | `with_capacity` borné par la taille du bloc |
| §2.8 | plus de `Vec` par document dans `SfxPostWriterV2` : une passe sur les entrées triées |
| §3 | 03, 04 §12, 05, 06, 07, `build.sh`, note du playground, CLAUDE.md corrigés |

**Réindexation navigateur rejouée** (05 §5.0) : deux morts en fusion de
fond — 603 Mo dans une fusion de niveau 2 (~10 000 docs), puis un realloc
de 2 Mo dans une fusion que le preload avait privée d'espace. Corrigé par
`LUCIVY_MAX_MERGED_DOCS` (2 000 en wasm) et `wait_merges_quiet()` avant tout
preload/drain. Résultat, session fraîche sans paramètre : **551 ms/requête,
médiane 244, 21/21 identiques** (après-midi : 567 / 281 avec `?rammax`).

**Puis l'allocateur** : le profil du panel montrait un écart strict/relaxed
de 14× sur le même terme, tout en CPU ; `-sMALLOC=mimalloc` (un drapeau) :
**551 → 188 ms/requête**, 172 avec 8 threads, médiane 244 → 97, ratio au
natif plat à 2-3×. La conclusion « le parallélisme ne paie pas » (04 §9-11,
05 §4) était un artefact du verrou global de `dlmalloc`.

**Puis les segments et l'indexation** : fusions à 800 en wasm (48 segments,
8 threads pleins) → **124-133 ms/requête, médiane 69-92** ; permis de build
à 2 et 512 docs en file → **indexation 10 k en 55 s** sous mimalloc. Ma
propre erreur en route : une attente bloquante dans un handler d'acteur,
que luciole refuse — corrigée par des permis coopératifs.

Faits ensuite, à la demande de Lucie : `entry_count` est un `u32` de bout
en bout (le lecteur ne plafonne plus à 65 535 ; V2 lit son `u16` et
l'élargit), les en-têtes SFP3 se lisent en `read_varint_u32` (une valeur
trop large arrête la lecture au lieu d'être tronquée), et les trois writers
refusent d'écrire un sidecar dont les offsets ne tiennent pas sur 32 bits
plutôt que d'écrire une table fausse.
