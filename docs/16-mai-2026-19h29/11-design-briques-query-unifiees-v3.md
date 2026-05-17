# Design : Briques query unifiées v3

**Date** : 17 mai 2026  
**Contexte** : l'indexation v3 est complète (77 tests, 7 fichiers). Le query layer v2 a 3 pipelines (contains, fuzzy, regex) qui dupliquent du code. On veut unifier en briques modulaires.

---

## 1. Problème actuel (v2)

### Duplication identifiée

| Opération | suffix_contains | fuzzy_contains | regex_continuation |
|-----------|:---:|:---:|:---:|
| FST walk | Propre (`prefix_walk`) | Via `literal_pipeline` | Via `sfx_dict.search_continuation` |
| Falling walk | Propre (continuation loop) | Via `literal_pipeline` | Via `continuation_score` |
| Cross-token DFS | Propre (`cross_token_search_with_terms`) | Via `cross_token_falling_walk` | Via sibling feeding au DFA |
| Adjacence verification | Propre (active set) | Via `resolve_chains` | Via position check dans continuation |
| Doc filtering | Aucun | Rarest-first | Prescan cache |

3 implémentations parallèles pour la même chose. Le code v2 dans `literal_pipeline.rs` est le plus modulaire (4 briques composables), mais `suffix_contains.rs` ne l'utilise pas pour le single-token.

### Ce qui disparaît en v3

- `concat_query()` → query telle quelle
- `boundary_positions()` / `boundary_trigram_indices()` → plus de boundary trigrams (overlap couvre tout)
- Sibling DFS → falling walk chaîné (TI+1 implicite)
- GapMap feeding au DFA → seps dans les tokens
- `continuation_score_sibling` → `continuation_score` seul (ou mieux, briques unifiées)

---

## 2. Architecture cible : 3 tiers de briques

```
┌─────────────────────────────────────────────────────┐
│                  Orchestrateurs                      │
│  contains_v3    fuzzy_v3    regex_v3                 │
│  (thin, ~100 lignes chacun)                         │
├─────────────────────────────────────────────────────┤
│              Tier 3 — Composites                     │
│  find_literal_v3    find_multi_token_v3              │
│  resolve_trigrams_v3    dfa_continuation_v3          │
├─────────────────────────────────────────────────────┤
│           Tier 2 — Résolution postings               │
│  resolve_single_v3    resolve_chains_v3              │
│  selectivity_v3                                      │
├─────────────────────────────────────────────────────┤
│            Tier 1 — FST Walk                         │
│  fst_candidates_v3    falling_walk_v3               │
│  cross_token_chain_v3                                │
└─────────────────────────────────────────────────────┘
```

Chaque tier ne dépend que du tier en dessous. Les orchestrateurs composent les briques pour chaque type de query.

---

## 3. Tier 1 — FST Walk (`briques/fst_walk.rs`)

Primitives FST pures. Pas de résolution de postings. Coût : O(query_len).

### 3.1 `fst_candidates_v3`

```rust
/// Cherche dans le FST v3 tous les suffixes qui matchent le littéral.
/// Remplace : fst_candidates, prefix_walk, prefix_walk_si0
pub fn fst_candidates_v3(
    sfx: &SfxFileReaderV3,
    query: &str,
    anchor_start: bool,  // SI=0 only (startsWith)
) -> Vec<FstCandidateV3>
```

```rust
pub struct FstCandidateV3 {
    pub raw_ordinal: u64,
    pub sti: u16,
    pub own_len: u16,
    pub content_len: u16,   // = own_len - sep_len
    pub sep_len: u8,
    pub overlap_len: u8,
    pub is_word_start: bool,
}
```

### 3.2 `falling_walk_v3`

Le mécanisme central. Remplace `falling_walk`, `fuzzy_falling_walk`, et le rôle de la sibling table.

```rust
/// Falling walk v3 : byte-by-byte dans le FST avec :
/// - Split point à STI + consumed == own_len
/// - Continuation dans l'overlap zone (ne pas break)
/// - Sep-skip intégré (strict_separators=false)
/// - Fuzzy via Levenshtein DFA (distance > 0)
pub fn falling_walk_v3(
    sfx: &SfxFileReaderV3,
    query: &str,
    strict_separators: bool,
    fuzzy_distance: u8,
) -> Vec<SplitCandidateV3>
```

