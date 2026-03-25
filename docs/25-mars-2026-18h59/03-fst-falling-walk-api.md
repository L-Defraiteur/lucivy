# Doc 03 — FST falling walk : API et algorithme cross-token

Date : 25 mars 2026

## API FST disponible (lucivy-fst)

Le FST expose tout ce qu'il faut pour un walk byte-by-byte :

```rust
// Accès au root
let node = fst.root();                    // Node<'_>

// Suivre un byte
node.find_input(byte) -> Option<usize>    // trouve la transition pour ce byte
node.transition(idx) -> Transition        // { inp: u8, out: Output, addr: CompiledAddr }

// Naviguer
fst.node(addr) -> Node<'_>               // noeud à l'adresse donnée

// État final
node.is_final() -> bool                  // ce noeud est une clé complète dans le FST
node.final_output() -> Output            // la valeur associée si final
```

Coût par step : 1 `find_input` (binary search sur ~1-4 transitions) + 1 pointer chase.
Total pour un walk de L bytes : O(L).

## Partitions SFX

Le SFX a deux partitions, préfixées par un byte :
- `\x00<suffix>` → SI=0 (le suffix EST le token entier, ou le token commence par ce suffix)
- `\x01<suffix>` → SI>0 (le suffix est une sous-chaîne interne du token)

Pour un cross-token search, le LEFT part peut être dans les deux partitions :
- SI=0 si la query prefix = le token entier
- SI>0 si la query prefix = un suffixe interne qui atteint la fin du token

Le RIGHT part doit être en SI=0 (début du token suivant).

## Algorithme : falling walk complet

### Principe

Un seul walk à travers le FST. À chaque byte, on enregistre les états finaux
rencontrés. Après le walk (ou le fall), on filtre les candidats par token_len
et on vérifie l'adjacence.

### Pseudo-code

```rust
fn cross_token_search(
    sfx_reader: &SfxFileReader,
    query: &str,
    resolver: impl Fn(u64) -> Vec<PostingEntry>,
) -> Vec<Match> {
    // 1. Single-token search (fast path)
    let single = sfx_reader.resolve_suffix(query);
    let single_matches = single.iter()
        .filter(|p| p.si as usize + query.len() <= p.token_len as usize)
        .collect();
    if !single_matches.is_empty() {
        return resolve_and_return(single_matches, &resolver);
    }

    // 2. Falling walk — collect ALL split candidates in ONE pass
    //    Walk both \x00 and \x01 partitions simultaneously.
    let mut candidates: Vec<SplitCandidate> = Vec::new();

    for partition in [SI0_PREFIX, SI_REST_PREFIX] {
        let fst = sfx_reader.fst();
        let mut node = fst.root();
        let mut output = Output::zero();

        // Follow partition prefix byte
        let Some(idx) = node.find_input(partition) else { continue };
        let trans = node.transition(idx);
        output = output.cat(trans.out);
        node = fst.node(trans.addr);

        // Walk query bytes
        for (i, &byte) in query.as_bytes().iter().enumerate() {
            let Some(idx) = node.find_input(byte) else { break }; // fall
            let trans = node.transition(idx);
            output = output.cat(trans.out);
            node = fst.node(trans.addr);

            // If this prefix is a complete key, record it as split candidate
            if node.is_final() {
                let val = output.cat(node.final_output()).value();
                let prefix_len = i + 1;
                let parents = decode_parents(val);  // decode ParentRef

                // Filter by token_len: does this prefix reach the end of the token?
                for parent in parents {
                    if parent.si as usize + prefix_len == parent.token_len as usize {
                        // This prefix consumes the token to the end → valid split
                        candidates.push(SplitCandidate {
                            prefix_len,
                            parent,
                            partition,
                        });
                    }
                }
            }
        }
    }

    if candidates.is_empty() {
        return Vec::new(); // No cross-token match possible
    }

    // 3. For each split candidate, check the remainder at next token
    let mut results = Vec::new();

    for cand in &candidates {
        let remainder = &query[cand.prefix_len..];
        if remainder.is_empty() { continue; }

        // Remainder must be a PREFIX of the next token (SI=0)
        let right_parents = sfx_reader.prefix_walk_si0(remainder);
        if right_parents.is_empty() { continue; }

        // 4. Resolve postings and check adjacency (position N, N+1)
        //    Use pivot-first: resolve the smaller side first.
        let left_postings = resolver(cand.parent.raw_ordinal);
        for left_entry in &left_postings {
            let expected_next_pos = left_entry.token_index + 1;
            for (_suffix, right_parents_list) in &right_parents {
                for right_parent in right_parents_list {
                    let right_postings = resolver(right_parent.raw_ordinal);
                    for right_entry in &right_postings {
                        if right_entry.doc_id == left_entry.doc_id
                            && right_entry.token_index == expected_next_pos
                        {
                            results.push(CrossTokenMatch {
                                doc_id: left_entry.doc_id,
                                left_entry: left_entry.clone(),
                                right_entry: right_entry.clone(),
                                left_si: cand.parent.si,
                            });
                        }
                    }
                }
            }
        }
    }

    results
}
```

