# 09 — Plans d'implémentation : optimisations fuzzy

Date : 2 avril 2026

---

## Plan A : LiteralMatch enrichi (PRIORITÉ 1)

### Objectif

Propager ordinal + token_text + gap_before depuis les briques de résolution
(find_literal, resolve_candidates, resolve_chains) jusqu'au candidat final.
Élimine les lookups posmap/termtexts/gapmap dans la validation DFA.

### Nouveau LiteralMatch

```rust
pub struct LiteralMatch {
    pub doc_id: DocId,
    pub position: u32,
    pub byte_from: u32,
    pub byte_to: u32,
    pub si: u16,
    pub token_len: u16,
    // NOUVEAU
    pub ordinal: u32,        // SFX ordinal du token parent
    pub token_text: String,  // texte du token parent (lowercased)
}
```

### D'où viennent ces données ?

- **ordinal** : déjà dans `FstCandidate.raw_ordinal` et `PostingEntry`.
  `resolve_candidates` a le `cand.raw_ordinal`. Il suffit de le propager.
- **token_text** : déjà disponible via `ord_to_term(ordinal)` dans les briques.
  Appelé UNE FOIS lors de la résolution, stocké dans le match.

### Propagation

1. `fst_candidates()` retourne déjà `raw_ordinal` → le passer à LiteralMatch
2. `resolve_candidates()` : appeler `ord_to_term(cand.raw_ordinal)` une fois,
   stocker dans LiteralMatch
3. `resolve_chains()` : pour les chains cross-token, le premier token
   a son ordinal. Les tokens suivants aussi (dans le cache ordinal_cache).
4. `find_literal()` : propager depuis SuffixContainsMatch
5. `group_by_doc()` / `MatchesByDoc` : ajouter ordinal + text au tuple
   (ou garder le LiteralMatch complet au lieu de destructurer en tuple)
6. `intersect_trigrams_with_threshold()` : propager dans les candidats

### Impact sur le DFA walk

Avant :
```rust
// Pour chaque position dans le concat :
let tok_ord = pm.ordinal_at(doc_id, pos);  // posmap lookup
let text = ord_to_term(tok_ord);            // termtexts lookup
```

Après :
```rust
// Les matches portent déjà ordinal + text
// Le concat peut être construit depuis les matches directement
// Zéro lookup posmap/termtexts pendant le DFA walk
```

### Étapes

1. Ajouter `ordinal: u32` et `token_text: String` à `LiteralMatch`
2. Propager dans `resolve_candidates()` (appeler ord_to_term une fois)
3. Propager dans `resolve_chains()` (depuis ordinal_cache)
4. Propager dans `find_literal()` / `SuffixContainsMatch`
5. Adapter `MatchesByDoc` — garder le LiteralMatch complet ou enrichir le tuple
6. Adapter `intersect_trigrams_with_threshold` — propager dans les candidats
7. Adapter le DFA walk — construire concat depuis les match texts au lieu de posmap
8. Tests : vérifier highlights identiques

### Risque

Allocation de String par match — potentiellement coûteux si beaucoup de
matches. Mitigation : utiliser des indices dans un interned string pool,
ou des `Arc<str>`.

Alternative : stocker un `ordinal` seulement, et cacher les texts dans
un `HashMap<u32, String>` une seule fois par segment (le termtexts reader
est O(1) par lookup, pas besoin de le rappeler).

### Qui en profite ?

**Pas que le fuzzy — tous les chemins de query :**

| Chemin | Lookup éliminé | Où |
|--------|---------------|-----|
| **Fuzzy d>0** (trigram path) | ord_to_term × N tokens × M candidats dans DFA walk | regex_continuation_query.rs:890 |
| **Fuzzy d>0** (validate_path) | ord_to_term × N tokens par gap validation | literal_resolve.rs:314 |
| **Regex** (multi-literal gaps) | ord_to_term dans validate_path pour chaque gap | regex_continuation_query.rs:1296,1325 |
| **Regex** (sibling DFS) | ord_to_term par sibling dans continuation | regex_continuation_query.rs:1453 |
| **Contains d=0** (cross-token) | ordinal perdu à la résolution, re-fetché si highlight | suffix_contains.rs:137 |
| **Pipeline briques** | ordinal_cache local à resolve_chains, perdu après | literal_pipeline.rs:273-284 |
| **bf_to_pos HashMap** | pourrait être éliminé si position déjà dans le match | regex_continuation_query.rs:819-825 |

**Conclusion** : l'enrichissement de LiteralMatch est transversal. Il bénéficie
à TOUS les chemins de query qui utilisent les briques du pipeline. C'est
l'optimisation la plus structurante car elle élimine les lookups au point de
consommation en les faisant UNE FOIS au point de production.

---

## Plan B : Grouper candidats par doc_id (PRIORITÉ 2)

### Objectif

Au lieu de traiter chaque candidat indépendamment (1 concat par candidat),
grouper par doc_id et construire UN concat par doc.

### Algorithme

