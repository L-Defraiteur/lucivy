# Plan : Branchement query-time de la sibling table v3

## Ce qu'on a

- Sibling table v3 construite à l'indexation (commit `42e2efa`)
- Format identique à v2 : `ordinal → [(next_ordinal, gap_len)]`
- Contient chunk siblings (0x00/0x01) ET word siblings (0x02)
- Fichier : `sibling_v3` dans le registry

## Référence v2

Le code v2 de cross-token search via sibling table est dans :
**`src/query/phrase_query/suffix_contains.rs:783-935`**

Algorithme v2 (lignes 880-918) :
```
1. falling_walk(query) → premier split (ordinal, split_byte)
2. remainder = query[split_byte..]
3. DFS via stack :
   stack = [(ordinal, remainder, chain)]
   
   while stack :
     (cur_ord, rem, chain) = stack.pop()
     siblings = sibling_table.contiguous_siblings(cur_ord)
     
     for next_ord in siblings :
       next_text = ord_to_term(next_ord)   // term dict lookup
       
       if rem == next_text :               // exact terminal
         emit chain + [next_ord]
       elif next_text.starts_with(rem) :   // sibling covers all
         emit chain + [next_ord]
       elif rem.starts_with(next_text) :   // partial, continue
         stack.push(next_ord, rem[next_text.len()..], chain + [next_ord])
```

Clés :
- `contiguous_siblings(ord)` = O(1) array lookup
- `ord_to_term(ord)` = texte du token via TermTexts (O(log N))
- MAX_CHAIN_DEPTH = 8
- DFS borné : peu de siblings par ordinal (1-5 typiquement)

## Adaptation v3

### Différence principale : overlap

En v2, `next_text` = texte exact du token (content+sep, pas d'overlap).
En v3, `next_text` = texte étendu (content+sep+overlap).

Pour la comparaison avec le remainder, on veut le **content** du sibling,
pas l'overlap. Donc :

```rust
let next_text = term_texts.text(next_ord);
let content_len = /* from parent entry or split table */ ;
let next_content = &next_text[..content_len];
// Comparer rem avec next_content (pas next_text entier)
```

Le content_len est disponible via :
- Le parent entry dans le FST (si on a l'ordinal, on peut le retrouver)
- Ou la split table (ordinal → content_len, O(1))
- Ou un champ supplémentaire dans le SiblingEntry

Option la plus simple : stocker content_len dans le SiblingEntry.
Modifier le format : `(next_ordinal: u32, content_len: u16)` au lieu
de `(next_ordinal: u32, gap_len: u16)`. Le gap_len n'est pas utilisé
en v3 (toujours 0 pour contiguous).

### Où brancher

Dans `briques/composite.rs::find_literal_v3()` :

```
// Actuel :
chunk_chains = cross_chunk_chain_v3(reader, query)   // falling walk
word_chains = cross_word_chain_v3(reader, query)      // falling walk

// Nouveau :
chunk_chains = sibling_chain_v3(ctx, query, partition=chunk)
word_chains = sibling_chain_v3(ctx, query, partition=word)
```

Avec `sibling_chain_v3` qui fait :
1. falling_walk → premiers splits (fast path)
2. fst_candidates → splits supplémentaires (quand falling walk rate)
3. Pour chaque split : DFS via sibling table (au lieu de re-falling_walk)

### Le problème du premier split (rappel)

La sibling table aide pour les continuations. Le premier split vient de :
- **falling_walk** : marche si le FST a un noeud final au content boundary
- **fst_candidates** : marche toujours (range query, indépendant du FST)

Pour les fst_candidates comme source de splits :
```rust
for cand in fst_candidates(query) :
  content_len = cand.own_len - cand.sep_len
  split_byte = content_len - cand.sti
  if split_byte > 0 && split_byte < query.len() :
    // C'est un split ! L'ordinal couvre query[..split_byte],
    // remainder = query[split_byte..]
    // → lancer le DFS sibling depuis cand.ordinal
```

### TermTexts pour ord_to_term

En v2, `ord_to_term` est fourni par le caller. En v3, on a le fichier
TermTexts (`termtexts_v3.rs`) qui mappe ordinal → texte étendu.

Il est déjà chargé par le segment reader. Juste besoin de l'ajouter
au BriquesContext.

## Étapes d'implémentation

1. **BriquesContext** : struct regroupant reader, resolver, posmap,
   bytemap, word_sfxpost, sibling_v3, termtexts. Refacto signatures.

2. **sibling_chain_v3()** dans fst_walk.rs : le DFS adapté v3
   (comparaison sur content, pas texte entier)

3. **find_literal_v3** : remplacer cross_chunk_chain + cross_word_chain
   par sibling_chain_v3 + splits depuis fst_candidates

4. **Propager** BriquesContext dans orchestrator, contains_query_v3,
   fuzzy_query_v3

5. **Tests** : ground truth 15/15

## Résultat attendu

- Plus de dépendance FST pour les continuations
- Plus de best_consumed
- Plus de look-ahead DFS
- Le falling walk reste un "fast path" pour le premier split
- fst_candidates rattrape les splits ratés
- Ajout d'un index file = un champ dans BriquesContext, rien d'autre
