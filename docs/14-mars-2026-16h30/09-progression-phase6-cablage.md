# Progression : PHASE-6 câblage + SuffixContainsQuery

Date : 14 mars 2026 — session 3

## Ce qui a été fait

### 1. resolve_raw_ordinal — lecture des vraies posting lists

Nouvelles fonctions dans `suffix_contains.rs` :

```rust
pub fn resolve_raw_ordinal(inv_idx: &InvertedIndexReader, ordinal: u64) -> Vec<RawPostingEntry>
pub fn make_raw_resolver(inv_idx: Arc<InvertedIndexReader>) -> impl Fn(u64) -> Vec<RawPostingEntry>
```

Flow : ordinal → `TermDictionary::term_info_from_ord()` → `TermInfo`
→ `InvertedIndexReader::read_postings_from_terminfo(WithFreqsAndPositionsAndOffsets)`
→ iterate `(doc_id, position, byte_from, byte_to)`

**Décommenté** `TermDictionary::term_info_from_ord()` dans `termdict/mod.rs`
(était commenté "not used" — nécessaire pour le lookup par ordinal).

**Test intégration** : `test_resolve_raw_ordinal_real_index` — vrai Index RAM,
2 documents, vérifie positions et byte offsets exacts. Passe.

### 2. SuffixContainsQuery — query autonome

Créé `suffix_contains_query.rs` — query standalone qui implémente
`Query` / `Weight` / `Scorer`. Indépendant de `NgramContainsQuery`.

- Si le .sfx existe → search direct, zéro stored text
- Si le .sfx n'existe pas → erreur claire (pas de fallback silencieux)
- Highlights via `HighlightSink::insert()` avec byte offsets exacts
- BM25 scoring via `FieldNormReader`

Le ngram path a d'abord été touché (fast path dans le scorer) puis **restoré**
via `git checkout` — les deux paths sont séparés.

### 3. Tests Unicode E2E

**8 tests dans `suffix_contains.rs`** (`test_e2e_unicode_characters`) :
- `résumé` (accents français, é = 2 bytes) ✓
- `café` (5 bytes, offset correct après résumé) ✓
- `françois` (ç = 2 bytes, lowercased) ✓
- `東京タワー` (CJK, 5 chars × 3 bytes = 15) ✓
- `hello` après CJK (offset 16) ✓
- `世界` (6 bytes) ✓
- `rust🦀lang` (emoji 🦀 = 4 bytes) ✓
- `brûlée` (û + é = 2+2 bytes) ✓

**Zéro concession.** Tous les byte offsets sont exacts sur full UTF-8.

### 4. SegmentReader.sfx_file() — pré-chargement .sfx

Ajouté `sfx_files: FnvHashMap<Field, FileSlice>` au `SegmentReader`.
Fonction `load_sfx_files()` au `open()` scanne les champs `._raw` et
essaie d'ouvrir leur .sfx.

### 5. Tests SuffixContainsQuery — 9 tests E2E

Tests avec vrai Index, vrais documents (accents, CJK, emoji), highlights :
- `test_suffix_query_exact_ascii` — "rag3db" dans doc ASCII
- `test_suffix_query_substring` — "g3db" comme sous-chaîne
- `test_suffix_query_french_accents` — "café"
- `test_suffix_query_cjk` — "世界"
- `test_suffix_query_emoji` — "rust🦀lang"
- `test_suffix_query_no_match` — "nonexistent"
- `test_suffix_query_highlights_cafe` — byte offsets [9, 14]
- `test_suffix_query_highlights_substring_unicode` — "afé" à SI=1
- `test_suffix_query_highlights_brûlée` — byte offsets [20, 28]

## Bug découvert : ManagedDirectory + Footer

### Le problème

Les 9 tests `SuffixContainsQuery` **échouent** avec :
```
Files does not exist: "00000000000000000000000000000000.0.sfx"
```

Le fichier est bien écrit (726 bytes) par le `SegmentWriter`, mais le
`SegmentReader` ne le trouve pas.

### Cause racine

Le `ManagedDirectory` (wrapper utilisé par Index) :

**Écriture** (`open_write`, ligne 298-307 de `managed_directory.rs`) :
```rust
// Wrappe le writer dans un FooterProxy qui AJOUTE un footer au fichier
Ok(io::BufWriter::new(Box::new(FooterProxy::new(
    self.directory.open_write(path)?.into_inner()...
))))
```

**Lecture** (`open_read`, ligne 290-296) :
```rust
// EXTRAIT le footer du fichier avant de retourner les données
let (footer, reader) = Footer::extract_footer(file_slice)?;
footer.is_compatible()?;
Ok(reader)
```

Le `open_write_custom` dans `segment_serializer.rs` passe par le
`ManagedDirectory`, donc le fichier .sfx est écrit AVEC un footer lucivy
ajouté automatiquement. Mais le `SfxFileReader::open()` ne connaît pas ce
footer — il cherche le magic "SFX1" au début du fichier, pas après un
footer.

