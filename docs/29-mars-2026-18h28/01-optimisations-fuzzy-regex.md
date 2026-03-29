# 01 — Optimisations envisageables fuzzy / regex

Date : 29 mars 2026

## Contexte perf actuel

Les queries contains exact sont rapides (~1-5ms sur 862 docs, ~700ms sur 90K).
Fuzzy d=1 et regex sont sensiblement plus lents car ils valident chaque
candidat avec un DFA (Levenshtein ou regex) feedé token par token.

Le pipeline actuel pour les deux :

```
trigrams/littéraux → SFX lookup → candidats → DFA validation → highlights
```

La phase "DFA validation" est le bottleneck. Pour fuzzy, c'est un sliding
window DFA sur un concat de ~8 tokens. Pour regex, c'est un feed séquentiel
entre deux positions connues via `validate_path()`.

## Analyse du bottleneck fuzzy

Le pipeline fuzzy a 3 phases avec des coûts très différents :

```
Phase 1: find_literal() × N trigrams     → O(N × SFX_walk)     ~60% du temps
Phase 2: intersect_trigrams              → O(candidates)        ~1% du temps
Phase 3: DFA validation × M candidats   → O(M × concat² )     ~39% du temps
```

### Phase 1 est le vrai bottleneck

`find_literal(trigram)` fait un suffix walk dans le SFX FST + résolution des
parents + sibling chain cross-token. Pour un trigram commun comme "the" sur
90K docs, c'est des milliers de matches. Et on le fait pour CHAQUE trigram
(8 trigrams pour une query de 10 chars).

**Optimisations possibles** :
- **Sélection intelligente des trigrams** : ne chercher que les K trigrams les
  plus sélectifs (ceux avec le moins de matches). On peut estimer la sélectivité
  via `sfxpost.doc_freq(ordinal)` — disponible en O(1).
