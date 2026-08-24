# sparse_vector — comparaison OLD (`search_context`) vs NEW (`wand`)

Revue en lecture seule, 24-08-2026. OLD = `posting_list.rs`, `posting_list_common.rs`,
`search_context.rs`, `scores_memory_pool.rs`, `top_k.rs` (dérivé de Qdrant), utilisé par
`index.rs`, `mmap_index.rs`, `handle.rs`. NEW = `src/wand/` (réécriture, pas encore branchée).
Benchmark : `sparse_vector/tests/bench_wand_compare.rs` (`#[ignore]`, APIs publiques seulement).

## 1. Matrice de comportements

Légende : **GAP** = à corriger avant de remplacer OLD ; **SIMPL** = simplification acceptable ;
**AMÉL** = amélioration ; **ÉQUIV** = même comportement.

| Aspect | OLD | NEW | Verdict |
|---|---|---|---|
| Condition de pruning | Seulement si top-k plein **et** si le seuil a changé depuis le dernier essai ; une seule liste (la plus longue) ; seulement si sa tête est strictement avant toutes les autres ; saute jusqu'à la tête suivante des autres listes | À chaque fenêtre : tri des lanes par id courant, accumulation des plafonds, pivot ; toutes les lanes avant le pivot sont `seek`ées d'un coup ; arrêt total quand la somme des plafonds ne peut plus battre le seuil | AMÉL (algorithme WAND complet) — mais coût par fenêtre, voir §3 |
| Pruning désactivé | Pour toute la requête dès qu'un poids de requête est `< 0` (ou NaN) ; flag statique `reliable_max_next_weight` | Par lane : un poids négatif rend la lane non bornée (`lower_bound = -inf`), les autres lanes continuent d'élaguer ; `SearchOptions::pruning=false` pour un mode exhaustif | AMÉL (exact, testé contre brute force) |
| Fin de recherche à une seule liste | Chemin rapide : toute la fin de la dernière liste est scorée sans pruning ni test de seuil avant push | Pas de cas spécial ; le pivot sur une lane unique termine dès que son plafond passe sous le seuil | AMÉL |
| Seuil k-ième | Min-heap `(score, id)` ; `f32::MIN` tant que pas plein ; test `score > seuil` sur chaque slot **avant** le filtre et le push | Min-heap `Hit` ; `threshold()` = `None` tant que pas plein, `Some(+inf)` pour k=0 ; `offer()` appelé pour **chaque** record vu, la comparaison (OrderedFloat + id) se fait dans le sink | ÉQUIV sémantique ; GAP perf (≈ +28 µs/requête, §3) |
| Fenêtre / mémoire | Batch fixe de 10 001 ids ; `Vec<f32>` ≈ 40 Ko depuis le pool | Fenêtre configurable, défaut 1024, plafond 4 Mi slots (20 Mo) ; `scores` f32 + `seen` bool = 5 o/slot | AMÉL (configurable) ; défaut 1024 sous-optimal (§3) |
| Plafond `max_next_weight` | Max **exclusif** du suffixe (dernier = `-inf`) ; le pruning utilise `max(weight, mnw)` ; le fichier mmap stocke la valeur exclusive telle quelle | `tail_max` **inclusif** ; `MmapCursor` replie `max(weight, max_next_weight)` : accepte les deux conventions ; `check_ceilings` vérifie l'invariant | ÉQUIV ; un fichier écrit avec des plafonds inclusifs reste lisible par OLD (il replie aussi) |
| Filtre (`allowed_ids`) | Appelé seulement pour les records qui battent déjà le seuil | Appelé pour chaque record vu, avant `offer` | GAP perf avec un `HashSet` (≈ 30 000 lookups/requête au lieu de quelques dizaines) ; sémantique identique |
| Ids supprimés / absents | Pas de tombstones ; `remove` retire des postings ; `handle.rs` bascule sur la RAM dès `dirty` | Idem, aucune notion de suppression logique | ÉQUIV |
| Score exactement `0.0` | Rejeté dans `advance_batch` (`score != 0.0`) **mais** conservé dans le chemin rapide dernière liste : incohérent selon le batch où tombe le record | Conservé (`seen`) : un poids stocké `0.0` ou une annulation exacte renvoie le record avec score `0.0` | Changement de comportement : le test `handle.rs::blob_store_survives_cache_cleanup` attend 4 résultats (doc de poids 0 exclu), NEW en rendrait 5 → décision nécessaire (P1) |
| Poids de requête `0.0` | Lane conservée (contribue 0) | Lane écartée à la construction du `Frontier` | AMÉL |
| Égalités et ordre | Tri final par score seul (stable sur l'ordre interne du heap) : ordre entre ex æquo non spécifié ; à l'éviction, l'id **le plus bas** parmi les pires ex æquo est sorti | Score décroissant puis id croissant, déterministe ; l'éviction sort l'id le plus haut ; identique au brute force `(score desc, id asc)` | AMÉL |
| `top == 0` | Parcourt tout l'index (seuil `f32::MIN`, rien n'est élagué) puis rend `[]` | `threshold = +inf` → `Skip::Nothing` dès la première fenêtre (sauf lane négative) | AMÉL |
| `top > candidats` | Rend tous les candidats | Idem | ÉQUIV |
| Dims dupliquées dans la requête | Une lane par occurrence : contribution comptée deux fois | Idem (les plafonds restent cohérents) | ÉQUIV (à documenter) |
| Dim inconnue / posting vide | Ignorée (`dim_map`, `pl.is_empty()`, `mmap.iter → None`) | Ignorée (`cursors → None`, `Frontier::new` filtre les curseurs épuisés) | ÉQUIV |
| Pool de scores | `ScoresMemoryPool` (Mutex parking_lot, 16 buffers gardés) dans `SparseIndex`, handle rendu au drop ; thread-safe mais `handle.rs` sérialise de toute façon derrière un `Mutex<Inner>` | `Scratch` fourni par l'appelant (`&mut`) ; `search()` en alloue un par appel (≈ 5 Ko à 1024) | SIMPL acceptable ; le wrapper devra garder un `Scratch` par thread (`thread_local!`) |
| Itérateur | `skip_to(id)` : `Some` seulement si l'id exact existe, sinon se place sur le suivant et rend `None` ; `for_each_till_id` inclusif ; `last_id` ; `current_index` ; `len_to_end` ; `skip_to_end` | `seek(id)` rend l'élément `>= id` ; `drain_through` inclusif ; `last_id` ; `remaining` ; `exhaust` ; + `upper_bound` / `lower_bound` / `is_exhausted` ; `position()` seulement sur `SliceCursor` | ÉQUIV ; `wand/mmap.rs` **dépend** de `posting_list_common::PostingListIter` et de `MmapPostingListIterator` (P1) |
| Upsert / delete | Recalcul du préfixe O(n) ; builder panique en debug sur doublon ; `upsert` sans changement de poids ne propage pas | Recalcul du préfixe O(n) ; builder déduplique (dernier gagne) ; `upsert`/`delete` rendent l'ancien poids ; `items_mut` + `recompute_tail_max` ; pas de sortie anticipée | ÉQUIV (les deux : 3,1 s pour 50 000 inserts en ordre croissant, §3) |
| Persistance | serde legacy (`Vec<Vec<(u64,f32)>>`), `write_mmap_file(&[PostingList])`, `load_posting_list → PostingList` | Aucune : ni serde, ni écriture, ni chargement de `Postings` | GAP (P0 pour le branchement) |
| Tests | ~5 posting_list, 2 top_k, 13 index, 12 handle ; pas de vérité terrain aléatoire | 22 tests : brute force avec/sans pruning, poids négatifs, tailles de fenêtre, filtre, k=0, égalités, invariants sous mutation aléatoire, adaptateur mmap | AMÉL |

