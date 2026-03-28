# 04 — Design : Fuzzy via trigram pigeonhole + literal_resolve

Date : 28 mars 2026

## Problème

Fuzzy d=1 "schdule" prend 18 secondes sur 90K docs. Le prescan fait un scan
FST complet avec le Levenshtein DFA (`fuzzy_falling_walk` × SFX entier).
C'est le même bottleneck que l'ancien regex avant l'optimisation par littéraux.

De plus, fuzzy a un bug d'ordre : "rag3db" fuzzy d=1 matche parfois des résultats
comme "Range1 orageD" où l'ordre des caractères n'est pas respecté.

## Idée : trigram pigeonhole

### Principe mathématique

Pour une query de longueur L avec distance d'édition d, tout match valide
doit contenir au moins `(L - 2) - d` trigrammes identiques parmi les
`L - 2` trigrammes de la query (pigeonhole principle).

### Exemple : "schdule" d=1

Trigrammes : ["sch", "chd", "hdu", "dul", "ule"] → 5 trigrammes.
Au plus 1 trigramme peut différer → au moins 4 sur 5 doivent être présents.

Match valide "schedule" : trigrammes = ["sch", "che", "hed", "edu", "dul", "ule"]
→ trigrammes communs avec "schdule" : "sch", "dul", "ule" → 3 sur 5.
Hmm, ça fait seulement 3. Le pigeonhole classique sur trigrammes est plus subtil.

### Reformulation correcte

Le pigeonhole sur trigrammes dit : si deux strings ont edit distance d,
elles partagent au moins `L - 2 - 2*d` trigrammes (chaque édition peut
casser au plus 3 trigrammes qui se chevauchent, mais typiquement 2-3).

Pour d=1, L=7 : au moins 7 - 2 - 2 = 3 trigrammes communs.
Pour d=2, L=7 : au moins 7 - 2 - 4 = 1 trigramme commun.

C'est un filtre, pas une preuve. Les candidats qui passent le filtre
doivent être validés par le Levenshtein DFA.

### Alternative : q-grams plus longs

Au lieu de trigrammes, on peut utiliser des fragments de longueur `ceil(L / (d+1))`.
Pour "schdule" d=1 : fragments de longueur ceil(7/2) = 4 : "schd", "dule".
Au moins 1 des 2 doit être présent exactement.

Plus les fragments sont longs, plus le filtre est sélectif (moins de candidats),
mais moins de fragments donc moins de chances de matcher.

## Architecture proposée

### Flow

```
fuzzy_contains_via_trigram("schdule", d=1):

  1. Générer les trigrammes de "schdule" : ["sch", "chd", "hdu", "dul", "ule"]

  2. Pour chaque trigramme :
     find_literal(sfx_reader, trigram, resolver, ord_to_term)
       → Vec<LiteralMatch> (réutilise contains exact, cross-token aware)

  3. Compter par doc_id combien de trigrammes sont présents
     → filtrer : garder les docs avec >= seuil trigrammes

  4. Pour chaque doc candidat :
     - Trouver la position du premier trigramme matché
     - Lire le texte via PosMap + ord_to_term entre first_pos et last_pos
     - Feeder le Levenshtein DFA sur ce texte
     - Si DFA accepte → match validé

  5. Retourner (doc_bitset, highlights)
```

### Seuil de filtrage

Pour d=1 et trigrammes (longueur 3) :
- Chaque édition peut casser au plus 3 trigrammes adjacents
- Seuil conservateur : `num_trigrams - 3 * d`
- Pour "schdule" : 5 - 3 = 2 trigrammes minimum

C'est un filtre conservateur (pas de faux négatifs). Les faux positifs sont
éliminés par la validation DFA.

### Réutilisation de l'infra regex