- **Short-circuit** : si un trigram retourne 0 matches et qu'il est obligatoire
  (threshold exige qu'il soit dans la chain), on peut abandonner immédiatement.
- **Trigrams ordonnés par sélectivité** : chercher le plus rare d'abord, puis
  filtrer les candidats au fur et à mesure (au lieu de tout chercher puis
  intersecter).

### Phase 3 : le concat est trop large

Le concat prend `back_bytes + 2` positions en arrière et `forward_bytes/2 + 3`
en avant. Pour "rag3weaver" d=1, ça fait ~8 tokens de chaque côté = ~16 tokens
dans le concat. Le sliding window est O(concat_len × query_len).

**Optimisations possibles** :
- **Ancrer le sliding window** : on sait exactement où le premier trigram a
  matché (fp). Le match DFA doit couvrir cette position. Au lieu de slider
  sur tout le concat, ne tester que les positions ±distance autour de
  `query_positions[first_tri_idx]`.
- **Feed token-par-token au lieu de byte-par-byte** : le DFA Levenshtein a la
  propriété que si `can_match` retourne false après un token, tous les bytes
  suivants dans le même token sont aussi dead. On peut break early par token.
- **Skip si le candidat est un duplicate** : si deux candidats pointent au
  même (doc_id, token_range), ne valider qu'une fois.

### Idée plus radicale : validation sans concat

Au lieu de construire un concat et slider un DFA :
1. On connait les tokens dans la zone (via PosMap + TermTexts)
2. On connait la query
3. On peut calculer l'edit distance directement sur la séquence de tokens
   concaténés, SANS construire le concat en mémoire

Le DFA Levenshtein feed byte par byte. Mais on pourrait feeder token par token :
pour chaque token, feeder ses bytes au DFA, puis les gap bytes, puis le token
suivant. Si le DFA meurt au milieu d'un token, on sait que ce starting position
est mort et on peut skip au token suivant comme point de départ.

Ça transforme le sliding window de O(concat_len × max_feed) en
O(n_tokens × avg_token_len) — beaucoup plus rapide car on ne reteste pas
les mêmes bytes depuis des positions de départ différentes.

## Optimisations par ordre d'impact estimé

### 1. ByteMap pré-filtre DFA (regex + fuzzy)

**Principe** : avant de feeder un token au DFA, vérifier que ses bytes sont
compatibles avec le pattern. Si le DFA ne peut accepter aucun byte du token,
skip le token entièrement.

**Données disponibles** : `.bytemap` est déjà construit et stocké (256 bits
par ordinal). Jamais lu par les queries actuellement.

**Implémentation** : module indépendant `dfa_byte_filter` qui prend un DFA +
ByteMap et pré-filtre les tokens avant le feed.

```rust
/// Check if a token's bytes are compatible with the DFA from current state.
/// Returns false if the DFA would die on every byte of this token.
fn can_token_advance_dfa<A: Automaton>(
    automaton: &A,
    state: &A::State,
    bytemap: &ByteMapReader,
    ordinal: u32,
) -> bool {
    // Get the 256-bit bitmap of bytes present in this token
    let bitmap = bytemap.bitmap(ordinal);
    // Check if ANY byte in the token can advance the DFA
    for byte in 0..=255u8 {
        if bitmap.contains(byte) {
            let next = automaton.accept(state, byte);
            if automaton.can_match(&next) {
                return true;
            }
        }
    }
    false
}
```

**Où brancher** :
- **Regex** : dans `validate_path()` (`literal_resolve.rs` ligne 260), avant
  le `for &byte in text.as_bytes()` loop. Si `!can_token_advance_dfa()`, skip
  ce token et couper le path.
- **Fuzzy** : dans la construction du concat (`regex_continuation_query.rs`
  ligne 655), on pourrait exclure les tokens dont aucun byte n'est compatible
  avec le DFA Levenshtein. Mais le DFA Levenshtein est très permissif (accepte
  presque tous les bytes à d≤1), donc le gain serait faible.

**Gain estimé** :
- Regex restrictif (`[a-z]+`, `\d{4}`) : **30-50%** — beaucoup de tokens éliminés
- Regex permissif (`.*foo.*`) : **~0%** — le `.*` accepte tout
- Fuzzy d=1 : **<5%** — le DFA Levenshtein accepte presque tous les bytes

**Complexité** : faible. Le bytemap est déjà construit et chargé. Juste un
check avant le feed.

### 2. Batch token skip dans validate_path (regex)

**Principe** : `validate_path()` feed le DFA byte par byte pour chaque token
entre deux positions. Si le DFA est dans un état "mange tout" (`.*`), on peut
skip les tokens intermédiaires sans les feeder.

**Implémentation** : détecter quand `automaton.is_match(state) &&
automaton.can_match(state)` (le DFA accepte déjà et peut continuer). C'est
le cas pour `.*` — on peut sauter directement au prochain littéral.

La fonction `dfa_accepts_anything()` existe déjà dans `literal_resolve.rs` :
```rust
pub fn dfa_accepts_anything<A: Automaton>(automaton: &A, state: &A::State) -> bool {
    automaton.is_match(state) && automaton.can_match(state)
}
```

**Gain estimé** : **20-40%** pour les patterns avec `.*` entre littéraux
(très commun : `foo.*bar`). Le path skip les tokens entre foo et bar.

### 3. Threshold adaptatif queries courtes (fuzzy)

**Principe** : Bug E de doc 12. `threshold = max(2, computed)`. Pour queries
≤ 4 chars avec d=1, les 2 bigrams doivent matcher mais 1 peut être cassé
par l'edit → 0 résultats.

**Fix** : `threshold = max(1, computed)` pour queries où
`ngrams.len() <= n * distance + 1`.

**Gain** : fonctionnel (rappel), pas perf. Mais important.

### 4. Early termination dans le DFA sliding window (fuzzy)

**Principe** : le sliding window essaie TOUTES les positions dans le concat
même après avoir trouvé un match à distance 0. Si on trouve d=0, on peut
arrêter immédiatement (impossible de faire mieux).

**Implémentation** : `if global_best_diff == 0 { break; }` après la mise à
jour du best.

**Gain estimé** : **5-15%** pour le fuzzy — évite de scanner la fin du concat
quand le match est trouvé tôt.

### 5. PosMap range check avant concat (fuzzy)

**Principe** : avant de construire le concat de ~8 tokens autour du candidat,
vérifier via PosMap que la plage de tokens couvre assez de bytes pour contenir
le match. Si le token range est trop court (ex: 3 tokens de 1 char pour un
query de 10 chars), skip sans construire le concat.

**Implémentation** :
```rust
let total_token_bytes: u32 = (start_pos..end_pos)
    .filter_map(|pos| pm.ordinal_at(doc_id, pos))
    .filter_map(|ord| ord_to_term(ord as u64))
    .map(|t| t.len() as u32)
    .sum();
if total_token_bytes < query_text.len() as u32 - distance as u32 {
    continue; // impossible match
}
```

**Gain estimé** : **5-10%** — élimine les candidats impossibles avant le
concat + DFA (qui est l'opération la plus chère).

### 6. Cache DFA states (fuzzy + regex)

**Principe** : le DFA est reconstruit pour chaque segment. Pour une même query
sur N segments, le DFA est identique. Le mettre en cache dans le Weight.

**Implémentation** : le `RegexContinuationWeight` stocke déjà le `dfa_kind`.
Il suffit de construire le DFA une seule fois dans `weight()` au lieu de dans
`scorer()`.

**Gain estimé** : **négligeable** — la construction du DFA est ~0.1ms, la
validation est ~10-100ms.

### 7. Paralleliser les candidats fuzzy par segment (fuzzy)

**Principe** : le DFA validation est par candidat, et les candidats sont
indépendants. On pourrait valider N candidats en parallèle via rayon.

**Problème** : le PosMap et SfxReader ne sont pas Send (lifetime refs).
Il faudrait cloner les données par thread.

**Gain estimé** : **linéaire avec le nombre de cores** mais complexité élevée.

### 8. Réduire la fenêtre du concat (fuzzy)

**Principe** : actuellement le concat prend `back_bytes + 2` positions en
arrière et `forward_bytes / 2 + 3` en avant. C'est conservateur. On pourrait
réduire à `query_len / average_token_len + 1` positions de chaque côté.

**Gain estimé** : **5-10%** — moins de tokens dans le concat = sliding window
plus rapide.

### 9. Trigrams ordonnés par sélectivité (fuzzy — Phase 1)

**Principe** : au lieu de chercher les 8 trigrams puis intersecter, estimer la
sélectivité de chaque trigram via un prefix check dans le SFX FST (combien
d'entrées matchent ce préfixe ?). Chercher le plus rare d'abord, obtenir la
liste de docs candidats, puis ne chercher les autres trigrams QUE dans ces docs.

**Implémentation** : utiliser `sfxpost.doc_freq(ordinal)` pour estimer le coût,
ou `sfx_reader.fst().get(trigram)` pour vérifier l'existence avant le full
resolve.

**Gain estimé** : **40-60%** sur Phase 1 — les trigrams rares éliminent 99%
des docs dès le premier lookup. On évite de résoudre les trigrams communs
pour les docs déjà éliminés.

### 10. Anchored sliding window (fuzzy — Phase 3)

**Principe** : on sait que le match DFA doit couvrir la position du premier
trigram (fp). Au lieu de slider sur tout le concat (positions 0..concat_len),
ne tester que les positions `[trigram_offset - distance, trigram_offset + distance]`.

Pour "rak3weaver" d=1, le premier trigram "3we" est à offset 3 dans la query.
Le match doit commencer entre `fp_concat_offset - 3 - 1` et
`fp_concat_offset - 3 + 1` dans le concat. Ça réduit le sliding window de
~50 positions à ~3 positions.

**Gain estimé** : **80-90%** sur Phase 3 — de O(concat_len × max_feed) à
O(2×distance × max_feed). C'est le plus gros gain possible.

### 11. Dedup candidats par (doc, token_range) (fuzzy)

**Principe** : deux trigrams différents dans le même doc peuvent produire des
candidats qui couvrent la même plage de tokens. Le DFA validation est faite
deux fois pour le même texte.

**Implémentation** : avant la validation, dedup les candidats par
(doc_id, start_pos, end_pos).

**Gain estimé** : **10-20%** — dépend du nombre de duplicates (plus fréquent
avec des queries longues qui ont beaucoup de trigrams).

## Priorisation

| # | Optimisation | Impact | Effort | Cible | Priorité |
|---|---|---|---|---|---|
| 10 | Anchored sliding window | **très fort** | moyen | fuzzy | **P0** |
| 9 | Trigrams par sélectivité | **fort** | moyen | fuzzy | **P0** |
| 1 | ByteMap pré-filtre | fort (regex) | faible | regex | **P1** |
| 2 | Batch skip `.*` | moyen-fort | faible | regex | **P1** |
| 4 | Early termination d=0 | faible-moyen | trivial | fuzzy | **P1** |
| 11 | Dedup candidats | moyen | faible | fuzzy | **P1** |
| 3 | Threshold adaptatif | fonctionnel | trivial | fuzzy | **P2** |
| 5 | PosMap range check | faible-moyen | faible | fuzzy | **P2** |
| 8 | Réduire fenêtre concat | faible | faible | fuzzy | **P3** |
| 6 | Cache DFA | négligeable | faible | les deux | **P4** |
| 7 | Paralléliser candidats | fort | élevé | fuzzy | **P4** |

**P0** : les deux plus gros gains, spécifiques au fuzzy. L'anchored sliding
window (#10) seul pourrait réduire le temps fuzzy de 80%. Combiné avec les
trigrams sélectifs (#9), le fuzzy serait comparable en vitesse au contains exact.

**P1** : quick wins. ByteMap (#1) et batch skip (#2) pour le regex. Early
termination (#4) et dedup (#11) pour le fuzzy.

**P2-P4** : améliorations mineures ou complexes.
