# 05 — Plan : fuzzy ngram walk via siblings (remplacement du trigram pigeonhole)

Date : 4 avril 2026 ~16h

---

## Problème actuel

Le fuzzy contains via trigram pigeonhole génère des ngrams (bi/trigrams) depuis la query, résout chacun indépendamment, puis `intersect_trigrams_with_threshold` construit des chaînes par position byte. Le DFA Levenshtein valide chaque candidat.

Problèmes fondamentaux :

1. **Bigrams trop courts** : pour "3db_val" (6 chars alpha), n=2 → bigrams "3d", "db", "va", "al". Ultra-communs → 21000+ candidats → 800ms+ de DFA walks.

2. **Faux candidats** : le chain builder accepte des bigrams de tokens non-adjacents ("3d" de "rag3db" à pos 100 + "va" de "valide" à pos 500).

3. **Pas de vérification d'adjacence** : le span_diff heuristique et le position gap check ne remplacent pas une vraie vérification de sibling.

4. **Monotonie violée** : d=0 (multi-token SuffixContainsQuery) trouve des résultats que d=1 (trigram pigeonhole) rate.

---

## Mécanisme actuel du falling_walk

### falling_walk exact (d=0)
```
Input  : query = "rag3weaver"
Action : parcourt le SFX byte par byte (r→a→g→3→w→e→a→v→e→r)
         à chaque noeud final, vérifie : si + prefix_len == token_len
         (= le prefix consomme exactement la fin du token parent)
Output : SplitCandidate { prefix_len: 4, parent: "rag3" (si=0, len=4) }
         signifie : "rag3" est consommé par les 4 premiers bytes de la query,
         il reste "weaver" à chercher chez les siblings.
```

### fuzzy_falling_walk (d>0)
```
Pareil mais parcourt le SFX en DFS guidé par un DFA Levenshtein.
Le DFA tolère des edits dans le prefix. fst_depth = position dans le SFX,
pas dans la query (les deux divergent quand il y a des edits).
```

### Sibling chain DFS (cross_token_falling_walk)
```
Après le falling_walk, pour chaque SplitCandidate :
  remainder = query[prefix_len..]  (ex: "weaver" après "rag3")
  
  DFS sur les contiguous_siblings du token parent :
    - sibling.text == remainder → TERMINAL (match exact)
    - sibling.text starts_with remainder → TERMINAL (token couvre le reste)
    - remainder starts_with sibling.text → PARTIAL (continue DFS avec remainder réduit)
  
  Produit des CrossTokenChain { ordinals: [ord_rag3, ord_weaver], first_si, prefix_len }
```

---

## Proposition : fuzzy multi-segment walk via siblings

### Idée

Au lieu de découper la query en ngrams indépendants puis intersect heuristique, faire un **walk unifié** qui :

1. Parse la query en segments alphanumériques : "3db_val" → ["3db", "val"]
2. Pour le premier segment ("3db") : falling_walk (exact ou fuzzy) dans le SFX
3. Pour chaque split candidate : vérifier les siblings pour trouver le segment suivant ("val")
4. La résolution des postings ne se fait que sur les chaînes validées par siblings

### Cas de figure à gérer

#### Cas 1 : segment entier dans un seul token
```
Query: "3db_val" → segments ["3db", "val"]
Content: token "rag3db" contient "3db" (suffix si=3)
         token "value" contient "val" (suffix si=0)
         siblings("rag3db") → ["value"] (adjacent)

Walk:
  falling_walk("3db") → SplitCandidate { prefix_len: 3, parent: "rag3db" (si=3) }
  remainder = "" (segment entièrement consommé)
  → passer au segment suivant "val"
  → chercher "val" chez les siblings de "rag3db"
  siblings("rag3db") contient "value" → "value".starts_with("val") ✓ → MATCH
```

#### Cas 2 : segment split sur CamelCase
```
Query: "rag3weaver" → segment unique ["rag3weaver"]
Content: tokens "rag3" + "weaver" (CamelCase split)

Walk:
  falling_walk("rag3weaver") → SplitCandidate { prefix_len: 4, parent: "rag3" }
  remainder = "weaver"
  siblings("rag3") → ["weaver"] → exact match ✓ → MATCH

C'est exactement ce que cross_token_falling_walk fait déjà.
```

#### Cas 3 : segment avec edit distance
```
Query: "rak3weaver" → segment unique ["rak3weaver"]
Content: tokens "rag3" + "weaver"

Walk:
  fuzzy_falling_walk("rak3weaver", d=1) 
  → SplitCandidate { prefix_len: 4, parent: "rag3" }
     (DFA accepte "rag3" comme match fuzzy de "rak3" à d=1)
  remainder = "weaver" (les bytes query restants après le split)
  siblings("rag3") → ["weaver"] → exact match ✓ → MATCH (1 edit consommé)
```

#### Cas 4 : multi-segment cross-word avec separator
```
Query: "3db_val" → segments ["3db", "val"]
Content: "rag3db" + sep "_" + "value"

Walk:
  falling_walk("3db") → SplitCandidate { prefix_len: 3, parent: "rag3db" (si=3) }
  Segment "3db" entièrement consommé → passer au segment "val"
  → BUT: on est entre deux segments query → accepter le séparateur
  → chercher "val" chez les siblings de "rag3db"
  
  MAIS: siblings donne les tokens CONTIGUOUS (gap_len==0).
  "rag3db" et "value" ont un gap "_" entre eux → gap_len > 0 → pas contiguous !
  
  → Il faut utiliser siblings() (pas contiguous_siblings()) et accepter
    les siblings avec gap_len > 0 quand on est à une transition cross-segment.
```

