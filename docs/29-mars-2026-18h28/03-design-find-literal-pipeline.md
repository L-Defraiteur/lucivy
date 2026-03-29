# 03 — Design : find_literal en pipeline de briques composables

Date : 29 mars 2026

## Problème

`find_literal()` et `suffix_contains_single_token_with_terms()` font tout
d'un bloc : FST walk + resolve postings + cross-token sibling chain. Pas
moyen de :

1. Estimer la sélectivité avant de résoudre les postings (pour trier les
   trigrams du plus rare au plus commun)
2. Filtrer par doc_ids entre les itérations (pour éliminer les docs
   impossibles au fur et à mesure)
3. Séparer single-token de cross-token (pour faire le single-token d'abord
   sur tous les trigrams, puis le cross-token seulement si nécessaire)

Le fuzzy trigram fait N appels `find_literal()` (un par trigram), chacun fait
le travail complet. Sur un gros index avec des trigrams communs ("the", "ing"),
c'est des millions de postings résolus pour rien.

## Principe : ne PAS modifier l'existant

Les fonctions actuelles (`find_literal`, `suffix_contains_single_token_*`,
`cross_token_search_with_terms`) restent intactes. Elles marchent et sont
utilisées par le contains exact, le startsWith, etc.

On crée des **nouvelles briques** dans un module séparé
(`literal_pipeline.rs`), qui sont des versions découpées du même algorithme.
Le fuzzy (et potentiellement regex) les compose différemment.

## Comment fonctionne l'existant (rappel détaillé)

### `find_literal(literal)` appelle :

```
suffix_contains_single_token_with_terms(sfx_reader, literal, resolver, ord_to_term)
  │
  ├── suffix_contains_single_token_inner(sfx_reader, literal, resolver)
  │     │
  │     ├── prefix_walk(literal) → Vec<(suffix, Vec<ParentEntry>)>
  │     │     FST range scan. Pour chaque suffix entry trouvé, décode les
  │     │     parents : (raw_ordinal, si, token_len). ZÉRO resolve posting.
  │     │
  │     └── Pour chaque parent → resolver(raw_ordinal) → Vec<PostingEntry>
  │           Resolve TOUT le posting list de cet ordinal. C'est le coût.
  │           Produit SuffixContainsMatch { doc_id, token_index, byte_from, byte_to, si }
  │
  └── Si 0 résultats → cross_token_search_with_terms(sfx_reader, literal, resolver, 0, ord_to_term)
        Le cross-token pour les queries qui chevauchent des frontières de token.
```

### `cross_token_search_with_terms(literal)` en détail :

```
Étape 1 — falling_walk(literal) → Vec<SplitCandidate>
│
│  Walk le FST byte par byte. À chaque noeud FINAL, regarde si le prefix
│  consommé atteint la FIN du token parent (si + prefix_len == token_len).
│  Si oui → SplitCandidate { prefix_len, parent: ParentEntry }.
│
│  Exemple : query "rag3weaver"
│    FST trouve "rag3" : si=0, token_len=4, prefix_len=4
│    → 0 + 4 == 4 ✓ → split candidate à position 4
│    remainder = "weaver"
│
│  Pour fuzzy d>0 : fuzzy_falling_walk utilise un DFA Levenshtein
│  en DFS dans le FST. Le fst_depth (pas le DFA match length) est le
│  split point — trouve le split malgré des typos dans la partie gauche.
│  Exemple : query "rak3weaver" d=1
│    Le DFA tolère "rak3" ≈ "rag3" → split candidate malgré le typo k→g
│
│  Coût : O(2L) pour exact, O(FST_size × DFA_states) pour fuzzy.
│  ZÉRO resolve posting. Quasi gratuit pour exact.
│
Étape 2 — Sibling chain DFS pour chaque split candidate
│
│  Pour chaque candidate, on prend le remainder (query[prefix_len..])
│  et on explore les siblings contigus (gap=0) du parent ordinal.
│
│  DFS worklist : (current_ord, remainder, chain, depth)
│  Pour chaque sibling (next_ord via SiblingTable) :
│    next_text = ord_to_term(next_ord)  ← via TermTexts
│    - rem == next_text         → exact match, chain terminée ✓
│    - next_text.starts_with(rem) → token couvre le remainder ✓
│    - rem.starts_with(next_text) → partial, continue DFS avec rem[next_text.len()..]
│    (sinon → dead branch)
│
│  MAX_CHAIN_DEPTH = 8 (évite explosion combinatoire)
│
│  Fallback sans sibling table : prefix_walk_si0(remainder) sur le FST
│  → cherche un token qui commence par le remainder.
│
│  Coût : O(n_candidates × branching × depth). Peut être cher si beaucoup
│  de siblings. Mais TermTexts est O(1) et pas de resolve ici.
│
Étape 3 — Resolve postings (seulement les ordinals des chains valides)
│
│  ordinal_cache : HashMap<ordinal, Vec<PostingEntry>>
│  Ne résout que les ordinals qui apparaissent dans au moins une chain valide.
│
│  Coût : O(n_unique_ordinals × avg_postings). Le cache évite de résoudre
│  le même ordinal deux fois.
│
Étape 4 — Adjacency check + byte continuity
│
│  Pour chaque chain, vérifier :
│  - Positions consécutives : token_index[i+1] == token_index[i] + 1
│  - Bytes contigus : byte_from[i+1] == byte_to[i]
│
│  Utilise binary search sur les postings triés par (doc_id, token_index)
│  pour matcher les tokens de la chain entre eux.
│
│  C'est la preuve PHYSIQUE que les tokens sont adjacents dans le document.
│  Sans ça, on pourrait avoir "rag3" au début du doc et "weaver" à la fin.
```

