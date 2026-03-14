# Design : Token Map + Ngram positions composées — contains sans stored text

Date : 14 mars 2026

## Problème

La recherche contains est lente car chaque candidat trigram nécessite :
1. `store_reader.get(doc_id)` — décompression LZ4 (~16KB par bloc, ~16 docs par bloc)
2. `tokenize_raw(texte)` — re-tokenisation (~50KB de code source → ~5000 tokens)
3. Vérification substring/fuzzy par sliding window sur les 5000 tokens

Sur 5201 docs : contains d=0 ~300ms, d=1 ~1400ms.

## Solution

Deux changements à l'indexation qui ensemble éliminent le stored text de la recherche :

1. **Positions ngram composées** — chaque trigram porte `token_pos × K + ngram_seq`,
   permettant de confirmer un match substring à 100% depuis le posting list seul.

2. **Token Map (.tmap)** — fichier binaire mmap'd stockant tokens + séparateurs par doc,
   avec accès O(1) par (doc_id, position). Pour les cas que le ngram ne peut pas résoudre
   seul (fuzzy, queries courtes, séparateurs stricts).

Pas de fallback stored text. Index sans `.tmap` → reindexation nécessaire.

---

## 1. Positions ngram composées

### Principe

Actuellement les trigrams ont des positions séquentielles (0, 1, 2, 3...) sans lien
avec le token raw dont ils proviennent. On ne peut pas savoir quel token contient
quel trigram.

Nouveau : chaque trigram porte une **position composée** qui encode à la fois le token
raw source et sa position séquentielle dans ce token :

```
position = token_pos × K + ngram_seq

K = 1024 (aucun token ne fait > 1026 chars → max 1024 trigrams par token)

Décodage :
  token_pos = position / K      // position du raw token dans le document
  ngram_seq = position % K      // index du trigram dans le token (0, 1, 2...)
```

### Exemple

```
Texte : "import rag3db from 'rag3db_core';"

Raw tokenizer :
  pos=0 "import"   pos=1 "rag3db"   pos=2 "from"   pos=3 "rag3db"   pos=4 "core"

Ngram tokenizer avec positions composées (K=1024) :
  "imp" pos=0     "mpo" pos=1     "por" pos=2     "ort" pos=3
  "rag" pos=1024  "ag3" pos=1025  "g3d" pos=1026  "3db" pos=1027
  "fro" pos=2048  "rom" pos=2049
  "rag" pos=3072  "ag3" pos=3073  "g3d" pos=3074  "3db" pos=3075
  "cor" pos=4096  "ore" pos=4097

Posting list de "rag" :
  doc=42, positions=[1024, 3072]
  → décodé : token_pos=[1, 3], ngram_seq=[0, 0]
```

### Vérification 100% par ngram seul

Query "g3db" → trigrams `["g3d" qseq=0, "3db" qseq=1]`

```
posting("g3d") → doc=42, positions=[1026, 3074]
                 → token_pos=[1, 3], ngram_seq=[2, 2]

posting("3db") → doc=42, positions=[1027, 3075]
                 → token_pos=[1, 3], ngram_seq=[3, 3]

Pour token_pos=1 :
  "g3d" ngram_seq=2, "3db" ngram_seq=3
  Séquence consécutive (3-2=1) et diff = query_seq diff (1-0=1) ✓
  → "g3db" est un substring contigu du token à position 1
  → CONFIRMÉ. Preuve mathématique : trigrams consécutifs qui se
    chevauchent de 2 chars = le substring original. Zéro faux positif.

Pour token_pos=3 :
  Même vérification → CONFIRMÉ.

Résultat : doc=42, occurrences aux token positions [1, 3].
Zéro lecture. Que du posting list.
```

### Garantie mathématique

Des trigrams consécutifs (ngram_seq i, i+1, i+2...) matchant les trigrams consécutifs
de la query forment forcément le substring recherché :

