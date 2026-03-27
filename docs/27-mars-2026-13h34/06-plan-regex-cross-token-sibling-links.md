# Doc 06 — Plan implémentation : regex cross-token via sibling links

Date : 27 mars 2026
Pour : prochaine session
Branche : `feature/cross-token-search`

## Contexte

Le regex cross-token existant (`RegexContinuationQuery`) fait un DFA walk
sur tout le SFX FST pour chaque segment → lent. Avec les sibling links,
on peut accélérer l'étape "trouver le token suivant" : sibling O(1) au lieu
de FST search O(N).

## Leçons à appliquer (de cette session)

1. **Ne PAS faire de DFS sur tout le SFX** — c'est O(FST_size), trop lent en WASM
2. **Utiliser les sibling links** — O(1) par step, pas de HashMap
3. **Filtrer tôt** — ne resolver que les ordinals utiles
4. **Pas d'eprintln dans le hot path** — catastrophique en WASM
5. **Le falling_walk exact est rapide** — l'utiliser comme point d'entrée
6. **Vec trié + partition_point** pour l'adjacency, pas HashMap
7. **ord_to_term** est gratuit (term dict standard, même ordinals que SFX)
8. **Le byte_to == byte_from check** élimine les faux positifs

## L'approche existante (RegexContinuationQuery)

Fichier : `src/query/phrase_query/regex_continuation_query.rs`

```
Walk 1: DFA regex walk sur tout le SFX → collect partial matches
Pour chaque partial match:
  Lire gap bytes (GapMap) → feed au DFA
  Walk 2: DFA continue sur le SFX (SI=0 only) → next token
  Repeat (max depth 64)
```

Problème : Walk 1 et Walk 2 scannent le SFX entier avec le DFA.

## L'approche proposée : regex + sibling links

### Principe

Le regex DFA walk ne sert que pour le PREMIER token (trouver où le regex
commence à matcher). Ensuite, les sibling links donnent les tokens suivants
en O(1). Le DFA continue à consommer les bytes du token text (via ord_to_term)
au lieu de re-scanner le FST.

### Algorithme

```
1. Walk initial : regex DFA × SFX FST (comme avant)
   → Pour chaque match partiel (DFA alive mais pas accepting) :
     - Stocker (ordinal, si, dfa_state)

2. Pour chaque match partiel :
   a. Lookup sibling_table[ordinal] → successeurs contigus (gap=0)
   b. Pour chaque successeur :
      - Get text via ord_to_term
      - Feed chaque byte du text au DFA : state = dfa.accept(state, byte)
      - Si DFA accepte en cours de route → match partiel (substring du token)
      - Si DFA accepte à la fin → match complet
      - Si DFA alive à la fin → nouveau match partiel, recurse via sibling

3. Resolve seulement les ordinals des matches finaux
4. Adjacency + byte continuity comme pour le cross-token exact
```

### Différence vs RegexContinuationQuery

| Étape | Ancien | Nouveau |
|-------|--------|---------|
| Walk 1 (initial) | DFA × SFX FST | DFA × SFX FST (identique) |
| Trouver token suivant | DFA × SFX FST (Walk 2) | sibling O(1) + ord_to_term |
| Gap handling | GapMap read + DFA feed | sibling gap_len check (gap=0 pour cross-token) |
| Coût Walk 2+ | O(FST_size) par step | O(1) + O(token_len) DFA bytes |

Le Walk 1 reste le même. C'est le Walk 2+ qui est accéléré.

### Gap handling

Pour le cross-token (gap=0), pas de gap bytes à feeder au DFA — les tokens
sont contigus. Le DFA passe directement du dernier byte du token courant
au premier byte du token suivant.

Pour gap>0 (phrase regex), il faudrait feeder les gap bytes au DFA.
Les gap bytes sont dans le GapMap (par doc_id, position). Mais on ne connaît
le doc_id qu'après le resolve → on ne peut pas feeder les gap bytes
pendant le walk. Solution : pour le regex cross-token, ne supporter que
gap=0 (contigus). Le regex phrase search reste sur RegexContinuationQuery.

