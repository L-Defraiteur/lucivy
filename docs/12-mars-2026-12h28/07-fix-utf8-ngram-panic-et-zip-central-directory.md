# Fix UTF-8 panic dans ngram_contains_query + ZIP central directory

## 1. Panic UTF-8 dans la recherche ngram

### Symptôme

```
panicked at src/query/phrase_query/ngram_contains_query.rs:368:46:
byte index 3694 is not a char boundary; it is inside '≥' (bytes 3692..3695)
```

Crash lors de la recherche sur des documents contenant des caractères multi-octets UTF-8 : `≥` (3 bytes), `→` (3 bytes), accents, etc.

### Cause

`ngram_contains_query.rs` reçoit des positions de tokens sous forme de byte offsets `(start, end)`. Ces offsets sont utilisés pour slicer `stored_text[start..end]`. En Rust, un string slice sur un byte index qui tombe au milieu d'un caractère multi-byte provoque un panic.

Fichiers indexés en français/markdown = beaucoup de `é`, `è`, `→`, `≥`, etc.

### Lignes affectées

Toutes les fonctions de vérification dans `ngram_contains_query.rs` :

- `score_single_token_fuzzy()` : lignes 350, 368, 388
- `check_multi_token_fuzzy()` : lignes 468, 487, 504, 513, 531
- Section regex hybrid : ligne 591

### Fix

Ajout de deux helpers pour aligner les byte indices sur les char boundaries :

```rust
fn floor_char_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() { return s.len(); }
    let mut i = idx;
    while i > 0 && !s.is_char_boundary(i) { i -= 1; }
    i
}

fn ceil_char_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() { return s.len(); }
    let mut i = idx;
    while i < s.len() && !s.is_char_boundary(i) { i += 1; }
    i
}
```

Chaque `stored_text[start..end]` est précédé de :
```rust
let start = floor_char_boundary(stored_text, start);
let end = ceil_char_boundary(stored_text, end);
if start >= end || end > stored_text.len() { continue; }
```

### Impact

Le token peut être légèrement plus large (inclut le caractère multi-byte complet au lieu de le couper). Ça n'affecte pas la qualité de recherche car le matching est fuzzy et les séparateurs ne sont généralement pas des caractères multi-byte.

---

## 2. ZIP central directory

### Symptôme

Import d'un .zip contenant des .md → "No text files found".

### Cause

Le parseur ZIP dans `playground/index.html` lisait les tailles (`compSize`) depuis le **local file header**. Mais les zips créés avec `zip` sur Linux utilisent des **data descriptors** (general purpose bit flag 3 = `0x0008`). Quand ce bit est set, `compSize` et `fileSize` dans le local header sont à **0**. Les vraies tailles sont dans un data descriptor après les données compressées.

### Fix

Réécrit `extractZip()` pour lire le **central directory** (à la fin du fichier zip) :

1. Trouver le End of Central Directory record (signature `0x06054b50`)
2. Lire l'offset et le nombre d'entrées du central directory
3. Parser chaque entrée du central directory (signature `0x02014b50`) qui contient les vraies tailles
4. Pour chaque fichier, lire le local header uniquement pour calculer `dataStart` (offset des données)

Le central directory a toujours les tailles correctes, indépendamment des data descriptors.
