# Design : Word Map pour vérification exacte des cross-token chains

## Contexte

Les content-prefix ordinals agrègent les postings de tokens partageant le même
contenu (indépendamment du sep). Le cross-token chain builder (falling walk +
fst_candidates) produit des chains valides au niveau FST, mais le resolve peut
trouver des adjacences fortuites dans le corpus : deux tokens adjacents (pos P,
pos P+1) dont les ordinals matchent les alternatives de la chain, mais qui ne
forment PAS le substring recherché dans le texte réel.

## Problème illustré

Query "struct", corpus 500 fichiers C++.

```
Falling walk: token "instruct" à SI=2 → suffix "struct" matche → split
Remainder: vide (query consommée)
→ single-token match, OK.

Falling walk: token "xyzabcs" à SI=6 → suffix "s" matche 1 byte → split
Remainder: "truct" (5 bytes) → fst_candidates retourne 241 ordinals (tous les
tokens SI=0 commençant par "truct" : "truction", "tructure", "tructor"...)
→ chain = [xyzabcs_ord, [241 alternatives]]
→ resolve: dans un doc, token A (ord=xyzabcs) à pos 61, par coïncidence un des
  241 ordinals a un posting à pos 62 avec byte_from continu → FP !
```

Le texte réel est "...DestroyWithNullDatabase..." — pas de "struct" dedans.

## Solution : Word Map (deux tables)

### Table 1 : ChunkWordMap — `ordinal → [(word_id, chunk_index, total_chunks)]`

Pour chaque content ordinal, la liste des mots dans lesquels ce token apparaît
et à quelle position (chunk index) dans le mot.

```
ordinal 42 → [
    (word_id=100, chunk_index=0, total_chunks=2),  // "instruct" chunk 0 de "instruction"
    (word_id=205, chunk_index=1, total_chunks=3),  // "instruct" chunk 1 de "redinstructable"
]
```

**Pourquoi `total_chunks` par entrée (pas par mot)** : le même mot peut être
chunké différemment selon le trailing sep. "function " (9 bytes) → 2 chunks,
"function" (8 bytes, dernier mot) → 1 chunk. Le chunking dépend du segment
(content + sep), pas du mot seul. Donc `total_chunks` est une propriété du
token dans ce contexte, pas du mot.

### Table 2 : NextWordMap — `word_id → [next_word_ids]`

Pour chaque mot, les IDs des mots qui peuvent le suivre dans le corpus indexé.
Construit à l'indexation en observant les paires (word_i, word_{i+1}).

```
word_id 100 ("instruction") → [55 ("set"), 200 ("pointer"), ...]
word_id 77  ("mutex")       → [12 ("lock"), 33 ("init"), ...]
```

### Vérification à la query

Pour un chain match (ordinal A à pos P, ordinal B à pos P+1) :

```rust
let entries_a = chunk_word_map.lookup(ord_a);  // [(word, chunk, total)]
let entries_b = chunk_word_map.lookup(ord_b);

let valid = entries_a.iter().any(|(word_a, chunk_a, total_a)| {
    entries_b.iter().any(|(word_b, chunk_b, _total_b)| {
        // Cas 1: intra-mot — chunks consécutifs du même mot
        (word_a == word_b && *chunk_b == chunk_a + 1)
        ||
        // Cas 2: inter-mot — dernier chunk d'un mot + premier chunk du suivant
        (*chunk_a == total_a - 1 && *chunk_b == 0
         && next_word_map.contains(word_a, word_b))
    })
});

if !valid { /* FP — rejeter */ }
```

### Identification des mots (word_id)

Un "mot" = un **segment** du tokenizer = content + trailing sep. C'est l'unité
produite par `split_into_segments()` dans l'equal_chunk tokenizer.

Exemples :
- `"mutex_lock"` → segment "mutex_" (content="mutex", sep="_") + segment "lock"
- `"mutex________________lock"` → segment "mutex________________" (16 underscores de sep)
  + segment "lock". Le premier segment est chunké en 3 tokens, tous du même word_id.
- `"__init__value"` → segment "__" (content="", sep="__") + segment "init__" + segment "value"

Le word_id est identifié par le **texte complet du segment** (content + sep, lowered).
Deux occurrences de "function " (avec espace) dans des docs différents ont le même
word_id. Mais "function " et "function\n-" ont des word_ids **différents** car le
sep diffère → le chunking diffère → le nombre de chunks diffère.

L'interning se fait sur le segment complet (content + sep, lowered) via un HashMap
dans le collector.

### Format binaire

**ChunkWordMap** (même pattern que OverlapSiblingTable) :
```
[4B] num_ordinals
[4B × (num_ordinals + 1)] offset table
Entries par ordinal:
  [4B word_id, 1B chunk_index, 1B total_chunks] × N  (6 bytes par entrée)
```

**NextWordMap** :
```
[4B] num_words
[4B × (num_words + 1)] offset table
Entries par mot:
  [4B next_word_id] × N
```

### Où ça se construit

Dans `SfxCollectorV3::into_data()` :

1. **Pendant add_value** : on track déjà word_id par chunk (ChunkMeta.word_id).
   Ajouter : interning global des mots (word content → word_id compact).
   Ajouter : pour chaque paire (word_i, word_{i+1}) dans la même value, enregistrer
   dans NextWordMap.

2. **Dans into_data** : pour chaque intern ordinal, on connaît word_id et
   chunk_index. On construit ChunkWordMap : content_ordinal → [(word_id, chunk_idx, total)].

3. **Sérialisation** : les deux tables sont sérialisées dans le SfxCollectorDataV3
   et stockées comme fichiers .sfx supplémentaires ou intégrés au .sfx v3.

### Où ça se lit

Dans `contains_query_v3.rs` (ou l'orchestrator v3 si on préfère) :
- Charger les deux maps depuis les registry files du segment
- Post-filtrer les chain matches (span > 1) avec la vérif ci-dessus

### Coût

- **Espace** : O(num_ordinals × avg_words_per_ord) + O(num_words × avg_next_words).
  Pour 500 fichiers C++ (~25K ordinals, ~15K mots uniques) : quelques centaines de KB.
- **Query** : O(1) lookup par ordinal + O(N×M) comparaison pour N×M combinaisons
  (typiquement N=1-3, M=1-3). Négligeable.

### Ce que ça résout

- **FP cross-token** : seuls les chains qui correspondent à des paires
  chunk/chunk réelles dans le corpus sont validés
- **FP inter-mot** : seules les transitions mot→mot observées dans le corpus
  sont validées
- **Robuste** : pas de dépendance aux bytes, purement structurel
- **Compatible content-prefix ordinals** : le 1→N de la ChunkWordMap gère
  nativement le fait qu'un ordinal apparaît dans plusieurs mots