| Brique | Regex | Fuzzy trigram |
|---|---|---|
| `find_literal()` | Cherche chaque littéral extrait du pattern | Cherche chaque trigramme |
| `intersect_literals_ordered()` | Tous les littéraux dans l'ordre | Variante : >= seuil dans l'ordre |
| `validate_path()` | DFA regex entre positions | DFA Levenshtein entre positions |
| `PosMap` | Lire tokens entre positions | Identique |
| `highlights_to_doc_tf()` | Convertir matches → BM25 | Identique |

### Vérification de l'ordre — même pattern que regex

Pour le regex, `intersect_literals_ordered` vérifie que `byte_from` de chaque
littéral est croissant dans le document. Pour les trigrammes c'est identique :
les trigrammes sont extraits dans l'ordre de la query, donc leurs positions
dans le texte DOIVENT être croissantes.

La différence : regex exige TOUS les littéraux dans l'ordre. Fuzzy exige
>= seuil dans l'ordre (certains trigrammes peuvent manquer à cause de l'édition).

#### `intersect_trigrams_with_threshold()`

Nouvelle fonction dans `literal_resolve.rs`. Même pattern que `intersect_literals_ordered`
mais avec tolérance sur les trous :

```rust
/// Intersect trigramme matches: find docs where at least `threshold` trigrams
/// appear in order (by byte position). Returns (doc_id, first_bf, last_bt).
///
/// Algorithm: scan linéaire O(n) identique à intersect_literals_ordered.
/// Pour chaque doc_id présent dans >= 1 trigramme :
///   1. Collecter tous les (trigram_index, byte_from, byte_to) triés par byte_from
///   2. Greedy scan : parcourir les matches triés par byte_from,
///      vérifier que trigram_index est croissant (l'ordre de la query est respecté)
///   3. Compter la plus longue sous-séquence croissante de trigram_index
///   4. Si count >= threshold → doc validé avec (first_bf, last_bt)
pub fn intersect_trigrams_with_threshold(
    grouped: &[MatchesByDoc],  // un MatchesByDoc par trigramme, dans l'ordre de la query
    threshold: usize,
) -> Vec<(DocId, u32, u32)>
```

Exemple pour "schdule" d=1, trigrammes ["sch", "chd", "hdu", "dul", "ule"] :

```
Doc 42, texte contient "schedule" :
  trigramme "sch" → byte_from=0
  trigramme "dul" → byte_from=4
  trigramme "ule" → byte_from=5
  (trigrammes "chd" et "hdu" absents car le texte a "che"/"hed" à la place)

Scan ordonné par byte_from : [(0, "sch"), (4, "dul"), (5, "ule")]
Trigram indices dans la query : [0, 3, 4] → croissant ✓
Count = 3, threshold = 2 → validé ✓

Doc 99, texte contient "orageDschul" (faux positif potentiel) :
  trigramme "sch" → byte_from=6
  trigramme "dul" → absent
  trigramme "ule" → absent
  Count = 1, threshold = 2 → rejeté ✗
```

#### Le bug d'ordre est fixé nativement

Le bug actuel (fuzzy "rag3db" matche "Range1 orageD") vient du fait que
le cross-token search ne vérifie pas l'ordre des caractères. Avec les trigrammes
ordonnés, la vérification que les positions byte_from sont croissantes ET que
les trigram_index sont croissants élimine les matches désordonnés.

"Range1 orageD" aurait des trigrammes "ran", "ang", "nge" etc. qui ne correspondent
pas aux trigrammes de "rag3db" ("rag", "ag3", "g3d", "3db") → rejeté.

### Validation sans DFA — trigrammes suffisants

On peut valider un fuzzy match **uniquement avec les trigrammes**, sans feeder
un DFA Levenshtein. Les trigrammes seuls prouvent le match si on vérifie
3 conditions :

#### 1. Ordre : trigram_index croissant

Les indices des trigrammes matchés dans la query doivent être croissants.
Ça garantit que l'ordre des caractères est respecté dans le texte.

#### 2. Seuil : >= `max(1, n_trigrams - 3*d)` trigrammes présents

Le pigeonhole garantit qu'un match à distance d ne peut casser que 3*d
trigrammes adjacents (chaque édition touche au plus 3 trigrammes chevauchants).