### Implémentation step by step

#### Step 1 : Extraire la logique de Walk 1 dans une fonction réutilisable

`continuation_score` dans `regex_continuation_query.rs` fait Walk 1 + gap +
Walk 2 en un seul bloc. Extraire le Walk 1 (initial DFA × SFX) dans une
fonction séparée qui retourne les matches partiels avec DFA end states.

Ça existe déjà : `SfxTermDictionary::search_continuation()` dans
`src/suffix_fst/term_dictionary.rs`.

#### Step 2 : Nouvelle fonction `regex_cross_token_search`

```rust
pub fn regex_cross_token_search(
    sfx_reader: &SfxFileReader,
    sfx_dict: &SfxTermDictionary,
    automaton: &impl Automaton,
    resolver: &dyn PostingResolver,
    ord_to_term: &dyn Fn(u64) -> Option<String>,
    max_doc: DocId,
) -> (BitSet, Vec<(DocId, usize, usize)>)
```

#### Step 3 : Walk 1 — utiliser search_continuation existant

```rust
let start = automaton.start();
let matches = sfx_dict.search_continuation(automaton, start, false); // any SI
```

#### Step 4 : Walk 2+ — sibling chain avec DFA feed

```rust
for m in &matches {
    if m.is_accepting {
        // Direct match — emit
        continue;
    }

    // DFA alive but not accepting → follow sibling chain
    let siblings = sibling_table.contiguous_siblings(m.raw_ordinal as u32);
    for &next_ord in &siblings {
        let next_text = ord_to_term(next_ord as u64)?;

        // Feed each byte of next_text to the DFA
        let mut state = m.end_state.clone();
        for &byte in next_text.as_bytes() {
            state = automaton.accept(&state, byte);
            if !automaton.can_match(&state) { break; }
        }

        if automaton.is_match(&state) {
            // Match found! Resolve postings...
        } else if automaton.can_match(&state) {
            // Still alive → continue with next sibling of next_ord
            // (recursive or iterative)
        }
    }
}
```

#### Step 5 : Resolve + adjacency

Identique au cross_token_search exact : Vec trié + partition_point,
byte continuity check, dedup.

#### Step 6 : Brancher dans le pipeline

Dans `build_contains_query` (lucivy_core/src/query.rs), quand
`regex: true` + sibling table disponible, utiliser la nouvelle
fonction au lieu de `RegexContinuationQuery`.

### Points d'attention

1. **Le Walk 1 reste O(FST_size)** — c'est inévitable pour le regex initial.
   Mais c'est le même coût que l'approche existante. L'optimisation est
   sur le Walk 2+ qui est O(1) + O(token_len) au lieu de O(FST_size).

2. **Le DFA state doit être clonable** — pour tester chaque sibling
   indépendamment. `search_continuation` retourne déjà le `end_state`.

3. **Max chain depth** — limiter à 8 comme pour le cross-token exact.

4. **Performance attendue** : Walk 1 ~identique à avant. Walk 2+ quasi-gratuit
   (sibling O(1) + DFA feed O(token_len)). Total devrait être ~2× plus rapide
   que RegexContinuationQuery car Walk 2 est éliminé.

5. **Le regex cross-token ne gère que gap=0** (tokens contigus). Pour les
   regex qui traversent des espaces, garder RegexContinuationQuery.

### Fichiers à modifier

| Fichier | Changement |
|---------|-----------|
| `src/query/phrase_query/suffix_contains.rs` | Nouvelle fonction `regex_cross_token_search` |
| `src/suffix_fst/term_dictionary.rs` | Peut-être exposer `search_continuation` mieux |
| `lucivy_core/src/query.rs` | `build_contains_regex` utilise la nouvelle fonction |
| `src/query/phrase_query/suffix_contains_query.rs` | Passer sibling table + ord_to_term |

### Tests

- Réutiliser les tests existants de `regex_continuation_query.rs` (lignes 455+)
- Ajouter des tests cross-token regex spécifiques (regex qui traverse CamelCaseSplit)
- Benchmark sur .luce : comparer ancien vs nouveau
