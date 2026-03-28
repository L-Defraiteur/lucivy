# 07 — Design : highlights fuzzy via token mapping

Date : 28 mars 2026

## Problème

Les highlights fuzzy cross-token sont incorrects. Exemples avec query "rag3weaver" d=1 :

| Texte indexé (tokens) | Highlight actuel | Highlight attendu |
|----------------------|-----------------|-------------------|
| `["rag3weaver"]` | `"rag3weaver"` ✓ | ✓ |
| `["rag3", "weaver"]` | aucun | `"rag3 weaver"` (tokens couverts) |
| `["ag", "3", "weaver"]` | `"weaver"` seul | `"ag, 3, weaver"` (3 tokens) |
| `"rak3weaver"` | `"3weaver"` | `"rak3weaver"` |
| `"rag3weavr"` (query d=1) | `"rag3weav"` (8 chars) | `"rag3weaver"` (10 chars) |

### Cause racine

L'approche actuelle calcule le highlight par arithmétique :
```
adj_bf = first_bf - query_positions[first_tri_idx]
adj_bt = adj_bf + match_len
```

Ce calcul suppose une correspondance 1:1 entre bytes query et bytes content.
C'est FAUX pour les matches cross-token : le content a des séparateurs
(espaces, virgules, etc.) que la query n'a pas.

De plus, l'ancrage du DFA (`expected_start ± 1`) est trop restrictif et rate
des matches légitimes quand le calcul d'ancrage est faux pour les trigrammes
cross-token.

## Données disponibles

À l'intérieur de `fuzzy_contains_via_trigram`, on dispose de :

1. **`concat_bytes`** : texte reconstruit token par token. On SAIT quels tokens
   sont dedans et à quels offsets ils commencent/finissent dans le concat.

2. **`posmap`** : `ordinal_at(doc_id, position) → term_ordinal`.
   Donne l'ordinal du token à n'importe quelle position dans le doc.

3. **`ord_to_term`** : `ordinal → String`. Texte du token.

4. **`gapmap`** : `read_separator(doc_id, pos_a, pos_b) → &[u8]`.
   Bytes séparateurs entre deux positions adjacentes.

5. **`first_bf`** : byte_from du premier trigramme matché (ancre connue en
   coordonnées content). Vient de `intersect_trigrams_with_threshold`.

6. **`fp`** : position du token qui contient le premier trigramme matché.

7. **Le DFA** : après le slide sur concat, donne `(match_start, match_end)`
   en coordonnées concat.

## Algorithme

### Étape 1 : Build concat avec tracking des tokens

Pendant la construction du concat, enregistrer les bornes de chaque token :

```rust
struct TokenSpan {
    pos: u32,            // position du token dans le doc
    concat_start: usize, // offset début dans concat_bytes
    concat_end: usize,   // offset fin dans concat_bytes
}

let mut concat_bytes: Vec<u8> = Vec::new();
let mut token_spans: Vec<TokenSpan> = Vec::new();

for pos in start_pos..end_pos {
    if include_gaps && pos > start_pos {
        // ajouter gap au concat (multi-mot seulement)
    }
    let concat_start = concat_bytes.len();
    if let Some(text) = get_token_text(pos) {
        concat_bytes.extend_from_slice(text.as_bytes());
    }
    let concat_end = concat_bytes.len();
    token_spans.push(TokenSpan { pos, concat_start, concat_end });
}
```

### Étape 2 : DFA validation (sliding window)

Slider le DFA sur TOUT le concat. Le concat est petit (~8 tokens, ~50 bytes),
donc c'est rapide. Prendre le match dont la longueur est la plus proche de
`query_text.len()` :

```rust
for sb in 0..concat_bytes.len() {
    // feed DFA from sb, track best match closest to query_len
}
// → (match_start, match_end) en coordonnées concat
```

### Étape 3 : Identifier les tokens touchés

Mapper le range DFA `[match_start, match_end)` vers les tokens :

```rust
let first_span = token_spans.iter()
    .find(|t| t.concat_end > match_start);

let last_span = token_spans.iter().rev()
    .find(|t| t.concat_start < match_end);
```

Résultat : `first_span.pos` et `last_span.pos` — les positions exactes
des premier et dernier tokens touchés par le match.

### Étape 4 : Content bytes via walk depuis l'ancre

On connaît `first_bf` (content byte du premier trigramme, au token `fp`).
On peut calculer le content byte de n'importe quel token en marchant
depuis `fp` avec posmap + gapmap + ord_to_term :

