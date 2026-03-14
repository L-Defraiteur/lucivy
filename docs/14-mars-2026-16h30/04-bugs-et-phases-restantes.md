# Bugs et phases restantes — Suffix FST

Date : 14 mars 2026 — 16h30

Pas de concessions. Chaque point est un bug ou une feature manquante à résoudre.

## BUG-1 : SI byte offset incorrect pour Unicode exotique

### Problème

SI est stocké une fois par suffix term dans le FST (partagé entre toutes les
occurrences). Mais le même token lowercase peut provenir d'originaux avec des
byte layouts différents :

```
Doc 1: "İMPORT" → lowercase "import", İ=2 bytes → 'M' à byte 2
Doc 2: "IMPORT" → lowercase "import", I=1 byte  → 'M' à byte 1

SI=1 dans le FST → byte_from = posting.byte_from + 1
Doc 1: byte_from = 0 + 1 = 1 → MILIEU de İ → BUG
Doc 2: byte_from = 0 + 1 = 1 → 'M' → correct
```

Un seul SI ne peut pas être correct pour les deux. C'est une contrainte
architecturale de la redirection (1 SI par suffix term, N occurrences).

### Caractères affectés

Seuls les caractères dont lowercase change la taille UTF-8 :
- İ (U+0130, 2 bytes) → i (U+0069, 1 byte) — turc
- Quelques caractères Unicode obscurs
- Jamais du ASCII (a-z/A-Z = toujours 1 byte)

### Piste de résolution

**Option A — Per-occurrence SI dans la posting list ._raw**

Ajouter un champ SI aux posting entries du ._raw. Chaque occurrence stocke
son propre SI (byte offset dans son texte original spécifique). Le .sfx FST
ne stocke plus SI — il stocke juste le raw_ordinal + un "char offset" pour
identifier quel suffix c'est.

À la recherche : lookup .sfx → char_offset → lookup ._raw posting avec
le SI per-occurrence → byte_from correct.

Impact : modifie le format du ._raw posting list. Lourd.

**Option B — Stocker byte_from directement dans le .sfx**

Au lieu de stocker (raw_ordinal, SI) dans le FST output, stocker un offset
vers une table per-occurrence (doc_id, byte_from) dans le .sfx. Ça revient
à avoir des posting lists dans le .sfx — on perd l'avantage de la redirection.

Impact : annule le gain de taille de la redirection.

**Option C — Double SI (lowercase + delta max)**

Stocker SI_lowercase dans le FST. À la recherche, si le caractère au byte
SI_lowercase dans le posting est un milieu de char UTF-8 (byte & 0xC0 == 0x80),
scanner vers l'avant pour trouver le vrai début de char.

```rust
fn adjust_si(original_bytes: &[u8], posting_byte_from: usize, si_lowercase: usize) -> usize {
    let mut pos = posting_byte_from + si_lowercase;
    // Reculer si on est au milieu d'un char UTF-8
    while pos > posting_byte_from && original_bytes[pos] & 0xC0 == 0x80 {
        pos -= 1;
    }
    pos - posting_byte_from
}
```

Problème : on n'a pas `original_bytes` à la recherche (pas de stored text).

**Option D — Tokenizer qui normalise les byte widths**

Si le tokenizer ._raw normalise les caractères problématiques (İ → I avant
lowercase), les byte widths sont préservées. Le token "İMPORT" serait normalisé
en "IMPORT" puis lowercased en "import". SI serait correct.

Impact : modifie le tokenizer ._raw. Pourrait affecter la précision de
la recherche (İ et I deviendraient indistinguables). Mais c'est déjà le cas
avec le lowercase standard.

**Option E — Accepter le bug pour les caractères exotiques**

Documenter que les highlights peuvent être décalés de 1-2 bytes pour les rares
caractères dont lowercase change la taille UTF-8. Le match est trouvé (pas de
faux négatif), seul le highlight est imprécis.

Impact : zéro. Mais c'est un bug, pas une concession.

### Recommandation

Option D est la plus propre. Le tokenizer ._raw applique déjà un lowercase —
ajouter une normalisation Unicode (NFC ou NFKC) avant le lowercase résoudrait
le problème à la source. Les caractères comme İ seraient décomposés en I + ◌̇
(combining dot above), le I lowercase en i, et les byte widths seraient
préservées.

À implémenter en Phase 7 (post-MVP).

## BUG-2 : byte_to incorrect pour Unicode exotique

### Problème

`byte_to = byte_from + query_lowercase.len()`. Si le query contient des
caractères dont le lowercase change la taille, byte_to sera décalé.

Même cause que BUG-1. Même fix (normalisation Unicode dans le tokenizer).