## Les briques proposées

### Brique 1 : `fst_candidates` — FST walk sans resolve

Copie de la boucle `prefix_walk` + décodage parents de
`suffix_contains_single_token_inner`, mais sans le resolve.

```rust
pub struct FstCandidate {
    pub raw_ordinal: u64,
    pub si: u16,
    pub token_len: u16,
}

/// FST walk for a literal. Returns parent entries without resolve.
/// Selectivity estimate = result.len() (fewer = more selective).
pub fn fst_candidates(
    sfx_reader: &SfxFileReader<'_>,
    literal: &str,
) -> Vec<FstCandidate>
```

**Source** : lignes 121-158 de `suffix_contains.rs` (la boucle prefix_walk +
parents), mais sans la boucle resolve `raw_term_resolver(parent.raw_ordinal)`.

**Coût** : O(FST range scan). Typiquement <0.01ms.

### Brique 2 : `resolve_candidates` — Resolve postings avec filtre doc_ids

```rust
/// Resolve posting entries for FstCandidates.
/// If filter_docs is Some, uses resolve_filtered for O(k log n).
pub fn resolve_candidates(
    candidates: &[FstCandidate],
    literal_len: usize,
    resolver: &dyn PostingResolver,
    filter_docs: Option<&HashSet<DocId>>,
) -> Vec<LiteralMatch>
```

**Source** : lignes 136-156 de `suffix_contains.rs` (la boucle resolve), mais
avec `resolve_filtered(ordinal, &doc_ids)` quand `filter_docs` est fourni.

`literal_len` est nécessaire pour calculer `byte_to = byte_from + si + literal_len`.

### Brique 3 : `cross_token_falling_walk` — Falling walk sans resolve

Copie de l'étape 1-2 de `cross_token_search_with_terms` : falling_walk +
sibling chain DFS. Retourne les chains valides SANS résoudre les postings.

```rust
pub struct CrossTokenChain {
    pub ordinals: Vec<u64>,    // ordinals dans l'ordre de la chain
    pub first_si: u16,        // si du premier candidat (split point)
    pub prefix_len: usize,    // bytes consommés par le premier token
}

/// Falling walk + sibling chain DFS. Returns valid chains without resolve.
pub fn cross_token_falling_walk(
    sfx_reader: &SfxFileReader<'_>,
    literal: &str,
    fuzzy_distance: u8,
    ord_to_term: &dyn Fn(u64) -> Option<String>,
) -> Vec<CrossTokenChain>
```

**Source** : lignes 1154-1272 de `suffix_contains.rs` (étapes 1-2).

**Coût** : O(n_candidates × sibling_branching × depth). Pas de resolve.

### Brique 4 : `resolve_chains` — Resolve + adjacency check avec filtre

```rust
/// Resolve postings for cross-token chains and verify adjacency.
/// If filter_docs is Some, only resolves for docs in the set.
pub fn resolve_chains(
    chains: &[CrossTokenChain],
    literal_len: usize,
    resolver: &dyn PostingResolver,
    filter_docs: Option<&HashSet<DocId>>,
) -> Vec<LiteralMatch>
```

**Source** : lignes 1292-1390 de `suffix_contains.rs` (étapes 3-4).

Le `ordinal_cache` reste pour éviter les double resolve. Le filtre s'applique
au resolve, pas au adjacency check (qui est O(n log n) et rapide).

## Pipeline fuzzy trigram optimisé