#### Cas 5 : edit au milieu d'un segment, pas encore fini
```
Query: "rak3db_val" → segments ["rak3db", "val"]
Content: tokens "rag3" + "db" (CamelCase split de "rag3db")

Walk segment "rak3db":
  fuzzy_falling_walk("rak3db", d=1)
  → SplitCandidate { prefix_len: 4, parent: "rag3" }
     (DFA accepte "rag3" ≈ "rak3" à d=1)
  remainder = "db" (bytes query restants)
  
  On est en milieu de segment query → follow siblings pour finir le segment
  siblings("rag3") → ["db"] → exact match pour remainder ✓
  
  Segment "rak3db" entièrement consommé (via 2 tokens + 1 edit) → passer à "val"
  → follow siblings de "db" → "value" → "val" matches ✓ → MATCH
```

#### Cas 6 : query single-word (pas de séparateur)
```
Query: "rag3weaver" → segment unique ["rag3weaver"]
→ C'est le cas standard du cross_token_falling_walk actuel.
  Rien ne change.
```

---

## Architecture proposée

### Nouvelle fonction : `fuzzy_multi_segment_walk`

```rust
pub fn fuzzy_multi_segment_walk(
    sfx_reader: &SfxFileReader<'_>,
    query_segments: &[&str],   // ["3db", "val"]
    distance: u8,
    ord_to_term: &dyn Fn(u64) -> Option<String>,
) -> Vec<MultiSegmentChain> {
    // Pour chaque segment, on fait un falling_walk (exact ou fuzzy)
    // puis on suit les siblings pour relier les segments entre eux.
    //
    // Le budget d'edit distance est GLOBAL : réparti entre les segments.
    // Un edit consommé dans le segment 1 réduit le budget pour le segment 2.
    //
    // DFS state: (segment_idx, remainder_in_segment, cur_ordinal, 
    //             chain_ordinals, edits_used)
}
```

### Retour
```rust
pub struct MultiSegmentChain {
    pub ordinals: Vec<u64>,    // tokens traversés dans l'ordre
    pub first_si: u16,         // offset dans le premier token
    pub edits_used: u8,        // nombre d'edits consommés
}
```

### Résolution des postings
Identique à `resolve_chains` : résoudre les postings du premier ordinal,
vérifier que les tokens suivants sont aux positions adjacentes dans le doc.

### Highlight
La chaîne donne le premier token (ordinal + si → byte_from) et le dernier
token (ordinal → byte_to). Le highlight couvre [first_bf, last_bt].

---

## Impact sur le pipeline fuzzy

### Avant (actuel)
```
query "3db_val" d=1
  → generate_ngrams → ["3d", "db", "va", "al"] (4 bigrams)
  → resolve chacun → des milliers de matches par bigram
  → intersect_trigrams_with_threshold → 21000+ candidats
  → DFA walk × 21000 → 800ms+
```

### Après (proposé)
```
query "3db_val" d=1
  → split en segments → ["3db", "val"]
  → fuzzy_multi_segment_walk:
      falling_walk("3db", d=1) → quelques SplitCandidates (sélectif!)
      pour chaque: siblings → chercher "val" → peu de chaînes valides
  → resolve_chains → quelques dizaines de candidats max
  → PAS de DFA walk (la validation est faite par le walk + siblings)
```

### Queries single-word
Rien ne change — 1 seul segment → c'est le `cross_token_falling_walk` actuel.

---

## Avantages

1. **Sélectivité** : le falling_walk est très sélectif (parcourt le SFX, pas les postings). Pas de bigrams ultra-communs.

2. **Adjacence exacte** : les siblings garantissent l'adjacence au niveau token. Pas de faux candidats.

3. **Pas de DFA walk séparé** : la validation est intégrée dans le walk (le DFA Levenshtein est utilisé dans le falling_walk, pas après).

4. **Monotonie** : si d=0 trouve un résultat (segments exacts, tokens adjacents), d=1 le trouve aussi (les mêmes segments avec 0 edits ⊂ d=1).

5. **Perf** : O(falling_walk × sibling_branching × segments) au lieu de O(ngrams × postings × candidates × DFA).

---

## Questions ouvertes

### Budget d'edit global vs per-segment
- **Global** (d=1 réparti entre tous les segments) : plus correct, plus complexe
- **Per-segment** (d=1 par segment) : plus simple, plus permissif
- Recommandation : global, le DFA Levenshtein gère naturellement le budget

### Gaps entre segments (séparateurs)
- `contiguous_siblings()` (gap_len==0) pour intra-query-word
- `siblings()` (tout gap_len) pour transitions cross-query-word
- Le gap réel ne compte pas dans le budget d'edit (separator-agnostic)

### Fallback pour queries sans SFX
- Si pas de sibling table : fallback sur le trigram pigeonhole actuel

### Coexistence avec le trigram pigeonhole
- Le trigram pigeonhole reste utile pour les queries single-word longues
  (où le falling_walk serait trop large)
- Le multi-segment walk est pour les queries multi-word avec séparateurs

---

## Fichiers à modifier

| Fichier | Modification |
|---------|-------------|
| `src/query/phrase_query/literal_pipeline.rs` | Nouvelle fn `fuzzy_multi_segment_walk` |
| `src/query/phrase_query/regex_continuation_query.rs` | Router les queries multi-word vers le nouveau walk |
| `src/suffix_fst/sibling_table.rs` | Exposer `siblings()` (pas juste `contiguous_siblings`) |
| `lucivy_core/tests/test_fuzzy_monotonicity.rs` | Valider que les 3 queries échouantes passent |
