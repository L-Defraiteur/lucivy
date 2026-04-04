# 03 — Fuzzy separator-agnostic : word_id + DFA gap normalization

Date : 4 avril 2026

---

## Problème

Le fuzzy contains via trigram (`RegexContinuationQuery`) compare le contenu byte-à-byte contre la query, séparateurs inclus. Si le contenu a des séparateurs différents de la query, ça compte comme des edits supplémentaires.

### Cas concrets (misses pré-existants + nouveau)

| Query | Contenu | Séparateur query | Séparateur contenu | Span diff |
|-------|---------|------------------|--------------------|-----------|
| `rag3weaver` d=1 | `rag3.*weaver` (tokens "rag3" + "weaver") | (rien) | `.*` (2 bytes) | +2 |
| `rag3weaver` d=0 | `rag3.*weaver` | (rien) | `.*` (2 bytes) | +2 |
| `...emscripten Only...` d=1 | `...emscripten ---\n# Only...` | ` ` (1 byte) | ` ---\n# ` (7 bytes) | +6 |

Tous rejetés par `span_diff > distance` dans `check_chain`.

### Pourquoi d=0 (SuffixContainsQuery) ne souffre pas

Le chemin multi-token d=0 tokenise la query en mots et les matche **indépendamment**. Il vérifie l'adjacence des positions de tokens, pas les byte spans. Les séparateurs sont ignorés par design.

---

## Principe : distance = edits sur l'alpha, pas sur les séparateurs

En mode non-strict, la distance d s'applique aux caractères **alphanumériques** uniquement. Les séparateurs entre mots sont libres (1 espace, 3 tirets, un newline — même chose).

Deux endroits à corriger :

1. **Chain building** (intersect_trigrams_with_threshold) — le span_diff check
2. **DFA validation** (fuzzy_contains_via_trigram) — le feeding des gap bytes

---

## Fix 1 : word_id dans generate_ngrams

### Changement

`generate_ngrams` retourne un `word_id: Vec<usize>` supplémentaire. Chaque trigram reçoit l'id du mot d'où il vient (0-indexed, incrémenté à chaque séparateur non-alphanum).

```rust
fn generate_ngrams(query: &str, distance: u8)
    -> (Vec<String>, Vec<usize>, Vec<usize>, usize)
//     ngrams       positions   word_ids    n
{
    let lower = query.to_lowercase();
    let mut ngrams = Vec::new();
    let mut positions = Vec::new();
    let mut word_ids = Vec::new();

    // Split en segments alphanumériques
    let mut segments: Vec<(usize, &str)> = Vec::new(); // (byte_offset, text)
    let mut seg_start = None;
    for (i, c) in lower.char_indices() {
        if c.is_alphanumeric() {
            if seg_start.is_none() { seg_start = Some(i); }
        } else if let Some(start) = seg_start {
            segments.push((start, &lower[start..i]));
            seg_start = None;
        }
    }
    if let Some(start) = seg_start {
        segments.push((start, &lower[start..]));
    }

    for (word_id, &(seg_offset, seg_text)) in segments.iter().enumerate() {
        let seg_bytes = seg_text.as_bytes();
        if seg_bytes.len() < n { /* handle short segments */ continue; }
        for i in 0..=seg_bytes.len() - n {
            let gram = &seg_text[i..i + n];
            ngrams.push(gram.to_string());
            positions.push(seg_offset + i);
            word_ids.push(word_id);
        }
    }

    (ngrams, positions, word_ids, n)
}
```

### Impact

Chaque trigram sait de quel mot il vient. Pas de changement de comportement pour les queries single-word (tous les trigrams ont word_id=0).

---

## Fix 2 : span_diff par mot dans check_chain

### Changement

Au lieu du span_diff global, vérifier le span_diff **uniquement entre paires de trigrams du même mot**. Les paires cross-word sont libres.

```rust
// Avant (global span_diff) :
let text_span = last.1 as i64 - first.1 as i64;
let query_span = query_positions[last.0] as i64 - query_positions[first.0] as i64;
let span_diff = (text_span - query_span).unsigned_abs();
if span_diff > distance as u64 { return false; }

// Après (per-word span_diff) :
// Global span_diff supprimé.
// Le check ne porte que sur les paires intra-word (see proven check below).
```

Pour le **proven** check (qui décide si on skip le DFA), on vérifie :
- Toutes les paires intra-word : span_diff ≤ distance
- Paires cross-word : pas de check (séparateur libre)

```rust
let mut proven = chain.len() == num_trigrams;
if proven && chain.len() >= 2 {
    for w in chain.windows(2) {
        // Skip cross-word pairs
        if word_ids[w[0].0] != word_ids[w[1].0] { continue; }
        let pair_text_span = w[1].1 as i64 - w[0].1 as i64;
        let pair_query_span = query_positions[w[1].0] as i64 - query_positions[w[0].0] as i64;
        if (pair_text_span - pair_query_span).unsigned_abs() > distance as u64 {
            proven = false;
            break;
        }
    }
}
```

### Filtrage de faux positifs