## Cas couverts

| Cas | Fonctionne ? | Explication |
|-----|-------------|-------------|
| Query dans 1 token | ✓ | Single-token fast path |
| Query chevauche 2 tokens | ✓ | Falling walk trouve le(s) split(s), adjacence vérifie |
| Query chevauche 3+ tokens | ✓ via récursion | Remainder trop long → cross_token_search(remainder) |
| Plusieurs splits valides | ✓ | Walk enregistre TOUS les is_final(), on teste tous |
| Intra-token (même ordinal) | ✓ | token_len filter : si + prefix_len < token_len → pas un split |
| Query courte (1-2 chars) | ✓ | Single-token path ou très peu de candidates |

## Cas subtils

### Split non-unique

```
Tokens: ["ab", "bc", "cd"]
Query: "abc"

Walk \x00 partition "abc":
  'a' → node, pas final
  'b' → node, final! val = parent("ab", si=0, token_len=2)
    si(0) + 2 == token_len(2) ✓ → candidate split at 2
  'c' → no transition → fall

Walk \x01 partition "abc":
  'a' → si=1 dans "ab" → final! val = parent("ab", si=1, token_len=2)
    si(1) + 1 == token_len(2) ✓ → candidate split at 1
  'b' → continue ou fall...

Candidates: split at 2 ("ab" | "c") et split at 1 ("a" | "bc")

Split at 2: remainder "c" → prefix_walk_si0("c") → "cd"(pos 2)
  Adjacence: "ab"(pos 0) + "cd"(pos 2) → 0+1=1 ≠ 2 ✗

Split at 1: remainder "bc" → prefix_walk_si0("bc") → "bc"(pos 1)
  Adjacence: "ab"(pos 0) + "bc"(pos 1) → 0+1=1 ✓

→ Seul le split at 1 produit un résultat.
```

### Multi-token (3+ tokens)

```
Tokens: ["abc", "def", "ghi"]
Query: "cdefg"

Falling walk finds: "c" at SI=2 in "abc", si(2)+1==3==token_len ✓ → split at 1
Remainder: "defg"

Récursion: cross_token_search("defg")
  Single-token: "defg" pas dans un seul token
  Falling walk: "def" at SI=0 in "def", si(0)+3==3 ✓ → split at 3
  Remainder: "g" → prefix_walk_si0("g") → "ghi" ✓

→ Match: positions [0, 1, 2]
```

## Performance

| Opération | Coût |
|-----------|------|
| Single-token resolve | O(log FST) |
| Falling walk (2 partitions) | O(2L) node lookups |
| token_len filter | O(K) comparaisons, K = is_final rencontrés |
| Remainder prefix_walk | O(log FST + R) pour R résultats |
| Posting resolve (pivot-first) | O(P) pour P postings du pivot |
| Adjacence check | O(P × Q) pour Q right postings |

Total naïf : O(L + log FST + P) — dominé par le posting resolve, pas le walk.

## Optimisation : déduplication des candidates

### Stratégie 1 : grouper par ordinal / remainder

Après le falling walk, on a N candidates. Beaucoup partagent le même left ordinal
(même token indexé, SI différents) ou le même remainder (même suffixe de query).

```
Candidates bruts après falling walk:
  [{split:2, ord:5, si:0}, {split:1, ord:5, si:1}, {split:3, ord:7, si:2}]

Étape 1 — Grouper par left ordinal :
  ord 5 → [split:2 (si=0), split:1 (si=1)]   // même token, résoudre postings 1 FOIS
  ord 7 → [split:3 (si=2)]

Étape 2 — Grouper par remainder :
  remainder "c"  → [split:2]
  remainder "bc" → [split:1]
  remainder "ef" → [split:3]
  → prefix_walk 1 FOIS par remainder unique

Étape 3 — Pivot-first au niveau des groupes :
  left_doc_ids  = resolve(ord 5) ∪ resolve(ord 7) → HashSet
  Pour chaque remainder : prefix_walk filtré par left_doc_ids
  Adjacence : (doc_id, position, position+1)
```

Coût optimisé : O(U_left × P_avg + U_right × walk_avg) où :
- U_left = nombre d'ordinals gauches uniques (souvent 1-3)
- U_right = nombre de remainders uniques (souvent 1-3)
- P_avg = taille moyenne d'une posting list

### Stratégie 2 : fusion intra-token

Quand plusieurs candidates pointent vers le **même ordinal** avec des SI différents,
elles correspondent à des substrings du **même token indexé**. Pas besoin de posting
lookup séparé — elles partagent exactement les mêmes postings.

```
Candidates pour ordinal 5 :
  split:2 → si=0, token_len=5 → si+2=2 < 5 → intra-token ? Non, si+2==token_len(2) pour un autre token ?

Mieux : si deux candidates ont le même ordinal, on résout les postings UNE SEULE FOIS
et on les réutilise pour toutes les vérifications d'adjacence.
```

