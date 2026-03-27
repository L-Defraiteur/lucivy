# Doc 08 — Design : sibling links dans le SFX pour cross-token

Date : 27 mars 2026
Branche : `feature/cross-token-search`

## Contexte

Le cross-token search doit trouver des queries qui traversent les frontières de tokens
créées par CamelCaseSplit (ou toute tokenisation arbitraire sans séparateur réel).

Les approches explorées (falling_walk, graph, DP) reconstituent les relations entre
tokens au **query time** — coûteux en I/O et allocations, surtout en WASM.

## Idée : pré-calculer les relations à l'indexation

Quand le CamelCaseSplit découpe "rag3Weaver" en "rag3" (Ti=0) + "weaver" (Ti=1),
on sait **à l'indexation** que ces deux tokens sont contigus dans le texte source
(byte_to de "rag3" == byte_from de "weaver").

On stocke cette information directement dans le SFX : pour chaque ordinal,
la liste de ses **successeurs possibles** avec la taille du gap.

## Format : sibling table par ordinal

### Pourquoi par ordinal (pas par posting entry)

- **Par posting entry** (+6 bytes/entry) : 90K docs × ~100 tokens/doc = 9M entries × 6 = **54MB**. Trop cher.
- **Par ordinal avec liste de successeurs** : on stocke les paires uniques (ordinal → next_ordinal, gap_len).
  Pour 1M tokens uniques × ~1.5 successeurs moyen = 1.5M paires × 6 bytes = **9MB**. Acceptable.
- **L'ordinal est un index direct** → accès O(1) dans un tableau, pas besoin de HashMap.

### SiblingEntry

```rust
pub struct SiblingEntry {
    pub next_ordinal: u32,  // ordinal du token suivant
    pub gap_len: u16,       // bytes de séparation entre les deux tokens
}
```

- `gap_len == 0` → tokens contigus dans le texte source → **cross-token search viable**
- `gap_len == 1` → séparés par un espace → phrase search, pas cross-token
- `gap_len > 1` → séparateur multi-byte (underscore+espace, ponctuation, etc.)
- `gap_len == 0xFFFF` → séparateur très long (rare, on tronque)

### Sibling table dans le .sfx

```
.sfx file layout:
[4 bytes] magic "SFX1"
[1 byte] version (bumped to 2)
[4 bytes] num_docs
[4 bytes] num_suffix_terms
[8 bytes] fst_offset
[8 bytes] fst_length
[8 bytes] parent_list_offset
[8 bytes] parent_list_length
[8 bytes] gapmap_offset
[8 bytes] sibling_table_offset    ← NOUVEAU
[FST data]
[Parent list (OutputTable)]
[GapMap data]
[Sibling table]                   ← NOUVEAU
```

Sibling table format :
```
[4 bytes] num_ordinals
[4 bytes × num_ordinals] list_offsets  (offset dans entries_data pour chaque ordinal)
[4 bytes] sentinel offset (= total entries_data len)
Entries data:
  Per ordinal (variable length):
    [2 bytes] num_siblings
    Per sibling:
      [4 bytes] next_ordinal
      [2 bytes] gap_len
```

Alternative plus simple si on assume ≤ 255 siblings par ordinal (réaliste) :
```
[4 bytes] num_ordinals
Per ordinal:
  [1 byte] num_siblings (0 = no siblings)
  Per sibling:
    [4 bytes] next_ordinal
    [2 bytes] gap_len
```

Pas d'offset table → lecture séquentielle. Mais lookup O(ordinal) nécessite
une table d'offsets. Gardons la première option avec offsets.

### Estimation de taille

| Corpus | Tokens uniques | Paires sibling | Taille table |
|--------|---------------|----------------|-------------|
| lucivy (846 docs) | ~10K | ~5K | ~30KB |
| Linux kernel (90K docs) | ~500K-1M | ~1.5M | ~9MB |
| Grand corpus (500K docs) | ~5M | ~10M | ~60MB |

Le ratio sibling_table / SFX total est ~3-5%. Acceptable.

## Algorithme de search avec sibling links

### Sémantique du gap_len

Le cross-token search ne suit un sibling link QUE si `gap_len == 0`.
C'est la garantie que les tokens sont contigus dans le texte — pas de
caractère entre eux, donc une substring query qui traverse la frontière
est valide.

Pour les gap_len > 0, les tokens sont séparés par des caractères dans le texte.
Une query "rag3 weaver" (avec espace) pourrait utiliser gap_len=1, mais c'est
du multi-token search classique (pas du cross-token). La sibling table sert
aussi pour ça — elle remplace partiellement le GapMap pour l'adjacency check.

### Cross-token search simplifié

```rust
fn cross_token_search(sfx_reader, query, resolver, ord_to_term, sibling_table):
    // 1. Falling walk : trouve les split candidates (premier token, n'importe quel SI)
    let candidates = sfx_reader.falling_walk(query)

    for cand in candidates:
        if cand.parent.si + cand.prefix_len != cand.parent.token_len:
            continue  // pas à la frontière du token

        let remainder = query[cand.prefix_len..]
        if remainder.is_empty():
            continue

        // 2. Suivre les sibling links — O(1) par lookup !
        let siblings = sibling_table.get(cand.parent.raw_ordinal)
        for sibling in siblings:
            if sibling.gap_len != 0:
                continue  // pas contigu → pas cross-token

            let next_text = ord_to_term(sibling.next_ordinal)

            if next_text.starts_with(remainder):
                // MATCH terminal! Le remainder est un préfixe du token suivant.
                // Resolve [cand.ordinal, sibling.next_ordinal] et verify adjacency.
                emit_result(cand, sibling.next_ordinal, remainder.len())

            else if remainder.starts_with(next_text):
                // Token suivant est plus court que le remainder
                // → chaîner : suivre le sibling du token suivant
                let sub_remainder = remainder[next_text.len()..]
                chain_search(sibling.next_ordinal, sub_remainder, ...)
```

