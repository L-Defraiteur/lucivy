# 02 — Arsenal sous-utilisé dans fuzzy / regex

Date : 29 mars 2026

## Constat

On a 7 structures d'index (SFX, SfxPost, PosMap, ByteMap, TermTexts, GapMap,
SiblingTable) mais le fuzzy et le regex n'en utilisent que 4-5. Les autres
sont construites, stockées, mergées, mais jamais lues au query time.

## Structures utilisées vs non utilisées

### Fuzzy contains (trigram pigeonhole + DFA)

| Structure | Utilisée | Comment | Potentiel non exploité |
|---|---|---|---|
| SFX FST | oui | find_literal pour chaque trigram | — |
| SfxPost | oui | resolve postings des trigrams | `doc_freq()` O(1) pour sélectivité |
| PosMap | oui | ordinals autour du candidat | — |
| TermTexts | oui | texte des tokens pour concat | — |
| GapMap | partiel | seulement si query contient espace | séparateurs utiles pour highlight |
| **ByteMap** | **NON** | jamais lu | pré-filtre DFA par token |
| **SiblingTable** | **NON** | jamais lu en fuzzy | filtrer candidats impossibles |

### Regex contains (littéraux + DFA)

| Structure | Utilisée | Comment | Potentiel non exploité |
|---|---|---|---|
| SFX FST | oui | find_literal pour chaque littéral | — |
| SfxPost | oui | resolve postings | `doc_freq()` pour sélectivité |
| PosMap | oui | validate_path entre positions | — |
| TermTexts | oui | texte des tokens pour DFA feed | — |
| GapMap | oui | gaps entre tokens pour DFA feed | — |
| **ByteMap** | **NON** | jamais lu | pré-filtre DFA par token |
| **SiblingTable** | **NON** | jamais lu en regex | vérifier adjacence tokens |

## Optimisations concrètes

### A. ByteMap pré-filtre DFA (fuzzy + regex)

**Où** : avant de feeder un token au DFA, vérifier que ses bytes sont
compatibles.

**Fuzzy** : dans la construction du concat, on pourrait détecter les tokens
dont AUCUN byte ne peut avancer le DFA depuis l'état courant. Si le DFA
Levenshtein pour "rak3weaver" est dans un état qui attend "r" ou "a" ou "k",
et le token n'a aucun de ces bytes → skip. Mais le DFA Levenshtein est
permissif (d=1 accepte des substitutions), donc le gain serait limité.

**Regex** : dans `validate_path()`, avant le `for &byte in text.as_bytes()`,
checker `can_token_advance_dfa()`. Pour un pattern comme `[a-z]+`, ça
éliminerait instantanément les tokens avec des chiffres ou ponctuation.
Gain fort pour les patterns restrictifs.

**Implémentation** :
```rust
fn can_token_advance_dfa<A: Automaton>(
    automaton: &A, state: &A::State,
    bytemap: &ByteMapReader, ordinal: u32,
) -> bool {
    let bitmap = bytemap.bitmap(ordinal);
    for byte in 0..=255u8 {
        if bitmap.contains(byte) {
            let next = automaton.accept(state, byte);
            if automaton.can_match(&next) { return true; }
        }
    }
    false
}
```

Problème : boucle de 256 itérations. Optimisable via popcount sur les 32
bytes du bitmap pour ne tester que les bytes présents (typiquement <30 bytes
par token).

### B. SfxPost doc_freq() pour sélectivité trigrams (fuzzy)

**Où** : avant `find_literal()` pour chaque trigram (Phase 1).

**Principe** : les trigrams communs ("the", "ing", "tion") matchent des
milliers de docs. Les trigrams rares ("k3w", "rak") matchent quelques docs.
Au lieu de résoudre les 8 trigrams puis intersecter, on peut :

1. Estimer la doc_freq de chaque trigram via un quick FST lookup
2. Trier par sélectivité (plus rare = plus sélectif)
3. Résoudre le plus rare d'abord → obtenir le set de docs candidats
4. Pour les trigrams suivants, ne résoudre que dans les docs candidats
   (via `resolve_filtered(ordinal, &doc_ids)`)

