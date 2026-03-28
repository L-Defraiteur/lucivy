# 09 — Inventaire fichiers index SFX + plan term texts

Date : 28 mars 2026

## Fichiers d'index per-field

### 1. `.sfx` — Suffix FST + GapMap + Sibling Table

| | |
|---|---|
| **Pattern** | `{uuid}.{field_id}.sfx` |
| **Format** | Header SFX1 + FST binaire + parent lists + sibling table + GapMap |
| **Builder** | `SfxCollector::build()` → `SuffixFstBuilder` + `GapMapWriter` + `SiblingTableWriter` |
| **Reader** | `SfxFileReader::open()` |
| **Segment creation** | `segment_serializer.write_sfx(field_id, bytes)` |
| **Merge** | `sfx_merge::build_fst()` + `copy_gapmap()` + `merge_sibling_links()` + `write_sfx()` |
| **GC** | `SegmentComponent::SuffixFst { field_id }` dans `all_components()` |
| **Queries** | `prefix_walk`, `falling_walk`, `fuzzy_walk`, cross-token via sibling links |

### 2. `.sfxpost` — Posting Index (ordinal → doc entries)

| | |
|---|---|
| **Pattern** | `{uuid}.{field_id}.sfxpost` |
| **Format** | SFP2 header + offset table + entries par ordinal (doc_ids + VInt payloads) |
| **Builder** | `SfxPostWriterV2::new(n).add_entry(ord, doc, ti, bf, bt).finish()` |
| **Reader** | `SfxPostReaderV2::open_slice()` |
| **Segment creation** | `segment_serializer.write_sfxpost(field_id, bytes)` |
| **Merge** | `sfx_merge::merge_sfxpost()` (remap doc_ids + ordinals) |
| **GC** | `SegmentComponent::SuffixPost { field_id }` |
| **Queries** | `resolve()` dans le posting resolver, `find_literal()` |

### 3. `.posmap` — Position → Ordinal

| | |
|---|---|
| **Pattern** | `{uuid}.{field_id}.posmap` |
| **Format** | PMAP header + offset table u64 + `u32[num_tokens]` par doc |
| **Builder** | `PosMapWriter::new().add(doc, pos, ord).serialize()` |
| **Reader** | `PosMapReader::open()` |
| **Segment creation** | `segment_serializer.write_posmap(field_id, bytes)` |
| **Merge** | Rebuild depuis sfxpost mergé (boucle N-way) |
| **GC** | `SegmentComponent::PosMap { field_id }` |
| **Queries** | `validate_path()`, fuzzy cross-token DFA validation |

### 4. `.bytemap` — Byte Presence Bitmap

| | |
|---|---|
| **Pattern** | `{uuid}.{field_id}.bytemap` |
| **Format** | BMAP header + `[u8; 32]` × num_ordinals (256-bit bitmap par ordinal) |
| **Builder** | `ByteBitmapWriter::new().record_token(ord, bytes).serialize()` |
| **Reader** | `ByteBitmapReader::open()` |
| **Segment creation** | `segment_serializer.write_bytemap(field_id, bytes)` |
| **Merge** | `copy_bitmap()` depuis sources ou `record_token()` fallback |
| **GC** | `SegmentComponent::ByteMap { field_id }` |
| **Queries** | Regex pre-filter (bytes absents → skip DFA) |

### 5. GapMap (interne au `.sfx`)

Pas un fichier séparé. Stocké dans la section D du `.sfx`.
Contient les bytes séparateurs entre tokens par document.

### 6. Term Texts — LE PROBLEME

**Aucun fichier dédié.** Tous les lookups `ordinal → texte` passent par
le term dictionary de tantivy via `ord_to_term(ordinal)`.

**MAIS les ordinals SFX ≠ ordinals term dict** (doc 08).

Le SfxCollector construit ses ordinals indépendamment du postings writer.
Les deux trient alphabétiquement, mais n'ont pas forcément les mêmes
tokens (le CamelCaseSplitFilter peut produire des tokens différents du
term dict, les long tokens > MAX_TOKEN_LEN sont skippés, etc.).

## Plan : nouveau fichier `.termtexts`

### Concept

Un fichier séparé (PAS dans le .sfx) qui stocke les textes des tokens
indexés par ordinal SFX. Format simple, lecture directe.

### Format proposé

```
[4 bytes] magic "TTXT"
[4 bytes] num_terms: u32 LE
[4 bytes × (num_terms + 1)] offset table: u32 LE (byte offset dans data)
[data] textes concaténés, UTF-8
```

Lookup O(1) : `text(ordinal) = data[offsets[ordinal]..offsets[ordinal+1]]`

### Naming

`{uuid}.{field_id}.termtexts`

### SegmentComponent

```rust
SegmentComponent::TermTexts { field_id }
```

Ajouté dans `all_components()` → GC protège le fichier.

### Builder

```rust
pub struct TermTextsWriter {
    texts: Vec<String>,  // index = ordinal
}
impl TermTextsWriter {
    fn add(&mut self, ordinal: u32, text: &str);
    fn serialize(&self) -> Vec<u8>;
}
```