Sans le span_diff global, on pourrait craindre plus de faux positifs. Mais :
- Le **threshold** (39/43 pour la longue query) filtre déjà la majorité
- Les paires **intra-word** vérifient l'alignement local
- Le **DFA** validation (fix 3) valide exactement pour les non-proven
- En pratique les faux positifs viennent de trigrams dispersés dans un doc, pas de trigrams bien ordonnés mais avec des gaps différents

### Sécurité : span_diff relaxé au lieu de supprimé

Si on préfère garder un filtre global, alternative :

```rust
// Compter les transitions cross-word dans la chaîne
let cross_word_count = chain.windows(2)
    .filter(|w| word_ids[w[0].0] != word_ids[w[1].0])
    .count();
// Tolérance : distance + marge par transition cross-word
let tolerance = distance as u64 + cross_word_count as u64 * MAX_SEP_TOLERANCE;
if span_diff > tolerance { return false; }
```

Avec `MAX_SEP_TOLERANCE = 64` (ou configurable), ça reste un filtre utile contre les faux positifs tout en tolérant les séparateurs variables.

---

## Fix 3 : DFA gap normalization

### Le problème

Le DFA Levenshtein est feedé les bytes du contenu token par token, avec les gap bytes entre tokens (via GapMap). Si le gap est `---\n# ` (7 bytes) et que la query attend ` ` (1 byte), c'est 6 edits.

### Le fix

Dans le DFA walk (lignes ~880-1000 de `regex_continuation_query.rs`), au lieu de feeder les vrais gap bytes, feeder un **séparateur normalisé** : un seul espace (0x20).

```rust
// Avant :
if let Some(gap_bytes) = gap {
    for &byte in gap_bytes {
        state = dfa.accept(&state, byte);
    }
}

// Après :
if gap.is_some() {
    // Normalize: any gap between tokens = single space in DFA
    state = dfa.accept(&state, b' ');
    if !dfa.can_match(&state) { break; }
}
```

### Aussi dans validate_path

`validate_path` dans `literal_resolve.rs` fait la même chose : feed gap bytes au DFA. Même fix :

```rust
// Avant (literal_resolve.rs:296-304) :
let gap = gapmap.read_separator(doc_id, pos - 1, pos);
if let Some(gap_bytes) = gap {
    if is_value_boundary(gap_bytes) { return None; }
    for &byte in gap_bytes {
        state = automaton.accept(&state, byte);
    }
}

// Après :
let gap = gapmap.read_separator(doc_id, pos - 1, pos);
if let Some(gap_bytes) = gap {
    if is_value_boundary(gap_bytes) { return None; }
    // Normalize gap to single space for separator-agnostic matching
    state = automaton.accept(&state, b' ');
    if !automaton.can_match(&state) { return None; }
}
```

### Et dans la query aussi

La query doit aussi être normalisée avant de construire le DFA. Sinon le DFA attend "emscripten only" (1 espace) et reçoit "emscripten " (1 espace normalisé) — ça marche. Mais si la query avait "emscripten  only" (2 espaces), le DFA attendrait 2 espaces et recevrait 1 → 1 edit compté.

Fix : normaliser aussi la query en remplaçant tous les runs de non-alphanum par un seul espace avant de construire le DFA.

```rust
// Avant de build le DFA :
let normalized_query: String = query_text.chars().fold(
    (String::new(), false),
    |(mut s, was_sep), c| {
        if c.is_alphanumeric() {
            s.push(c);
            (s, false)
        } else if !was_sep {
            s.push(' ');
            (s, true)
        } else {
            (s, true)
        }
    }
).0.trim().to_string();
```

---

## Fix 4 : content_byte_starts séparateur-agnostique

Dans la construction du highlight (content_byte_starts dans regex_continuation_query.rs), les byte positions sont calculées en incluant les gap bytes. Avec la normalisation, les positions restent celles du contenu original (les highlights pointent sur le texte réel). Donc **pas de changement** pour les highlights — on normalise uniquement pour le DFA, pas pour le mapping highlight.

---

## Ordre d'implémentation

1. **Fix 2 : word_id + span_diff per-word** — corrige le chain building
   - Modifier `generate_ngrams` pour retourner word_ids
   - Modifier `intersect_trigrams_with_threshold` pour recevoir word_ids
   - Modifier `check_chain` pour skip les paires cross-word

2. **Fix 3 : DFA gap normalization** — corrige la validation
   - Normaliser la query (non-alpha runs → single space)
   - Normaliser les gaps dans le DFA walk (gap → single space byte)
   - Normaliser les gaps dans `validate_path`

3. **Tests**
   - Les 3 misses pré-existants doivent passer
   - La query longue "Build rag3weaver... Native" d=1 doit trouver
   - Non-régression sur test_fuzzy_ground_truth (296/296 + 260/260 highlights)
   - Non-régression sur test_playground_repro (single-word queries)

---

## Fichiers modifiés

| Fichier | Modification |
|---------|-------------|
| `src/query/phrase_query/regex_continuation_query.rs` | generate_ngrams (word_ids), DFA gap normalization, query normalization |
| `src/query/phrase_query/literal_resolve.rs` | intersect_trigrams_with_threshold (word_ids, span_diff per-word), validate_path (gap normalization) |
| `lucivy_core/tests/test_playground_repro.rs` | Déjà modifié (query longue d=0/d=1) |