## BUG-3 : Position phantom Ti dans multi-value

### Problème

Pour multi-value avec POSITION_GAP=1, Ti=2 est un "fantôme" (aucun posting).
`read_separator(doc, 1, 2)` retourne le suffix de value 0 au lieu de
VALUE_BOUNDARY.

### Résolution

Ajouter un check dans `read_separator` : si Ti tombe dans un POSITION_GAP
(entre deux values), retourner None. Nécessite de connaître les value
boundaries, déjà stockées dans la GapMap (value_offsets table).

Phase 7.

## PHASE-4 : Multi-token search

### À implémenter

Le placeholder `suffix_contains_multi_token` retourne `Vec::new()`.

Logique :
1. Premier token : .sfx exact, tout SI, vérifier fin de token
2. Milieu : ._raw exact, SI=0
3. Dernier : .sfx prefix walk, SI=0
4. Intersection curseurs triés (Ti consécutifs)
5. GapMap validation séparateurs (mode strict)

### Dépendances

- Accès au SegmentReader pour lire les posting lists ._raw réelles
- GapMap reader intégré

## PHASE-5 : Fuzzy (d>0)

### À implémenter

Levenshtein DFA sur le suffix FST. Même mécanisme que startsWith fuzzy.

Logique :
1. Construire un DFA Levenshtein pour la query
2. Walk le suffix FST avec le DFA (même code que AutomatonWeight)
3. Pour chaque terme trouvé : résoudre parent → ._raw posting
4. Ajuster byte_from par SI

### Dépendances

- `tantivy_fst::automaton` API (déjà utilisée dans le codebase)
- DFA wrapper existant dans ngram_contains_query.rs

## PHASE-6 : Branchement inverted index réel

### À implémenter

Connecter le suffix_contains search aux vraies posting lists ._raw du segment,
au lieu des fake postings en HashMap.

Logique :
1. Ouvrir le .sfx depuis le SegmentReader (lazy, on first contains query)
2. raw_ordinal → TermDictionary::term_info_from_ord() → TermInfo
3. InvertedIndexReader::read_postings(TermInfo) → posting list réelle
4. Extraire (doc_id, position, byte_from, byte_to) de chaque posting

### Dépendances

- SegmentReader modifications (ouvrir .sfx file)
- TermDictionary API du champ ._raw

## PHASE-7 : Normalisation Unicode + fix BUG-1/2/3

### À implémenter

Ajouter une normalisation Unicode (NFC) au tokenizer ._raw avant le lowercase.
Résout BUG-1 et BUG-2 à la source.

Fix BUG-3 : check POSITION_GAP dans read_separator.

### Impact

- Modification du tokenizer ._raw (configure_tokenizers dans handle.rs)
- Les index existants devront être réindexés
- Tests UTF-8 exotiques (İ, ß, etc.)

## PHASE-8 : Merger .sfx

### À implémenter

Merger les fichiers .sfx lors du merge de segments.

Logique :
1. Le FST suffix doit être reconstruit (les raw_ordinals changent au merge)
2. Les parent lists sont recalculées
3. La GapMap est concaténée avec remapping doc_ids
4. Pattern similaire à write_storable_fields dans merger.rs

## PHASE-9 : Supprimer ._ngram

### À implémenter

Retirer le champ ._ngram de l'indexation. Le .sfx le remplace complètement.

Fichiers impactés :
- lucivy_core/src/handle.rs — build_schema(), configure_tokenizers()
- lucivy_fts/rust/src/bridge.rs — auto_duplicate_field()
- lucivy_fts/rust/src/query.rs — build_query() ngram_field_pairs
- Tous les bindings

Gain : taille d'index réduite + temps d'indexation réduit.

## PHASE-10 : Benchmark et validation corpus réel

### À faire

1. Indexer le corpus rag3db (5201 docs) avec le .sfx
2. Comparer taille : .sfx vs ._ngram + stored text
3. Benchmark contains d=0 : suffix_contains vs ngram_contains
4. Benchmark contains d=1 (après Phase 5)
5. Vérifier 100% des résultats identiques entre les deux paths
6. Mesurer la taille du FST suffix sur un vrai corpus (estimation 20-40MB)

## Ordre recommandé

```
PHASE-6  Branchement inverted index réel    ← débloquer les tests E2E
PHASE-4  Multi-token search                  ← feature complète
PHASE-5  Fuzzy d>0                           ← feature complète
PHASE-10 Benchmark corpus réel               ← validation
PHASE-7  Normalisation Unicode (BUG-1/2/3)   ← correctness
PHASE-8  Merger .sfx                          ← production ready
PHASE-9  Supprimer ._ngram                    ← cleanup
```