En pratique, la fusion intra-token arrive quand :
- Un token long (ex: "abcdefgh") a plusieurs suffixes qui atteignent sa fin
  depuis différents points de la query
- Impossible par définition (un seul split point atteint la fin d'un token donné)
  SAUF si le même token apparaît à des ordinals différents (dans des segments diff)

La vraie valeur de la stratégie 2 c'est la **déduplication de posting resolve** :
un `HashMap<u64, Vec<PostingEntry>>` cache les résultats par ordinal.

### Les deux stratégies combinées

```rust
fn resolve_candidates(
    candidates: &[SplitCandidate],
    sfx_reader: &SfxFileReader,
    resolver: &impl Fn(u64) -> Vec<PostingEntry>,
) -> Vec<CrossTokenMatch> {
    // Cache postings par ordinal (stratégie 2)
    let mut posting_cache: HashMap<u64, Vec<PostingEntry>> = HashMap::new();

    // Grouper par remainder pour dédupliquer les prefix_walks (stratégie 1)
    let mut remainder_cache: HashMap<String, Vec<(String, Vec<ParentEntry>)>> = HashMap::new();

    // Résoudre le côté gauche — extraire doc_ids pivot (stratégie 1 pivot-first)
    let mut all_left_doc_ids: HashSet<u32> = HashSet::new();
    for cand in candidates {
        let postings = posting_cache
            .entry(cand.parent.raw_ordinal)
            .or_insert_with(|| resolver(cand.parent.raw_ordinal));
        for p in postings.iter() {
            all_left_doc_ids.insert(p.doc_id);
        }
    }

    // Résoudre les remainders — filtré par left_doc_ids
    let mut results = Vec::new();
    for cand in candidates {
        let remainder = &cand.remainder;
        let right_walks = remainder_cache
            .entry(remainder.clone())
            .or_insert_with(|| sfx_reader.prefix_walk_si0(remainder));

        let left_postings = &posting_cache[&cand.parent.raw_ordinal];
        for left_entry in left_postings {
            if !all_left_doc_ids.contains(&left_entry.doc_id) { continue; }
            let expected_pos = left_entry.token_index + 1;
            for (_suffix, right_parents) in right_walks.iter() {
                for right_parent in right_parents {
                    let right_postings = posting_cache
                        .entry(right_parent.raw_ordinal)
                        .or_insert_with(|| resolver(right_parent.raw_ordinal));
                    for right_entry in right_postings.iter() {
                        if right_entry.doc_id == left_entry.doc_id
                            && right_entry.token_index == expected_pos
                        {
                            results.push(CrossTokenMatch { /* ... */ });
                        }
                    }
                }
            }
        }
    }

    results
}
```

### Résumé des optimisations

| Stratégie | Ce qu'elle évite | Gain typique |
|-----------|-----------------|--------------|
| Grouper par ordinal | Posting resolve dupliqué pour le même token | 2-5x |
| Grouper par remainder | prefix_walk dupliqué pour le même suffixe | 2-3x |
| Pivot-first (left doc_ids) | Posting resolve inutile côté droit | 10-100x |
| Cache par ordinal | Re-resolve du même ordinal gauche/droit | 2-5x |

Total combiné : le coût est dominé par le nombre d'ordinals **uniques** côté pivot,
pas par le nombre de candidates.

## Implémentation

### Phase 1 : API FST

Ajouter sur `SfxFileReader` :

```rust
/// Walk the FST byte-by-byte with the query, collecting all split candidates
/// where a prefix reaches the end of its parent token (si + prefix_len == token_len).
pub fn falling_walk(&self, query: &str) -> Vec<SplitCandidate> { ... }
```

### Phase 2 : cross_token_search

Ajouter dans `suffix_contains.rs` :

```rust
/// Search for a query that may span multiple indexed tokens.
/// Falls back from single-token search to cross-token via falling_walk.
pub fn cross_token_search(...) -> Vec<Match> { ... }
```

### Phase 3 : Intégration

Remplacer l'appel single-token dans `suffix_contains_single_token_inner` par :
```rust
let matches = single_token_search(query);
if matches.is_empty() {
    cross_token_search(query, sfx_reader, resolver)
} else {
    matches
}
```

## Questions

1. Le falling walk doit-il aussi collecter les entrées multi-parent (OutputTable) ?
   Oui — quand node.is_final() et que le val décode en ParentRef::Multi, on doit
   lire la OutputTable pour obtenir tous les parents et filtrer par token_len.

2. Faut-il un cache pour éviter de re-résoudre le même ordinal dans les posting lookups ?
   Oui, un HashMap<u64, Vec<PostingEntry>> serait utile si plusieurs candidates partagent
   le même ordinal.

3. La récursion pour 3+ tokens a-t-elle une profondeur max ?
   En théorie non, mais en pratique les queries > 3 tokens sont très rares avec des
   tokens de 256 chars max. On peut limiter à 5 niveaux par sécurité.