```rust
pub struct SplitCandidateV3 {
    /// Bytes de la query consommés par ce token (jusqu'au split point).
    pub query_consumed: usize,
    /// Le parent entry v3 (ordinal, sti, own_len, sep_len, overlap_len, is_word_start).
    pub parent: ParentEntryV3,
    /// Byte offset dans la query où le prochain token commence.
    /// Diffère de query_consumed quand strict_sep=false (les seps sont skippés).
    pub remainder_start: usize,
    /// Nombre de bytes d'overlap validés (0..overlap_len).
    pub overlap_validated: usize,
}
```

#### Algorithme falling walk v3 (exact, d=0)

```
Pour chaque partition (0x00, 0x01) :
  node = racine FST
  query_cursor = 0    // position dans la query
  token_cursor = 0    // position dans le token (= sti + depth)

  Boucle sur les bytes :
    // Sep-skip check
    Si token_cursor == content_len ET !strict_separators :
      Skip sep_len bytes dans le token (avancer token_cursor sans comparer)
      Skip bytes non-alphanum dans la query (avancer query_cursor)
      Continuer la boucle
    
    Si strict_separators ET token_cursor >= content_len ET token_cursor < own_len :
      // Dans la zone sep : comparer byte par byte (sep query vs sep token)
      // Si mismatch : break

    byte = query[query_cursor]
    Suivre transition pour byte dans le FST node
    Si pas trouvé : break
    
    token_cursor += 1
    query_cursor += 1
    
    Si node.is_final() :
      Décoder parents
      Pour chaque parent :
        Si sti + depth == own_len :
          → SPLIT POINT
          Émettre SplitCandidate(query_consumed=query_cursor, remainder_start=query_cursor)
          (ne pas break — continuer dans l'overlap)
```

#### Sep-skip détaillé

```
Texte indexé : "mutex____lock"
Token : "mutex____lo" (content_len=5, sep_len=4, overlap_len=2, own_len=9)

Query "mutexlock" strict_sep=false :
  Walk bytes 0-4 : m-u-t-e-x → match (content zone)
  token_cursor=5 == content_len → SEP SKIP
    token_cursor += 4 (sep_len) → token_cursor=9 = own_len
    Avancer query_cursor : query[5]='l' est alphanum → 0 bytes skippés
  → On est au split point déjà (token_cursor == own_len)
  → Émettre split. Puis tenter continuation dans overlap :
    query[5]='l' vs token[9]='l' → match (overlap zone)
    query[6]='o' vs token[10]='o' → match (overlap zone)
  → overlap_validated = 2
  Remainder = query[7..] = "ck"

Query "mutex lock" strict_sep=false :
  Walk bytes 0-4 : m-u-t-e-x → match
  token_cursor=5 == content_len → SEP SKIP
    token_cursor += 4 (sep_len)
    Avancer query_cursor : query[5]=' ' est non-alphanum → skip 1 byte → query_cursor=6
  → Split point. Overlap :
    query[6]='l' vs token[9]='l' → match
    query[7]='o' vs token[10]='o' → match
  Remainder = query[8..] = "ck"

Query "mutex____lock" strict_sep=true :
  Walk bytes 0-8 : m-u-t-e-x-_-_-_-_ → match (content + sep, byte par byte)
  token_cursor=9 == own_len → SPLIT
  Overlap : l-o → match
  Remainder = "ck"
```

### 3.3 `cross_token_chain_v3`

```rust
/// Chaîner les falling walks : pour chaque split, nouveau falling_walk sur le remainder.
/// Pas de sibling DFS — le FST trie est le filtre le plus sélectif.
///
/// Remplace : cross_token_falling_walk, cross_token_search_with_terms
pub fn cross_token_chain_v3(
    sfx: &SfxFileReaderV3,
    query: &str,
    strict_separators: bool,
    fuzzy_distance: u8,
    max_depth: usize,  // guard rail, défaut 8
) -> Vec<TokenChainV3>
```

```rust
pub struct TokenChainV3 {
    pub ordinals: Vec<u64>,
    pub first_sti: u16,
    pub total_query_consumed: usize,
}
```