En fait, le `open_read` extrait le footer et retourne les données sans
footer. Mais l'erreur est "file does not exist", pas "invalid magic".
Le fichier est probablement bien écrit mais le `RamDirectory` ne le retrouve
pas après le commit (l'index fait un reload qui crée un nouveau snapshot).

**Investigation complémentaire nécessaire** : le fichier pourrait ne pas
survivre au cycle write → commit → reader reload. Le `RamDirectory` est
`Arc`-shared mais les fichiers non-managed pourraient être garbage-collectés.

## Plan d'action : refactor SegmentComponent

### Problème actuel

`open_write_custom` / `open_read_custom` contournent le système de
composants (SegmentComponent) et passent directement par le directory.
C'est fragile : le ManagedDirectory ajoute des footers, le garbage collector
peut supprimer les fichiers custom, et les fichiers ne sont pas déclarés
dans les segment metas.

### Solution : utiliser SegmentComponent comme les autres données

Les postings, positions, offsets, terms — tous utilisent `SegmentComponent` :
- Le `Segment` sait quels fichiers existent pour chaque composant
- Le `ManagedDirectory` gère le footer automatiquement
- Le garbage collector préserve les fichiers déclarés dans les metas
- Le merger sait quels fichiers copier

### Approche : un SegmentComponent par champ ._raw

On a déjà `SegmentComponent::SuffixFst` dans le enum. Mais un seul composant
pour plusieurs champs ne suffit pas. Deux options :

**Option A — Un composant par champ (recommandé)**

Étendre `SegmentComponent` pour supporter des composants paramétrés par
field_id. Le `relative_path()` retournerait `"{field_id}.sfx"`.

```rust
enum SegmentComponent {
    // ... existants ...
    SuffixFst,  // → on change pour SuffixFst(u32) avec le field_id
}
```

Avantage : chaque champ a son propre fichier, chargeable indépendamment.
Le `SegmentReader` ne charge que les .sfx des champs utilisés par la query.

Inconvénient : le enum devient paramétré, ce qui change l'API de `iterator()`.

**Option B — CompositeFile comme les postings**

Un seul fichier `.sfx` par segment, contenant les données de tous les champs
`._raw` dans un `CompositeFile` indexé par field_id.

```rust
// Écriture
let sfx_file = segment.open_write(SegmentComponent::SuffixFst);
let mut composite = CompositeFileWriter::new(sfx_file);
composite.write_field(field_id_0, &sfx_bytes_0);
composite.write_field(field_id_1, &sfx_bytes_1);
composite.close();

// Lecture
let sfx_file = segment.open_read(SegmentComponent::SuffixFst);
let composite = CompositeFile::open(&sfx_file);
let field_0_data = composite.open_read(field_id_0);
```

Avantage : un seul fichier, pattern identique aux postings/positions/offsets.
Pas besoin de modifier l'enum.

Inconvénient : charge le header du composite même si on n'utilise qu'un
champ. Mais c'est négligeable (le header est petit).

### Recommandation : Option B (CompositeFile)

C'est le même pattern que les postings, positions, offsets. Le code est déjà
là, testé, et gère le footer du ManagedDirectory automatiquement.

Le `SegmentComponent::SuffixFst` existe déjà. Il suffit de :

1. **Écriture** : dans `segment_writer.rs`, collecter tous les .sfx bytes
   par field, puis écrire un seul `CompositeFile` pour `SuffixFst`
2. **Lecture** : dans `segment_reader.rs`, ouvrir le `CompositeFile` pour
   `SuffixFst`, puis extraire les données par field_id
3. **Supprimer** `open_write_custom` / `open_read_custom` et le champ
   `sfx_files` du `SegmentReader` — remplacer par un `sfx_composite`

### Fichiers à modifier

```
src/indexer/segment_writer.rs      — écrire CompositeFile au lieu de write_sfx
src/indexer/segment_serializer.rs  — retirer write_sfx, ajouter sfx composite
src/index/segment_reader.rs        — charger sfx_composite au open()
src/index/segment.rs               — retirer open_write_custom/open_read_custom
src/query/phrase_query/suffix_contains_query.rs — lire depuis sfx_composite
```

### Impact

- Les 9 tests `SuffixContainsQuery` devraient passer
- Le footer est géré automatiquement
- Le garbage collector est happy
- Le merger pourra copier le .sfx comme les autres composants (PHASE-8)
- Pas besoin de supprimer les méthodes custom immédiatement, mais elles
  deviennent inutilisées pour le .sfx

## État des fichiers

### Modifiés (non committé)
```
src/query/phrase_query/suffix_contains.rs      — resolve_raw_ordinal + test Unicode
src/query/phrase_query/suffix_contains_query.rs — NOUVEAU : SuffixContainsQuery
src/query/phrase_query/mod.rs                   — +suffix_contains_query module
src/index/segment_reader.rs                     — sfx_files + load_sfx_files (à refactorer)
src/termdict/mod.rs                             — décommenté term_info_from_ord
src/indexer/segment_writer.rs                   — debug eprintln (à retirer)
```

### Debug temporaire à retirer
```
src/index/segment_reader.rs    — eprintln dans load_sfx_files
src/indexer/segment_writer.rs  — eprintln dans finalize
```

## Pour reprendre

1. Retirer les eprintln de debug
2. Implémenter Option B (CompositeFile pour SuffixFst)
   - Modifier segment_serializer pour écrire via CompositeFile
   - Modifier segment_reader pour lire via CompositeFile
   - Modifier suffix_contains_query pour lire depuis le composite
3. Les 9 tests SuffixContainsQuery devraient passer
4. Committer tout (PHASE-6 complète)
5. PHASE-5 : ajouter fuzzy d>0 (~20 lignes)