```
Trigrams de "rag3db" : r·a·g | a·g·3 | g·3·d | 3·d·b
                       seq=0   seq=1   seq=2   seq=3

Trigrams de "g3d"    : g·3·d
                       qseq=0

Match : ngram_seq=2 correspond à qseq=0.
Le trigram "g3d" à seq=2 dans "rag3db" = caractères [2..5].
Un seul trigram → on sait que "g3d" apparaît à cet endroit dans le token.
Pas de faux positif possible avec un seul trigram de 3 chars.

Avec 2+ trigrams consécutifs, le chevauchement de 2 chars entre trigrams
adjacents garantit la continuité du substring.
```

### Cas couverts par le ngram seul (Niveau 0)

- ✅ Query ≥ 3 chars, d=0 (au moins 1 trigram, vérification par séquence)
- ✅ Multi-token d=0 (chaque token vérifié indépendamment, positions consécutives pour phrase)
- ✅ contains_split d=0 (chaque token vérifié indépendamment, pas besoin de positions consécutives)
- ❌ Query < 3 chars (pas de trigram possible → Niveau 1)
- ❌ Fuzzy d>0 (le ngram threshold réduit donne des candidats mais pas de preuve → Niveau 1)
- ❌ Séparateurs stricts (ngram ne connaît pas les chars entre tokens → Niveau 1)

---

## 2. Token Map (.tmap)

Fichier binaire mmap'd par segment. Stocke, pour chaque document, la séquence complète
de tokens et séparateurs dans l'ordre du texte original. Accès O(1) par (doc_id, position).

### Format binaire

```
┌───────────────────────────────────────────────────────────────────┐
│ File Header                                                       │
│   magic: [u8; 4] = b"TMAP"                                       │
│   version: u8 = 1                                                 │
│   num_docs: u32 (LE)                                              │
├───────────────────────────────────────────────────────────────────┤
│ Doc Offset Table — accès O(1) par doc_id                          │
│   [u64; num_docs + 1] (LE)                                        │
│   doc_offsets[i] = position dans le fichier des données du doc i   │
│   doc_offsets[num_docs] = sentinelle fin                           │
├───────────────────────────────────────────────────────────────────┤
│ Doc Data (concaténé)                                              │
│                                                                   │
│ ┌───────────────────────────────────────────────────────────────┐ │
│ │ Doc i                                                         │ │
│ │                                                               │ │
│ │   num_tokens: u16 (LE)                                        │ │
│ │                                                               │ │
│ │   Token Offset Table — accès O(1) par position dans le doc    │ │
│ │     [u32; num_tokens] (LE)                                    │ │
│ │     token_offsets[p] = offset relatif dans ce doc data         │ │
│ │     du début du bloc (gap + token) pour la position p          │ │
│ │                                                               │ │
│ │   Token Data (séquentiel, dans l'ordre du texte) :            │ │
│ │     pos 0 : [gap_len: u8][gap_bytes...][tok_len: u16][tok_bytes...] │
│ │     pos 1 : [gap_len: u8][gap_bytes...][tok_len: u16][tok_bytes...] │
│ │     ...                                                       │ │
│ │     pos N-1 : [gap_len: u8][gap_bytes...][tok_len: u16][tok_bytes...] │
│ │     trailing : [gap_len: u8][gap_bytes...]                    │ │
│ │                                                               │ │
│ │   gap_len encoding :                                          │ │
│ │     0..254 : longueur directe en bytes                        │ │
│ │     255 : extended, suivi de [ext_len: u16 LE] puis bytes     │ │
│ │                                                               │ │
│ │   Chaque entrée contient :                                    │ │
│ │     gap  = caractères AVANT le token (séparateur/prefix)      │ │
│ │     token = le token raw (lowercase)                           │ │
│ │   Le trailing gap = caractères APRÈS le dernier token          │ │
│ │                                                               │ │
│ └───────────────────────────────────────────────────────────────┘ │
└───────────────────────────────────────────────────────────────────┘
```

### Accès O(1)