Algorithme :
```
falling_walk_v3(query) → split candidates
Pour chaque split :
  Si remainder vide → chain terminée, émettre
  Si depth >= max_depth → abandonner
  falling_walk_v3(remainder) → nouveaux splits
  Pour chaque sous-split :
    Combiner dans la chain, continuer récursivement
```

---

## 4. Tier 2 — Résolution postings (`briques/resolve.rs`)

### 4.1 `resolve_single_v3`

```rust
/// Résoudre des candidats single-token en matches doc.
/// Remplace : resolve_candidates (literal_pipeline)
pub fn resolve_single_v3(
    candidates: &[FstCandidateV3],
    query_len: usize,
    resolver: &dyn PostingResolver,
    filter_docs: Option<&HashSet<DocId>>,
) -> Vec<MatchV3>
```

### 4.2 `resolve_chains_v3`

```rust
/// Résoudre des chaînes cross-token avec vérification adjacence.
/// Adjacence = position_right == position_left + 1.
/// Remplace : resolve_chains (literal_pipeline), le active set de suffix_contains,
/// et la vérification adjacence de continuation_score_sibling.
pub fn resolve_chains_v3(
    chains: &[TokenChainV3],
    query_len: usize,
    resolver: &dyn PostingResolver,
    filter_docs: Option<&HashSet<DocId>>,
) -> Vec<MatchV3>
```

### 4.3 Type unifié

```rust
/// Match unifié v3.
/// Remplace : LiteralMatch, SuffixContainsMatch, SuffixContainsMultiMatch
pub struct MatchV3 {
    pub doc_id: DocId,
    pub position: u32,    // premier token position
    pub span: u32,        // nombre de tokens couverts
    pub byte_from: u32,
    pub byte_to: u32,
    pub sti: u16,
    pub ordinal: u64,     // premier ordinal
}
```

---

## 5. Tier 3 — Composites (`briques/composite.rs`)

### 5.1 `find_literal_v3`

Point d'entrée unifié : "trouve cette string dans l'index, single ou cross-token".

```rust
pub fn find_literal_v3(
    sfx: &SfxFileReaderV3,
    query: &str,
    resolver: &dyn PostingResolver,
    strict_separators: bool,
    filter_docs: Option<&HashSet<DocId>>,
) -> Vec<MatchV3> {
    // 1. fst_candidates_v3 → single-token matches
    // 2. resolve_single_v3
    // 3. cross_token_chain_v3 → cross-token chains
    // 4. resolve_chains_v3
    // 5. Union des résultats
}
```

### 5.2 `find_multi_token_v3`

Multi-token avec pivot optimization.

```rust
pub fn find_multi_token_v3(
    sfx: &SfxFileReaderV3,
    query_tokens: &[&str],
    resolver: &dyn PostingResolver,
    anchor_start: bool,
    exact_match: bool,
    strict_separators: bool,
    filter_docs: Option<&HashSet<DocId>>,
) -> Vec<MatchV3> {
    // 1. Par token : find_literal_v3 (ou fst_candidates_v3 pour anchor_start)
    // 2. Pivot = token le plus sélectif
    // 3. Bidirectional join depuis le pivot
    // 4. Vérifier adjacence entre sous-tokens
}
```

### 5.3 `resolve_trigrams_v3`

Pipeline fuzzy simplifié. Plus de concat_query, plus de boundary trigrams.

```rust
pub fn resolve_trigrams_v3(
    sfx: &SfxFileReaderV3,
    query: &str,            // telle quelle, seps inclus
    distance: u8,
    resolver: &dyn PostingResolver,
    strict_separators: bool,
    max_doc: DocId,
) -> (BitSet, Vec<(DocId, usize, usize)>, Vec<(DocId, f32)>) {
    // 1. lowercase(query) — PAS de concat_query
    // 2. generate_trigrams(query, distance)
    //    → trigrams incluent les bytes de sep
    //    → n=2 si len ≤ 3*(d+1), sinon n=3
    // 3. threshold = max(T - n*d, 1) — PAS de correction boundary
    // 4. Par trigram : fst_candidates_v3 + selectivity estimate
    //    → PAS de cross_token_falling_walk (tous les trigrams sont single-token grâce à l'overlap)
    // 5. Trier par sélectivité croissante
    // 6. Résoudre rarest sans filtre → doc_filter
    // 7. Résoudre reste avec filtre
    // 8. build_hits_by_doc → find_matches (two-pointer sliding window)
    // 9. Scoring : miss_count
}
```

