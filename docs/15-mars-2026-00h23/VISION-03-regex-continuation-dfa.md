# VISION-03 — Regex Continuation DFA à travers GapMap

Date : 15 mars 2026

## Problème

Actuellement les regex/fuzzy queries matchent des tokens individuels. Un regex
`rag3db.*cool` ne peut pas matcher le texte "rag3db is cool" car "rag3db",
"is" et "cool" sont des tokens séparés. Le regex ne voit qu'un token à la fois.

Pour du contains multi-token, on a SuffixContainsQuery qui résout chaque token
littéral indépendamment puis valide l'adjacence via GapMap. Mais ça ne marche
que pour des littéraux, pas pour des patterns arbitraires.

## Approche — Walk DFA chaîné avec transfert d'état

L'idée : au lieu d'un seul walk DFA sur le suffix FST, on fait une série de
walks chaînés. À chaque frontière de token, on transfère l'état du DFA à
travers le gap (séparateur), puis on relance un walk FST depuis ce nouvel état.

### Algorithme

```
WALK 1 — Point d'entrée
  1. Construire le DFA (Levenshtein, regex, etc.)
  2. Walk suffix FST avec le DFA depuis l'état initial
  3. Pour chaque suffixe matché :
     - Si le DFA accepte → match direct (single-token)
     - Si le DFA est vivant (can_match) mais pas acceptant → continuation
     - Collecter (token_ordinal, SI, dfa_end_state)
  4. Résoudre les posting lists → set de (doc_id, position) candidats

CONTINUATION (boucle)
  5. Pour chaque (doc_id, position) candidat :
     - Lire les bytes du gap via GapMap pour (doc, position)
     - Feed les bytes du gap au dfa_end_state → dfa_after_gap
     - Si dfa_after_gap accepte → doc matche (regex finit dans le gap)
     - Si dfa_after_gap est mort → éliminer ce candidat
     - Si dfa_after_gap est vivant → garder pour walk suivant
  6. Grouper les candidats survivants par dfa_after_gap (les gaps identiques
     comme " " donnent le même état → un seul walk pour tous ces docs)
  7. Pour chaque état DFA unique :
     - Walk suffix FST depuis cet état (SI=0 — tokens complets uniquement)
     - Résoudre posting lists des tokens trouvés
     - Intersect avec les doc_ids candidats du walk précédent (trim)
     - Si intersection vide → short-circuit, pas de walk suivant
  8. Collecter les nouveaux (doc_id, position+1, dfa_end_state)
  9. Répéter depuis l'étape 5 jusqu'à :
     - Plus de candidats vivants, ou
     - Tous les DFA sont en état acceptant, ou
     - Limite de profondeur atteinte
```

### Optimisations clés

**Trim progressif des candidats** : à chaque walk, le set de doc_ids
rétrécit. Walk 1 donne N docs, walk 2 intersecte avec les tokens suivants
→ M docs (M ≤ N), walk 3 → K docs (K ≤ M). Plus on avance, plus c'est
rapide.

**Groupement par état DFA** : les gaps identiques (typiquement " " espace)
produisent le même état DFA après gap. Au lieu de N walks pour N docs, on
fait P walks pour P états distincts (P << N en pratique).

**Short-circuit** : si le set candidat est vide après intersection → stop
immédiat. Pas de walks inutiles.

**Single-token fast path** : si le DFA accepte dès le walk 1 (le regex
matche entièrement dans un seul token), pas besoin de continuation. C'est
le cas courant pour les regex courts.

## 3 modes

### contains
Le walk 1 utilise n'importe quel SI (le regex peut commencer au milieu d'un
token). C'est le mode le plus général et le plus cher.

### startsWith
Le walk 1 utilise SI=0 uniquement (le regex doit commencer au début du
premier token). Moins de points d'entrée → plus rapide.

### strict
Comme startsWith mais le DFA doit aussi être en état acceptant à la fin du
dernier token (le regex doit couvrir le texte entier). Validation finale
qu'il n'y a pas de tokens après le dernier match.

## Structure de données

```rust
/// State for one continuation level in the DFA chain.
struct ContinuationLevel {
    /// (doc_id, position, dfa_state) — surviving candidates
    candidates: Vec<(DocId, u32, DfaState)>,
}

/// Result of walking the suffix FST from a given DFA state.
struct WalkResult {
    /// (token_ordinal, dfa_end_state) — tokens that keep the DFA alive
    alive: Vec<(u64, DfaState)>,
    /// (token_ordinal) — tokens where the DFA accepts
    accepted: Vec<u64>,
}
```

## Ce qui existe déjà

- `SfxFileReader::prefix_walk()` — walk le suffix FST par préfixe
- `SfxTermDictionary::search_automaton()` — walk avec un automate lucivy_fst
- `GapMapReader::doc_data()` — bytes bruts des gaps pour un doc
- `SuffixContainsQuery` — multi-token contains avec pivot, adjacence GapMap
- `SfxAutomatonAdapter` — bridge tantivy_fst::Automaton → lucivy_fst::Automaton
- `SfxDfaWrapper` — Levenshtein DFA pour lucivy_fst

## Ce qu'il faut créer

### 1. Walk FST depuis un état DFA arbitraire

`SfxTermDictionary::search_automaton()` part toujours de `automaton.start()`.
Il faut une variante qui part d'un état donné :

```rust
pub fn search_automaton_from_state<A: Automaton>(
    &self,
    automaton: &A,
    start_state: A::State,
    si_filter: SiFilter,  // Any | Zero
) -> Vec<(u64, A::State)>  // (ordinal, end_state)
```

Ceci nécessite un walk custom sur le FST (pas le `.search()` standard qui
part toujours de `start()`).

### 2. GapMap lecture séquentielle

Lire le gap entre position i et i+1 pour un document donné. L'API actuelle
`doc_data()` retourne les bytes bruts — il faut parser pour extraire le gap
à une position spécifique.

### 3. RegexContinuationQuery

Nouveau type de query qui orchestre la boucle walk → gap → walk → gap.
Implémente `Query` + `Weight` + `Scorer`.

## Complexité

- Walk 1 : O(FST_size × DFA_states) — comme un regex normal
- Chaque continuation : O(P × FST_size × DFA_states) où P = nombre d'états
  DFA distincts (petit en pratique, typiquement 1-5)
- Total : O(depth × P × FST_size × DFA_states) où depth = nombre de tokens
  traversés
- En pratique depth est petit (regex courts → 1-3 tokens) et P est petit
  (gaps uniformes)

## Limitations connues

- Un regex très long qui traverse 100 tokens ferait 100 walks FST — c'est
  cher. Mais c'est un cas pathologique rare.
- Le groupement par état DFA suppose que les états sont comparables/hashables.
  Les DFA de levenshtein_automata utilisent u32 comme état → hashable.
  Les regex DFA de tantivy_fst aussi.

## Fichiers à créer/modifier

```
src/query/regex_continuation_query.rs     ← NOUVEAU (query + weight + scorer)
src/suffix_fst/term_dictionary.rs         ← search_automaton_from_state()
src/suffix_fst/gapmap.rs                  ← gap_at_position(doc, pos)
lucivy_core/src/query.rs                  ← routing "regex_contains" etc.
```
