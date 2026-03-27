# 09 — Design : position-to-ordinal map + byte bitmap — regex alien technology

## Problème

Phase 3c explore les sibling links à l'aveugle à partir d'un token, en espérant atteindre le prochain littéral. Avec `.*` le DFA ne prune jamais → explosion exponentielle. Même avec 26 docs survivants et intersection multi-littérale, un seul segment fait 16 secondes.

Le problème fondamental : **on explore l'inconnu alors qu'on connaît la destination**.

Quand l'intersection dit "rag3 est à pos 3, ver est à pos 7 dans doc 42", il suffit de VALIDER le chemin 3→7, pas de l'EXPLORER via 64 niveaux de siblings.

## Proposition 1 : Position-to-ordinal map

### Concept

L'inverse du posting index. Le posting fait `ordinal → [(doc_id, position)]`. La map fait `(doc_id, position) → ordinal`.

Pour chaque document, un tableau d'ordinals dans l'ordre des positions :

```
Doc 42: [ord_import, ord_rag3, ord_weaver, ord_from, ord_core, ord_semicolon]
         pos 0       pos 1     pos 2       pos 3    pos 4     pos 5
```

### Usage pour regex

Pour `rag3.*ver` avec intersection → doc 42, "rag3" à pos 1, "ver" à pos 4 :

```
1. Lire la map doc 42 : positions 2, 3 → ord_weaver, ord_from
2. ord_to_term(ord_weaver) → "weaver"
3. ord_to_term(ord_from) → "from"
4. GapMap : gap(1,2) → "" (contigu), gap(2,3) → " ", gap(3,4) → " "
5. Feed DFA depuis state après "rag3" :
   "" + "weaver" + " " + "from" + " " + "ver"
   = "weaverfromver" avec espaces
6. DFA `.*ver` : .* matche tout jusqu'à "ver" → ACCEPTE
7. Total : ~20 bytes feedés au DFA. Instantané.
```

**Plus aucune exploration de siblings. Plus aucune profondeur 64. O(distance × avg_token_len).**

### Format de stockage

```
Fichier .posmap (par segment) :

HEADER:
  [4 bytes] num_docs: u32 LE
  [8 bytes × (num_docs + 1)] offsets: u64 LE  // byte offset per doc

DATA (per doc):
  [4 bytes × num_tokens] ordinals: u32 LE     // ordinal at each position
```

### Estimation taille

- 862 docs × ~100 tokens/doc moyen = 86 200 positions
- 86 200 × 4 bytes = **344 KB** par segment
- Pour 6 segments : **~2 MB** total
- Pour 90K docs × 200 tokens : 90K × 200 × 4 = **72 MB** (acceptable, compressible)

### Construction

Dans le `SegmentWriter` / `SfxCollector`, pendant l'indexation on a déjà (doc_id, position, ordinal) pour chaque token. Il suffit de collecter :

```rust
struct PosMapWriter {
    // Per doc: Vec of ordinals in position order
    docs: Vec<Vec<u32>>,
}

impl PosMapWriter {
    fn add_token(&mut self, doc_id: u32, position: u32, ordinal: u32) {
        let doc = &mut self.docs[doc_id as usize];
        if position as usize >= doc.len() {
            doc.resize(position as usize + 1, u32::MAX);
        }
        doc[position as usize] = ordinal;
    }

    fn serialize(&self) -> Vec<u8> { ... }
}
```

### Reader

```rust
struct PosMapReader<'a> {
    data: &'a [u8],
    num_docs: u32,
}

impl<'a> PosMapReader<'a> {
    fn ordinal_at(&self, doc_id: u32, position: u32) -> Option<u32> {
        // O(1) : offset table → read u32 at position
    }

    fn ordinals_range(&self, doc_id: u32, pos_from: u32, pos_to: u32) -> Vec<u32> {
        // O(distance) : lire la tranche
    }
}
```

### Impact sur regex

Remplace Phase 3c entièrement pour les patterns multi-littéraux :

```
Avant (Phase 3c) :
  Pour chaque doc survivant :
    Pour chaque depth 0..64 :
      Pour chaque sibling :
        clone DFA, feed gap, feed text, resolve sibling
  → O(docs × 64 × siblings × resolve) = explosion

Après (PosMap) :
  Pour chaque doc survivant :
    distance = pos_end - pos_start
    ordinals = posmap.ordinals_range(doc, pos_start+1, pos_end)
    Pour chaque ordinal : ord_to_term → feed DFA
  → O(docs × distance × token_len) = linéaire
```

