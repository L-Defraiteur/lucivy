# Design : GapMap multi-value

Date : 14 mars 2026 — 16h30

## Problème

Un champ texte peut avoir plusieurs valeurs par document :

```
Doc 0, field "tags" : ["hello world", "foo bar"]
```

Le tokenizer produit des tokens séparés par value, avec un `POSITION_GAP=1`
entre les values dans le posting list :

```
Value 0: hello(Ti=0), world(Ti=1)
  → indexing_position.end_position = 2 + POSITION_GAP = 3

Value 1: foo(Ti=3), bar(Ti=4)
  → indexing_position.end_position = 5 + POSITION_GAP = 6
```

### Le problème du mapping

La GapMap stocke les gaps en indices séquentiels (seq=0,1,2,3).
Les posting Ti utilisent des positions avec gaps (Ti=0,1,3,4).

```
Token      seq    Ti     Gap entre token précédent
─────      ───    ──     ─────────────────────────
hello      0      0      gap[0] = prefix de value 0
world      1      1      gap[1] = " " (entre hello et world)
                         gap[2] = VALUE_BOUNDARY (entre les values)
foo        2      3      gap[3] = prefix de value 1
bar        3      4      gap[4] = " " (entre foo et bar)
                         gap[5] = suffix de value 1
```

Pour accéder au gap via Ti, on a besoin de convertir : `Ti → seq`.

## Design

### Format GapMap étendu

```
Per doc :
  num_tokens: u16          // total tokens across all values
  num_values: u8           // 1 = single-value (fast path), >1 = multi

  if num_values > 1 :
    value_offsets: [(seq_start: u16, ti_start: u32) × num_values]
    // Mapping value → (premier index séquentiel, premier Ti du posting)
    // Permet la conversion Ti → seq en O(log(num_values))

  gaps: [gap × (num_tokens + num_values)]
    // num_tokens + num_values gaps au total :
    // Pour chaque value : 1 prefix + N-1 séparateurs + 1 suffix
    // Le suffix de la dernière value est aussi le suffix du doc
    //
    // Layout séquentiel :
    //   [prefix_v0] [seps_v0...] [suffix_v0]
    //   [prefix_v1] [seps_v1...] [suffix_v1]
    //   ...
    //
    // gap[seq+1] = séparateur après le token à seq
    // (dans la même value uniquement)

  Gap encoding (inchangé) :
    len = 0..253   : normal gap
    len = 254      : VALUE_BOUNDARY marker (0 bytes, marqueur pur)
    len = 255      : extended length (>253 bytes)
```

### Fast path single-value (99% des cas)

