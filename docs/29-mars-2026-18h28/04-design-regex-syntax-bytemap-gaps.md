# 04 — Design : regex-syntax AST + ByteMap pour validation des gaps

Date : 30 mars 2026

## Contexte

On a `extract_all_literals` artisanal qui parse le regex à la main pour
extraire les littéraux. Les gaps entre littéraux sont classifiés en
`AcceptAnything` (.*) ou `NeedsValidation` (tout le reste). Pour les gaps
contraints, on utilise `validate_path` qui feed le DFA byte par byte —
c'est lent.

## Idée

Utiliser `regex-syntax` (déjà dans nos deps) pour parser le pattern en AST
typé (`Hir`). Puis classifier chaque gap non pas en 2 catégories mais en 3 :

1. **AcceptAnything** : `.*`, `.+`, `.*?` — gratuit
2. **ByteMapCheck** : `[a-z]+`, `[0-9]*`, `\w+` — check via ByteMap O(1)
3. **DfaValidation** : tout le reste — validate_path avec DFA

## L'AST regex-syntax

`regex_syntax::parse(pattern)` retourne un `Hir` (High-level IR) :

```
Hir::Literal(bytes)           — littéral exact
Hir::Class(ClassBytes/Unicode) — character class [a-z], \d, \w
Hir::Dot                       — le .
Hir::Repetition { sub, min, max, greedy }  — *, +, ?, {n,m}
Hir::Concat(Vec<Hir>)         — séquence
Hir::Alternation(Vec<Hir>)    — |
Hir::Group(Hir)               — (...)
Hir::Empty                    — ε
```

Pour `rag.*ver` :
```
Concat([Literal("rag"), Repetition(Dot, 0..∞), Literal("ver")])
```

Pour `rag[a-z]+ver` :
```
Concat([Literal("rag"), Repetition(Class([a-z]), 1..∞), Literal("ver")])
```

## Classification des gaps

On walk le `Hir` pour extraire les littéraux et les gaps. Pour chaque gap :

```rust
fn classify_gap(hir: &Hir) -> GapKind {
    match hir {
        // .* ou .+ → accept anything
        Hir::Repetition { sub, .. } if is_dot(sub) => AcceptAnything,

        // [a-z]+ ou [0-9]* → ByteMap check
        Hir::Repetition { sub, .. } if is_byte_class(sub) => {
            let ranges = extract_byte_ranges(sub);
            ByteMapCheck(ranges)
        }

        // Tout le reste → DFA
        _ => DfaValidation,
    }
}
```

## ByteMap check pour character classes

Pour un gap `[a-z]+` entre positions pos_a et pos_b :

```rust
fn validate_gap_bytemap(
    posmap: &PosMapReader,
    bytemap: &ByteBitmapReader,
    doc_id: DocId,
    pos_from: u32,  // exclusive
    pos_to: u32,    // exclusive
    ranges: &[(u8, u8)],  // byte ranges from the character class
) -> bool {
    // Every token between pos_from and pos_to must have ALL its bytes
    // within the allowed ranges.
    for pos in (pos_from + 1)..pos_to {
        if let Some(ord) = posmap.ordinal_at(doc_id, pos) {
            if let Some(bm) = bytemap.bitmap(ord) {
                // Check that every set bit in the bitmap is within a range
                for chunk_idx in 0..32 {
                    let chunk = bm[chunk_idx];
                    if chunk == 0 { continue; }
                    let mut bits = chunk;
                    while bits != 0 {
                        let bit_pos = bits.trailing_zeros() as u8;
                        let byte_val = (chunk_idx as u8) * 8 + bit_pos;
                        let in_range = ranges.iter().any(|&(lo, hi)| byte_val >= lo && byte_val <= hi);
                        if !in_range { return false; }
                        bits &= bits - 1;
                    }
                }
            }
        }
    }
    true
}
```

**Coût** : O(n_tokens × popcount) au lieu de O(n_tokens × avg_token_len).
Typiquement ~5 iterations par token (popcount of 32 bytes) au lieu de ~6
(avg token length). Le gain est modeste par token mais élimine les tokens
sans même les lire via TermTexts.

Le vrai gain : si un token a un byte hors range, on **skip immédiatement**
sans feeder le DFA. C'est un early-out gratuit.

## Plan d'implémentation

### Étape 1 : Parse via regex-syntax

Remplacer `extract_literals_with_gaps` par un parse propre :

```rust
fn parse_regex_structure(pattern: &str) -> Vec<RegexSegment> {
    let hir = regex_syntax::parse(pattern).unwrap();
    walk_hir(&hir)
}

enum RegexSegment {
    Literal(String),
    Gap(GapKind),
}

enum GapKind {
    AcceptAnything,
    ByteRangeCheck(Vec<(u8, u8)>),  // [(lo, hi), ...]
    DfaValidation,
}
```

### Étape 2 : ByteMap validation dans validate_path

Ajouter une variante de `validate_path` pour les gaps ByteMapCheck :

```rust
fn validate_gap_bytemap(...)  // voir ci-dessus
```

### Étape 3 : Brancher dans le multi-literal path

Pour chaque gap :
- AcceptAnything → skip (déjà fait)
- ByteRangeCheck → `validate_gap_bytemap()` — O(1) par token
- DfaValidation → `validate_path()` — O(token_len) par token

## Fichiers modifiés

- `regex_continuation_query.rs` : remplacer `extract_literals_with_gaps`
  par `parse_regex_structure`, brancher les 3 types de gaps
- `literal_resolve.rs` : ajouter `validate_gap_bytemap`
- `dfa_byte_filter.rs` : ajouter `token_bytes_in_ranges` helper

## Patterns couverts

| Pattern | Gap type | Validation |
|---------|----------|------------|
| `rag.*ver` | AcceptAnything | skip |
| `rag.+ver` | AcceptAnything | skip |
| `rag[a-z]+ver` | ByteRangeCheck([a-z]) | bytemap |
| `rag\d+ver` | ByteRangeCheck([0-9]) | bytemap |
| `rag\w+ver` | ByteRangeCheck([a-zA-Z0-9_]) | bytemap |
| `rag[^0-9]+ver` | DfaValidation | validate_path |
| `rag(foo|bar)ver` | DfaValidation | validate_path |
| `rag.{3,5}ver` | AcceptAnything | skip (. = any) |
