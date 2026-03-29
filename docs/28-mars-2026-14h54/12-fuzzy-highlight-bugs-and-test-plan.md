# 12 — Bugs potentiels fuzzy highlights + plan de test

Date : 29 mars 2026

## Bugs identifiés dans fuzzy_contains_via_trigram

### Bug A : DFA sliding window prend le mauvais match

**Lignes 682-706.** Le sliding window essaie TOUTES les positions dans le
concat et garde le match avec `global_best_diff` le plus petit. Mais :

1. **Faux positifs possibles** : si le concat inclut des tokens avant le match
   réel, le DFA peut matcher un autre texte avec le même diff=0.
   Ex: le concat contient `...xrag3weaver...` et le DFA commence à `x` avec
   d=1 (insertion de x). match_start pointe vers `x`, pas vers `rag3weaver`.

2. **Aucun tie-breaking** : si deux positions ont la même best_diff, la
   première trouvée gagne (itération séquentielle). Pas de préférence pour
   la position la plus proche du trigram anchor.

**Sévérité** : moyen. Les faux positifs sont rares car le concat est petit
(~8 tokens) et le DFA est strict. Le tie-breaking peut donner le mauvais
highlight byte range mais le doc est quand même trouvé.

### Bug B : tri_offset cross-token = 0 faux

**Ligne 731-732.** Pour les trigrammes cross-token (qui span 2 tokens),
`fp_tok_text.find(&ngrams[first_tri_idx])` retourne None → unwrap_or(0).
L'offset est 0, donc `fp_cb_start = first_bf` au lieu de
`first_bf - real_offset`. Le point d'ancrage pour le content byte walk
est décalé.

**Impact** : le highlight start/end est décalé pour les matches dont le
premier trigramme est cross-token. Le highlight couvre les mauvais tokens.

### Bug C : gapmap.read_separator default = 1

**Lignes 738-739, 751-752.** `unwrap_or(1)` quand le gapmap ne retourne pas
de séparateur. Ça ajoute 1 byte fictif au gap. Si le gapmap est absent ou
corrompu, le walk accumule des erreurs.

**Impact** : highlight décalé de N bytes (N = nombre de gaps traversés).

### Bug D : merge highlight gap ≤ 1 byte

**Ligne 781.** Le merge fusionne les highlights adjacents avec gap ≤ 1 byte.
Mais pour des matches séparés (ex: deux occurrences distinctes de "rag3weaver"
dans le même doc), si elles sont à ≤ 1 byte d'écart, elles fusionnent en un
seul highlight trop large.

**Impact** : rare, cosmétique.

### Bug E : threshold min=2 peut rater des matches courts

**Ligne 586.** `threshold = max(2, computed)`. Pour query "rag3" d=1 :
ngrams = ["ra", "ag", "g3"] (3 bigrams), computed = max(2, 3 - 2 - 1) = 2.
OK. Mais pour "rag" d=1 : ngrams = ["ra", "ag"] (2 bigrams),
computed = max(2, 2 - 2 - 1) = max(2, -1) = 2. Threshold=2 = les 2 bigrams
doivent matcher. Mais avec d=1, un des 2 bigrams peut être cassé → aucun
match possible.

**Impact** : queries très courtes (3-4 chars) avec d=1 retournent 0 résultats
car threshold trop haut. "rag" d=1 ne trouvera jamais "rak".

**Fix** : threshold = max(1, computed) pour queries ≤ 4 chars.

### Bug F : pas de posmap = fallback sans validation

**Lignes 767-770.** Sans posmap, le code accepte tous les candidats du
trigram filter SANS validation DFA et utilise `first_bf..last_bt` comme
highlight. C'est le fallback pour les vieux index sans .posmap.

**Impact** : faux positifs + highlights = byte range des trigrammes, pas du
match réel.

## Plan de test : ground truth fuzzy sur .luce

### Concept

Créer un test natif (`test_fuzzy_ground_truth`) qui :
1. Indexe les fichiers du repo (comme test_merge_contains)
2. Pour la query "rag3weaver" d=1 :
   a. Fait un scan brut de tous les docs : pour chaque doc, tokenise avec
      CamelCaseSplit + lowercase, concatène les tokens sans séparateurs,
      cherche toutes les sous-chaînes à distance ≤ 1 de "rag3weaver"
   b. C'est le **ground truth** : la liste exacte des (doc_id, substring_matched)
3. Fait une recherche fuzzy via la query
4. Compare : chaque résultat de la query doit être dans le ground truth,
   et chaque entrée du ground truth doit être dans les résultats
5. Pour chaque highlight, vérifie que le texte highlighté (lu depuis le store)
   est bien à distance ≤ 1 de la query (ignorer les séparateurs entre tokens)

### Vérification des highlights

```rust
fn verify_highlight(stored_text: &str, hl_start: usize, hl_end: usize, query: &str, distance: u8) -> bool {
    let hl_text = &stored_text[hl_start..hl_end];
    // Strip separators (non-alphanumeric) from highlighted text
    let stripped: String = hl_text.chars().filter(|c| c.is_alphanumeric()).collect();
    // Check Levenshtein distance
    levenshtein(&stripped.to_lowercase(), &query.to_lowercase()) <= distance as usize
}
```

### Queries à tester

| Query | d | Attendu |
|-------|---|---------|
| rag3weaver | 0 | tous les docs avec le token "rag3weaver" ou cross-token "rag3"+"weaver" |
| rag3weaver | 1 | idem + "rak3weaver", "rag3weavr", "rag3weaverr", etc. |
| rak3weaver | 1 | "rag3weaver" (g→k), "rak3weaver" (exact) |
| weaver | 1 | "weaver" (exact), "weavr" (1 edit), "weaverr" (1 edit) |
| rag3db | 1 | "rag3db" (exact), "rak3db" (1 edit), "rag3xb" (1 edit) |

### Ce que le test valide

1. **Recall** : tous les docs du ground truth sont trouvés
2. **Precision** : tous les résultats sont dans le ground truth (pas de faux positifs)
3. **Highlights** : le texte highlighté est à distance ≤ d de la query
4. **Highlights cross-token** : quand le match span 2+ tokens, le highlight
   couvre du début du premier token à la fin du dernier (séparateurs inclus)