```rust
fn read_token(mmap: &[u8], doc_offsets: &[u64], doc_id: u32, position: u16) -> (&[u8], &[u8]) {
    let doc_start = doc_offsets[doc_id as usize] as usize;
    let doc_data = &mmap[doc_start..];
    let num_tokens = u16::from_le_bytes([doc_data[0], doc_data[1]]) as usize;
    let table_start = 2;
    let p = position as usize;

    // Lire l'offset de cette position dans la token offset table
    let off_pos = table_start + p * 4;
    let entry_offset = u32::from_le_bytes(doc_data[off_pos..off_pos+4].try_into().unwrap()) as usize;

    // Parser gap + token à cet offset
    let entry = &doc_data[entry_offset..];
    let (gap, rest) = read_gap(entry);
    let (token, _) = read_token_bytes(rest);
    (gap, token)
}
```

### Exemple concret

```
Doc 42 : "import rag3db from 'rag3db_core';"

Token Map doc 42 :
  num_tokens = 5
  token_offsets = [offset_0, offset_1, offset_2, offset_3, offset_4]

  pos=0 : gap=""      token="import"       // début du texte
  pos=1 : gap=" "     token="rag3db"       // espace avant
  pos=2 : gap=" "     token="from"         // espace avant
  pos=3 : gap=" '"    token="rag3db"       // espace+quote avant
  pos=4 : gap="_"     token="core"         // underscore avant
  trailing : gap="';"                       // quote+semicolon après
```

---

## Niveaux de vérification

```
┌─────────────────────────────────────────────────────────────────────┐
│ Niveau 0 — Ngram posting list seul                                  │
│                                                                     │
│ Quand : d=0, query ≥ 3 chars, pas de séparateur strict             │
│ Comment : intersection (doc, token_pos) + vérification ngram_seq   │
│ Coût : zéro lecture, que des posting lists                          │
│ Précision : 100% (preuve mathématique par trigrams consécutifs)     │
│ Couverture estimée : ~90% des queries en production                 │
├─────────────────────────────────────────────────────────────────────┤
│ Niveau 1 — Token Map (mmap, O(1))                                   │
│                                                                     │
│ Quand : d>0 (fuzzy), query < 3 chars, séparateurs stricts          │
│ Comment : tmap[doc][position] → lire token (~8 bytes) + gap         │
│ Coût : 1 accès mmap par (doc, position), zéro décompression        │
│ Couverture : tous les cas restants                                  │
└─────────────────────────────────────────────────────────────────────┘
```

---

## Flows détaillés

### contains "rag3" d=0 — Niveau 0 (ngram seul)

```
"rag3" → trigrams ["rag" qseq=0, "ag3" qseq=1]

posting("rag") → doc=42 positions=[1024, 3072]
posting("ag3") → doc=42 positions=[1025, 3073]

Intersection par token_pos :
  token_pos=1 : "rag" seq=0, "ag3" seq=1 → consécutifs, diff=1=qdiff ✓
  token_pos=3 : "rag" seq=0, "ag3" seq=1 → consécutifs, diff=1=qdiff ✓

→ doc=42 confirmé, 2 occurrences aux positions raw [1, 3]
→ Zéro lecture
```

### contains "rag3db main" phrase d=0 — Niveau 0

```
Tokens query : ["rag3db", "main"]

Pour "rag3db" : trigrams ["rag","ag3","g3d","3db"] qseq=[0,1,2,3]
  Intersection positions → token_pos=1 (seq 0,1,2,3 consécutifs ✓)
  Intersection positions → token_pos=3 (seq 0,1,2,3 consécutifs ✓)

Pour "main" : trigrams ["mai","ain"] qseq=[0,1]
  Intersection positions → token_pos=5 (seq 0,1 consécutifs ✓)

Vérification phrase : token_pos consécutifs ?
  (1, 5) → non
  (3, 5) → non (3+1≠5)
  → pas de match phrase

(Si "main" était à position 2 ou 4, le match phrase fonctionnerait)
```

### contains_split "rag3db main" d=0 — Niveau 0

```
Split → 2 queries indépendantes (should/OR)

"rag3db" : token_pos=[1, 3] confirmé ✓
"main"   : token_pos=[5] confirmé ✓

→ doc=42 matche (au moins un token trouvé)
→ Zéro lecture
```

### contains "ab" d=0 — Niveau 1 (query trop courte)

