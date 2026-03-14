# Design : fichier `.gaps` — séparateurs tokenizer en binaire mmap'd

Date : 14 mars 2026

## Contexte

Le bottleneck de la recherche contains est la lecture du stored text via `store_reader.get(doc_id)` :
décompression LZ4 d'un bloc de ~16KB + re-tokenisation du texte entier pour chaque candidat.

Sur 5201 docs (clone rag3db), le profil typique :
- contains 'rag3db' d=0 : ~300ms (500 candidats × décompression LZ4 + tokenize_raw)
- contains 'rag3db' d=1 : ~1400ms (idem + edit_distance × 5000 tokens par doc)

Le posting list du raw field contient déjà :
- Les termes (dictionnaire FST)
- Les doc_ids par terme
- Les positions de chaque terme dans chaque doc
- Les byte offsets (début/fin) de chaque occurrence

Ce qui manque : les **caractères entre les tokens** (séparateurs). Sans eux, impossible de :
- Valider les séparateurs entre tokens consécutifs ("rag3db main" → espace entre les deux ?)
- Valider le prefix/suffix (caractères avant/après le match)
- Reconstruire le texte original pour highlights (on a les offsets mais pas le contenu inter-token)

## Objectif

Stocker les séparateurs dans un fichier binaire `.gaps` par segment, mmap'd, pour éliminer
complètement le stored text du chemin de recherche contains pour les tokens FST-confirmés.

## Format binaire

```
┌─────────────────────────────────────────────────┐
│ Header                                          │
│   magic: [u8; 4] = b"GAPS"                     │
│   version: u8 = 1                              │
│   num_docs: u32 (little-endian)                 │
├─────────────────────────────────────────────────┤
│ Offset table                                    │
│   offsets: [u64; num_docs + 1] (little-endian)  │
│   offset[i] = position dans le fichier où       │
│   commencent les données du doc i               │
│   offset[num_docs] = fin des données            │
├─────────────────────────────────────────────────┤
│ Data (concaténé, un bloc par doc)               │
│                                                 │
│   Doc i :                                       │
│     num_tokens: u16 (little-endian)             │
│     gap_0: [len: u8][bytes...]  // avant token 0 (prefix du doc) │
│     gap_1: [len: u8][bytes...]  // entre token 0 et token 1      │
│     gap_2: [len: u8][bytes...]  // entre token 1 et token 2      │
│     ...                                         │
│     gap_N: [len: u8][bytes...]  // après token N-1 (suffix du doc)│
│                                                 │
│   Total gaps par doc = num_tokens + 1           │
│   (prefix + N-1 séparateurs + suffix)           │
└─────────────────────────────────────────────────┘
```

### Encoding des gaps

Format simple pour v1 : `[len: u8][bytes...]`
- len = 0 : pas de séparateur (tokens collés)
- len = 1..254 : séparateur de 1 à 254 bytes
- len = 255 : extended length, suivi de [len_ext: u16 LE][bytes...] (pour les rares gaps > 254 bytes)

**Optimisation future possible** : single-byte encoding. Si bit haut de `len` est 0 (valeur 0-127),
c'est directement le byte du séparateur (pas de longueur séparée). Divise par 2 la taille pour
les gaps d'un seul byte (espaces, newlines, tabs = 90%+ des cas en code source).
Pas prioritaire pour v1.

### Taille estimée

Pour un fichier code source typique de 50KB avec 5000 tokens :
- Stored text : ~50KB (compressé LZ4 dans un bloc de ~16KB)
- Gap map : ~6-8KB (5001 gaps × ~1.3 bytes moyen)

Sur 5201 docs : ~35-40MB de gaps vs ~80MB de stored text. Le gaps est plus petit ET mmap'd
(zéro décompression).

## Accès à la recherche

### Lecture d'un gap

```rust
fn read_gap(mmap: &[u8], doc_id: DocId) -> &[u8] {
    let offset_start = HEADER_SIZE + (doc_id as usize) * 8;
    let doc_offset = u64::from_le_bytes(mmap[offset_start..offset_start+8]) as usize;
    let next_offset = u64::from_le_bytes(mmap[offset_start+8..offset_start+16]) as usize;
    &mmap[doc_offset..next_offset]
}
```

### Lecture d'un séparateur à une position

