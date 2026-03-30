# 05 — Design : SepByteMap — bitmap des séparateurs par ordinal

Date : 30 mars 2026

## Problème

Le regex `rag3[a-z]+ver` ne devrait PAS matcher à travers des séparateurs
non-alphabétiques. Le ByteMap check valide les tokens mais pas les gaps
entre eux. Le GapMap contient les bytes de séparateurs mais il est per-doc
(format séquentiel) — lire les gaps coûte ~50ms sur 74 docs.

## Solution : SepByteMap

Un nouveau fichier d'index `.sepmap` dans le registre. Pour chaque ordinal,
un bitmap de 256 bits des bytes de séparateur observés **après** ce token
(tous docs confondus).

### Format

Identique au ByteMap : `SMAP` header + u32 num_ordinals + 32 bytes par ordinal.

### Contenu

Pour l'ordinal A (token "rag3"), le bitmap contient les bits de TOUS les
bytes observés comme séparateur entre le token A et son successeur, dans
tous les documents :
- Si dans un doc, "rag3" est suivi de " " (espace), bit 0x20 est set
- Si dans un autre doc, "rag3" est suivi de "\n", bit 0x0A est set
- Si dans un doc, "rag3" est contiguous avec le token suivant (gap=0),
  un flag spécial "has_contiguous" est set (bit 0 réservé, ou champ séparé)

### Flag contiguous

Le bit 0 (byte 0x00) est un bon candidat — un séparateur de byte 0x00 est
impossible en pratique (les textes sont UTF-8). Donc :
- bit 0x00 set → au moins une occurrence contiguous (gap=0) observée
- bits 0x20, 0x0A, etc. → séparateurs observés

### Construction (indexation)

Dans `SfxCollector`, quand on traite les paires de tokens consécutifs
(même endroit où on construit le sibling table et le gapmap) :

```rust
// Pour chaque paire (ordinal_a, gap_bytes):
if gap_bytes.is_empty() {
    sepmap_writer.record_byte(ordinal_a, 0x00); // contiguous flag
} else {
    for &byte in gap_bytes {
        sepmap_writer.record_byte(ordinal_a, byte);
    }
}
```

### Construction (merge)

Dans le merger, on a déjà les postings remappés. Pour chaque paire
consécutive de postings dans un doc, lire le gapmap du segment source
et recorder les bytes dans le SepByteMap du segment mergé.

Alternative plus simple : merger les SepByteMaps des segments sources
via OR bitmap (comme pour le ByteMap). C'est une approximation conservative
(union de tous les séparateurs observés dans tous les segments).

### Usage query time

Pour un gap `[a-z]+` entre positions pos_a et pos_b :

```rust
// Pour chaque token intermédiaire:
let sep_bm = sepmap.bitmap(ord_of_previous_token);
// Check: les séparateurs observés après ce token sont-ils tous dans [a-z] ?
if !sep_bytes_in_ranges(sep_bm, &[(b'a', b'z')]) {
    // Ce token peut avoir des séparateurs hors range → invalide
    // MAIS : si le token a aussi des occurrences contiguous (bit 0x00 set),
    // les occurrences contiguous sont potentiellement valides.
    // → On ne peut rejeter que si le bit contiguous N'EST PAS set.
    if !sep_bm_has_contiguous(sep_bm) {
        return false; // jamais contiguous, séparateurs tous hors range
    }
    // Contiguous possible — on ne peut pas rejeter via sepmap seul,
    // fallback to gapmap read pour ce doc spécifique.
}
```

### Fast path pour patterns "no separators allowed"

Pour `[a-z]+`, `\d+`, `\w+` — aucun séparateur standard (espace, newline,
ponctuation) n'est dans le range. Le check devient :

```rust
// Est-ce que ce token a JAMAIS été suivi d'un token contiguous (gap=0) ?
if sepmap.has_contiguous(ord) {
    // Possible match — check bytemap du token suivant
} else {
    // Jamais contiguous → impossible que [a-z]+ traverse cette frontière
    return false;
}
```

C'est un O(1) par token — pas de gapmap read du tout.

### Taille estimée

Identique au ByteMap : 32 bytes par ordinal.
- 862 docs : ~160 KB
- 5K docs : ~800 KB
- 90K docs : ~5 MB

### Fichiers à modifier

| Fichier | Changement |
|---------|-----------|
| `src/suffix_fst/sepmap.rs` | **NOUVEAU** — SepMapWriter/Reader + SepMapIndex |
| `src/suffix_fst/index_registry.rs` | Ajouter SepMapIndex dans `all_indexes()` |
| `src/suffix_fst/collector.rs` | Builder SepByteMap dans SfxBuildContext |
| `src/suffix_fst/mod.rs` | Export sepmap |
| `src/indexer/merger.rs` | Merge SepByteMap (OR bitmap) |
| `src/indexer/sfx_dag.rs` | Build SepByteMap dans WriteSfxNode |
| `src/query/phrase_query/regex_gap_analyzer.rs` | Utiliser SepByteMap |
| `src/index/segment_reader.rs` | Load `.sepmap` |

### Effort estimé

~150 lignes nouveau code (copie du bytemap avec modifications mineures).
Le builder et reader sont quasi identiques au ByteMap. Le merge est un OR
bitmap. L'intégration dans le registry est 1 ligne.
