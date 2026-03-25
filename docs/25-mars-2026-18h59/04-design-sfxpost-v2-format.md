# Doc 04 — Design : sfxpost V2 — posting lists optimisées pour filtered access

Date : 25 mars 2026

## Motivation

Le cross-token search avec pivot-first a besoin de résoudre les postings d'un ordinal
**filtré par un set de doc_ids**. Le format actuel est séquentiel VInt — O(n) decode
obligatoire même pour n'en garder que 5 sur 10K.

Un nouveau format avec des doc_ids séparés et binary-searchable permet O(log n)
au lieu de O(n).

## Format actuel (V1)

```
Per ordinal:
  Offset table: [u32 offset] × (num_terms + 1)     ← byte offsets into entry_data
  Entry data (VInt packed, sequential):
    [VInt doc_id][VInt token_index][VInt byte_from][VInt byte_to] × n_entries
```

Propriétés :
- Compact (VInt = 1-5 bytes par valeur)
- Séquentiel seulement — pas de random access par doc_id
- Pour lire les entries d'un ordinal : decode ALL VInts de offset[ord] à offset[ord+1]

## Format V2 proposé

### Structure par ordinal

```
Per ordinal (dans entry_data):
  [u16 num_unique_docs]
  [u32 doc_id] × num_unique_docs                    ← TRIÉ, binary searchable
  [u16 entries_offset] × num_unique_docs             ← offset dans payload pour chaque doc
  [u16 num_entries] × num_unique_docs                ← nombre d'entries par doc
  Payload (VInt packed):
    Per doc (dans l'ordre du doc_id array):
      [VInt token_index, VInt byte_from, VInt byte_to] × num_entries[i]
```

### Layout mémoire pour un ordinal avec 3 docs

```
Doc IDs:        [42]  [100]  [200]          ← 12 bytes (3 × u32), sorted
Entries offset: [0]   [9]    [15]           ← 6 bytes (3 × u16)
Num entries:    [3]   [2]    [1]            ← 6 bytes (3 × u16)
Payload:        [ti,bf,bt][ti,bf,bt][ti,bf,bt] [ti,bf,bt][ti,bf,bt] [ti,bf,bt]
                ↑ doc 42 (3 entries)          ↑ doc 100 (2)        ↑ doc 200 (1)
```

### Opérations

#### Resolve complet (comme V1)
```rust
fn entries(&self, ordinal: u32) -> Vec<PostingEntry> {
    let header = self.read_header(ordinal);
    let mut result = Vec::with_capacity(header.total_entries());
    for i in 0..header.num_unique_docs {
        let doc_id = header.doc_ids[i];
        let entries = self.decode_payload(header, i);
        for e in entries {
            result.push(PostingEntry { doc_id, ..e });
        }
    }
    result
}
```

#### Resolve filtré par doc_ids (NOUVEAU)
```rust
fn entries_filtered(
    &self, ordinal: u32, doc_ids: &HashSet<u32>
) -> Vec<PostingEntry> {
    let header = self.read_header(ordinal);
    let mut result = Vec::new();
    for i in 0..header.num_unique_docs {
        let doc_id = header.doc_ids[i];
        if !doc_ids.contains(&doc_id) { continue; }   // ← SKIP sans décoder payload
        let entries = self.decode_payload(header, i);
        for e in entries {
            result.push(PostingEntry { doc_id, ..e });
        }
    }
    result
}
```

#### Binary search par doc_id (pour un seul doc)
```rust
fn entries_for_doc(&self, ordinal: u32, target_doc: u32) -> Vec<PostingEntry> {
    let header = self.read_header(ordinal);
    // Binary search O(log n) sur le tableau de doc_ids
    match header.doc_ids.binary_search(&target_doc) {
        Ok(idx) => self.decode_payload(header, idx),
        Err(_) => Vec::new(),
    }
}
```

#### Check d'existence (zéro decode)
```rust
fn has_doc(&self, ordinal: u32, target_doc: u32) -> bool {
    let header = self.read_header(ordinal);
    header.doc_ids.binary_search(&target_doc).is_ok()
}
```

## Comparaison V1 vs V2

| Opération | V1 | V2 |
|-----------|----|----|
| Resolve complet | O(n) VInt decode | O(n) VInt decode (pareil) |
| Resolve filtré (5/10K) | O(n) decode + filter | O(log n) search + O(5) decode |
| Check doc existence | O(n) decode | O(log n) binary search |
| Taille par entry | ~4-8 bytes VInt | ~4-8 bytes VInt + 8 bytes header/doc |
| Random access par doc | impossible | O(log n) |

### Overhead mémoire

Header par doc : 4 (doc_id) + 2 (offset) + 2 (entries count) = **8 bytes par doc unique**.

Pour un ordinal avec 1000 entries réparties sur 500 docs : 500 × 8 = 4KB de header.
Les entries V1 feraient ~1000 × 8 = 8KB. Le V2 fait ~4KB header + 6KB payload = 10KB.
Overhead : **~25%** en taille. Acceptable pour les gains en perf.

