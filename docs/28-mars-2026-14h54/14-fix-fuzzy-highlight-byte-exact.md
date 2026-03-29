# 14 — Fix fuzzy highlight : calcul byte-exact

Date : 29 mars 2026

## Problème

Le highlight fuzzy utilise un walk token-entier (`fspan` → `lspan`) au lieu
des bytes exacts du match DFA. Ça produit 40/286 highlights invalides (14%).

### 3 patterns de failure

**Pattern 1 : Highlight trop large (tokens entiers)**
Le code prend le premier token qui *touche* `match_start` dans le concat et le
dernier qui *touche* `match_end`, puis walk de l'un à l'autre. Si le match DFA
ne couvre que la fin d'un token et le début du suivant, le highlight inclut les
deux tokens en entier + les séparateurs entre eux.

Exemple : `hl=[5122,5142] raw="rag3weaver", "weaver"` — le highlight couvre
"rag3weaver" ET le "weaver" du token suivant dans `["rag3weaver", "weaver", ...]`.

**Pattern 2 : Highlight couvre un paragraphe entier (tables markdown)**
Même cause, mais les tokens sont dispersés dans une table markdown. Le walk
fspan→lspan englobe tout : `len=104`, `len=66`, etc.

**Pattern 3 : Highlight commence trop tôt (Bug B doc 12)**
`fp_tok_text.find(&ngrams[first_tri_idx])` retourne None pour les trigrams
cross-token → `unwrap_or(0)`. L'ancre `fp_cb_start` est faussée, le highlight
commence 5-7 bytes trop tôt.

Exemple : `raw="search("ag3weaver"` au lieu de `"ag3weaver"`.

## Solution : content byte mapping via token_spans

### Données disponibles

Le code construit déjà `token_spans` — un vecteur qui mappe chaque token à sa
position dans le concat :

```
token_spans[i] = (position, concat_start, concat_end, text_len)
```

Et le DFA sliding window produit `match_start` et `match_end = match_start + match_len`
dans le concat.

### Nouveau calcul

1. **Construire une table content_byte_starts** : pour chaque token dans
   `token_spans`, calculer son byte offset dans le content original via
   le walk posmap + gapmap (une seule passe, O(n_tokens)).

2. **Mapper match_start → content byte** : trouver le token span qui contient
   `match_start`, calculer l'offset intra-token, ajouter au content byte start
   du token.

3. **Mapper match_end → content byte** : idem pour `match_end`.

4. **Résultat** : `hl_start = content_byte_starts[token_idx] + intra_offset`
   et `hl_end` calculé de même. Pas de walk fspan→lspan, pas de tri_offset,
   pas de merge agressif.

### Détail du mapping

```
Pour chaque token_span (pos, cs, ce, tlen):
  content_byte_start[i] = byte_from du premier token (connu via posmap)
                         + somme des gaps et token lengths des tokens précédents

Pour match_start dans le concat:
  1. Trouver i tel que cs[i] <= match_start < ce[i]
  2. intra_offset = match_start - cs[i]
  3. hl_start = content_byte_start[i] + intra_offset

Pour match_end dans le concat:
  1. Trouver j tel que cs[j] < match_end <= ce[j]
  2. intra_offset = match_end - cs[j]
  3. hl_end = content_byte_start[j] + intra_offset
```

### Ce qui est supprimé

- `fspan` / `lspan` (first/last touched token span) → remplacé par mapping direct
- `tri_offset` = `fp_tok_text.find(&ngrams[first_tri_idx])` → plus nécessaire
- `fp_cb_start` anchor → remplacé par content_byte_starts table
- Walk backward/forward de fp → plus nécessaire
- Merge `gap ≤ 1 byte` → plus nécessaire (les highlights sont déjà précis)

### Complexité

O(n_tokens) pour construire la table, O(log n) ou O(n) pour le lookup.
Pas d'allocation supplémentaire significative (un Vec<u32> de taille n_tokens).

## Fichier modifié

`src/query/phrase_query/regex_continuation_query.rs` — fonction
`fuzzy_contains_via_trigram`, section Step 3-4 (lignes ~714-764).

## Validation

Le test `test_fuzzy_ground_truth` doit passer 286/286 highlights.