**Gain estimé** : 40-60% sur Phase 1. Le trigram le plus rare élimine
99% des docs, les suivants ne font que confirmer sur le petit set restant.

**Difficulté** : la doc_freq d'un trigram dans le SFX n'est pas directe — un
trigram peut correspondre à plusieurs ordinals (suffixes de différents tokens).
Il faudrait sommer les doc_freq de tous les ordinals matchant ce trigram.
Approximation possible : juste compter le nombre d'ordinals dans le
`prefix_walk(trigram)`.

### C. SiblingTable filtre de candidats (fuzzy)

**Où** : après l'intersection trigram, avant la validation DFA.

**Principe** : le sibling table stocke quels tokens sont observés comme
adjacents pendant l'indexation. Si le match candidat implique les tokens
"rag3" → "weaver" mais que le sibling table ne contient PAS "rag3" → "weaver"
comme paire adjacente, le candidat est impossible et peut être éliminé sans DFA.

**Implémentation** : pour chaque candidat, lire les token positions via PosMap,
et vérifier que chaque paire consécutive (pos, pos+1) est dans le sibling table.

```rust
for i in 0..token_count-1 {
    let ord_a = pm.ordinal_at(doc_id, pos + i);
    let ord_b = pm.ordinal_at(doc_id, pos + i + 1);
    if !sibling_table.has_sibling(ord_a, ord_b) {
        // tokens never observed adjacent → impossible match
        skip = true;
        break;
    }
}
```

**Gain estimé** : 10-30% — élimine les candidats dont les tokens ne sont
jamais adjacents. Plus efficace sur les gros index où beaucoup de tokens
existent mais ne sont pas tous adjacents.

**Attention** : le sibling table ne stocke que les paires observées à gap=0
(contiguës en bytes). Les paires avec séparateur (gap>0) ne sont pas dans
`contiguous_siblings()` mais dans `siblings()`. Il faut utiliser `siblings()`
qui inclut les deux.

### D. Éliminer le concat — feeder le DFA directement depuis TermTexts

**Où** : Step 1 + Step 2 du fuzzy path.

**Principe** : on construit un `Vec<u8>` concat en copiant les bytes des
tokens, puis on slide le DFA dessus. On pourrait feeder le DFA byte par byte
directement depuis les token texts (via TermTexts) sans construire le concat.

**Avantage** : élimine l'allocation + copie du concat Vec. Pour 8 tokens de
~6 bytes chacun, c'est ~48 bytes copiés — pas énorme, mais l'allocation
dynamique a un coût fixe.

**Difficulté** : le sliding window doit pouvoir démarrer au milieu d'un token
(quand l'anchor est intra-token). Avec l'anchored window (~3 positions), la
plupart démarrent au début ou à 1-2 bytes dans un token. Il faut gérer le
offset intra-token pour le point de départ.

**Alternative** : pré-allouer un buffer fixe (stack, pas heap) pour le concat.
`[u8; 256]` couvre tous les cas raisonnables. Élimine l'allocation dynamique
sans changer la logique.

### E. Résumé : ce que l'arsenal pourrait apporter

| Optim | Structure | Phase | Gain estimé | Effort |
|---|---|---|---|---|
| ByteMap pré-filtre | ByteMap | DFA validation | 30-50% regex, <5% fuzzy | faible |
| doc_freq sélectivité | SfxPost | trigram lookup | 40-60% fuzzy | moyen |
| Sibling filtre | SiblingTable | pré-DFA | 10-30% fuzzy | faible |
| Éliminer concat | TermTexts | concat build | 5-10% fuzzy | moyen |
| **Combiné** | | | **50-80% fuzzy, 30-50% regex** | |

## Principe directeur

Si on construit une structure pendant l'indexation et qu'on la stocke dans
chaque segment, elle DOIT être utilisée au query time. Sinon c'est du poids
mort (taille index + temps de build + temps de merge) sans bénéfice.

ByteMap et SiblingTable sont aujourd'hui du poids mort pour fuzzy/regex.