#### 3. Cohérence des écarts byte : ±d

L'écart byte entre le premier et le dernier trigramme matché dans le texte
doit être cohérent avec l'écart attendu dans la query, à ±d près.

```
écart_texte  = last_bf - first_bf     (positions dans le texte)
écart_query  = last_tri_pos - first_tri_pos  (positions dans la query)
| écart_texte - écart_query | <= d
```

Ça élimine les faux positifs où les trigrammes sont éparpillés dans le texte.

#### Exemple complet

```
"schdule" d=1 vs "schedule" :

Trigrammes query (index → trigram → position dans query) :
  0: "sch" pos=0
  1: "chd" pos=1
  2: "hdu" pos=2
  3: "dul" pos=3
  4: "ule" pos=4

Matchés dans texte "schedule" :
  "sch" → byte_from=0  (tri_index=0, query_pos=0)
  "dul" → byte_from=4  (tri_index=3, query_pos=3)
  "ule" → byte_from=5  (tri_index=4, query_pos=4)

Vérification :
  1. Ordre : [0, 3, 4] croissant ✓
  2. Seuil : 3 matchés >= max(1, 5-3) = 2 ✓
  3. Écart byte : last_bf(5) - first_bf(0) = 5
     Écart query : query_pos(4) - query_pos(0) = 4
     |5 - 4| = 1 <= d=1 ✓

→ Match validé SANS DFA ✓
```

#### Faux positif rejeté

```
Doc avec "schXXXXXXXule" (trigrammes "sch" et "ule" séparés) :

  "sch" → byte_from=0  (tri_index=0, query_pos=0)
  "ule" → byte_from=9  (tri_index=4, query_pos=4)

  Écart byte : 9 - 0 = 9
  Écart query : 4 - 0 = 4
  |9 - 4| = 5 > d=1 ✗ → rejeté
```

#### Tolérance par distance

| Distance | Trigrammes cassés max | Seuil (sur 5 tri) | Tolérance écart byte |
|---|---|---|---|
| d=0 | 0 | 5/5 | ±0 |
| d=1 | 3 | 2/5 | ±1 |
| d=2 | 6 | 1/5 | ±2 |
| d=3 | 9 | 1/5 | ±3 |

Pour d=2-3, le seuil tombe à 1 trigramme — la vérification d'écart byte
devient le filtre principal. Ça fonctionne pour toutes les distances.

#### Pas de DFA — trigrammes purs

Les 3 vérifications (ordre + seuil + écart byte) sont **suffisantes**.

Si les bons trigrammes sont présents, dans le bon ordre, avec le bon écart byte,
alors le texte EST à distance <= d de la query. Raison :
- L'écart byte ±d élimine les insertions/suppressions excessives
- L'ordre élimine les transpositions/réarrangements
- Le seuil garantit que suffisamment de la query est préservé

Il n'existe pas de contre-exemple : un texte qui a les bons trigrammes dans
le bon ordre avec le bon écart est nécessairement un match valide.

**Conséquence : pas de DFA Levenshtein, pas de PosMap, pas de validate_path.**
Le fuzzy ne dépend que de `find_literal` + `intersect_trigrams_with_threshold`.

### Nouvelle fonction : `fuzzy_contains_via_trigram()`

```rust
pub fn fuzzy_contains_via_trigram(
    query_text: &str,
    distance: u8,
    sfx_reader: &SfxFileReader,
    resolver: &dyn PostingResolver,
    ord_to_term: &dyn Fn(u64) -> Option<String>,
    mode: ContinuationMode,
    max_doc: DocId,
) -> Result<(BitSet, Vec<(DocId, usize, usize)>)>
```

Pas de `posmap_data`, pas de `automaton`. Uniquement trigrammes.

### Flow détaillé

