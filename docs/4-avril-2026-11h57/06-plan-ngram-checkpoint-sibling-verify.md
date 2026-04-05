# 06 — Plan : ngram checkpoint + sibling verification

Date : 4 avril 2026 ~17h

---

## Contexte

Le trigram pigeonhole actuel résout les postings de chaque ngram indépendamment, puis intersecte par byte position (heuristique). Pour les queries multi-mots avec des bigrams courts, ça produit des milliers de faux candidats et 800ms+ de DFA walks.

Le fuzzy_falling_walk pur est trop lent sur des segments longs.

**Solution** : utiliser les ngrams comme fast-path FST pour identifier les tokens parents, puis les siblings pour vérifier l'adjacence. Pas de DFA walk séparé.

---

## Les ngrams se chevauchent

Pour "rag3weaver" (1 segment alphanum), les trigrams sont :
```
pos 0: "rag" → si=0 dans parent "rag3" (ou "rag3weaver" si pas CamelCase split)
pos 1: "ag3" → si=1 dans le MÊME parent "rag3"
pos 2: "g3w" → CROSS-TOKEN : si=2 dans "rag3" (ne finit PAS le token)
                → OU cross-token falling_walk : "g3" fin de "rag3" + "w" début de "weaver"
pos 3: "3we" → cross-token : "3" fin de "rag3" + "we" début de "weaver"
pos 4: "wea" → si=0 dans parent "weaver"
pos 5: "eav" → si=1 dans le MÊME parent "weaver"
pos 6: "ave" → si=2 dans "weaver"
pos 7: "ver" → si=3 dans "weaver"
```

Points clés :
- Les ngrams consécutifs du même token ont des **si qui se suivent** (si+1 à chaque pas)
- Les ngrams cross-token ("g3w", "3we") traversent la frontière token → le falling_walk les détecte
- Un ngram à si=0 marque le **début** d'un token
- Un ngram dont si + n == token_len marque la **fin** d'un token

---

## Checkpoint par ngram

Chaque ngram résolu par `fst_candidates` donne un checkpoint :

```rust
struct NgramCheckpoint {
    tri_idx: usize,         // index dans la query
    parent_ordinal: u64,    // token parent dans le SFX
    si: u16,                // début du ngram dans le token
    token_len: u16,         // longueur du token parent
    // Dérivés :
    si_end: u16,            // si + n (fin du ngram dans le token)
    at_token_end: bool,     // si_end == token_len
    at_token_start: bool,   // si == 0
}
```

Pour les ngrams cross-token (trouvés via `cross_token_falling_walk`), on a une `CrossTokenChain` avec N ordinals. Le checkpoint est différent :

```rust
struct NgramCheckpoint {
    // ... same fields ...
    // Pour cross-token :
    last_ordinal: u64,      // dernier token de la chaîne cross-token
    last_si_end: u16,       // position de fin dans le dernier token
}
```

---

## Règles de cohérence entre checkpoints consécutifs

### Règle 1 : même mot, ngrams consécutifs (pos i, pos i+1)

Les deux ngrams se chevauchent de (n-1) bytes dans la query. Dans le contenu :

**Cas A — même token parent** :
```
ngram[i].parent_ordinal == ngram[i+1].parent_ordinal
ngram[i+1].si == ngram[i].si + 1
```
Vérification : ordinal identique, si incrémente de 1. Trivial.

**Cas B — cross-token (le ngram chevauche la frontière)** :
```
ngram[i] est en fin de token (at_token_end ou proche)
ngram[i+1] est en début de token suivant
```
Vérification : `siblings(ngram[i].parent_ordinal)` contient `ngram[i+1].parent_ordinal`
(contiguous_siblings car même mot → pas de séparateur)

**Cas C — ngram manquant (pigeonhole, d>0)** :
Le ngram[i+1] manque (pas trouvé par fst_candidates). C'est OK si le budget d'edit le permet. On saute au prochain ngram trouvé et on vérifie la cohérence avec un gap toléré.

### Règle 2 : transition cross-word (séparateur dans la query)

Le dernier ngram du mot N et le premier ngram du mot N+1 sont dans des tokens DIFFÉRENTS séparés par un gap.

```
ngram_last_of_word_N.at_token_end == true (ou proche, selon edits)
siblings(ngram_last_of_word_N.parent_ordinal) avec gap_len > 0
    contient ngram_first_of_word_N1.parent_ordinal
```
Vérification : `siblings()` (pas `contiguous_siblings()`) car on accepte un gap (le séparateur query).

### Règle 3 : tolérance d'edit dans les si

Avec d=1, les si peuvent être décalés de ±1 :
- Insertion dans le contenu : si+1 au lieu de si attendu
- Suppression dans le contenu : si-1 au lieu de si attendu
- Substitution : si identique mais le byte est différent (pas visible au niveau si)

Tolérance : `|si_actual - si_expected| <= distance`

---

## Algorithme proposé

### Étape 1 : générer les checkpoints