```
"ab" → 2 chars, pas de trigram possible

Candidats : trigrams impossibles → scan complet ou heuristique
            (ou bigrams si on ajoute un champ bigram, point ouvert)

Token Map :
  Pour chaque doc candidat, pour chaque position :
    tmap[doc][pos] → token → token.contains("ab") ?

Ou : utiliser le raw field FST → trouver les termes contenant "ab" →
     posting lists → doc_ids. Puis tmap pour highlights/séparateurs.
```

### contains "rag3" d=1 — Niveau 1 (fuzzy)

```
"rag3" d=1 → trigrams ["rag","ag3"] avec threshold réduit (1 manquant autorisé)

Ngram candidats : doc=42, token_pos=[1, 3, ...] (plus de candidats qu'en d=0)

Token Map : pour chaque (doc, token_pos) candidat :
  tmap[42][pos=1] → token="rag3db"
  token_match_distance("rag3db", "rag3", 1) → substring exact, distance 0 ≤ 1 ✓

  tmap[42][pos=7] → token="rng3xx"
  token_match_distance("rng3xx", "rag3", 1) → sliding window, min distance 2 > 1 ✗

→ Vérification rapide : 1 accès mmap (~8 bytes) par candidat, pas 50KB
```

### Multi-token avec séparateur strict — Niveau 1

```
Query "rag3db core", séparateur attendu " ", mode strict

Niveau 0 confirme les tokens individuels :
  "rag3db" → token_pos=3 ✓
  "core"   → token_pos=4 ✓
  Consécutifs ✓

Mais séparateur strict → besoin du gap réel :
  tmap[42][pos=4].gap → "_"
  edit_distance(" ", "_") = 1
  Budget = 0 → REJETÉ

  (Budget ≥ 1 → accepté)
```

---

## Highlights

Les byte offsets pour highlights sont calculables depuis le token map :

```
Option A — Byte offsets dans la Token Offset Table (recommandé)

  Token Offset Table étendue :
    [u32; num_tokens × 2]
    token_entries[p] = (tmap_offset: u32, text_byte_offset: u32)

  Accès O(1) : text_byte_offset[p] → début du token dans le texte original
  Fin du token : text_byte_offset[p] + tok_len

Option B — Calculer à la volée

  byte_offset(p) = Σ(gap_len[i] + tok_len[i]) pour i=0..p-1 + gap_len[p]
  O(p) mais p est généralement petit
```

L'option A ajoute 4 bytes par token dans la table, mais donne des highlights O(1)
sans aucune lecture supplémentaire. Le highlight est juste un lookup dans la table.

---

## Taille estimée

Pour un fichier code source de 50KB avec 5000 tokens :

```
Composant                 Taille
─────────                 ──────
Tokens (texte brut)       ~30KB
Gaps (séparateurs)        ~6KB
Token Offset Table        5000 × 8 = 40KB  (avec byte offsets, Option A)
Doc header                2 bytes
                          ──────
Total par doc             ~76KB

Sur 5201 docs :           ~390MB
Stored text actuel :      ~80MB (LZ4 compressé)
```

Le token map est ~5x plus gros que le stored text compressé. Mais :
- C'est mmap'd → seules les pages accédées sont en RAM
- La recherche ne touche que ~50 entrées de ~8 bytes = ~400 bytes par query
- Le stored text compressé nécessitait de décompresser ~16KB par doc accédé

### Optimisation taille (futur)

- Compresser le token map par doc (LZ4 micro-blocs, ~10μs par décompression)
- Ne stocker que les gaps (pas les tokens) → les tokens sont reconstruisibles
  depuis le raw field FST + posting list. Mais complexifie le niveau 1.
- Varint encoding pour tok_len et offsets

---

## Changements à l'indexation

### 1. Ngram tokenizer — positions composées

Fichier : tokenizer ngram (dans le segment writer ou le tokenizer custom)

Chaque trigram émis porte `position = token_pos × 1024 + ngram_seq` au lieu d'une
position séquentielle globale. Le segment writer doit coordonner la position raw
avec le tokenizer ngram.