```
Content layout :
  [tok_a text] [gap(a,a+1)] [tok_a+1 text] [gap(a+1,a+2)] [tok_a+2 text]

content_byte_start(pos+1) = content_byte_start(pos) + len(tok_pos) + len(gap(pos, pos+1))
content_byte_start(pos-1) = content_byte_start(pos) - len(tok_pos-1) - len(gap(pos-1, pos))
```

**Ancre** : le token à position `fp` a son content byte start calculable depuis
`first_bf` et l'offset du trigramme dans le token :

```rust
let tri_offset = tok_text.find(&ngrams[first_tri_idx]).unwrap_or(0);
let fp_content_start = first_bf - tri_offset;
```

Pour les trigrammes cross-token (find retourne None → offset=0), `fp_content_start`
est approximatif mais on n'en a besoin que pour calculer les tokens voisins, pas
pour le highlight final.

**Walk vers first_span.pos** (backward si < fp, forward si > fp) :

```rust
// Backward walk
let mut cb = fp_content_start;
for pos in (target_pos..fp).rev() {
    cb -= gap_len(pos, pos+1) + tok_text_len(pos);
}
hl_start = cb;

// Forward walk for end
cb = fp_content_start + tok_text_len(fp);
for pos in (fp+1)..=last_span.pos {
    cb += gap_len(pos-1, pos) + tok_text_len(pos);
}
hl_end = cb;
```

**Highlight final** : `[hl_start, hl_end]` — du premier byte du premier token
touché au dernier byte du dernier token touché, séparateurs inclus.

## Exemples

### Single token : "rag3weaver"

```
concat = "rag3weaver"  (1 token)
token_spans = [(pos=5, cs=0, ce=10)]

DFA match: [0, 10) → match_start=0, match_end=10
first_span = pos=5, last_span = pos=5

hl_start = content_byte_start(pos=5) = 200
hl_end   = content_byte_start(pos=5) + 10 = 210

Highlight: [200, 210] = "rag3weaver" ✓
```

### Cross-token : ["rag3", "weaver"]

```
concat = "rag3weaver"  (2 tokens, pas de gap car single-word query)
token_spans = [(pos=5, cs=0, ce=4), (pos=6, cs=4, ce=10)]

DFA match: [0, 10) → match_start=0, match_end=10
first_span = pos=5 (ce=4 > 0), last_span = pos=6 (cs=4 < 10)

Walk from fp:
  hl_start = content_byte_start(pos=5) = 200
  hl_end   = content_byte_start(pos=6) + len("weaver")
           = 200 + 4 + gap_len + 6 = 211 (si gap = 1 space)

Highlight: [200, 211] = "rag3 weaver" ✓ (séparateur inclus)
```

### Cross-token 3 tokens : ["ag", "3", "weaver"]

```
concat = "ag3weaver"  (3 tokens, pas de gap)
token_spans = [(pos=5, cs=0, ce=2), (pos=6, cs=2, ce=3), (pos=7, cs=3, ce=9)]

DFA "rag3weaver" d=1 match: [0, 9) (d=1: "ag3weaver" vs "rag3weaver" = deletion du 'r')
  ou pas de match car distance("ag3weaver", "rag3weaver") = 1 ✓
first_span = pos=5, last_span = pos=7

Walk:
  hl_start = content_byte_start(pos=5)
  hl_end   = content_byte_start(pos=7) + len("weaver")

Highlight couvre "ag, 3, weaver" (séparateurs inclus) ✓
```

## Avantages par rapport à l'approche actuelle

1. **Pas d'arithmétique `first_bf - query_positions`** : plus de décalage dû aux séparateurs
2. **Tokens exacts** : on sait EXACTEMENT quels tokens sont touchés
3. **Highlights toujours valides** : du début du premier token au fin du dernier, jamais de coupure au milieu
4. **Sliding window complet** : pas d'ancrage ±1 qui rate des matches
5. **Content bytes fiables** : walk depuis l'ancre connue, pas de calcul inverse fragile

## Complexité

- Build concat + token_spans : O(N) tokens, N ≈ 8
- DFA sliding window : O(N × query_len) ≈ O(50 × 10) = 500 ops
- Token mapping : O(N) lookups
- Content byte walk : O(N) gapmap reads

Total par candidat : ~1000 ops. Négligeable.