Pour chaque ngram de la query :
```rust
let checkpoints: Vec<Vec<NgramCheckpoint>> = ngrams.iter().enumerate()
    .map(|(i, gram)| {
        let mut cps = Vec::new();
        // Single-token matches
        for cand in fst_candidates(sfx_reader, gram) {
            cps.push(NgramCheckpoint {
                tri_idx: i,
                parent_ordinal: cand.raw_ordinal,
                si: cand.si,
                token_len: cand.token_len,
                si_end: cand.si + n as u16,
                at_token_end: cand.si + n as u16 == cand.token_len,
                at_token_start: cand.si == 0,
                last_ordinal: cand.raw_ordinal,
                last_si_end: cand.si + n as u16,
            });
        }
        // Cross-token matches
        for chain in cross_token_falling_walk(sfx_reader, gram, 0, ord_to_term) {
            let first = chain.ordinals[0];
            let last = *chain.ordinals.last().unwrap();
            // ... build checkpoint with first/last ordinals ...
            cps.push(...);
        }
        cps
    })
    .collect();
```

Coût : O(FST walk) par ngram. Pas de resolve de postings. Rapide.

### Étape 2 : construire des chaînes cohérentes

Parcourir les ngrams dans l'ordre de la query. Pour chaque ngram, garder les checkpoints compatibles avec le checkpoint précédent :

```rust
fn build_coherent_chains(
    checkpoints: &[Vec<NgramCheckpoint>],
    word_ids: &[usize],
    sibling_table: &SiblingTableReader,
    distance: u8,
) -> Vec<CoherentChain> {
    // DFS/BFS sur les checkpoints compatibles
    // State: (ngram_idx, current_checkpoint, edits_used, chain_so_far)
    
    // Pour chaque checkpoint du ngram[0], essayer de chaîner avec ngram[1], etc.
    // Compatibilité vérifiée par les règles 1-3 ci-dessus.
    
    // Pigeonhole : on peut sauter des ngrams manquants (budget d'edit)
    // Threshold : besoin de K ngrams sur N (K = N - n*d)
}
```

Coût : O(checkpoints_per_ngram × chain_length). Typiquement très peu de checkpoints par ngram (1-5 tokens parents). Beaucoup plus rapide que 21000 candidates.

### Étape 3 : resolve postings seulement pour les chaînes cohérentes

Pour chaque chaîne cohérente, résoudre les postings du premier ordinal :
```rust
let postings = resolver.postings(chain.first_ordinal);
for entry in postings {
    // Vérifier adjacence dans le doc : les ordinals de la chaîne
    // doivent être à des positions consécutives dans ce doc
    // (même vérif que resolve_chains actuel)
}
```

Coût : O(postings de la chaîne). Seulement les chaînes validées par siblings.

### Étape 4 : highlight

La chaîne donne :
- Premier token : ordinal + si → byte_from = posting.byte_from + si
- Dernier token : ordinal → byte_to = posting.byte_to (ou byte_from + query_end)

Pas de DFA walk pour le highlight — il est exact depuis les positions des postings.

---

## Gestion du budget d'edit

Le budget d'edit d est **global** pour toute la query. Un edit consommé dans un segment réduit le budget pour les autres.

Dans l'étape 2, le DFS porte `edits_used`. Quand un ngram manque (pas de checkpoint compatible) :
- `edits_used += n` (un ngram de taille n manquant coûte au plus n edits)
- Si `edits_used > d` → branche impossible, prune

Quand un checkpoint a un si décalé :
- `edits_used += |si_actual - si_expected|`

Quand une transition cross-word utilise un sibling indirect (1 hop) :
- `edits_used += longueur du token intermédiaire`

---

## Comparaison avec l'approche actuelle

| | Actuel (trigram pigeonhole) | Proposé (ngram checkpoint + siblings) |
|---|---|---|
| Identification | Résout postings de chaque ngram | FST walk par ngram (pas de resolve) |
| Filtrage | intersect_trigrams_with_threshold (heuristique) | Cohérence ordinal + siblings (exact) |
| Validation | DFA walk par candidat (21000×) | Intégrée dans le checkpoint (0 DFA) |
| Coût "3db_val" d=1 | 877ms (21000 DFA walks) | ~5ms estimé (few checkpoints × sibling lookups) |
| Monotonie | Violée (9 docs missing) | Garantie (siblings = adjacence exacte) |

---

## Fichiers à modifier

| Fichier | Modification |
|---------|-------------|
| `src/query/phrase_query/literal_pipeline.rs` | `NgramCheckpoint`, `build_coherent_chains()` |
| `src/query/phrase_query/regex_continuation_query.rs` | Router multi-word d>0 vers le nouveau pipeline |
| `src/suffix_fst/sibling_table.rs` | Exposer `siblings()` (pas juste contiguous) |

---

## Questions à trancher avant implémentation

1. **Quand utiliser le nouveau pipeline vs l'ancien ?**
   - Nouveau : queries multi-mot (normalized query contient des espaces)
   - Ancien (trigram pigeonhole) : queries single-word (rag3weaver, rak3weaver)
   - Ou : toujours le nouveau, le single-word est juste 1 segment

2. **Cross-token ngrams dans les checkpoints**
   - Un ngram cross-token ("g3w" entre "rag3" et "weaver") a 2 ordinals
   - Le checkpoint doit porter les deux (first_ordinal, last_ordinal)
   - La cohérence avec le ngram suivant se base sur last_ordinal

3. **Performance du DFS checkpoints**
   - Si un ngram a beaucoup de checkpoints (token très commun), le DFS explose
   - Mitigation : pigeonhole → commencer par les ngrams les plus rares (moins de checkpoints)
   - Même stratégie de sélectivité que l'actuel
