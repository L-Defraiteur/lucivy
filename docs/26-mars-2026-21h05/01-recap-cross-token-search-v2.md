# Doc 01 — Recap session : cross-token search + sfxpost V2

Date : 26 mars 2026
Branche : `feature/cross-token-search`

## Ce qui a été implémenté

### sfxpost V2 (format binaire)

Nouveau format "SFP2" avec doc_ids binary-searchable :
- `SfxPostWriterV2` : groupement par doc_id, header trié + payload VInt
- `SfxPostReaderV2` : owned (`Vec<u8>`), Send+Sync, lazy decode
- `entries_filtered(ordinal, doc_ids)` : O(log n) binary search, skip les docs non-matchants
- `entries_for_doc(ordinal, doc_id)` : O(log n) single doc lookup
- `has_doc(ordinal, doc_id)` : O(log n) existence check, zéro decode
- `doc_freq(ordinal)` : O(1) depuis le header

Migration complète : collector, merger, sfx_merge, validator, posting_resolver.
Plus de V1.

### token_len dans ParentEntry

```rust
pub struct ParentEntry {
    pub raw_ordinal: u64,
    pub si: u16,
    pub token_len: u16,  // NOUVEAU
}
```

Encodé inline dans le u64 FST output (bits 40-55). Aussi dans le OutputTable (+2 bytes/entry).
Permet le **filtrage géométrique** : `si + prefix_len == token_len` → le match atteint la fin du token.

### falling_walk

```rust
pub fn falling_walk(&self, query: &str) -> Vec<SplitCandidate>
```

Walk byte-by-byte dans les deux partitions SFX (\x00 SI=0, \x01 SI>0).
À chaque noeud final, check `si + prefix_len == token_len`.
Coût : O(2L) node lookups. Retourne tous les split candidates triés par prefix_len desc.

### cross_token_search

Fallback automatique dans `suffix_contains_single_token` quand le single-token SFX retourne 0.

Algorithme :
1. falling_walk → split candidates
2. Remainder walks (prefix_walk_si0 ou fuzzy_walk_si0 si distance > 0)
3. Dynamic pivot : compare left_parents vs right_parents count, résout le plus petit d'abord
4. Filtered resolve : l'autre côté filtré par pivot doc_ids
5. HashMap adjacency : index right par (doc_id, token_index), lookup O(1) par left entry
6. Dedup par (doc_id, byte_from)

### tokenize_query simplifié

`SimpleTokenizer + LowerCaser` uniquement. Plus de CamelCaseSplit dans la query.
Le SFX + cross_token_search gère tout : substrings intra-token ET cross-token.

### Fuzzy cross-token

Quand `fuzzy_distance > 0`, le remainder matching utilise `fuzzy_walk_si0` au lieu de
`prefix_walk_si0`. Permet "rag3weavr" (typo dans la partie droite) → match.

## Performances mesurées (playground, 5k docs code source)

| Query | Type | Temps |
|-------|------|-------|
| "weaver" | single-token exact | ~5ms |
| "rag3weaver" | cross-token exact | ~19ms |
| "rag3w" | cross-token (remainder court "w") | ~19ms (après HashMap fix) |
| "weavr" d=1 | single-token fuzzy | ~10ms |
| "rag3weavr" d=1 | cross-token fuzzy (right) | ~20ms |

## Limitation actuelle : fuzzy sur la partie LEFT

Le falling_walk est **exact** byte-by-byte. Si le typo est dans la partie gauche
du split (avant la frontière de token), le walk FST ne peut pas suivre le chemin
→ 0 candidates → 0 résultats.

Exemples qui ne marchent PAS :
- "rak3weaver" (k au lieu de g) → falling walk échoue sur "rak"
- "rag4weaver" (4 au lieu de 3) → falling walk échoue sur "rag4"

## Solution proposée : fuzzy falling walk via Levenshtein DFA

### Principe

Remplacer le walk byte-by-byte exact par un walk avec un automate Levenshtein.
L'automate a des états qui trackent la distance d'édition. À chaque byte :
- L'automate DFA transite (peut rester en état acceptant malgré un mismatch)
- Le noeud FST avance
- Si l'automate est en état acceptant ET le noeud FST est final → split candidate

### API FST disponible

lucivy-fst a déjà tout :
- `levenshtein.rs` : `Levenshtein` struct qui implémente le trait `Automaton`
- `Automaton` trait : `start()`, `is_match(&state)`, `accept(&state, byte)`, `can_match(&state)`
- Le fuzzy_walk existant utilise déjà cet automate pour le range scan