Pour des ordinals rares (1-5 entries) : le header domine. Mais ces ordinals sont
les plus rapides à décoder de toute façon (trivial).

## Impact sur le cross-token search

Avec V2, le flow pivot-first devient :

```
1. Falling walk → split candidates (ordinals left + right)
2. Resolve pivot (left) complet → extract pivot_doc_ids
3. Pour chaque right ordinal :
   → binary_search(right_ordinal, doc_id) pour chaque doc_id du pivot
   → O(log n) par doc → skip les ordinals qui n'ont pas le doc
   → decode payload seulement pour les matchs
4. Adjacence check sur les entries décodées
```

Pour un token courant ("the") avec 10K entries sur 5K docs, et un pivot avec 50 docs :
- V1 : decode 10K entries, filter → garder ~50. Coût : O(10K)
- V2 : 50 binary searches × O(log 5K) = 50 × 13 = 650 comparisons. Coût : O(650)
- **15x plus rapide** sur les posting lookups.

## Implémentation

### Phase 1 : Écriture (sfxpost writer)

Modifier le writer pour produire le format V2 :

```rust
struct SfxPostingsWriterV2 {
    // Per ordinal: collect entries, group by doc_id, sort, write header + payload
}

impl SfxPostingsWriterV2 {
    fn add_entry(&mut self, ordinal: u32, doc_id: u32, ti: u32, bf: u32, bt: u32);
    fn finish(&self) -> Vec<u8>;  // produce V2 binary
}
```

L'écriture se fait au commit (segment_writer) et au merge (merger).
Les deux passent par `SfxPostingsWriter` — un seul point de changement.

### Phase 2 : Lecture (sfxpost reader)

Modifier le reader pour parser le format V2 :

```rust
impl SfxPostingsReaderV2 {
    fn entries(&self, ordinal: u32) -> Vec<PostingEntry>;
    fn entries_filtered(&self, ordinal: u32, doc_ids: &HashSet<u32>) -> Vec<PostingEntry>;
    fn entries_for_doc(&self, ordinal: u32, doc_id: u32) -> Vec<PostingEntry>;
    fn has_doc(&self, ordinal: u32, doc_id: u32) -> bool;
}
```

### Phase 3 : Intégration

1. Modifier le `raw_ordinal_resolver` pour accepter un filtre optionnel
2. Le cross_token_search passe le filtre au resolver
3. Le suffix_contains existant utilise `entries()` (pas de filtre, backward compat)

### Phase 4 : Migration merger

Le merger lit les sfxpost V1 des segments source et écrit V2 dans le segment mergé.
Pendant la transition, supporter les deux formats en lecture (check magic byte).

## Magic / Version

```
V1 : pas de magic (legacy, format reconnu par le contexte)
V2 : [4 bytes] "SFP2" magic + [u32] num_terms + offset table + entry data V2
```

Le reader check le magic. Si absent → V1. Si "SFP2" → V2.
Les vieux segments restent lisibles. Les nouveaux segments utilisent V2.

## Questions

1. Faut-il un `entries_offset` par doc (u16) ou un simple compteur cumulatif ?
   → offset permet le random access direct. Compteur nécessite de parcourir.
   → offset est mieux pour O(log n) access. +2 bytes par doc.

2. Faut-il limiter num_unique_docs à u16 (65535) ?
   → En pratique un ordinal rare a ~1-100 docs. Un token courant ("the") peut
   avoir 50K+ docs. u16 ne suffit pas. → utiliser **u32** pour num_unique_docs.

3. Le payload pourrait-il être en fixed-width au lieu de VInt ?
   → Fixed-width (3 × u32 = 12 bytes/entry) permet le random access dans le payload.
   → VInt (~6-9 bytes/entry) est plus compact mais nécessite séquentiel.
   → Compromis : si on a déjà le random access par doc via le header, le payload
   séquentiel par doc est OK (on ne decode que les entries d'UN doc à la fois).

4. Delta encoding des doc_ids ?
   → Les doc_ids sont triés. Delta encoding réduirait la taille du header.
   → Mais ça empêche le binary search (faut prefix-sum pour reconstruire).
   → On garde les doc_ids en valeur absolue pour le binary search.

## Future : séparation en 3 fichiers

Le `.sfx` actuel contient le FST + parent list + GapMap. Le GapMap n'est utilisé
que pour `strict_separators=true` (désactivé par défaut) et le cross-token search
n'en a pas besoin.

Séparation naturelle :
```
.sfx      — FST + parent list (suffix lookup)
.sfxpost  — posting lists V2 (ordinal → doc entries)
.sfxgap   — GapMap (séparateurs inter-tokens, optionnel)
```

Avantages : search ne charge que `.sfx` + `.sfxpost`, mmap plus granulaire,
GapMap peut évoluer ou disparaître indépendamment.

Pas prioritaire — à faire après avoir validé le V2 et le cross-token search.