Pour `num_values = 1` :
- Pas de value_offsets stocké (économie d'espace)
- `Ti → seq` est l'identité : `seq = Ti`
- `gap_index = Ti + 1` pour le séparateur après le token à Ti
- Zéro overhead par rapport au format actuel

### Conversion Ti → seq pour multi-value

```rust
fn ti_to_seq(ti: u32, value_offsets: &[(u16, u32)]) -> u16 {
    // Binary search pour trouver quelle value contient ce Ti
    let value_idx = value_offsets
        .partition_point(|&(_, ti_start)| ti_start <= ti) - 1;
    let (seq_start, ti_start) = value_offsets[value_idx];
    seq_start + (ti - ti_start) as u16
}
```

### Accès au gap pour séparateur entre Ti_a et Ti_b

```rust
fn gap_between(ti_a: u32, ti_b: u32, ...) -> Option<&[u8]> {
    // 1. Vérifier que Ti sont consécutifs (ti_b == ti_a + 1)
    if ti_b != ti_a + 1 {
        return None; // pas consécutifs → pas de match
    }

    // 2. Convertir Ti_b en seq
    let seq_b = ti_to_seq(ti_b, value_offsets);

    // 3. Lire le gap
    let gap = gapmap.read_gap(doc_id, seq_b_gap_index);

    // 4. Vérifier que c'est pas un VALUE_BOUNDARY
    if gap == VALUE_BOUNDARY {
        return None; // cross-value → rejeté
    }

    Some(gap)
}
```

### Pourquoi le POSITION_GAP protège déjà

Pour le multi-token contains, le check `Ti_b == Ti_a + 1` rejette déjà
les matches cross-value car les Ti sautent de `POSITION_GAP=1` :

```
Value 0: A(Ti=0), B(Ti=1)
Value 1: C(Ti=3), D(Ti=4)

"B C" : Ti_a=1, Ti_b=3, 3 ≠ 1+1=2 → REJETÉ ✓ (par le Ti check)
"A B" : Ti_a=0, Ti_b=1, 1 == 0+1   → OK ✓ (même value)
"C D" : Ti_a=3, Ti_b=4, 4 == 3+1   → OK ✓ (même value)
```

Le VALUE_BOUNDARY dans la GapMap est un **filet de sécurité** — le Ti check
suffit pour la correctness dans la plupart des cas. Le VALUE_BOUNDARY
sert pour :
1. Validation explicite en mode strict (double check)
2. Single-token queries qui vérifient prefix/suffix (rare)
3. Robustesse si POSITION_GAP change un jour

## Collector API

```rust
collector.begin_doc();

// Value 0
collector.begin_value("hello world");  // raw text of this value
collector.add_token("hello", 0, 5);
collector.add_token("world", 6, 11);
collector.end_value();

// Value 1
collector.begin_value("foo bar");
collector.add_token("foo", 0, 3);
collector.add_token("bar", 4, 7);
collector.end_value();

collector.end_doc();
```

Internalement :
- `begin_value(text)` : sauve le texte, reset les tokens de la value courante
- `add_token()` : accumule les tokens de la value courante
- `end_value()` : calcule les gaps de cette value depuis son texte
- `end_doc()` : assemble tous les gaps avec VALUE_BOUNDARY entre les values,
  écrit dans la GapMap

## Interaction avec le segment writer

```rust
// Dans index_document(), pour un champ ._raw :

// begin_doc une seule fois en début de document
// (déplacé hors de la boucle values)

for value in values {
    let text = value.as_str();

    // begin_value avec le texte brut
    collector.begin_value(text);

    // Interceptor capture les tokens
    let mut interceptor = SfxTokenInterceptor::wrap(token_stream);
    postings_writer.index_text(&mut interceptor, ...);

    for tok in interceptor.take_captured() {
        collector.add_token(&tok.text, tok.offset_from, tok.offset_to);
    }

    collector.end_value();
}

collector.end_doc();
```

## Impact sur le code existant

### GapMapWriter : ajouter VALUE_BOUNDARY

- Nouveau `add_doc_multi()` qui prend `Vec<Vec<&[u8]>>` (gaps par value)
- Insère `len=254` (VALUE_BOUNDARY) entre chaque value
- L'ancien `add_doc()` reste pour single-value (pas de boundary)

### GapMapReader : ajouter ti_to_seq

- Nouveau `read_gap_by_ti()` qui convertit Ti → seq
- Fast path pour `num_values=1` : seq = Ti direct
- L'ancien `read_gap()` par index séquentiel reste disponible

### SfxCollector : refactor begin_doc/end_doc

- `begin_doc()` / `begin_value(text)` / `add_token()` / `end_value()` / `end_doc()`
- Accumule les gaps par value, assemble à `end_doc()`

### Segment writer : déplacer begin_doc hors de la boucle values

- Fix du bug actuel (begin_doc dans la boucle)
- `begin_doc()` avant le `for value in values`
- `end_doc()` après la boucle

## Taille additionnelle

Pour single-value : +1 byte (num_values=1). Négligeable.

Pour multi-value : +1 byte (num_values) + 6 bytes par value supplémentaire
(seq_start u16 + ti_start u32) + 1 byte par VALUE_BOUNDARY marker.
Sur un doc multi-value typique (2-3 values) : +10-20 bytes. Négligeable.