### Implémentation

```rust
pub fn fuzzy_falling_walk(&self, query: &str, distance: u8) -> Vec<SplitCandidate> {
    let lev = Levenshtein::new(query, distance).unwrap();

    for &partition in &[SI0_PREFIX, SI_REST_PREFIX] {
        let fst = self.fst.as_fst();
        let root = fst.root();

        // Follow partition byte
        let Some(idx) = root.find_input(partition) else { continue };
        let trans = root.transition(idx);
        let mut stack = vec![(fst.node(trans.addr), trans.out, lev.start(), 0usize)];

        // DFS through FST guided by Levenshtein DFA
        while let Some((node, output, lev_state, depth)) = stack.pop() {
            // Check: is this a final state AND is the DFA accepting?
            if node.is_final() && lev.is_match(&lev_state) {
                let val = output.cat(node.final_output()).value();
                let parents = self.decode_parents(val);
                for parent in parents {
                    if parent.si as usize + depth == parent.token_len as usize {
                        candidates.push(SplitCandidate { prefix_len: depth, parent });
                    }
                }
            }

            // Can the DFA still match? (pruning)
            if !lev.can_match(&lev_state) { continue; }

            // Explore all transitions
            for t in node.transitions() {
                let next_state = lev.accept(&lev_state, t.inp);
                stack.push((
                    fst.node(t.addr),
                    output.cat(t.out),
                    next_state,
                    depth + 1,
                ));
            }
        }
    }
}
```

### Coût

- O(FST_nodes_explored) — guidé par le DFA pruning (`can_match`)
- Pour distance=1 : le DFA a ~2L+1 états, explore au plus ~4× les noeuds du walk exact
- Pour distance=0 : identique au falling_walk exact (le DFA est un match exact)
- **Unifie exact et fuzzy** en un seul algorithme

### Points d'attention

1. Le `Levenshtein` de lucivy-fst construit le DFA par rapport à la query ENTIÈRE.
   Mais on veut matcher des PRÉFIXES (le walk ne va pas jusqu'au bout de la query).
   → Il faut un DFA qui accepte à chaque état intermédiaire compatible, pas seulement
   à la fin. Le `is_match` standard ne fait ça que pour l'état final.
   → **Vérifier** si `Levenshtein::is_match` accepte les préfixes ou seulement le match complet.

2. Le `can_match` est le pruning clé — il coupe les branches du FST qui ne peuvent
   plus matcher. Sans ça, l'exploration serait exponentielle.

3. La profondeur `depth` doit correspondre au nombre de bytes consommés dans la query,
   pas au nombre de transitions FST (qui est la même chose pour exact, mais pas pour fuzzy
   à cause des insertions/suppressions).

### Alternatives si le DFA Levenshtein ne supporte pas les préfixes

Option A : construire un DFA Levenshtein pour chaque préfixe possible de la query.
Coûteux (L constructions de DFA) mais correct.

Option B : utiliser le DFA Levenshtein standard mais checker `is_match` sur des
sous-states correspondant à chaque préfixe. Nécessite d'inspecter l'état interne du DFA
pour savoir "est-ce que les i premiers bytes de la query sont matchés avec distance ≤ d?".

Option C : implémenter un Levenshtein prefix DFA custom qui accepte à chaque position.
Plus de travail mais optimal.

## Fichiers modifiés (branche feature/cross-token-search)

| Fichier | Changement |
|---------|-----------|
| `suffix_fst/builder.rs` | token_len dans ParentEntry + encode/decode |
| `suffix_fst/file.rs` | falling_walk + SplitCandidate |
| `suffix_fst/sfxpost_v2.rs` | Nouveau format writer + reader |
| `suffix_fst/collector.rs` | Écrit V2 |
| `suffix_fst/mod.rs` | Module sfxpost_v2 |
| `indexer/merger.rs` | Lit/écrit V2 |
| `indexer/sfx_merge.rs` | Lit/écrit V2 + validate V2 |
| `query/posting_resolver.rs` | SfxPostResolverV2 lazy |
| `query/phrase_query/suffix_contains.rs` | cross_token_search + pivot + HashMap adjacency |
| `query/phrase_query/suffix_contains_query.rs` | tokenize_query simplifié |
| `suffix_fst/stress_tests.rs` | Tests mis à jour (cross-token matchs) |
| `lucivy_core/handle.rs` | Tests E2E (flexible positions, fuzzy, all query types) |
