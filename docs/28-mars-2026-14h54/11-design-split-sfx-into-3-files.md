# 11 — Design : split .sfx en 3 fichiers séparés

Date : 29 mars 2026

## Problème

Le `.sfx` est un fichier composite qui contient 4 sections :
1. Suffix FST (trie compressé des suffixes)
2. Parent lists (ordinal → entries)
3. Sibling table (graphe d'adjacence token→successeur)
4. GapMap (bytes séparateurs entre tokens par doc)

Ces 4 sections ont des cycles de vie différents :
- FST + parents : rebuilt ensemble, couplés par les ordinals
- GapMap : copié doc par doc pendant le merge (pas de rebuild)
- Sibling table : remappé pendant le merge, peut être absent

Les mettre dans le même fichier complique le merge (le deferred merge
devait écrire un "partial .sfx" avec FST vide), le GC, et l'ajout de
nouvelles features.

## Split proposé

### 1. `.sfx` → FST + parent lists uniquement

Le cœur. Contient :
- Header (magic, version, num_terms)
- Section A : Suffix FST binaire
- Section B : Parent lists

Le `SfxFileWriter`/`SfxFileReader` restent mais sans gapmap ni sibling.
`SfxFileWriter::new(fst, parents, num_terms)` — plus de gapmap/num_docs.

### 2. `.gapmap` → fichier séparé

Nouveau fichier standalone. Format inchangé (le GapMapWriter/Reader
sérialisent déjà indépendamment).

```rust
pub struct GapMapIndex;
impl SfxIndexFile for GapMapIndex {
    fn id(&self) -> &'static str { "gapmap" }
    fn extension(&self) -> &'static str { "gapmap" }
    // build: sérialiser gapmap_writer depuis le collector
    // merge: copier doc_data dans l'ordre du doc_mapping
}
```

### 3. `.sibling` → fichier séparé

Nouveau fichier standalone. Format inchangé (SiblingTableWriter/Reader
sérialisent déjà indépendamment).

```rust
pub struct SiblingIndex;
impl SfxIndexFile for SiblingIndex {
    fn id(&self) -> &'static str { "sibling" }
    fn extension(&self) -> &'static str { "sibling" }
    // build: sérialiser sibling_writer depuis le collector
    // merge: remap ordinals depuis les sources
}
```

## Impact sur le code

### SfxFileWriter/Reader (file.rs)

**Avant** :
```rust
SfxFileWriter::new(fst, parents, gapmap, num_docs, num_terms)
    .with_sibling_data(sibling)
    .to_bytes()
```

**Après** :
```rust
SfxFileWriter::new(fst, parents, num_terms).to_bytes()
// gapmap et sibling sont écrits séparément via le registry
```

Le `SfxFileReader` perd :
- `fn gapmap() → &GapMapReader` (déplacé vers segment_reader)
- `fn sibling_table() → Option<&SiblingTableReader>` (idem)

### SfxCollector (collector.rs)

Le collector construit toujours tout (gapmap, sibling, sfx).
Mais au lieu de tout assembler dans un seul SfxBuildOutput.sfx,
il met le gapmap et sibling comme registry_files séparés.

### segment_reader.rs

Les accès changent :
```rust
// AVANT
let gapmap = sfx_reader.gapmap();
let sibling = sfx_reader.sibling_table();

// APRÈS
let gapmap_bytes = reader.sfx_index_file("gapmap", field)?.read_bytes()?;
let gapmap = GapMapReader::open(&gapmap_bytes);
let sibling_bytes = reader.sfx_index_file("sibling", field)?.read_bytes()?;
let sibling = SiblingTableReader::open(&sibling_bytes);
```

### Fonctions impactées

| Fichier | Accès à changer |
|---------|----------------|
| suffix_contains.rs | `sfx_reader.gapmap()` → gapmap séparé |
| suffix_contains.rs | `sfx_reader.sibling_table()` → sibling séparé |
| literal_resolve.rs | `sfx_reader.gapmap()` → gapmap séparé |
| regex_continuation_query.rs | `sfx_reader.gapmap()` → gapmap séparé |
| merger.rs | `sfx_reader.gapmap().doc_data()` → gapmap séparé |
| merger.rs | `sfx_reader.sibling_table()` → sibling séparé |
| sfx_merge.rs | idem |

### SfxBuildContext

Étendu pour passer les données gapmap/sibling au registry :

```rust
pub struct SfxBuildContext<'a> {
    pub token_texts: &'a [&'a str],
    pub token_postings: &'a [&'a [(u32, u32, u32, u32)]],
    pub num_docs: u32,
    // Données pré-construites par le collector
    pub gapmap_data: Option<&'a [u8]>,
    pub sibling_data: Option<&'a [u8]>,
}
```

### SfxMergeContext

Étendu :

```rust
pub struct SfxMergeContext<'a> {
    pub merged_terms: &'a [(u32, &'a str)],
    pub ordinal_maps: &'a [HashMap<u32, u32>],
    pub reverse_doc_map: &'a [HashMap<DocId, DocId>],
    pub sfxpost_readers: &'a [...],
    pub doc_mapping: &'a [DocAddress],     // pour gapmap copy
    pub source_gapmaps: &'a [Option<&'a [u8]>],  // gapmap sources
    pub source_siblings: &'a [Option<&'a [u8]>],  // sibling sources
}
```

## Ordre d'implémentation

1. Étendre SfxBuildContext + SfxMergeContext avec gapmap/sibling fields
2. Créer GapMapIndex + SiblingIndex (impl SfxIndexFile)
3. Modifier SfxCollector::build() pour ne plus assembler dans .sfx
4. Modifier SfxFileWriter/Reader pour enlever gapmap/sibling
5. Modifier toutes les fonctions qui accèdent gapmap/sibling via sfx_reader
6. Ajouter au registry
7. Tester