## 2. Points de correction détaillés

- **Exactitude** : sur 200 requêtes × 12 configurations (uniformes et asymétriques), OLD et NEW
  rendent exactement les mêmes ids et scores (écart < 1e-4, en pratique 0) que la vérité terrain ;
  aucune différence d'ordre sur ex æquo n'est apparue (poids flottants aléatoires, égalités improbables).
- **Marge d'arrondi** : NEW borne en f64 avec 8 ulps de marge (`can_beat`) ; OLD compare en f32 sans
  marge mais son élagage ne concerne que des records mono-lane (score = un seul produit), donc exact.
  Les deux exigent un dépassement strict du seuil.
- **Pruning OLD faible** : « seuil inchangé → pas d'essai » et « une seule liste par batch » font que
  OLD n'élague quasiment jamais ; NEW élague davantage mais, sur ce corpus, les plafonds de suffixe
  globaux restent proches de 1,0 jusqu'à la fin des listes : 0,4 % des postings évités (99,6 % consommés
  à 1024, 98,3 % à 256, 100 % à 16384). Même avec des poids asymétriques (`BENCH_SKEW=1`, u⁶) : 0,2 %.
  Le WAND à plafond global n'aide pas sur des listes longues ; il faudrait des plafonds par bloc
  (block-max), ce que l'abstraction `upper_bound()` du curseur permet sans toucher à la boucle.

## 3. Performance (release, 50 000 records, 2 000 dims, 30 nnz, 200 requêtes de 10–40 dims, top-10)

Listes les plus longues : 24 779, 12 291, 9 824, 8 267, 7 484 ; médiane 539 ; 45 178 postings
touchés par requête en moyenne. Trois exécutions, écarts < 3 %. Valeurs de la 2ᵉ exécution (µs).

