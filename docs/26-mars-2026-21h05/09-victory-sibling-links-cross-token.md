# Doc 09 — Victory : sibling links cross-token search

Date : 27 mars 2026
Branche : `feature/cross-token-search`

## Résultat

Cross-token substring search fonctionne sur 5k docs (clone rag3db) en **15ms**.
Toutes les queries exact testées avec succès.

## Benchmarks

### .luce (855 docs, 6 segments)

| Query | Temps |
|-------|-------|
| weaver | 0.8ms |
| rag3weaver | 0.93ms |
| rag3w | 0.86ms |
| rag3db | 0.59ms |
| getElementById | 0.46ms |

### Clone rag3db (5k+ docs)

~15ms pour les queries cross-token.

## Historique des approches

| Approche | getElementById (natif) | getElementById (WASM) | Problèmes |
|----------|----------------------|---------------------|-----------|
| Single-split (falling_walk + prefix_walk) | 1.6ms | ~50ms | Pas de multi-split |
| Graph multi-split (worklist) | **8.8s** | OOM | Explosion combinatoire |
| Graph dedupliqué (unique remainders) | 10ms | ~500ms | Trop d'ordinals resolved |
| DP avec ord_to_term | 22ms | non testé | Trop de FST walks pour collecter tokens |
| **Sibling links** | **0.46ms** | **~15ms** | ✅ |

## Ce qui a été implémenté

### Nouveaux fichiers
- `src/suffix_fst/sibling_table.rs` — SiblingTableWriter/Reader, SiblingEntry

### Fichiers modifiés

| Fichier | Changement |
|---------|-----------|
| `src/suffix_fst/mod.rs` | Export sibling_table module |
| `src/suffix_fst/collector.rs` | Collecte paires sibling (intern_id, next_intern_id, gap_len) dans end_value(), remap dans build() |
| `src/suffix_fst/file.rs` | SfxFileWriter.with_sibling_data(), SfxFileReader.sibling_table(), sibling table entre parent list et gapmap |
| `src/indexer/sfx_merge.rs` | merge_sibling_links() — remap old→new ordinals |
| `src/indexer/sfx_dag.rs` | MergeSiblingLinksNode, WriteSfxNode accepts siblings port |
| `src/query/phrase_query/suffix_contains.rs` | cross_token_search_with_terms() suit les sibling links, byte continuity check |
| `src/query/phrase_query/suffix_contains_query.rs` | run_sfx_walk() prend ord_to_term, prescan+scorer passent le term dict |
| `lucivy_core/src/search_dag.rs` | run_sfx_walk() avec ord_to_term=None |
| `src/suffix_fst/stress_tests.rs` | Tests corrigés pour byte continuity |

### Concept

Chaque token indexé stocke la liste de ses **successeurs possibles** observés
pendant l'indexation, avec le gap_len (bytes de séparation).

```
sibling_table[ordinal("rag3")] = [
    SiblingEntry { next_ordinal: ordinal("db"), gap_len: 0 },
    SiblingEntry { next_ordinal: ordinal("weaver"), gap_len: 0 },
]
```

Au query time, le cross-token search suit les pointeurs O(1) au lieu de
reconstruire les relations via FST walks.

### Algorithme

```
1. falling_walk(query) → premier split (n'importe quel SI)
2. sibling_table[ordinal] → successeurs contigus (gap_len == 0)
3. ord_to_term(next_ordinal) → texte du token suivant
4. remainder.starts_with(next_text) → chaîner ou terminal
5. Resolve seulement les ordinals de la chaîne valide
6. Adjacency check avec byte continuity (Vec trié + partition_point)
```

## État du fuzzy

Le fuzzy contains est **cassé** actuellement — le cross_token_search_with_terms
ne gère pas le fuzzy. Mais avec les sibling links en place, le fuzzy devrait
être facile à ajouter :

- **Fuzzy terminal** : le dernier token de la chaîne peut être matché en fuzzy
  (`next_text` fuzzy-matches remainder au lieu de starts_with)
- **Fuzzy intermédiaire** : chaque step de la chaîne pourrait tolérer des edits
  (mais c'est plus complexe et rarement utile)
- **startsWith** : unifier avec contains en forçant SI=0 pour le premier split

## Prochaines étapes

1. Fuzzy sur le terminal de la chaîne sibling
2. Unifier startsWith avec contains (SI=0 constraint)
3. Regex cross-token via sibling links (au lieu de RegexContinuationQuery)
4. Benchmark sur corpus plus gros (90K docs Linux kernel)
