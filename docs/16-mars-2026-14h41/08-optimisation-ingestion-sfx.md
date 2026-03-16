# Optimisation ingestion SFX — 16 mars 2026

## Constat

Le bench `bench_contains` indexe 213k fichiers texte depuis `/tmp/rag3db_bench`. L'indexation prend ~20 minutes en debug. Les bottlenecks identifiés sont dans le SfxCollector et le SuffixFstBuilder.

## Problème 1 : allocations String redondantes dans SfxCollector

### Code actuel

```rust
// SfxCollector::add_token — appelé pour CHAQUE occurrence de CHAQUE token
self.token_postings
    .entry(text.to_string())     // ← clone String à chaque appel
    .or_default()
    .push((doc_id, ti, byte_from, byte_to));

self.current_value_tokens.push(TokenCapture {
    text: text.to_string(),      // ← 2ème clone pour le gap tracking
    offset_from, offset_to,
});
```

**Structure** : `BTreeMap<String, Vec<(u32, u32, u32, u32)>>`

**Coût** : pour un token "import" présent dans 10 000 docs :
- 10 000 allocations String pour `entry(text.to_string())` (la plupart sont dédupliquées par le BTreeMap mais le String est quand même alloué avant le lookup)
- 10 000 allocations String pour `TokenCapture.text`
- BTreeMap lookup O(log n) à chaque insertion (comparaisons de strings)

**Total segment de 10k docs, 50k tokens uniques, 500k occurrences** : ~1M allocations String inutiles.

### Solution : token interning

```rust
pub struct SfxCollector {
    // Interned tokens : chaque token stocké UNE fois
    token_intern: HashMap<String, u32>,   // token text → ordinal
    token_texts: Vec<String>,             // ordinal → token text
    // Posting entries indexées par ordinal (pas par String)
    token_postings: Vec<Vec<(u32, u32, u32, u32)>>,  // ordinal → entries

    // ... rest unchanged ...
}
```

`add_token` devient :
```rust
pub fn add_token(&mut self, text: &str, offset_from: usize, offset_to: usize) {
    let ordinal = match self.token_intern.get(text) {
        Some(&ord) => ord,
        None => {
            let ord = self.token_texts.len() as u32;
            self.token_intern.insert(text.to_string(), ord);  // UNE allocation
            self.token_texts.push(text.to_string());
            self.token_postings.push(Vec::new());
            ord
        }
    };
    let ti = self.current_value_ti_start + self.current_value_tokens.len() as u32;
    self.token_postings[ordinal as usize].push((self.current_doc_id, ti, offset_from as u32, offset_to as u32));
    // TokenCapture n'a plus besoin du text — juste ordinal + offsets
    self.current_value_tokens.push(TokenCapture { ordinal, offset_from, offset_to });
}
```

**Gain** : pour 500k occurrences → 50k allocations au lieu de 1M. Lookup HashMap O(1) au lieu de BTreeMap O(log n).

**Impact sur build()** : au lieu d'itérer le BTreeMap (déjà trié), on trie les token_texts une fois pour obtenir l'ordre BTreeSet :
```rust
let mut sorted_indices: Vec<u32> = (0..self.token_texts.len() as u32).collect();
sorted_indices.sort_by(|&a, &b| self.token_texts[a as usize].cmp(&self.token_texts[b as usize]));
// sorted_indices[new_ordinal] = old_ordinal
```

## Problème 2 : SuffixFstBuilder accumulation lente

### Code actuel

```rust
// SuffixFstBuilder::add_token — pour chaque token unique
for si in 0..max_si {
    let suffix = &lower[si..];
    self.suffix_to_parents.entry(suffix.to_string()).or_default()  // ← allocation par suffix
        .push(ParentEntry { raw_ordinal, si });
}
```

**Structure** : `BTreeMap<String, Vec<ParentEntry>>`

**Coût** : pour un token de 10 chars → 10 allocations String (une par suffix). BTreeMap insert O(log n) pour chacune. Deduplicate via linear scan `parents.iter().any(...)`.

Pour 50k tokens uniques × avg 8 chars = 400k suffix entries. Chacune avec allocation String + BTreeMap insert.

### Solution : batch suffix generation

Au lieu de `BTreeMap<String, Vec<ParentEntry>>`, accumuler les (suffix_string, ParentEntry) dans un `Vec`, puis trier et grouper :

