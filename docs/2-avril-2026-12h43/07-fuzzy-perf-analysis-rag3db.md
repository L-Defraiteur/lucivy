# 07 — Analyse perf fuzzy d=1 "rag3db" + algorithme actuel

Date : 2 avril 2026

## Le problème

`contains "rag3db" d=1` sur 4307 docs (repo rag3db) : **2035ms**.
`contains "rag3weaver" d=1` sur le même index : **90ms**.

## Chiffres clés "rag3db" d=1

```
total=2010ms fst=9ms resolve=22ms intersect=17ms dfa=1960ms
ngrams=5 candidates=14085 (proven=8646 unproven=5439 skipped=2933 dfa_walked=2506)
```

- 14085 candidats passent le trigram threshold
- 8646 proven (skip DFA, gratuit)
- 5439 unproven → 2933 skippés (doc déjà dans bitset) → **2506 DFA walks**
- Chaque DFA walk = ~0.6ms → 2506 × 0.6 = ~1.5s

## Pourquoi autant de candidats ?

"rag3db" = 6 chars → 4 trigrams (n=3) : "rag", "ag3", "g3d", "3db"
(en fait 5 ngrams car le code génère aussi des bigrams pour les queries courtes)

Threshold = `(5 - 3*1 - 1).max(2) = 2`. Seulement 2 trigrams suffisent.

"rag3db" est le nom du repo → apparaît dans presque tous les fichiers.
Les trigrams "rag", "ag3" matchent des milliers de tokens.
Résultat : 14085 candidats.

## Tokenisation CamelCaseSplit