**Simplification clé** : l'étape 4 n'a PLUS de `cross_token_falling_walk`. Chaque trigram est single-token grâce à l'overlap. C'est le gain de ×40-100 en latence.

### 5.4 `dfa_continuation_v3`

Pour regex cross-token. Le DFA traverse les seps naturellement (ils sont dans le token).

```rust
pub fn dfa_continuation_v3<A: Automaton>(
    automaton: &A,
    sfx: &SfxFileReaderV3,
    resolver: &dyn PostingResolver,
    ord_to_term: &dyn Fn(u64) -> Option<String>,
    anchor_start: bool,
    max_doc: DocId,
) -> (BitSet, Vec<(DocId, usize, usize)>) {
    // Walk 1 : DFA × FST → candidates(dfa_state)
    //   Token inclut trailing sep + overlap → DFA avance plus loin
    // Walk 2 : Pour chaque non-accepting :
    //   search_continuation(DFA, dfa_state) → FST walk avec état DFA sauvé
    //   Pas de gap feeding (seps dans le token)
    //   Pas de sibling enumeration
}
```

---

## 6. Orchestrateurs (fichiers minces)

### 6.1 `contains_v3.rs`

```rust
pub fn contains_prescan_v3(
    sfx: &SfxFileReaderV3,
    query: &str,
    resolver: &dyn PostingResolver,
    anchor_start: bool,
    exact_match: bool,
    strict_separators: bool,
) -> PrescanResult {
    let tokens = tokenize_query(query);
    if tokens.len() <= 1 {
        let matches = find_literal_v3(sfx, query, resolver, strict_separators, None);
        // Appliquer anchor_start (filtre sti=0 + is_word_start)
        // Appliquer exact_match (filtre span couvre le mot entier)
    } else {
        find_multi_token_v3(sfx, &tokens, resolver, anchor_start, exact_match, strict_separators, None)
    }
}
```

### 6.2 `fuzzy_v3.rs`

```rust
pub fn fuzzy_prescan_v3(
    sfx: &SfxFileReaderV3,
    query: &str,
    distance: u8,
    resolver: &dyn PostingResolver,
    strict_separators: bool,
    max_doc: DocId,
) -> (BitSet, Vec<highlights>, Vec<coverage>) {
    resolve_trigrams_v3(sfx, query, distance, resolver, strict_separators, max_doc)
}
```

### 6.3 `regex_v3.rs`

```rust
pub fn regex_prescan_v3(
    sfx: &SfxFileReaderV3,
    pattern: &str,
    resolver: &dyn PostingResolver,
    ord_to_term: &dyn Fn(u64) -> Option<String>,
    anchor_start: bool,
    max_doc: DocId,
) -> (BitSet, Vec<highlights>) {
    let automaton = compile_regex(pattern);
    dfa_continuation_v3(&automaton, sfx, resolver, ord_to_term, anchor_start, max_doc)
}
```

---

## 7. Cas concrets détaillés

### 7.1 `contains("mutex_lock")` strict_sep=true

```
Orchestrateur : contains_v3 → 1 token → find_literal_v3
  → fst_candidates_v3("mutex_lock") → pas de match single-token (token fait 6 ou 5 bytes, pas 10)
  → cross_token_chain_v3("mutex_lock") :
      falling_walk_v3("mutex_lock") dans "mutex_lo" :
        m-u-t-e-x-_-l-o → 8 bytes
        STI(0)+6 = 6 = own_len → SPLIT à byte 6
        Continue overlap : l-o → 2 bytes validés
      Remainder = "ck" (bytes 8..10)
      falling_walk_v3("ck") → match "lock" à STI=2
      Chain : [ord_mutex_lo, ord_lock]
  → resolve_chains_v3 → vérifier adjacence → MatchV3
```

### 7.2 `contains("mutexlock")` strict_sep=false

```
cross_token_chain_v3("mutexlock", strict_sep=false) :
  falling_walk_v3("mutexlock") dans "mutex_lo" :
    m-u-t-e-x → 5 bytes = content_len → SEP SKIP
    Skip sep_len=1 dans le token
    Skip 0 bytes dans la query (query[5]='l' est alphanum)
    l-o → 2 bytes dans overlap
    STI(0)+9 > own_len(6) → on est dans l'overlap
    SPLIT à query_consumed=7, remainder_start=7
  Remainder = "ck"
  falling_walk_v3("ck") → match dans "lock" à STI=2
  Chain : [ord_mutex_lo, ord_lock]
```