Construit pendant `SfxCollector::build()` en même temps que posmap/bytemap.

### Reader

```rust
pub struct TermTextsReader<'a> {
    num_terms: u32,
    offsets: &'a [u8],
    data: &'a [u8],
}
impl TermTextsReader<'_> {
    fn open(bytes: &[u8]) -> Option<Self>;
    fn text(&self, ordinal: u32) -> Option<&str>;
    fn num_terms(&self) -> u32;
}
```

### Segment creation

```rust
// Dans SfxCollector::build()
let mut termtexts_writer = TermTextsWriter::new();
for (new_ordinal, &old_ord) in sorted_indices.iter().enumerate() {
    termtexts_writer.add(new_ordinal as u32, &self.token_texts[old_ord as usize]);
}
output.termtexts = termtexts_writer.serialize();

// Dans segment_writer
segment_serializer.write_termtexts(field_id, &output.termtexts)?;
```

### Merge

```rust
// Dans merge_sfx_deferred ou merge_sfx_legacy
let mut termtexts_writer = TermTextsWriter::new();
// Dans la boucle N-way qui itère les tokens triés :
termtexts_writer.add(new_ord, &current_key_str);
// Après la boucle :
serializer.write_termtexts(field_id, &termtexts_writer.serialize())?;
```

### Segment reader

```rust
// Dans load_sfx_files() de segment_reader.rs
let mut termtexts_files = FnvHashMap::default();
// ...
if let Ok(file_slice) = segment.open_read_custom(&format!("{field_id}.termtexts")) {
    termtexts_files.insert(field, file_slice);
}
```

### Migration des queries

Remplacer TOUS les `ord_to_term` callbacks par `sfx_reader.term_text(ord)`
ou un `TermTextsReader` dédié.

| Avant | Après |
|-------|-------|
| `ord_to_term(sfx_ordinal)` | `termtexts_reader.text(sfx_ordinal)` |
| `term_dict.ord_to_term(ord, &mut buf)` | `termtexts_reader.text(ord)` |

**Fonctions à migrer** (de doc 08) :
- `cross_token_search_with_terms` (suffix_contains.rs)
- `find_literal` (literal_resolve.rs)
- `validate_path` (literal_resolve.rs)
- `fuzzy_contains_via_trigram` (regex_continuation_query.rs)
- `regex_contains_via_literal` (regex_continuation_query.rs)
- `run_sfx_walk` (suffix_contains_query.rs)
- `run_regex_prescan` (regex_continuation_query.rs)
- `run_fuzzy_prescan` (regex_continuation_query.rs)

## Plan : queries REQUIERENT les index features

### Concept

Chaque query déclare les fichiers d'index dont elle a besoin. Si un
fichier manque → **erreur explicite, pas de fallback silencieux**.

### Interface proposée

```rust
pub enum IndexFeature {
    SuffixFst,
    SuffixPost,
    PosMap,
    ByteMap,
    TermTexts,
    SiblingTable,  // dans le .sfx mais peut être absent
}

// Sur le Weight ou le Query :
fn required_features(&self) -> Vec<IndexFeature>;

// Dans le scorer :
fn scorer(&self, reader: &SegmentReader, boost: Score) -> Result<Box<dyn Scorer>> {
    for feature in self.required_features() {
        if !reader.has_feature(feature, self.field) {
            return Err(LucivyError::MissingIndexFeature {
                feature,
                segment: reader.segment_id(),
                field: self.field,
            });
        }
    }
    // ... build scorer
}
```

### Mapping queries → features requises

| Query type | Features requises |
|-----------|-------------------|
| `contains` (single token) | SuffixFst, SuffixPost |
| `contains` (cross-token) | SuffixFst, SuffixPost, TermTexts, SiblingTable |
| `startsWith` | SuffixFst, SuffixPost |
| `contains` fuzzy d>0 | SuffixFst, SuffixPost, TermTexts, PosMap |
| `contains` regex | SuffixFst, SuffixPost, TermTexts, PosMap |
| `term`, `phrase`, `fuzzy` (top-level) | (aucun SFX, term dict standard) |

### Avantages

1. **Pas de résultats silencieusement incomplets** — crash explicite si
   l'index ne supporte pas la query
2. **Messages d'erreur clairs** — "Missing TermTexts for field 'content'
   in segment abc123 — rebuild index"
3. **Documentation implicite** des dépendances query → index
4. **Forward-compatible** — ajouter un nouveau fichier d'index ne casse pas
   les anciennes queries (elles ne le requièrent pas)

## Ordre d'implémentation

1. **TermTextsWriter/Reader** — nouveau module `src/suffix_fst/termtexts.rs`
2. **SfxBuildOutput.termtexts** — ajout dans le collector
3. **segment_serializer.write_termtexts** — écriture
4. **SegmentComponent::TermTexts** — GC protection
5. **segment_reader** — chargement
6. **Migration queries** — remplacer ord_to_term par termtexts_reader
7. **Merge** — écriture pendant merge
8. **IndexFeature** — trait + validation au scorer time
9. **Supprimer les fallbacks silencieux** — crash si feature manquante