```
Phase A — Estimation sélectivité (quasi gratuit)
  Pour chaque trigram i:
    fst_cands[i] = fst_candidates(sfx, trigram[i])
    ct_cands[i]  = cross_token_falling_walk(sfx, trigram[i], 0, ord_to_term)
    selectivity[i] = fst_cands[i].len() + ct_cands[i].len()
  Trier les indices par selectivity croissante (plus rare = premier)

Phase B — Resolve progressif avec filtrage
  doc_set = None  // pas de filtre pour le premier
  Pour chaque trigram en ordre de sélectivité:
    single = resolve_candidates(fst_cands[i], resolver, doc_set)
    cross  = resolve_chains(ct_cands[i], resolver, doc_set)
    all_matches[i] = single ∪ cross
    // Mettre à jour doc_set pour le prochain trigram
    new_docs = {m.doc_id for m in all_matches[i]}
    doc_set = match doc_set {
      None => Some(new_docs),
      Some(prev) => Some(prev ∩ new_docs),
    }
    if doc_set.as_ref().map(|s| s.is_empty()) == Some(true) {
      break  // short-circuit : plus aucun doc possible
    }

Phase C — Intersection (inchangée)
  intersect_trigrams_with_threshold(grouped, ...) → candidats finaux

Phase D — DFA validation (inchangée, avec anchored window + proven skip)
```

### Gains estimés par phase

| Phase | Avant | Après | Gain |
|---|---|---|---|
| A. Sélectivité | 0 (pas fait) | O(N × FST_walk) ≈ 0.1ms | permet Phase B |
| B. Resolve trigram commun | O(n_postings) ≈ 10ms | O(k × log n) ≈ 0.1ms | **99%** |
| B. Resolve trigram rare | O(n_postings) ≈ 0.1ms | O(n_postings) ≈ 0.1ms | 0% (premier) |
| C. Intersection | inchangé | inchangé | 0% |
| D. DFA validation | déjà optimisé (anchored + proven) | inchangé | 0% |
| **Total** | ~20ms | ~2ms | **~90%** |

Les gains sont dominés par Phase B : les trigrams communs ("the", "ing") ne
résolvent plus que dans les quelques docs survivants du trigram rare.

## Détail : pourquoi le falling walk est séparé du prefix_walk

`prefix_walk(query)` cherche le query comme **suffix** de tokens existants.
Si le query est "3we", il cherche les tokens qui contiennent "3we" comme
sous-chaîne. Ça matche si un seul token contient "3we" (ex: token "a3west").

`falling_walk(query)` cherche des points de **split** où le query peut
se couper entre deux tokens. Il walk byte par byte et à chaque noeud final
vérifie si `si + prefix_len == token_len` — le prefix consommé atteint la
fin du token. Ça ne matche PAS les tokens qui contiennent le query au milieu.

Pour "3we" :
- `prefix_walk("3we")` → trouve les tokens qui contiennent "3we" (substring)
- `falling_walk("3we")` → trouve les tokens qui TERMINENT par un prefix de
  "3we", ex: token "rag3" finit par "3" (1 char), token "ab3" finit par "3".
  Le split point est après le "3", remainder = "we".
  Puis sibling chain : "rag3" → sibling "weaver" → "we".starts_with("we") ✓

Les deux sont nécessaires : prefix_walk pour single-token matches,
falling_walk pour cross-token matches.

## Fichiers

- `src/query/phrase_query/literal_pipeline.rs` — les 4 briques
- Pas de modification de `suffix_contains.rs` ni `literal_resolve.rs`
- `fuzzy_contains_via_trigram` dans `regex_continuation_query.rs` utilisera
  les briques au lieu de `find_literal`

## Ce qui profite aux autres queries

- **Regex multi-literal** : pourrait utiliser le même pipeline pour résoudre
  les littéraux extraits du regex par ordre de sélectivité
- **Contains exact multi-token** : pourrait bénéficier du filtrage doc_ids
  progressif (actuellement fait en post-intersection)
- **startsWith** : pourrait utiliser `fst_candidates` avec `prefix_walk_si0`

## Estimation d'effort

| Brique | Lignes | Source | Complexité |
|---|---|---|---|
| 1. fst_candidates | ~30 | suffix_contains.rs:121-158 | copie partielle |
| 2. resolve_candidates | ~40 | suffix_contains.rs:136-156 | + filter_docs |
| 3. cross_token_falling_walk | ~80 | suffix_contains.rs:1154-1272 | copie étapes 1-2 |
| 4. resolve_chains | ~80 | suffix_contains.rs:1292-1390 | + filter_docs |
| Pipeline fuzzy | ~50 | regex_continuation_query.rs:577-587 | refactor Step 1 |
| **Total** | **~280** | | 0 lignes modifiées dans l'existant |