```
fuzzy_contains_via_trigram("schdule", d=1):

  1. Générer trigrammes : ["sch", "chd", "hdu", "dul", "ule"]
     threshold = max(1, 5 - 3*1) = 2

  2. Pour chaque trigramme :
     find_literal(sfx_reader, trigram, resolver, ord_to_term)
       → Vec<LiteralMatch { doc_id, position, byte_from, byte_to }>

  3. intersect_trigrams_with_threshold(grouped, threshold, distance)
     Pour chaque doc_id présent dans >= 1 trigramme :
       a. Collecter (tri_index, byte_from, byte_to) trié par byte_from
       b. Greedy scan : vérifier tri_index croissant
       c. Compter les trigrammes dans la sous-séquence croissante
       d. Vérifier écart byte vs écart query (±d)
       e. Si count >= threshold ET écart OK → match validé

  4. Retourner (doc_bitset, highlights)
```

### Gestion du cross-token

Les trigrammes sont courts (3 chars). Un trigramme peut chevaucher une
frontière de token (ex: "rag3" + "db" → trigramme "3db" est cross-token).
`find_literal` gère déjà le cross-token via `suffix_contains_single_token_with_terms`
+ sibling chain. Donc ça marche nativement.

## Seuils par distance

| Distance | Trigrammes de "schdule" (5) | Seuil conservateur | Seuil pragmatique |
|---|---|---|---|
| d=0 | 5/5 (exact) | 5 | 5 |
| d=1 | >= 2/5 | 5 - 3 = 2 | max(1, n_tri - 3) |
| d=2 | >= 1/5 | 5 - 6 = -1 → 1 | max(1, n_tri - 6) |

Pour d=2-3, le seuil tombe à 1 trigramme — la vérification d'écart byte
devient le filtre principal. Ça fonctionne pour toutes les distances.

## Fichiers à modifier

| Fichier | Changement |
|---|---|
| `regex_continuation_query.rs` | Nouvelle fonction `fuzzy_contains_via_trigram()`, appel depuis DfaKind::Fuzzy |
| `literal_resolve.rs` | `intersect_trigrams_with_threshold()` — variante de `intersect_literals_ordered` avec seuil + écart byte |

### Réutilisé tel quel

| Fichier | Raison |
|---|---|
| `find_literal()` | Cherche chaque trigramme (cross-token natif) |
| `SuffixContainsScorer` | Réutilisé pour BM25 |

### Plus nécessaire pour fuzzy

| Fichier | Raison |
|---|---|
| `validate_path()` | Pas de validation DFA — trigrammes suffisants |
| `PosMap` | Pas de walk inter-positions |
| `SfxDfaWrapper` / Levenshtein DFA | Pas de DFA du tout |

## Performance attendue

Pour "schdule" d=1 sur 90K docs :
- 5 trigrammes × `find_literal` : ~5 × 1ms = 5ms (chaque trigramme est court, très sélectif)
- Intersection + vérification écart : O(total_matches) ~ quelques centaines
- **Total estimé : < 10ms** (vs 18 937ms actuel)

Pas de DFA, pas de PosMap walk, pas de resolve per-candidate. Que des lookups FST.

## Questions ouvertes

### 1. Trigrammes vs fragments plus longs ?

Pour des queries courtes (3-4 chars), les trigrammes dégénèrent (1-2 trigrammes).
Pour des queries longues (20+ chars), les trigrammes sont très nombreux.
Un compromis : utiliser des fragments de longueur `max(3, ceil(len / (d+1)))`.

### 2. Prescan DAG ?

Le fuzzy devrait-il passer par le prescan DAG comme le regex ?
Oui — même pattern : `run_fuzzy_prescan()` appelé par le PrescanShardNode,
résultats cachés pour le scorer. Pas de DFA à partager, juste les trigrammes.

### 3. Combo regex + fuzzy ?

Futur : `pattern: "rag[0-9]+.*ver"` avec `fuzzy: 1` appliquerait la distance
de Levenshtein sur les littéraux extraits du regex. Les trigrammes du littéral
serviraient de filtre, la validation regex sur le texte complet.
C'est une extension naturelle de l'architecture.