### Autres usages

- **Debug / diagnostic** : reconstruire le texte tokenisé d'un doc sans toucher le store
- **Phrase query optimisation** : vérifier les positions exactes en O(1)
- **Highlight reconstruction** : byte_from/byte_to déjà dans les postings, mais l'ordinal donne le token text

## Proposition 2 : Byte presence bitmap par ordinal

### Concept

Pour chaque ordinal, un bitset de 256 bits (32 bytes) indiquant quels bytes apparaissent dans le token text.

```
"weaver" → bits {97(a), 101(e), 114(r), 118(v), 119(w)} = 1, reste = 0
"rag3"   → bits {51(3), 97(a), 103(g), 114(r)} = 1
```

### Usage pour regex

Avant de feeder un token au DFA, on vérifie que le token PEUT matcher le regex localement.

Pour `[a-z]+ver` : le regex exige que les bytes soient dans [a-z] (0x61-0x7A) pour la partie `[a-z]+`. Si un token a le bit '3' (0x33) set, il contient un chiffre → `[a-z]+` échouerait → skip sans feeder le DFA.

Pour `rag3.*ver` : le `.*` accepte tout → le bitmap ne prune pas (puisque tout est accepté). Donc le bitmap n'aide pas pour `.*` mais aide pour les patterns restrictifs.

### Cas où ça aide

| Pattern | Constraint extractible | Bitmap prune |
|---|---|---|
| `[a-z]+ver` | tous bytes dans [a-z] | oui — skip tokens avec chiffres/symbols |
| `[0-9]{4}` | tous bytes dans [0-9] | oui — skip tokens avec lettres |
| `rag3.*ver` | aucune (.*) | non |
| `foo_bar` | contient _ (0x5F) | oui — skip tokens sans underscore |
| `\d+\.\d+` | contient . et [0-9] | oui |

### Format de stockage

```
Fichier .bytemap (par segment) :

[4 bytes] num_ordinals: u32 LE
[32 bytes × num_ordinals] bitmaps
```

### Estimation taille

- 5K ordinals × 32 bytes = **160 KB** par segment
- 50K ordinals (90K docs) × 32 bytes = **1.6 MB** par segment
- Très compact.

### Construction

```rust
struct ByteBitmapWriter {
    bitmaps: Vec<[u8; 32]>, // 256 bits per ordinal
}

impl ByteBitmapWriter {
    fn record_token(&mut self, ordinal: u32, text: &[u8]) {
        let bm = &mut self.bitmaps[ordinal as usize];
        for &byte in text {
            bm[byte as usize / 8] |= 1 << (byte % 8);
        }
    }
}
```

### Reader

```rust
struct ByteBitmapReader<'a> {
    data: &'a [u8],
    num_ordinals: u32,
}

impl<'a> ByteBitmapReader<'a> {
    fn bitmap(&self, ordinal: u32) -> &[u8; 32] {
        // O(1) : direct offset
    }

    fn contains_byte(&self, ordinal: u32, byte: u8) -> bool {
        let bm = self.bitmap(ordinal);
        bm[byte as usize / 8] & (1 << (byte % 8)) != 0
    }

    fn all_bytes_in_range(&self, ordinal: u32, lo: u8, hi: u8) -> bool {
        // Check que TOUS les bytes set dans le bitmap sont dans [lo, hi]
        let bm = self.bitmap(ordinal);
        for i in 0..32 {
            let byte_base = i * 8;
            let mut mask = bm[i];
            while mask != 0 {
                let bit = mask.trailing_zeros();
                let byte_val = byte_base as u8 + bit as u8;
                if byte_val < lo || byte_val > hi {
                    return false;
                }
                mask &= mask - 1;
            }
        }
        true
    }
}
```

### Usage dans le regex walk

```rust
// Pendant Phase 3c ou le nouveau PosMap walk :
for ordinal in path_ordinals {
    // Pre-check bitmap avant de feeder le DFA
    if !regex_byte_constraint_matches(bitmap_reader, ordinal) {
        // Ce token ne peut PAS satisfaire le regex → skip/dead
        break;
    }
    let text = ord_to_term(ordinal);
    state = feed(state, text);
}
```

## Proposition 3 : Bigram bloom filter par ordinal

### Concept

