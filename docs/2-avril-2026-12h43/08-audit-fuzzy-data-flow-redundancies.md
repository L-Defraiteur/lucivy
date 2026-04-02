# 08 — Audit : data flow fuzzy, redondances et optimisations possibles

Date : 2 avril 2026

## Contexte

Après le fix HashMap (scan linéaire → O(1)), "rag3db" d=1 passe de 2035ms
à 68ms. Mais il reste des redondances. Cet audit analyse chaque étape de
`fuzzy_contains_via_trigram` pour identifier les données calculées deux fois,
les lookups qui pourraient être cachés, et les synergies manquantes.

---

## Vue d'ensemble du data flow

```
generate_ngrams
     │ → ngrams[], query_positions[], n
     ▼
FST walk + falling walk (par ngram)
     │ → fst_cands_per_gram[], ct_chains_per_gram[], selectivity[]
     ▼
Resolve postings (rarest first + doc filter)
     │ → all_matches[ngram_idx] = Vec<LiteralMatch>
     ▼
bf_to_pos HashMap (pré-construit)
     │ → (doc_id, byte_from) → token_position
     ▼
intersect_trigrams_with_threshold
     │ → candidates[] = (doc_id, first_bf, last_bt, first_tri_idx, first_si, proven, last_tri_idx)
     ▼
Pour chaque candidat :
     ├─ proven → highlight depuis trigram positions (gratuit)
     └─ unproven → DFA posmap walk :
           ├─ bf_to_pos lookup → fp (position token)
           ├─ pm.ordinal_at(doc_id, pos) × N tokens
           ├─ ord_to_term(ord) × N tokens
           ├─ gapmap.read_separator() × N tokens
           ├─ build concat_bytes + token_spans
           ├─ DFA sliding window sur concat
           └─ content_byte_starts table + highlight mapping
```

---

## Redondance 1 : query_text.contains(' ') évalué par candidat

**Ligne ~874** : `let include_gaps = query_text.contains(' ');`

Évalué pour chaque candidat non-proven. Résultat constant.

**Fix** : pré-calculer une fois avant la boucle de candidats.

---

## Redondance 2 : gaps lus deux fois par candidat

**Première lecture** (concat building, ligne ~881) :
```rust
let gap = sfx_reader.gapmap().read_separator(doc_id, pos - 1, pos);
```

**Deuxième lecture** (content_byte_starts, lignes ~972 et ~985) :
```rust
let gap = sfx_reader.gapmap().read_separator(doc_id, cur_pos, next_pos)
```

Les mêmes gaps entre les mêmes positions sont relus.

**Fix** : stocker les gap lengths dans `token_spans` pendant le concat building :
```rust
token_spans.push((pos, cs, concat_bytes.len(), tlen, gap_len));
```
Puis réutiliser dans content_byte_starts sans relire le gapmap.

---

## Redondance 3 : ordinal + term text relus par candidat pour même doc

Si un doc a 5 candidats non-proven, on rebuild le concat 5 fois.
Chaque rebuild fait ~5-10 appels `pm.ordinal_at()` + `ord_to_term()`.

**Fix** : cacher les ordinals + texts par `(doc_id, position_range)`.
Exemple : HashMap<(DocId, u32), (u32, String)> pour (doc_id, pos) → (ord, text).
Ou mieux : grouper les candidats par doc_id, builder le concat une seule fois
par doc, puis valider tous les candidats de ce doc sur le même concat.

---

## Redondance 4 : token_spans scanné linéairement 3 fois

- **Ligne ~905** : `token_spans.iter().find(|pos == fp|)` → trouver fp_span
- **Ligne ~957** : `token_spans.iter().position(|pos == fp|)` → trouver fp_idx
- **Lignes ~995-1004** : `token_spans.iter().position(...)` × 2 → trouver start/end span

4 scans linéaires sur le même vecteur de ~10 éléments.

**Fix** : construire un HashMap<u32, usize> (pos → index dans token_spans)
une fois après le concat building. Ou utiliser binary search vu que les
positions sont triées.

---

## Redondance 5 : tous les ngrams FST-walkés même si inutiles

**Ligne ~694-701** : on walk les FST candidates + falling walk pour TOUS
les ngrams, puis on trie par sélectivité et ne résout que `threshold` ngrams.