### Multi-token chaînage (3+ tokens)

```
query = "rag3dbfromcore"
1. falling_walk → cand "rag3" (SI=0, prefix_len=4)
2. sibling_table[rag3] → [{next: "db", gap: 0}]
3. ord_to_term(db) = "db", remainder "dbfromcore".starts_with("db") ✓
   sub_remainder = "fromcore"
4. sibling_table[db] → [{next: "from", gap: 0}]
5. ord_to_term(from) = "from", "fromcore".starts_with("from") ✓
   sub_remainder = "core"
6. sibling_table[from] → [{next: "core", gap: 0}]
7. ord_to_term(core) = "core", "core".starts_with("core") ✓ → MATCH!
```

Chaîne finale : [rag3, db, from, core] — 4 ordinals à résoudre (pas 85).

### Complexité

| Opération | Coût |
|-----------|------|
| falling_walk | O(L) — une seule fois |
| sibling lookup par step | O(1) — index direct |
| ord_to_term par step | O(log N) — term dict binary search |
| Total chain walk | O(num_splits × log N) |
| Posting resolve | O(num_splits × avg_posting_size) |
| Comparé à graph/DP | O(L² × FST_walks) → **massive réduction** |

### Gestion des successeurs multiples

"get" peut être suivi de "Element", "Value", "Name"... dans différents docs.
La sibling table stocke la **liste** de tous les successeurs observés.

Au query time, pour chaque successeur on vérifie si son texte matche le remainder.
En pratique, un ordinal a rarement plus de 5-10 successeurs uniques → la liste
est courte et le check `starts_with` est trivial.

Si la liste est longue (token très fréquent), on peut la trier par texte et faire
un binary search sur le premier byte du remainder.

## Impact sur l'indexation

### SfxCollector

Le SfxCollector voit les tokens dans l'ordre d'apparition dans le document.
Il connaît les byte_from/byte_to de chaque token.

```rust
// Pendant end_value() ou end_doc() :
for i in 0..self.value_tokens.len() - 1 {
    let current = &self.value_tokens[i];
    let next = &self.value_tokens[i + 1];
    let gap_len = (next.byte_from - current.byte_to) as u16;

    // Stocker la paire (ordinal_current, ordinal_next, gap_len)
    // Les ordinals sont finalisés après le tri global (build()),
    // donc on stocke d'abord les intern_ids et on remappe après.
    self.sibling_pairs.insert((intern_id_current, intern_id_next), gap_len);
}
```

À `build()`, après le tri des tokens (intern_id → final ordinal), on remappe
les paires et on construit la sibling table.

### Merger

Le merger reçoit les sibling tables des segments à merger.
Les ordinals changent (re-numérotation globale) → remap via la table
de correspondance old_ordinal → new_ordinal (déjà utilisée pour les postings).

```rust
for (old_ord, siblings) in &old_sibling_table {
    let new_ord = ordinal_remap[old_ord];
    for sibling in siblings {
        let new_next = ordinal_remap[sibling.next_ordinal];
        new_sibling_table.add(new_ord, new_next, sibling.gap_len);
    }
}
```

### SfxPostResolverV2

Pas de changement — les postings restent les mêmes.

## Avantages vs approches précédentes

| | falling_walk simple | graph/DP | sibling links |
|---|---|---|---|
| Multi-split | ❌ 1 split max | ✅ mais lent | ✅ et rapide |
| Query time | O(L) | O(L² × FST) | O(L + splits × log N) |
| Allocations | Peu | Beaucoup (HashMap) | Zéro (pointer chase) |
| Ordinals resolved | Tous les candidats | 50-85 | 2-5 (chaîne exacte) |
| Faux positifs | Oui (splits parasites) | Oui (byte check aide) | Non (liens exacts) |
| Fuzzy | Via remainder walk | Difficile | Exact chaîne + fuzzy terminal |
| Stockage | 0 | 0 | +6 bytes/paire (~9MB/1M tokens) |
| WASM perf | ✅ | ❌ (allocations) | ✅ (pointer chase) |

## Bonus : remplacement partiel du GapMap

La sibling table avec `gap_len` contient implicitement une partie de l'information
du GapMap. Pour le cas `gap_len == 1` (espace), on sait qu'il y a exactement
un byte de séparation. On pourrait même stocker les bytes de gap directement
(pour les gaps courts ≤ 4 bytes) si nécessaire.

À terme, la sibling table + les gap bytes inline pourraient remplacer le GapMap
pour les cas simples, réduisant la taille de l'index.

## Plan d'implémentation

1. **SiblingTable reader/writer** : nouveau struct dans `src/suffix_fst/`
2. **SfxFileWriter/Reader** : ajouter le sibling_table_offset au header, lire/écrire la table
3. **SfxCollector** : collecter les paires (ordinal, next_ordinal, gap_len) pendant l'indexation
4. **SfxCollector::build()** : remapper les intern_ids et sérialiser la sibling table
5. **Merger** : re-mapper les sibling links lors du merge
6. **`cross_token_search`** : suivre les sibling links au lieu du falling_walk multi-split
7. **Passer ord_to_term** depuis le term dict pour la vérification textuelle
8. Tests unitaires + benchmark .luce
