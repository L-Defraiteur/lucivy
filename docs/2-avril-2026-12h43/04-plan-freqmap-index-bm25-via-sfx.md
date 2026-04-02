# 04 — Plan : FreqMap index — BM25 scoring via SFX

Date : 2 avril 2026

## Problème

Le term dict tantivy est un index redondant avec le SFX. Les queries
term/phrase/fuzzy/regex top-level l'utilisent pour le BM25 scoring
(doc_freq, term_freq). Pour pouvoir virer le term dict, il faut que
le SFX fournisse ces fréquences.

## Solution : FreqMap — nouvel index dérivé

### Données stockées

- **doc_freq(ordinal)** : nombre de docs contenant ce token → u32, O(1)
- **term_freq(ordinal, doc_id)** : fréquence du token dans un doc → u16, O(log n)

### Format binaire

```
[4 bytes] magic "FREQ"
[4 bytes] num_terms: u32
[4 bytes × num_terms] doc_freq per ordinal
[4 bytes × (num_terms + 1)] offset table (into tf_data section)
[tf_data] per ordinal: (doc_id: vint, tf: vint) × doc_freq, sorted by doc_id
```

- doc_freq : array fixe, O(1) lookup par ordinal
- term_freq : binary search sur doc_id dans la section de l'ordinal, O(log n)
- Total memory : ~8 bytes par terme + ~4-6 bytes par (terme, doc) entry

### SfxDerivedIndex implémentation

```rust
pub struct FreqMapIndex {
    // Pendant la single-pass : accumule les fréquences
    freqs: HashMap<(u32, u32), u32>,  // (ord, doc_id) → tf
}

impl SfxIndexFile for FreqMapIndex {
    fn id(&self) -> &'static str { "freqmap" }
    fn extension(&self) -> &'static str { "freqmap" }
    fn kind(&self) -> IndexKind { IndexKind::Derived }

    fn on_posting(&mut self, ord: u32, doc_id: u32, _pos: u32, _bf: u32, _bt: u32) {
        *self.freqs.entry((ord, doc_id)).or_insert(0) += 1;
    }

    fn serialize(&self) -> Vec<u8> { ... }
}
```

### Reader API

```rust
pub struct FreqMapReader<'a> { ... }

impl FreqMapReader {
    pub fn open(bytes: &[u8]) -> Option<Self>;
    pub fn doc_freq(&self, ordinal: u32) -> u32;           // O(1)
    pub fn term_freq(&self, ordinal: u32, doc_id: u32) -> u32;  // O(log n)
    pub fn num_terms(&self) -> u32;
    pub fn total_term_freq(&self, ordinal: u32) -> u64;    // sum of tf across all docs
}
```

### Intégration BM25

L'`EnableScoring` / `Bm25StatisticsProvider` aurait accès au FreqMapReader
pour chaque segment. Les queries SFX pourraient scorer directement :

```rust
let df = freqmap.doc_freq(matched_ordinal);
let tf = freqmap.term_freq(matched_ordinal, doc_id);
let score = bm25(tf, df, fieldnorm, num_docs);
```

### Étapes

1. Créer `src/suffix_fst/freqmap.rs` : FreqMapWriter + FreqMapReader
2. Implémenter `SfxIndexFile` (kind: Derived, on_posting)
3. Ajouter à `all_indexes()` dans index_registry.rs
4. Charger dans segment_reader.rs (automatique via registry)
5. Tests unitaires roundtrip
6. (futur) Brancher sur le BM25 scoring pour les queries SFX