Pour "rag3db" (5 bigrams, threshold=2), on walk 5 bigrams mais n'en résout
que 2-3. Les walks des 2-3 restants sont gaspillés.

**Fix** : walk lazy — estimer la sélectivité par la taille de la réponse FST
(quasi gratuit), puis ne walk les falling_walk que pour les ngrams qui seront
résolus. Ou walk en ordre de sélectivité et s'arrêter après threshold.

---

## Redondance 6 : doc_filter est une union, pas intersection

**Ligne ~741** : `prev.extend(gram_docs)` → union des doc sets.

Le pigeonhole dit : un doc valide contient AU MOINS threshold ngrams.
L'union donne "docs qui contiennent AU MOINS UN des threshold rarest ngrams".
C'est correct mais moins sélectif qu'une intersection.

L'intersection donnerait "docs qui contiennent TOUS les threshold rarest ngrams",
ce qui est un sous-ensemble beaucoup plus petit.

**Fix** : utiliser intersection au lieu de union pour le doc_filter :
```rust
doc_filter = Some(match doc_filter {
    None => gram_docs,
    Some(prev) => prev.intersection(&gram_docs).copied().collect(),
});
```

Attention : ça ne marche que si les threshold grams sont obligatoires
(pigeonhole guarantee). C'est le cas quand threshold = ngrams.len() - n*d.

---

## Optimisation manquante : grouper candidats par doc_id

**Impact potentiel : HAUT**

Actuellement : pour chaque candidat, on fait un DFA walk indépendant.
Si un doc a 10 candidats non-proven, on fait 10 walks.

Optimal : grouper les candidats par doc_id. Pour chaque doc :
1. Builder le concat UNE SEULE FOIS (le range le plus large)
2. Content byte starts UNE SEULE FOIS
3. Valider tous les candidats sur le même concat (DFA sliding window
   à différents anchor points)

Ça divise le nombre de posmap/gapmap/termtexts reads par le nombre
de candidats par doc.

---

## Optimisation manquante : DFA trace partagée

Dans le DFA sliding window, on clone le start state et refeed le concat
pour chaque position `sb` dans `[window_lo, window_hi)`.

Si la fenêtre fait 3 positions et le concat fait 15 bytes, on feed :
- sb=0 : bytes 0-14
- sb=1 : bytes 1-14
- sb=2 : bytes 2-14

Les bytes 2-14 sont feedés 3 fois.

**Fix** : construire un tableau de DFA states [state_at_byte_0, state_at_byte_1, ...]
en une seule passe forward. Puis pour chaque `sb`, reprendre à `state_at_byte[sb]`.
O(concat_len) au lieu de O(concat_len × window_size).

---

## Synergies manquantes entre briques

### LiteralMatch pourrait porter plus d'info

Actuellement `LiteralMatch` a : doc_id, position, byte_from, byte_to, si, token_len.

Pourrait aussi porter :
- **ordinal** du token parent → évite le lookup posmap dans le DFA walk
- **token_text** → évite le lookup ord_to_term
- **gap_before** (gap bytes avec le token précédent) → évite le gapmap read

Si `find_literal` / `resolve_candidates` retournait ces infos, la validation
DFA pourrait skipper la majorité des lookups posmap/gapmap/termtexts.

### intersect_trigrams_with_threshold pourrait retourner plus

Actuellement retourne : (doc_id, first_bf, last_bt, first_tri_idx, first_si, proven, last_tri_idx).

Pourrait aussi retourner :
- **chain complète** : tous les (tri_idx, bf, bt, si) de la chain
- **fp (token position)** : déjà connu dans les matches, évite le bf_to_pos lookup
- **ordinals de la chain** : si les matches portaient les ordinals

---

## Priorités

| # | Fix | Impact estimé | Effort |
|---|-----|--------------|--------|
| 1 | Grouper candidats par doc_id | -50% DFA walks | moyen |
| 2 | Stocker gaps dans token_spans | -30% gapmap reads | faible |
| 3 | doc_filter intersection | -60% candidats | faible |
| 4 | query_text.contains(' ') | négligeable | trivial |
| 5 | token_spans pos → idx HashMap | négligeable (10 éléments) | trivial |
| 6 | Lazy ngram walking | -20% FST walks | moyen |
| 7 | DFA trace partagée | -50% DFA bytes feedés | élevé |
| 8 | LiteralMatch enrichi | -80% lookups/candidat | élevé (propagation) |