```rust
// Grouper candidats par doc_id
let mut by_doc: HashMap<DocId, Vec<&Candidate>> = HashMap::new();
for cand in &candidates {
    by_doc.entry(cand.doc_id).or_default().push(cand);
}

// Pour chaque doc : un seul concat, plusieurs validations
for (doc_id, cands) in &by_doc {
    // Trouver le range le plus large parmi tous les candidats
    let min_pos = cands.iter().map(|c| fp(c) - lookback).min();
    let max_pos = cands.iter().map(|c| fp(c) + forward).max();

    // Builder le concat UNE FOIS
    let (concat, token_spans, content_byte_starts) = build_concat(doc_id, min_pos, max_pos);

    // Valider chaque candidat sur le même concat
    for cand in cands {
        if cand.proven {
            // highlight direct
        } else {
            // DFA sliding window à l'anchor de ce candidat
        }
    }
}
```

### Impact

- Divise les reads posmap/gapmap/termtexts par le nombre de candidats par doc
- Pour "rag3db" : ~3 candidats/doc → 3× moins de reads
- Se combine avec Plan A (si les matches portent les texts, le concat
  se construit sans aucun lookup)

### Étapes

1. Après intersect_trigrams, grouper candidats par doc_id
2. Pour chaque groupe, calculer le range de positions
3. Builder le concat une seule fois
4. Boucler sur les candidats du groupe pour la validation DFA
5. Gérer les highlights par candidat

---

## Plan C : Stocker gaps dans token_spans (PRIORITÉ 3)

### Objectif

Éviter de relire les gaps dans content_byte_starts (lignes ~972, ~985)
après les avoir déjà lus dans le concat building (ligne ~881).

### Changement

```rust
// Avant : token_spans = (pos, concat_start, concat_end, text_len)
// Après : token_spans = (pos, concat_start, concat_end, text_len, gap_before_len)

// Dans le concat building loop :
let gap_len = if pos > start_pos {
    let gap = sfx_reader.gapmap().read_separator(doc_id, pos - 1, pos);
    // ... extend concat ...
    gap.map(|g| g.len()).unwrap_or(0)
} else { 0 };
token_spans.push((pos, cs, concat_bytes.len(), tlen, gap_len));

// Dans content_byte_starts :
// Utiliser gap_len directement au lieu de relire le gapmap
let gap = token_spans[i].4 as u32;  // au lieu de sfx_reader.gapmap().read_separator(...)
```

### Impact

- Élimine ~N gapmap reads par candidat non-proven (où N = nombre de tokens dans le concat)
- Faible impact en absolu mais gratuit à implémenter

### Étapes

1. Changer le type de token_spans : ajouter gap_before_len
2. Stocker le gap_len dans le concat building loop
3. Utiliser le gap_len stocké dans content_byte_starts
4. Supprimer les appels gapmap.read_separator dans content_byte_starts

---

## Plan D : Precompute query_text.contains(' ') (PRIORITÉ 4)

### Trivial

```rust
// Avant la boucle de candidats :
let include_gaps = query_text.contains(' ');

// Au lieu de le recalculer à chaque candidat non-proven
```

---

## Plan E : token_spans position index (PRIORITÉ 5)

### Objectif

Remplacer les 3-4 scans linéaires de token_spans par un HashMap.

```rust
let span_idx: HashMap<u32, usize> = token_spans.iter()
    .enumerate()
    .map(|(i, &(pos, ..))| (pos, i))
    .collect();

// Au lieu de :
// token_spans.iter().find(|(pos, ..)| *pos == fp)
// token_spans.iter().position(|(pos, ..)| *pos == fp)
let fp_idx = span_idx[&fp];
```

Impact négligeable (token_spans a ~10 éléments) mais code plus propre.

---

## Plan F : Lazy ngram walking (PRIORITÉ 6)

### Objectif

Ne walk les falling_walk que pour les ngrams qui seront effectivement résolus.

Actuellement : walk ALL ngrams → trie par sélectivité → résout threshold rarest.
Optimisé : walk les ngrams en ordre de sélectivité FST (quasi gratuit),
ne falling_walk que pour les threshold premiers.

### Impact

- Réduit le nombre de falling_walks de ~5-15 à ~2-3
- Le falling_walk est plus coûteux que le fst_candidates (sibling DFS)
- Gain estimé : 20-30% sur la phase FST

---

## Plan G : DFA trace partagée (PRIORITÉ 7)

### Objectif

Pré-calculer les états DFA à chaque byte du concat en une seule passe forward,
puis pour chaque start position `sb`, reprendre à l'état pré-calculé.

### Complexité

Élevée — le DFA Levenshtein a un état riche (set de positions NFA).
Le clone d'état est O(1) ou O(état_size) selon l'implémentation.
Pour d=1, l'état est petit (~20 positions NFA).

### Impact

- Réduit le DFA feeding de O(concat_len × window_size) à O(concat_len)
- Gain estimé : 30-50% sur la phase DFA
- Mais le DFA est déjà rapide (quelques ms), donc gain absolu faible

---

## Ordre d'implémentation recommandé

1. **Plan A** (LiteralMatch enrichi) — structurant, bénéficie à tout
2. **Plan B** (grouper par doc_id) — se combine avec A
3. **Plan C** (gaps dans token_spans) — trivial, on fait en passant
4. **Plan D** (precompute contains(' ')) — trivial
5. **Plan E** (span index) — trivial
6. **Plan F** (lazy walking) — moyen effort
7. **Plan G** (DFA trace) — si le DFA reste un goulot après A+B
