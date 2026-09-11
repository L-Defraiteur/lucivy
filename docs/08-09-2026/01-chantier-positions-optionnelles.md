# Chantier 4.1 — index sans positions (`positions: false`), spans par relecture

Branche `v4.1` depuis `main` (8 septembre 2026). Une option de création, jamais
le défaut ; le format reste 4.x (un index existant s'ouvre tel quel, un index
bâti avec l'option ne s'ouvre pas en 4.0.x — le contrat de `derived_in_ram`).

## 1. Ce qu'on vise, mesuré

Index du noyau (5 156 Mo, 857 Mo de texte, `docs/07-09-2026/09`) :

| | aujourd'hui | sans positions |
|---|---|---|
| `.sfxpost` | 771 Mo (167 M d'entrées) | 168 Mo (76 M de paires terme-doc + tf) |
| `.word_sfxpost` | 626 Mo | 128 Mo |
| `.posmap`, `.word_pos_map`, `.sibling_v3` | 1 667 Mo | 0 |
| reste (FST, `.termtexts`, `.gmap`, `store`) | 1 948 Mo | 1 948 Mo |
| **total** | **5 156 Mo, ×6,0** | **≈ 2 250 Mo, ×2,6** |

Le prix : tout ce qui a besoin d'une position (adjacence entre jetons, spans,
fuzzy, regex bornée) se vérifie en relisant le texte stocké des candidats.
`stored: true` devient obligatoire pour le champ.

## 2. Où les positions servent aujourd'hui (cartographie du 8 septembre)

- **Phase FST** (`plan.rs`, `fst_walk.rs`, `falling_walk_v3`, `cross_token_chain_v3`,
  `sibling_table` à la lecture) : **aucune position**. Elle produit des ordinaux et
  des chaînes d'ordinaux. Inchangée.
- **Résolution** (`resolve.rs`) : `resolve_single_v3` émet un `MatchV3` par
  occurrence (position = ancre du span) ; `resolve_chains_impl` vérifie
  l'adjacence stricte par `posmap.ordinal_at(doc, pos + 1)` — seule la première
  liste lit les postings, le reste lit `.posmap`.
- **Composite** : `find_multi_token_v3` (positions consécutives), les chaînes
  de trigrammes du fuzzy (`build_trigram_chains`, distances en positions),
  `rebuild_window_opts` (fenêtre rebâtie depuis `posmap` + `termtexts`, ancrée
  par un `byte_at`), `verify_candidates`.
- **Placement** : `orchestrator::place_spans` — le seul endroit qui produit des
  octets, deux `byte_at` par match. Le sink d'highlights ne voit que des octets.
- **BM25** : tf = nombre de `MatchV3` par document (donc des occurrences
  énumérées par position) ; df = `SfxPostReaderV2::doc_freq`, sans position.
- **Précédent doc-only** : `regex_verified.rs:161-181`, branche « motif non
  borné ou sans littéral » : documents candidats rebâtis entiers et balayés
  par `find_iter`, spans par `back[]`. Exact par construction. Sa source est
  `posmap` + `termtexts` ; elle devient le docstore.
- **Danger** : `derived_in_ram` rebâtit les dérivés *depuis les postings*
  (`derived.rs`). Sans positions il n'y a rien à rebâtir et rien à dériver : les
  deux options sont exclusives (refusées ensemble, comme `shared_dictionary`
  contredit par `sfx_version`).

## 3. Le design

**Layout.** `SFP6` : par ordinal, `(delta doc, tf)` en varint, blocs et points
de contrôle comme `SFP5` ; `WSP6` : idem pour les mots (plus de `first`/`last`,
plus de `tail_off`). Les lecteurs répondent `has_positions()` comme ils
répondent `has_byte_spans()`. Rien de dérivé n'est écrit ni rebâti
(`components_for` sans `posmap`, `word_pos_map`, `sibling_v3`). FST,
`.termtexts`, `.gmap`, docstore inchangés.

**Requête, trois régimes, tous branchés sur `reader.has_positions()`.**

1. *Un seul jeton, séparateurs stricts* (`contains "mutex_lock"`, mot entier,
   préfixe) : l'ensemble des documents = union des listes des ordinaux trouvés
   par la FST — **exact sans relecture**, compte en millisecondes ; tf = somme
   des tf des ordinaux ; spans par relecture des seuls documents qu'on
   affiche (le sink d'highlights sait déjà refaire une passe restreinte aux ids
   du top-k : `LUCIVY_HIGHLIGHT_SPAN_CAP` et sa relance).