```rust
// Pseudo-code dans le segment writer
for (raw_pos, token) in raw_tokens.enumerate() {
    let trigrams = generate_trigrams(&token.text);
    for (ngram_seq, trigram) in trigrams.enumerate() {
        let composed_pos = raw_pos as u32 * 1024 + ngram_seq as u32;
        ngram_postings.record(trigram, doc_id, composed_pos);
    }
}
```

Le posting list ngram doit utiliser `WithFreqsAndPositions` (déjà le cas).

### 2. Token Map writer

Nouveau composant dans le segment serializer :

```rust
struct TokenMapWriter {
    writer: WritePtr,
    doc_offsets: Vec<u64>,
    buffer: Vec<u8>,
}

impl TokenMapWriter {
    /// Appelé une fois par document après tokenisation du champ texte.
    fn add_document(&mut self, text: &str, tokens: &[(usize, usize)]) {
        self.doc_offsets.push(self.buffer.len() as u64);
        let num_tokens = tokens.len() as u16;
        self.buffer.extend_from_slice(&num_tokens.to_le_bytes());

        // Réserver la token offset table (remplie après)
        let table_pos = self.buffer.len();
        self.buffer.resize(table_pos + num_tokens as usize * 8, 0); // 2× u32

        let mut prev_end = 0usize;
        for (i, &(start, end)) in tokens.iter().enumerate() {
            let entry_offset = (self.buffer.len() - (table_pos - 2)) as u32; // relatif
            let text_byte_offset = start as u32;

            // Écrire (tmap_offset, text_byte_offset) dans la table
            let table_entry = table_pos + i * 8;
            self.buffer[table_entry..table_entry+4]
                .copy_from_slice(&entry_offset.to_le_bytes());
            self.buffer[table_entry+4..table_entry+8]
                .copy_from_slice(&text_byte_offset.to_le_bytes());

            // Écrire gap (chars entre prev token et ce token)
            let gap = &text[prev_end..start];
            encode_gap(&mut self.buffer, gap.as_bytes());

            // Écrire token
            let tok = &text[start..end];
            let tok_len = tok.len() as u16;
            self.buffer.extend_from_slice(&tok_len.to_le_bytes());
            self.buffer.extend_from_slice(tok.as_bytes());

            prev_end = end;
        }

        // Trailing gap (après dernier token)
        let trailing = &text[prev_end..];
        encode_gap(&mut self.buffer, trailing.as_bytes());
    }

    fn close(self) -> io::Result<()> {
        let num_docs = self.doc_offsets.len() as u32;
        // Header
        self.writer.write_all(b"TMAP")?;
        self.writer.write_all(&[1u8])?;
        self.writer.write_all(&num_docs.to_le_bytes())?;
        // Doc offset table (offsets absolus)
        let header_size = 9u64; // 4 + 1 + 4
        let table_size = (num_docs as u64 + 1) * 8;
        let data_start = header_size + table_size;
        for &offset in &self.doc_offsets {
            self.writer.write_all(&(data_start + offset).to_le_bytes())?;
        }
        self.writer.write_all(&(data_start + self.buffer.len() as u64).to_le_bytes())?;
        // Data
        self.writer.write_all(&self.buffer)?;
        self.writer.terminate()
    }
}

fn encode_gap(buf: &mut Vec<u8>, gap: &[u8]) {
    if gap.len() < 255 {
        buf.push(gap.len() as u8);
        buf.extend_from_slice(gap);
    } else {
        buf.push(255);
        buf.extend_from_slice(&(gap.len() as u16).to_le_bytes());
        buf.extend_from_slice(gap);
    }
}
```

### 3. SegmentComponent

```rust
// src/index/segment_component.rs
pub enum SegmentComponent {
    // ... existants ...
    TokenMap,  // .tmap
}

// src/index/index_meta.rs
SegmentComponent::TokenMap => ".tmap".to_string(),
```

### 4. Points de branchement

- `src/indexer/segment_serializer.rs` :
  - `for_segment()` : ouvrir `WritePtr` pour `.tmap`
  - `close()` : finaliser le `TokenMapWriter`