```rust
fn read_separator_at(gap_data: &[u8], position: u32) -> &[u8] {
    // Skip num_tokens (u16)
    let mut cursor = 2;
    // Skip gaps 0..position (position+1 gaps à skipper pour arriver au gap après token `position`)
    // gap_index = position + 1 pour le séparateur APRÈS le token à `position`
    // gap_index = position     pour le séparateur AVANT le token à `position`
    for _ in 0..target_gap_index {
        let len = gap_data[cursor] as usize;
        cursor += 1;
        if len == 255 {
            let ext_len = u16::from_le_bytes([gap_data[cursor], gap_data[cursor+1]]) as usize;
            cursor += 2 + ext_len;
        } else {
            cursor += len;
        }
    }
    // Lire le gap courant
    let len = gap_data[cursor] as usize;
    if len == 255 { ... } else { &gap_data[cursor+1..cursor+1+len] }
}
```

Note : l'accès séquentiel aux gaps est O(position) car il faut skipper les gaps précédents.
Pour un accès O(1), on pourrait ajouter une sous-table d'offsets intra-doc, mais c'est de
l'optimisation prématurée — les positions cherchées sont généralement petites (< 100).

## Chemins de recherche avec `.gaps`

### Cas 1 : Single token, FST exact (d=0)

```
Avant :  FST lookup → doc_ids → store_reader.get() → tokenize_raw() → match
Après :  FST lookup → doc_ids → MATCH (rien d'autre à faire)
```

Pas besoin du .gaps : si le FST a le terme, le doc le contient comme token entier.
Prefix/suffix validation : position > 0 → il y a forcément un séparateur avant (le tokenizer
split sur les non-alnum). Mais pour vérifier quel séparateur exactement (strict mode) → .gaps.

### Cas 2 : Single token, FST fuzzy (d>0)

```
Avant :  FST fuzzy walk → termes proches → posting lists → doc_ids
         → store_reader.get() → tokenize_raw() → edit_distance × 5000
Après :  FST fuzzy walk → termes proches → MATCH
         (le FST a déjà confirmé la distance, pas besoin de re-vérifier)
```

Le gain est énorme ici : on élimine 100% des lectures stored text.

### Cas 3 : Multi token, FST exact, avec séparateurs

```
Avant :  posting list intersection → position match → store_reader.get()
         → validate_separators() (lit le texte entre les tokens)
Après :  posting list intersection → position match → gaps_reader.read_separator_at()
         → validate (mmap, zéro décompression)
```

### Cas 4 : Highlights

```
Avant :  store_reader.get() → re-tokenize → byte offsets
Après :  posting list WithOffsets → byte offsets directement
         (déjà disponible, pas besoin du .gaps pour ça)
```

Les offsets (byte_from, byte_to) de chaque token sont dans le posting list.
Le .gaps n'est nécessaire que si on veut reconstruire le texte original.

### Cas 5 : Substring trigram (non-FST)

```
Avant :  trigram candidates → store_reader.get() → tokenize_raw() → substring match
Après :  INCHANGÉ — les trigrams donnent des candidats pour des substrings,
         pas des tokens entiers. Le .gaps ne contient pas le texte des tokens,
         seulement ce qu'il y a entre eux.
```

**Question ouverte** : pour les substrings, on pourrait stocker aussi les tokens dans le .gaps
(pas seulement les séparateurs). Ça transformerait le .gaps en "token map" complet :
`[token_0][gap_0][token_1][gap_1]...`. On pourrait reconstruire le texte entier sans stored text.
Mais ça doublerait la taille du .gaps (~50KB par doc au lieu de ~8KB). À évaluer.

**Alternative pour substrings** : résoudre le trigram vers le terme raw correspondant via le FST,
puis utiliser le posting list. Ex: trigram "rag" matche dans le doc → FST walk pour trouver
quel terme du doc contient "rag" comme substring → "rag3db" → posting list confirme.
Complexité : O(termes × len) pour le FST walk. À benchmarker.

## Écriture à l'indexation

### Où se brancher

1. **`src/indexer/segment_writer.rs`** — `index_document()` (ligne ~170)
   - Après la tokenisation du champ, on a accès au flux de tokens avec leurs offsets.
   - Accumuler les gaps entre tokens consécutifs : `text[token_i.offset_to..token_{i+1}.offset_from]`.
   - Écrire les gaps dans un buffer par document.

2. **`src/indexer/segment_serializer.rs`** — `for_segment()`
   - Ouvrir le `WritePtr` pour le `.gaps` : `segment.open_write(SegmentComponent::Gaps)`.
   - Passer le writer au `GapsWriter`.

3. **`src/indexer/segment_serializer.rs`** — `close()`
   - Finaliser le `GapsWriter` (écrire le header + offset table).

### GapsWriter