```rust
pub struct SuffixFstBuilder {
    entries: Vec<(String, ParentEntry)>,  // unsorted, with duplicates
    min_suffix_len: usize,
}

impl SuffixFstBuilder {
    pub fn add_token(&mut self, token: &str, raw_ordinal: u64) {
        let lower = token.to_lowercase();
        let max_si = lower.len().min(MAX_CHUNK_BYTES);
        for si in 0..max_si {
            if !lower.is_char_boundary(si) { continue; }
            let suffix = &lower[si..];
            if si > 0 && suffix.len() < self.min_suffix_len { break; }
            self.entries.push((suffix.to_string(), ParentEntry { raw_ordinal, si: si as u16 }));
        }
    }

    pub fn build(mut self) -> Result<(Vec<u8>, Vec<u8>), Error> {
        // Sort by suffix key → groups identical suffixes together
        self.entries.sort_by(|a, b| a.0.cmp(&b.0));
        // Deduplicate and group
        let mut fst_builder = MapBuilder::memory();
        let mut output_table = OutputTableBuilder::new();
        let mut i = 0;
        while i < self.entries.len() {
            let key = &self.entries[i].0;
            let mut parents = vec![self.entries[i].1.clone()];
            let mut j = i + 1;
            while j < self.entries.len() && self.entries[j].0 == *key {
                let p = &self.entries[j].1;
                if !parents.iter().any(|q| q.raw_ordinal == p.raw_ordinal && q.si == p.si) {
                    parents.push(p.clone());
                }
                j += 1;
            }
            // Insert into FST
            let output = if parents.len() == 1 { ... } else { ... };
            fst_builder.insert(key.as_bytes(), output)?;
            i = j;
        }
        ...
    }
}
```

**Gain** : plus de BTreeMap insert O(log n) pendant add_token. Juste des Vec::push O(1). Le sort unique à la fin est O(n log n) une seule fois. La déduplication est linéaire grâce au tri.

**Allocation** : même nombre de Strings (une par suffix), mais plus de BTreeMap overhead (pas de nœuds d'arbre, pas de rebalancing).

### Variante parallélisable

La génération des suffixes est embarrassingly parallel :
```rust
// Partition tokens en chunks
let chunks: Vec<Vec<(&str, u64)>> = tokens.chunks(1000).collect();
// Chaque thread génère ses entries
let thread_entries: Vec<Vec<(String, ParentEntry)>> = chunks.par_iter()
    .map(|chunk| {
        let mut entries = Vec::new();
        for (token, ordinal) in chunk {
            generate_suffixes(token, ordinal, &mut entries);
        }
        entries
    })
    .collect();
// Merge + sort
let mut all_entries: Vec<_> = thread_entries.into_iter().flatten().collect();
all_entries.sort_by(|a, b| a.0.cmp(&b.0));
```

Gain estimé : 4-8x sur la phase d'accumulation (sur 12 threads). Le sort final reste séquentiel mais c'est O(n log n) une seule fois.

## Problème 3 : double tokenization sans stemmer

Depuis Phase 7c, quand "lucivy_raw" est enregistré et le champ utilise un tokenizer différent, le segment_writer tokenise deux fois :
1. Tokenizer du champ (ex: "default" = SimpleTokenizer + LowerCaser) → inverted index
2. "lucivy_raw" (SimpleTokenizer + CamelCaseSplit + LowerCaser) → SfxCollector

Quand il n'y a pas de stemmer, les deux tokenizers produisent des résultats quasi-identiques (seul CamelCaseSplit diffère). Double coût pour un gain marginal.

### Solution

Quand pas de stemmer, utiliser "lucivy_raw" comme tokenizer du champ principal. Avantages :
- Single tokenization via intercepteur (pas de double pass)
- CamelCaseSplit dans l'inverted index = meilleur pour code search
- Même comportement qu'avant Phase 7c (._raw utilisait "lucivy_raw")

Changement dans `handle.rs` :
```rust
let main_tokenizer = if has_stemmer {
    STEMMED_TOKENIZER
} else {
    RAW_TOKENIZER  // ← au lieu de "default"
};
```

## Priorité d'implémentation

1. **Token interning** — le plus gros gain, le plus simple. ~50 lignes.
2. **Single tokenization sans stemmer** — quick fix dans handle.rs. ~5 lignes.
3. **Batch suffix generation** — gain moyen, remplace BTreeMap par Vec+sort. ~40 lignes.
4. **Parallélisation suffix** — nécessite rayon ou thread pool. Gain 4-8x mais complexité.

## Métriques à suivre

- Temps d'indexation pour 5k docs (bench_contains avec le repo lucivy)
- Nombre d'allocations (via `#[global_allocator]` counting)
- Peak memory du SfxCollector
- Temps de `SfxCollector::build()` vs temps total de commit