- `src/indexer/segment_writer.rs` :
  - `index_document()` : après tokenisation raw, appeler `tmap_writer.add_document()`
  - Le segment writer a accès au texte original et aux offsets des tokens

- `src/indexer/merger.rs` :
  - Concaténer les `.tmap` des segments source
  - Remapper les doc_ids (données par doc inchangées, juste la table d'offsets)

---

## Changements à la recherche

### NgramContainsWeight::scorer()

Remplacer le flow actuel (trigram candidates → stored text verify) par :

```rust
fn scorer(&self, reader: &SegmentReader, boost: Score) -> Result<Box<dyn Scorer>> {
    let ngram_inverted = reader.inverted_index(self.ngram_field)?;
    let tmap_reader = reader.token_map_reader(self.raw_field)?;

    // Collecter les candidats (doc_id, token_pos) depuis les ngram posting lists
    let candidates = collect_ngram_candidates_with_positions(
        &ngram_inverted, &self.trigram_sources, self.fuzzy_distance,
    )?;

    // Niveau 0 : vérification par ngram_seq (d=0, query ≥ 3 chars)
    // Niveau 1 : vérification par token map (d>0, query < 3 chars)
    Ok(Box::new(TokenMapScorer::new(
        candidates, tmap_reader, self.verification, ...
    )))
}
```

### TokenMapScorer

Nouveau scorer qui remplace `NgramContainsScorer` :

```rust
struct TokenMapScorer {
    candidates: Vec<(DocId, u16)>,  // (doc_id, token_pos)
    cursor: usize,
    tmap_reader: TokenMapReader,
    // ... verification params, BM25, highlights ...
}

impl TokenMapScorer {
    fn verify(&self, doc_id: DocId, token_pos: u16) -> bool {
        match self.verification {
            Niveau0 { query_trigrams } => {
                // Déjà confirmé par l'intersection ngram_seq
                true
            }
            Niveau1 { query, distance } => {
                let (gap, token) = self.tmap_reader.read_token(doc_id, token_pos);
                token_match_distance(token, query, distance).is_some()
            }
        }
    }
}
```

---

## Merge

Le merger lit les `.tmap` des segments source et les concatène :

1. Pour chaque segment source, mmap le `.tmap`
2. Pour chaque doc (dans l'ordre du mapping doc_id) :
   - Copier le bloc doc data brut (tokens + gaps inchangés)
3. Recalculer la doc offset table avec les nouvelles positions

C'est du memcpy — les données par doc sont identiques, seul le doc_id change.
Même pattern que le store merge mais sans recompression.

Les positions ngram composées ne changent pas au merge (elles sont relatives au doc,
pas au segment).

---

## Points ouverts

### 1. Queries < 3 chars sans trigram

"ab" ne produit pas de trigram. Options :
- Ajouter un champ `._bigram` (bigrams, 2 chars) pour couvrir les queries de 2 chars
- Utiliser le raw field FST pour trouver les termes contenant "ab"
- Scan du token map (lent mais rare en pratique)

Pour v1 : raw field FST + token map. Les queries de 1-2 chars sont rares.

### 2. Champs multiples

Un `.tmap` par champ text, nommé `{segment_uuid}.{field_id}.tmap`.
Le field_id est déjà disponible dans le schema.

### 3. Taille sur disque

~390MB pour 5201 docs (~76KB/doc) vs ~80MB stored text compressé.
Acceptable pour un index de code source. Les pages mmap non accédées
ne consomment pas de RAM.

Optimisations futures : varint encoding, compression par doc, ne stocker
que les gaps (reconstruire les tokens depuis le posting list).

### 4. Constante K pour positions composées

K=1024 limite les tokens à 1026 chars max. Pour des documents avec des
tokens très longs (base64, URLs), augmenter K ou utiliser un encoding
variable. Pour du code source, 1024 est largement suffisant.

### 5. Position composée et posting list compression

Les positions composées ont des sauts de ~1024 entre tokens consécutifs
(au lieu de +1). Le delta encoding du posting list est moins efficace.
Impact : posting list ngram ~2-3x plus gros. Acceptable vu le gain
en vitesse de recherche.