### 7.3 `fuzzy("mutex_lck", d=1)` 

```
Orchestrateur : fuzzy_v3 → resolve_trigrams_v3
  Query telle quelle : "mutex_lck" (9 bytes, "_" inclus)
  Trigrams : "mut","ute","tex","ex_","x_l","_lc","lck" (7 grams)
  Boundary = [] (0 ! overlap couvre tout)
  Threshold = 7 - 3*1 = 4
  
  Par trigram : fst_candidates_v3 SEULEMENT (single-token)
    "x_l" → suffix de "mutex_lo" à STI=4 ✓ (single-token grâce à l'overlap)
  
  Pas de cross_token_falling_walk → gain ×40-100
  
  Résolution rarest-first → sliding window → miss_count scoring
```

### 7.4 `regex("mutex.*lock")`

```
Orchestrateur : regex_v3 → dfa_continuation_v3
  DFA compilé pour "mutex.*lock"
  Walk 1 : DFA × FST
    "mutex_lo" (8 bytes) : DFA consomme m-u-t-e-x-_-l-o → alive, pas accepting
    (en v2 : DFA consommait "mutex" (5 bytes) puis devait être feedé manuellement avec "_")
    → 3 bytes de plus consommés qu'en v2 grâce aux seps + overlap
  Walk 2 : search_continuation(dfa_state_après_overlap)
    "ck..." matche "lock" → DFA accepting
```

---

## 8. Interaction avec le tokenizer custom

La user veut garder le fuzzy falling walk pour le cas où quelqu'un fait une tokenization custom (pas EqualChunkTokenizer). Dans ce cas :

- Les tokens peuvent ne PAS avoir d'overlap ni de trailing sep
- Le falling walk v3 fonctionne quand même : si `overlap_len=0` et `sep_len=0`, le comportement est identique à v2
- Le `cross_token_chain_v3` fonctionne aussi : il fait un falling walk sur le remainder, qui trouvera les tokens suivants dans le FST

Le seul truc perdu : les boundary trigrams ne seront pas couverts si pas d'overlap. Le threshold sera `T - n*d` (pas de compensation boundary), ce qui est plus strict. L'utilisateur custom devra accepter un recall potentiellement plus faible sur les queries fuzzy cross-token, ou ajouter l'overlap manuellement dans son tokenizer.

---

## 9. Fichiers

### À créer

| Fichier | Contenu | Lignes est. |
|---------|---------|:-----------:|
| `src/query/phrase_query/briques/mod.rs` | Déclarations | 5 |
| `src/query/phrase_query/briques/fst_walk.rs` | Tier 1 | ~350 |
| `src/query/phrase_query/briques/resolve.rs` | Tier 2 | ~200 |
| `src/query/phrase_query/briques/composite.rs` | Tier 3 | ~400 |
| `src/query/phrase_query/contains_v3.rs` | Orchestrateur | ~100 |
| `src/query/phrase_query/fuzzy_v3.rs` | Orchestrateur | ~100 |
| `src/query/phrase_query/regex_v3.rs` | Orchestrateur | ~100 |

### À modifier

| Fichier | Changement |
|---------|------------|
| `src/query/phrase_query/mod.rs` | Ajouter modules briques + orchestrateurs |

### Inchangés (v2 compat)

`literal_pipeline.rs`, `fuzzy_contains.rs`, `suffix_contains.rs`, `regex_continuation_query.rs` — gardés pour les segments v2.

---

## 10. Ordre d'implémentation

1. **`briques/fst_walk.rs`** — le cœur. Tests sep-skip, overlap, chaînage.
2. **`briques/resolve.rs`** — adjacence, filtrage docs. Tests unitaires.
3. **`briques/composite.rs`** — `find_literal_v3` d'abord (dépend de 1+2), puis trigrams, puis DFA.
4. **Orchestrateurs** — contains, fuzzy, regex (thin wrappers).
5. **Routing** — détecter SFX3 dans SuffixContainsQuery/RegexContinuationQuery, router vers v3.