| Configuration | médiane | moyenne | p90 |
|---|---|---|---|
| OLD RAM (`SparseIndex::search`, batch 10 001) | 152,0 | 152,9 | 188,5 |
| OLD mmap (`search_mmap`) | 152,8 | 152,2 | 182,9 |
| NEW RAM fenêtre 1024, pruning (défaut) | **204,0** | 200,5 | 234,7 |
| NEW RAM fenêtre 1024, sans pruning | 178,2 | 177,5 | 209,2 |
| NEW RAM fenêtre 256 | 284,0 | 282,0 | 354,3 |
| NEW RAM fenêtre 4096 | 178,2 | 177,4 | 209,0 |
| NEW RAM fenêtre 16384 | 189,0 | 187,1 | 219,2 |
| NEW RAM fenêtre 65536 | 196,1 | 194,0 | 227,7 |
| NEW mmap fenêtre 1024 (`MmapCursor`) | 213,1 | 206,0 | 235,5 |
| Expérience : sink avec pré-test f32, fenêtre 1024 | 169,0 | 170,8 | 205,5 |
| Expérience : sink avec pré-test f32, fenêtre 4096 | **149,8** | 152,1 | 188,7 |
| Expérience : pré-test f32, fenêtre 4096, sans pruning | 145,3 | 146,6 | 183,6 |

Construction : `SparseIndex::insert` 3 138 ms ; `PostingsBuilder` 23 ms ; `Postings::upsert` 3 075 ms
(les deux chemins incrémentaux réécrivent tout le préfixe des plafonds à chaque append).

Décomposition du surcoût NEW (défaut 204 µs vs OLD 152 µs) :
1. **`offer()` par record vu** (≈ 30 000/requête, comparaison OrderedFloat + id dans le heap) :
   ≈ 28 µs — le sink avec pré-test `score <= worst` ramène 178 → 150 à fenêtre 4096.
2. **Fenêtre 1024 trop petite** pour des ids denses : 49 fenêtres au lieu de 13 à 4096 ; chaque fenêtre
   paie `retire_exhausted` + `skip_below` (tri des lanes) + `min_id` + `max_last_id` + deux memsets :
   ≈ 25 µs (204 → 178). À 256 c'est +80 µs. Au-delà de 4096, le balayage de slots vides et la
   perte de granularité de pruning reprennent le dessus (16384 : 189 ; 65536 : 196).
3. **Pruning à chaque fenêtre** : ≈ 25 µs à 1024 (204 vs 178 sans pruning) pour 0,4 % d'économie ;
   OLD ne retente que si le seuil a changé.
4. La boucle de scoring elle-même (`drain_through` + `seen`) est au niveau d'OLD ou légèrement mieux
   (145 vs 152 sans pruning ni offer par record). `MmapCursor` coûte ≈ +9 µs (closure `for_each_till_id`
   + `refresh` par fenêtre, `advance` via `for_each_till_id`).

## 4. Verdict

NEW est plus mûr **algorithmiquement** (pivot WAND complet, poids négatifs exacts, ordre déterministe,
k=0 en O(1), invariants vérifiés, 22 tests avec vérité terrain) et plus propre (pas de pool global,
curseur abstrait, adaptateur mmap tolérant aux deux conventions de plafond). OLD est plus mûr
**opérationnellement** : branché, persisté, et 25–35 % plus rapide dans sa configuration par défaut
sur ce corpus. Le remplacement est justifié une fois les points suivants traités.

**P0 — bloquant**
- Persistance : `SparseIndex` doit porter des `Vec<Postings>` avec serde legacy (`Vec<Vec<(u64,f32)>>`
  → `Postings::from_pairs`), un `write_mmap_file` à partir de `Postings` et un chargement mmap →
  `Postings` (le fichier peut stocker `tail_max` inclusif : OLD replie déjà `max(weight, mnw)`).
- Branchement `index.rs` / `mmap_index.rs` / `handle.rs` sur `wand::search_with` avec un `Scratch`
  par thread ; conserver les court-circuits `allowed_ids` vide, requête vide, index vide.

**P1 — devrait**
- Dans la boucle : tester `score > threshold` (lu une fois par fenêtre, valable car le seuil ne
  fait que monter) **avant** le filtre et `offer` ; ramène NEW au niveau d'OLD (150 µs) et supprime
  les 30 000 appels au filtre par requête.
- Défaut `window` 4096 (ou dérivé de la densité `records / (max_id − min_id)`).
- Décider du sort des scores `0.0` : soit refuser les poids `0.0` à l'insertion (recommandé, un zéro
  n'est pas un non-nul), soit filtrer `score == 0.0` dans la boucle ; mettre à jour
  `blob_store_survives_cache_cleanup` en conséquence.
- Découpler `wand/mmap.rs` de `PostingListIter` / `MmapPostingListIterator` : un curseur direct sur
  `&[PostingEntry]` (repr(C), 16 o) sans closure, sinon OLD ne peut pas être supprimé.

**P2 — souhaitable**
- N'appeler `skip_below` que si le seuil a changé depuis le dernier appel (heuristique OLD), ou
  toutes les N fenêtres : −25 µs à 1024 sur ce corpus.
- `refresh_tail_max` : arrêt anticipé dès que `tail_max` existant == valeur recalculée (valide pour
  upsert et delete) ; rend les appends en ordre croissant O(1) amorti (3,1 s → quelques ms sur 50 000).
- Plafonds par bloc (block-max) via `upper_bound()` pour que le pruning devienne effectif sur les
  listes longues ; documenter que les dims dupliquées dans une requête sont sommées deux fois.