2. *Chaînes* (séparateurs relâchés, plusieurs jetons, `find_multi_token_v3`) :
   candidats = intersection des listes des ordinaux de la chaîne — un
   sur-ensemble (comme le AND de trigrammes de tantivy, mais sur des jetons
   exacts, donc étroit) — puis **vérification par relecture** du texte stocké
   avec un apparieur en espace d'octets (le harnais en a un :
   `grep_spans` / séparateurs relâchés), qui rend compte, tf et spans exacts.
3. *Fuzzy et regex* : candidats par la FST comme aujourd'hui, vérification en
   espace d'octets par `fuzzy_spans` (déjà sans position) et `find_iter`
   (précédent ci-dessus), source = docstore.

Les matches sortent **déjà placés en octets** : `place_spans` et
`place_overlap_overflow` sont sautés. Le harnais vérifie les deux layouts avec
le même panel ; le contrat est le même, seul le temps des spans change.

**Coût attendu.** Comptes exacts et rapides pour le régime 1 (la majorité des
requêtes du panel) ; le régime 2 paie la relecture des candidats (ordre de
100 ms pour 5 000 documents, tantivy fait 96 sur 5 145) ; le régime 3 aussi.
Une requête sans highlights ne relit rien en régime 1.

## 4. Les étapes, chacune avec sa mesure

1. **Layout + plomberie** : `positions: bool` dans `SchemaConfig` →
   `IndexSettings` → `components_for` → écrivain (`sfx_dag_v3.rs`) → lecteurs
   (`has_positions`) ; `derived_in_ram` refusé avec ; `list_files_for` ;
   snapshot/sync suivent. Mesure : taille de l'index 10 000 et noyau dans les
   deux layouts, les lecteurs s'ouvrent, le panel refuse proprement (pas encore
   de chemin de requête) avec un message net.
2. **Régime 1** : ensembles de documents depuis les listes, tf depuis les
   postings, spans par relecture du top-k. Mesure : lignes strict/mot
   entier/préfixe du panel 10/10, temps de compte et temps des spans à part.
3. **Régime 2** : intersection + apparieur d'octets (déplacé du harnais dans
   le moteur, testé contre lui). Mesure : lignes relâchées et multi-jetons du
   panel, nombre de candidats relus par requête.
4. **Régime 3** : fuzzy et regex sur le docstore. Mesure : lignes fz1/fz2/rx.
5. **Le noyau entier**, `V3_POSITIONS=0` dans le harnais, A/B temps contre le
   layout par défaut, tableau dans le rapport.
6. Bindings (`positions` dans les quatre), README, CHANGELOG, playground
   (`?nopos`), et la comparaison mise à jour.

Règle du chantier : jamais un chiffre sans le panel vert à côté ; le layout par
défaut ne bouge pas d'un octet (les tests existants le prouvent).


---

## 5. État au 11 septembre — étapes 1 à 3 faites (branche `v4.1`, `2f5c6b0`, `c481cf0`)

### Ce qui a changé dans le design en le codant

- **La table des voisins (`.sibling_v3`) n'est pas écrite non plus.** La
  cartographie la classait « sans position », mais elle n'est qu'un
  *complément* de la marche FST (`if ctx.has_sibling_chains()`). Sans elle,
  les chaînes sont bâties par la FST seule depuis **toutes** les têtes (celles
  de la marche descendante et celles des candidats FST), en avant — ce que le
  pipeline à positions fait déjà quand il ne peut pas vérifier une tête en
  arrière. C'est un sur-ensemble ; la vérification tranche.
- **Les trois fichiers non écrits sont exactement les trois dérivés.**
  `IndexSettings::skips_derived_files()` = `derived_in_ram || !positions` sert
  à toutes les listes de fichiers (snapshot, sync, GC, tailles) et aux deux
  écrivains ; la reconstruction à l'ouverture ne se déclenche que sur
  `derived_in_ram`.
- **La vérification est la vérité terrain, pas une imitation.** Les
  prédicats du harnais (`grep_spans`, `filter_boundaries`, `fuzzy_spans`,
  `jaro_spans`, `find_iter`) tournent sur les valeurs stockées : mêmes
  replis Unicode, mêmes occurrences chevauchantes, mêmes bornes, mêmes
  spans. Un index sans positions répond donc par construction ce que la
  vérité terrain dit.
- **Le fuzzy garde le générateur du pipeline à positions**
  (`composite::fuzzy_generator` : pièces du pigeonhole, ou les n-grammes les
  plus rares) ; ses littéraux passent par la même génération de candidats.
  Une requête que le générateur ne sait pas découper ne trouve rien, comme
  avec positions.