Pour chaque ordinal, un bloom filter (64 ou 128 bits) des bigrams (paires de bytes consécutifs) du token.

```
"weaver" → bigrams: we, ea, av, ve, er → bloom(we)∪bloom(ea)∪bloom(av)∪bloom(ve)∪bloom(er)
```

### Usage

Le regex extrait ses bigrams obligatoires. Un token doit contenir tous les bigrams pour pouvoir matcher.

Pour `ver` : bigrams "ve", "er" → check bloom → skip tokens qui n'ont pas ces bigrams.

### Taille

- 64 bits (8 bytes) par ordinal → 5K × 8 = **40 KB**
- Faux positifs possibles (bloom) mais pas de faux négatifs

### Avantages vs bitmap

- Plus sélectif que le byte bitmap (bigram "ve" est plus rare que byte 'v' + byte 'e' séparément)
- Détecte l'ORDRE des bytes, pas juste la présence

### Inconvénients

- Faux positifs du bloom filter
- Plus complexe à implémenter (extraction des bigrams du regex, hash, bloom check)
- Pour des tokens courts (3-5 bytes), le bitmap byte est déjà très sélectif

### Verdict

Intéressant mais rapport complexité/gain inférieur au bitmap byte. Le bitmap byte est quasi-gratuit et couvre 80% des cas. Le bigram bloom serait un raffinement futur.

## Proposition 4 : Skip Phase 3c quand has_multi_literal

### Concept (quick win)

Quand l'intersection multi-littérale a validé les positions, émettre directement le match sans Phase 3c.

### Limitation

Pas de validation DFA entre les littéraux. Faux positifs pour les patterns restrictifs (ex: `rag3[a-z]+ver` matcherait même si les tokens entre contiennent des chiffres).

### Quand c'est correct

- `.*` entre les littéraux → toujours correct (tout est accepté)
- `[a-z]*` entre les littéraux → correct si les tokens entre sont all-lowercase (probable en code)
- `[a-z]+` → correct si au moins un byte lowercase entre les littéraux

### Quand c'est incorrect (faux positifs)

- `rag3[0-9]+ver` → matcherait "rag3weaver" alors que "weaver" n'est pas `[0-9]+`
- Taux de faux positifs très faible en pratique (les users écrivent rarement des regex aussi spécifiques en mode contains)

### Avec PosMap (proposition 1) : plus de faux positifs

Si on a la PosMap, on peut valider le DFA en O(distance) entre les littéraux. Le skip direct est un fallback quand la PosMap n'est pas encore disponible.

## Plan d'implémentation

### Phase 1 : PosMap (priorité haute)

1. `PosMapWriter` dans `src/suffix_fst/posmap.rs` — collecte (doc_id, position, ordinal)
2. Intégrer dans `SfxCollector::end_value()` — on a déjà l'ordinal et la position
3. `PosMapReader` — O(1) lookup, range read
4. Stocker dans le .sfx (après sibling table, avant gapmap) ou fichier séparé .posmap
5. Dans `regex_contains_via_literal` : quand has_multi_literal + PosMap disponible :
   - Lire ordinals entre pos_start et pos_end
   - Feed DFA en séquence
   - O(distance) par doc

### Phase 2 : Byte bitmap (priorité moyenne)

1. `ByteBitmapWriter` dans `src/suffix_fst/bytemap.rs`
2. Intégrer dans le SfxCollector pendant la construction du term dict
3. `ByteBitmapReader` — O(1) bitmap lookup
4. Stocker dans le .sfx ou fichier séparé
5. Pré-filtre dans le regex walk (PosMap ou Phase 3c) : check bitmap avant DFA feed

### Phase 3 : Bigram bloom (futur)

Pas prioritaire. À explorer quand bitmap + PosMap sont en place et qu'on mesure les gains résiduels.

## Estimation des gains

| Approche | `rag3.*ver` actuel | Avec PosMap | Avec PosMap + Bitmap |
|---|---|---|---|
| Phase 3c (26 docs × 64 depth) | 16 900 ms | — | — |
| PosMap walk (26 docs × ~5 tokens) | — | **< 1 ms** | **< 1 ms** |
| Pattern `[a-z]+ver` sans bitmap | — | ~1 ms (DFA feed) | — |
| Pattern `[a-z]+ver` avec bitmap | — | — | **< 0.5 ms** (skip non-alpha) |

Le PosMap seul donne un gain de **~17 000x** pour les regex multi-littéraux avec `.*`.
