# Theories pour les 2 derniers fails

> 28 mai 2026, fin de session 7

## Etat : 13/15

| Query | Mode | Grep | V3 | Status | Detail |
|---|---|---|---|---|---|
| function | relax | 1467 | 1468 | **1 FP** | V3 trouve un doc en trop |
| TableFunction | relax | 221 | 220 | **1 FN** | V3 rate un doc |

Les deux sont en mode **relax** uniquement. Tous les strict passent.

## Theorie 1 : le query-time ne connait pas le double ordinal

Avec notre partitionnement, un meme texte (ex: "functional") a maintenant
**deux ordinals** dans le FST : un chunk (partition 0x00/0x01) et un
word-stripped (partition 0x02). Le builder les enregistre tous les deux
sous la meme cle FST lowercase.

`fst_candidates_v3` retourne les deux parents (deux ordinals differents).
`resolve_single_v3` resout les postings de chaque ordinal. Le word-stripped
ordinal a des **postings vides** dans sfxpost (ses postings sont dans
WordSfxPost, un format separe). Le resolve produit un match avec
byte_from=0, byte_to=0 → **faux positif**.

**Ou regarder** : `src/suffix_fst/briques/resolve.rs`, `resolve_single_v3()`.
Verifier si des matches avec byte_from==byte_to==0 sont produits.

**Fix possible** : filtrer les matches vides dans resolve_single, ou bien
ne pas indexer les word-stripped ordinals dans les partitions 0x00/0x01
(ils ne devraient etre que dans 0x02).

## Theorie 2 : le builder met le word-stripped dans les partitions chunk

Le builder (`src/indexer/sfx_dag_v3.rs`, lignes 89-101) itere
`sorted_indices` et skip `is_word_stripped` pour les partitions 0x00/0x01.
Mais `sorted_indices` contient TOUS les intern_ids. Le filtre est :

```rust
if meta.is_word_stripped { continue; }
```

Avec notre fix, chunk et word-stripped ont des intern_ids separes. Chacun
a son propre `is_word_stripped` flag correct. Donc le builder devrait
correctement ignorer les word-stripped pour 0x00/0x01. **A verifier** que
c'est bien le cas — si le flag est incorrect quelque part, le word-stripped
serait indexe dans les partitions chunk avec un ordinal sans postings sfxpost.

## Theorie 3 : content_len filter dans l'orchestrateur

`orchestrator::contains_v3()` (ligne ~65) fait :
```rust
matches.retain(|m| m.span > 1 || m.byte_to - m.byte_from >= query_content_len);
```

Si le word-stripped ordinal produit un match avec byte_to - byte_from = 0
(postings vides), ce filtre le vire. Mais si le match a un span > 1
(cross-token), il passe quand meme. **A verifier** le span des FP.

## Theorie 4 : scotch pre-partitionnement dans le word pipeline

Le word pipeline (`resolve_word_chains_v3`) utilise `WordSfxPostReader`
pour resoudre les word-stripped ordinals. Avant notre fix, chunk et
word-stripped partageaient le meme ordinal. Le code faisait :

```rust
let postings = word_sfxpost.entries(ordinal);
```

Avec deux ordinals maintenant, le word-stripped ordinal a ses postings
dans WordSfxPost, mais le chunk ordinal n'en a pas. Si le word pipeline
recoit un chunk ordinal par erreur (parce que le FST retourne les deux
parents pour la meme cle), il ne trouvera rien dans WordSfxPost →
le doc est perdu → **FN**.

**C'est probablement la cause du FN TableFunction relax** : la chain
word utilise un ordinal chunk au lieu du word-stripped, et WordSfxPost
ne connait pas cet ordinal.

**Ou regarder** :
- `src/suffix_fst/briques/fst_walk.rs` — `cross_word_chain_v3()` et
  `falling_walk_words()`. Est-ce que le falling walk sur partition 0x02
  filtre correctement pour ne prendre que les parents word-stripped ?
- Le builder `add_word_stripped()` dans `builder_v3.rs` — quels ordinals
  sont enregistres dans la partition 0x02 ?

## Theorie 5 : le vrai probleme est un seul

Les deux fails (1 FP + 1 FN) pourraient etre les **deux faces du meme
bug** : le FST a deux ordinals pour le meme texte, et selon la partition
traversee (0x00 vs 0x02), on utilise le mauvais :
- 0x00 avec ordinal word-stripped → postings vides → FP (phantom match)
- 0x02 avec ordinal chunk → WordSfxPost vide → FN (match perdu)

**Le fix unifie** : le FST devrait avoir des parents qui portent
l'information "cet ordinal est chunk" ou "cet ordinal est word-stripped".
Le range scan / falling walk filtrerait par type selon la partition
traversee. Pas de melange cross-partition dans les parents.

## Prochaine etape recommandee

1. Verifier theorie 5 : dans le forensics, comparer les ordinals retournes
   par la partition 0x00 vs 0x02 pour la query "function" et "tablefunction"
2. Si confirme : ajouter un flag `is_word_stripped` dans `ParentEntryV3`
   et filtrer dans `fst_candidates_v3` selon la partition traversee
3. Alternative plus simple : ne pas mettre les word-stripped ordinals
   dans le FST pour les partitions 0x00/0x01 (le builder a deja ce filtre
   via `is_word_stripped`, verifier qu'il fonctionne post-fix)