```rust
struct GapsWriter {
    writer: WritePtr,
    // Buffer des données gaps par doc (concaténé)
    data_buffer: Vec<u8>,
    // Offset dans data_buffer pour chaque doc
    doc_offsets: Vec<u64>,
}

impl GapsWriter {
    fn new(writer: WritePtr) -> Self { ... }

    /// Appelé une fois par document, après tokenisation du champ texte.
    /// `gaps` = vec des séparateurs entre tokens (prefix, sep_0, sep_1, ..., suffix)
    fn add_doc_gaps(&mut self, gaps: &[&str]) {
        self.doc_offsets.push(self.data_buffer.len() as u64);
        let num_tokens = if gaps.is_empty() { 0u16 } else { (gaps.len() - 1) as u16 };
        self.data_buffer.extend_from_slice(&num_tokens.to_le_bytes());
        for gap in gaps {
            encode_gap(&mut self.data_buffer, gap.as_bytes());
        }
    }

    /// Écrit le fichier final : header + offset table + data.
    fn close(mut self) -> io::Result<()> {
        let num_docs = self.doc_offsets.len() as u32;
        // Header
        self.writer.write_all(b"GAPS")?;
        self.writer.write_all(&[1u8])?; // version
        self.writer.write_all(&num_docs.to_le_bytes())?;
        // Calculer les offsets absolus (header + offset_table + data_offset)
        let header_size = 4 + 1 + 4; // magic + version + num_docs
        let offset_table_size = (num_docs as u64 + 1) * 8;
        let data_start = header_size as u64 + offset_table_size;
        for &offset in &self.doc_offsets {
            self.writer.write_all(&(data_start + offset).to_le_bytes())?;
        }
        // Sentinelle finale
        self.writer.write_all(&(data_start + self.data_buffer.len() as u64).to_le_bytes())?;
        // Data
        self.writer.write_all(&self.data_buffer)?;
        self.writer.terminate()
    }
}
```

### Quels champs ?

Le `.gaps` est écrit pour les champs qui ont un `._raw` field (champs text et string avec
ngram). Le segment writer doit savoir quel champ est le "source" pour capturer les gaps.

Option : un `.gaps` par champ raw field, nommé `{segment_uuid}.{field_id}.gaps`.
Ou un seul `.gaps` avec un header multi-champ. Pour v1, un par champ est plus simple.

## Merge

Le merger lit les `.gaps` des segments source et les concatène dans le segment merged :

1. Pour chaque segment source, mmap le `.gaps`
2. Pour chaque doc (dans l'ordre du mapping) :
   - Lire les données du doc dans le `.gaps` source
   - Copier vers le `.gaps` merged (données identiques, seul le doc_id change)
3. Recalculer la table d'offsets

C'est le même pattern que le store merge (`write_storable_fields` dans `merger.rs`) mais
plus simple car pas de recompression — c'est du memcpy.

## Points d'attention

### Champs multiples

Un document peut avoir plusieurs champs texte. Chaque champ a sa propre tokenisation et
donc ses propres gaps. Le `.gaps` doit être par champ ou contenir un multiplexage.

### Documents sans champ texte

Un doc qui n'a pas le champ texte concerné doit quand même avoir une entrée dans le `.gaps`
(données vides : `num_tokens = 0`, zéro gaps).

### Compatibilité

Les index existants n'ont pas de `.gaps`. Le reader doit gérer l'absence du fichier :
si pas de `.gaps`, fallback sur le stored text (comportement actuel).

### WASM

En WASM, pas de vrai mmap mais le fichier est chargé en mémoire (comme FST, postings).
Le `MemoryDirectory` / `BlobDirectory` retournent des `FileSlice` qui sont des `&[u8]` —
même interface que mmap.

## Priorités d'implémentation

1. **GapsWriter** : écriture du `.gaps` à l'indexation
2. **GapsReader** : lecture mmap'd à la recherche
3. **NgramContainsWeight::scorer()** : utiliser les gaps au lieu du stored text pour les
   candidats FST-confirmés
4. **Merger** : concaténation des `.gaps`
5. **ContainsScorer** : utiliser les gaps pour la validation séparateurs
6. **Optimisation encoding** : single-byte pour les gaps courts

## Impact estimé

- **contains d=0 FST-exact** : ~0ms (au lieu de ~300ms) — le FST confirme, pas de vérification
- **contains d=1 FST-fuzzy** : ~10ms (au lieu de ~1400ms) — juste le FST walk
- **contains multi-token** : ~1ms (au lieu de ~50ms par doc) — position match + gaps mmap'd
- **contains substring (trigram)** : inchangé — toujours stored text
- **Taille index** : +40-50% sur le segment (gaps non compressés) mais absolu ~35-40MB pour 5201 docs
