# Doc 11 — Idée : gap dictionary dans la sibling table

Date : 27 mars 2026

## Idée

Remplacer `gap_len: u16` dans SiblingEntry par un `gap_id: u16` qui indexe
une table de gaps uniques. Comme un term dict mais pour les séparateurs.

### Format

```rust
// Table de gaps uniques (écrite en appendice du .sfx ou dans la sibling table)
gap_dict: Vec<Vec<u8>>  // ["", " ", "_", "::", "->", ": ", ", ", ".", "\n"]

// Sibling entry
pub struct SiblingEntry {
    pub next_ordinal: u32,
    pub gap_id: u16,      // index dans gap_dict (0 = "" = contigu)
}
```

### Avantages

- **gap_id == 0** → contigu → cross-token search (comme avant)
- **gap_id > 0** → on connaît le contenu exact du gap sans GapMap
- **Strict separators** → compare `query[pos..pos+gap.len()] == gap_dict[gap_id]`
  directement dans la chaîne sibling, pas besoin de GapMap
- **Taille** : les gaps uniques dans un codebase = ~10-20 strings.
  La table fait < 100 bytes. Négligeable.
- **u16 gap_id** → supporte jusqu'à 65K gaps uniques (largement suffisant)

### Variante : gap FST / gap term dict

Au lieu d'un simple Vec, un mini term dict trié (FST) pour les gaps.
Permettrait des lookups par contenu (query substring → gap_id).
Probablement overkill pour 10-20 entrées — un Vec + scan linéaire suffit.

### Impact

- Remplace `gap_len` par `gap_id` dans SiblingEntry (même taille : u16)
- Ajoute une petite table de gaps dans le .sfx
- Le SfxCollector collecte les gaps uniques et assigne des IDs
- Le merger fusionne les gap dicts (union des gaps de chaque segment)
- Le cross_token_search utilise gap_dict[gap_id] pour strict separator check
- Le GapMap devient potentiellement redondant pour l'adjacency check
  (la sibling table + gap_dict contient toute l'info)

### Relation avec le GapMap

Le GapMap stocke les gaps **par document × position**. C'est plus fin
(le même token pair peut avoir des gaps différents dans différents docs).

La sibling table + gap_dict stocke les gaps **par ordinal pair**.
Si "get"→"Element" a toujours gap="" (contigu), un seul gap_id suffit.

Mais si "get"→"Element" a gap="" dans un doc et gap=" " dans un autre,
il faut deux SiblingEntry pour la même paire (un avec gap_id=0, un avec gap_id=1).
C'est déjà supporté — la sibling table est une liste de successeurs par ordinal.

À terme, la sibling table + gap_dict pourrait remplacer complètement le GapMap
pour les use cases search (adjacency, strict separators). Le GapMap resterait
utile uniquement pour le highlighting (byte-exact offsets).
