# Arsenal index v3 — structures et usages

> 1 juin 2026

## Structures par fichier

| # | Extension | Magie | Contenu | Lookup | Taille |
|---|---|---|---|---|---|
| 1 | `.sfx` | SFX3 | FST 3 partitions (0x00 SI=0, 0x01 SI>0, 0x02 word-stripped) + parents (ordinal, sti, own_len, sep_len, overlap) | prefix walk, range scan | O(tokens) |
| 2 | `.sfxpost` | SFP2 | (doc_id, position, byte_from, byte_to) par ordinal chunk | ordinal -> postings | O(total_postings) |
| 3 | `.word_sfxpost` | WSP1 | (doc_id, first_pos, last_pos, byte_from, byte_to) par ordinal word-stripped | ordinal -> postings | O(word_postings) |
| 4 | `.posmap` | PMAP | (doc_id, position) -> ordinal | O(1) par (doc, pos) | O(total_tokens) |
| 5 | `.bytemap` | BMAP | ordinal -> 256-bit bitmap des bytes presents | O(1) contains_byte | O(ordinals * 32) |
| 6 | `.sibling_v3` | - | ordinal -> [(next_ordinal, gap_len)] | O(1) par ordinal | O(sibling_pairs) |
| 7 | `.termtexts` | TTXT | ordinal -> texte du token | O(1) par ordinal | O(total_text) |
| 8 | `.word_pos_map` | WMAP | (doc_id, position) -> word_id | O(1) par (doc, pos) | O(total_tokens) |
| 9 | `.chunk_word_map` | - | ordinal -> [(word_id, chunk_idx, total_chunks)] | O(1) par ordinal | O(chunk_entries) |
| 10 | `.next_word_map` | - | word_id -> [next_word_ids] | O(1) par word | O(word_pairs) |
| 11 | `.freqmap` | FREQ | ordinal -> doc_freq; (ordinal, doc_id) -> tf | O(1) df, O(log n) tf | O(postings) |
| 12 | `.sepmap` | SMAP | ordinal -> 256-bit bitmap des bytes separateurs | O(1) | O(ordinals * 32) |
| 13 | gapmap | in .sfx | (doc_id, position) -> bytes separateurs entre tokens | seq scan | O(total_gap_bytes) |

## Qui utilise quoi

| Pipeline | .sfx | .sfxpost | .word_sfxpost | .posmap | .bytemap | .sibling | .termtexts |
|---|---|---|---|---|---|---|---|
| contains strict | X | X | - | - | - | X | X |
| contains relax | X | X | X | X | X | X | X |
| fuzzy (trigram) | X | X | X | - | - | - | - |
| regex | X | X | - | X | X | - | - |
| BM25 scoring | - | - | - | - | - | - | - |

(.freqmap pour BM25)

## Ce qui manque pour le fuzzy

### Probleme actuel
`build_trigram_chains` fait O(n^2) par doc (pour chaque hit, scan tous les suivants).
35s pour "uint64" sur 500 docs en debug.

### Ce qu'on pourrait utiliser

**`.posmap`** : (doc_id, position) -> ordinal. Si on connait la position d'un
trigram hit, on peut verifier l'adjacence en O(1) :
- trigram A a position P -> posmap(doc, P+1) -> ordinal du token suivant
- verifier si cet ordinal correspond a un trigram attendu

**`.sibling_v3`** : ordinal -> next ordinals. Verification structurelle : le
token qui suit ce trigram est-il dans la liste des siblings ? Evite le scan.

**`.word_pos_map`** : (doc, pos) -> word_id. Pour savoir si deux positions
sont dans le meme mot ou pas. Utile pour fuzzy cross-word.

### Approche proposee

Au lieu de construire des chaines en O(n^2), utiliser le posmap pour
verifier l'adjacence en O(1) :

```
Pour chaque doc candidat :
  Pour chaque hit du trigram[0] (le plus rare) :
    anchor = hit.position
    Pour i in 1..ngrams.len() :
      expected_pos = anchor + query_positions[i] - query_positions[0]
      // Ajuste pour les frontieres de token (overlap)
      ord_at_pos = posmap.get(doc_id, expected_pos)
      Verifier si ord_at_pos matche ngrams[i] via bytemap ou termtexts
```

Complexite : O(hits_trigram_0 * ngrams.len()) au lieu de O(all_hits^2).
Mais necessite le posmap dans le BriquesContext fuzzy (actuellement pas charge).

### Alternative : pre-filtrage par le trigram le plus rare

Le trigram le plus selectif a peu de hits. Si on ne chaine QUE a partir
de ses positions, le O(n^2) devient O(rare_hits * total_hits) ce qui
est acceptable. C'est ce que fait deja la v2 (selectivity sort).