- **`fuzzy_spans_long`** : `fuzzy_spans` bâtit une matrice aiguille × texte ;
  sur une valeur de 1 Mo et une aiguille de 10 octets, 44 Mo par fil — trop
  pour le navigateur. La version longue rend les mêmes occurrences avec la
  dernière ligne calculée colonne par colonne, puis le même retour arrière
  sur une matrice locale (une cellule `(i, j)` ne dépend d'aucun octet avant
  `j − 2i`, et un retour arrière recule sans monter au plus `d` fois).
  Égalité vérifiée sur 3 000 cas aléatoires et sur un long texte.
- **Fusions.** Une source `SFP6` / `WSP6` est recopiée en réémettant chaque
  occurrence à une position fictive `0..tf`, que l'écrivain « documents
  seulement » recompte en `tf`. Le total des occurrences d'un corpus (qui ne
  dépend d'aucune segmentation) est vérifié égal à celui d'un index avec
  positions, en v3 et en dictionnaire.

### Mesuré

Index de référence, 10 000 fichiers du noyau (Linux 7.2), dictionnaire
partagé, 160 segments (commits tous les 500, huit fils d'indexation) :

| | avec positions | sans positions |
|---|---|---|
| total | 352,3 Mo | **220,9 Mo (−37 %)** |
| `dict-*.sfx` | 104,8 | 104,8 (47 % de l'index) |
| `.sfxpost` | 49,7 | 19,0 |
| `.word_sfxpost` | 32,1 | 15,7 |
| `.posmap`, `.word_pos_map`, `.sibling_v3` | 26,5 + 32,4 + 25,3 | 0 |
| `dict-*.termtexts`, `.gmap`, `store` | 33,0 + 21,3 + 18,7 | idem |
| indexation | 8,4 s | 8,0 s |

À 10 000 fichiers la FST du dictionnaire pèse presque la moitié de l'index
sans positions : le gain est plus grand sur le noyau, où les positions
pèsent plus (estimé ×2,7 le texte, à mesurer).

**Panel de vérité terrain** (`v3_ground_truth_demo`, 10 requêtes, comptes
et spans comparés au balayage des fichiers) : **10/10 dans les deux
layouts**. Temps d'un seul passage à froid, à confirmer par des passes
répétées :

| requête | avec positions | sans |
|---|---|---|
| `mutex_lock` strict / relâché | 15,5 / 10,5 ms | 2,6 / 2,5 ms |
| `spin_lock` strict | 6,0 | 2,3 |
| `sched` mot entier / sous-chaîne | 3,2 / 2,6 | 4,7 / 3,4 |
| `printk` début de mot | 2,6 | 2,8 |
| `schdule` fz1 | 5,4 | **24,6** |
| `regsiter` fz2 | 40,7 | 42,1 |
| `spin_lock_[a-z]+` | 5,4 | 2,8 |
| `schdule` Jaro-Winkler | 7,6 | **163,4** |

Les littérales ne perdent rien : relire quelques dizaines de documents
coûte moins que résoudre les chaînes par `.posmap`. Le fuzzy et surtout le
Jaro-Winkler perdent, parce que la vérification balaie la valeur entière de
chaque candidat (programmation dynamique sur tout le texte).

### Vérifié

- Tests unitaires : `SFP6`, `WSP6` (et `to_docs_only` octet pour octet),
  prédicat de la vérité terrain, `fuzzy_spans_long`.
- `lucivy_core/tests/test_positions_off.rs` (300 fichiers du noyau, v3 et
  dictionnaire, commits tous les 40 et fusions de la politique) : fichiers et
  `meta.json` ; fréquences à travers les fusions ; refus de configuration ;
  **13 requêtes littérales** (strict, relâché, mot entier, début de mot,
  séparateurs dans l'aiguille, casse, `de`, `pin_loc`) et **8 fuzzy / regex**
  (fz1, fz2, à travers les jetons, Jaro-Winkler, regex avec littéral, non
  bornée, sans littéral) : mêmes documents, mêmes spans, **mêmes scores**
  que l'index avec positions.
- Les suites complètes (lib avec et sans features par défaut, `lucivy-core`)
  sont vertes après l'étape 1 : le layout par défaut n'a pas bougé.

### Incident

`/tmp` est vidé de ce qui a plus de 10 jours (`/etc/tmpfiles.d/tmp.conf`) :
le noyau du 28 août et les index de bench y avaient disparu, et le premier
test d'intégration a tourné sur le corpus synthétique de repli sans le dire.
Le noyau vit maintenant dans `~/lucivy_bench/linux-7.2`, `/tmp/lucivy-cmp` et
`/tmp/lucivy-cmp-90k` sont des liens vers lui ; les index de mesure vont dans
`~/lucivy_bench/`.

### Reste

Le panel de vérité terrain sur 10 000 fichiers et ses temps (en cours),
l'A/B de temps sur 30 000, la taille du noyau entier, puis les bindings
(`positions` dans les quatre), le playground (`?nopos`), les README et le
CHANGELOG.