"rag3db" → split à la frontière 3→d (digit→letter) :
- Token "rag3" (4 chars)
- Token "db" (2 chars, < MIN_CHUNK_CHARS=4 → mergé ? non, c'est le dernier)

Ou : "rag" (3 < 4) merge avec "3" → "rag3", puis "db" reste seul.
Résultat : tokens ["rag3", "db"]

## Algorithme actuel : fuzzy_contains_via_trigram

```
Query "rag3db" d=1
     │
     ▼
┌─────────────────────────────────┐
│ 1. generate_ngrams()            │
│    "rag3db" → bigrams/trigrams  │
│    ["rag", "ag3", "g3d", "3db", │
│     + éventuellement "ra", ...]  │
│    positions: [0, 1, 2, 3, ...] │
│    threshold = 2                │
└──────────────┬──────────────────┘
               │
               ▼
┌─────────────────────────────────────────────┐
│ 2. Pour chaque ngram :                       │
│    a) fst_candidates(sfx_reader, ngram)      │
│       → FST prefix_walk → FstCandidate list  │
│       → O(FST range scan), rapide            │
│                                              │
│    b) cross_token_falling_walk(ngram, d=0)   │
│       → falling_walk + sibling chain DFS     │
│       → CrossTokenChain list                 │
│                                              │
│    c) Estimate selectivity (count candidates)│
└──────────────┬──────────────────────────────┘
               │
               ▼
┌─────────────────────────────────────────────┐
│ 3. Resolve postings (rarest first)           │
│    a) Phase B1: resolve `threshold` rarest   │
│       ngrams WITHOUT doc filter              │
│       → build doc_filter set                 │
│                                              │
│    b) Phase B2: resolve remaining ngrams     │
│       WITH doc filter                        │
│       → LiteralMatch per ngram               │
└──────────────┬──────────────────────────────┘
               │
               ▼
┌─────────────────────────────────────────────┐
│ 4. intersect_trigrams_with_threshold()       │
│    Pour chaque doc :                         │
│    - Collecter tous (tri_idx, bf, bt, si)    │
│    - Greedy scan : chains avec tri_idx       │
│      croissant, span_diff ≤ distance         │
│    - proven = ALL trigrams + consistent span  │
│    → 14085 candidats                         │
│      (8646 proven, 5439 unproven)            │
└──────────────┬──────────────────────────────┘
               │
               ▼
┌─────────────────────────────────────────────┐
│ 5. Validation DFA (LE GOULOT)                │
│                                              │
│    Pour chaque candidat :                    │
│                                              │
│    a) Si proven → skip DFA, highlight        │
│       depuis trigram positions. Gratuit.      │
│                                              │
│    b) Si doc déjà dans bitset && !proven     │
│       → skip (2933 skippés)                  │
│                                              │
│    c) Sinon : DFA posmap walk (2506 walks)   │
│       - Lookup fp position via all_matches   │
│       - Build concat : walk posmap tokens    │
│         autour de fp, lire ord_to_term pour  │
│         chaque token, lire gapmap gaps       │
│       - DFA sliding window sur le concat     │
│       - Si match : add to bitset + highlight │
│       → ~0.6ms par walk                      │
│       → 2506 × 0.6ms = 1500ms               │
└──────────────┬──────────────────────────────┘
               │
               ▼
           Résultats
```

## Comparaison avec contains d=0

```
Query "rag3db" d=0
     │
     ▼
┌─────────────────────────────────┐
│ 1. suffix_contains_single_token │
│    prefix_walk("rag3db")        │
│    → 0 résultats (token existe  │
│      pas : CamelCaseSplit)      │
└──────────────┬──────────────────┘
               │
               ▼
┌─────────────────────────────────┐
│ 2. cross_token_falling_walk     │
│    falling_walk("rag3db")       │
│    → split à "rag3" | "db"      │
│    → sibling chain validate     │
│    → résolu en ~1ms             │
└──────────────┬──────────────────┘
               │
               ▼
┌─────────────────────────────────┐
│ 3. resolve_chains               │
│    → resolve postings pour      │
│      "rag3" + "db" adjacents    │
│    → ~15ms                      │
└──────────────┬──────────────────┘
               │
               ▼
           19ms total
```

Le contains d=0 est 100× plus rapide car il utilise le falling walk
(split au bon endroit + sibling chain) au lieu des trigrams.

## Observations

1. **Le falling walk connaît la structure des tokens** (CamelCaseSplit boundaries).
   Les trigrams ne la connaissent pas.

2. **Les trigrams "rag" et "ag3"** sont ultra-fréquents (matchent dans des milliers
   de tokens). Les trigrams cross-boundary "g3d" et "3db" sont beaucoup plus
   sélectifs mais le threshold=2 ne les exige pas.

3. **Le DFA walk est le goulot** : 0.6ms × 2506 walks. Chaque walk fait
   ~10-20 lookups posmap + termtexts + gapmap.

4. **Le proven check est trop permissif** : il accepte des matches décalés
   (ex: "librag3db" où tous les trigrams sont présents mais décalés de +3).

## Pistes d'optimisation

### A. Falling walk fuzzy (hybride)

Le contains d=0 utilise falling_walk → sibling chain → resolve.
On pourrait faire pareil pour d=1 :

1. `fuzzy_falling_walk("rag3db", d=1)` → trouve les splits fuzzy
   ("rag3" + "db", "rak3" + "db", "rag3" + "dc", etc.)
2. Resolve chains → postings
3. Résultat direct, pas de trigrams

Le `fuzzy_falling_walk` existe déjà dans `literal_pipeline.rs`.
Pour les tokens courts comme "rag3db", ça serait beaucoup plus rapide
car le falling walk est O(FST) pas O(n_docs).

### B. Trigrams cross-boundary prioritaires

Les trigrams "g3d" et "3db" traversent la frontière CamelCaseSplit.
Ils ne matchent que si les tokens "rag3" et "db" sont adjacents.
Si on les utilise comme filtre principal (threshold les exige),
on réduit drastiquement les candidats.

### C. Intersection trigrams + falling walk

1. Trigrams → doc_id set (rapide, grossier)
2. Falling walk fuzzy → résolution par token (précis)
3. Intersect les deux → résultat

### D. Cap sur le nombre de DFA walks

Simplement limiter à N DFA walks max (genre 100), les suivants
sont acceptés comme proven si le doc est nouveau. Perte de précision
sur les highlights mais temps borné.
