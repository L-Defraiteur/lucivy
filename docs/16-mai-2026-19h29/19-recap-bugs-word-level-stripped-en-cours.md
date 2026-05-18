# Récap : tentatives word-level stripped — en cours

**Date** : 17 mai 2026  
**État** : 10 tests échouent, régression par rapport à la version chunk-level stripped

---

## Ce qu'on a fait

### 1. Bug sep_len overflow → FIXÉ ✓
- sep_len passé de 3 bits (max 7) à 8 bits (max 255)
- STI réduit à 12 bits (max 4095, suffisant avec MAX_TOKEN=8)
- own_len réduit à 14 bits (max 16383)

### 2. Content-aware overlap → PARTIELLEMENT FIXÉ
- Le collector calcule `content_overlap` : skip les pure-sep tokens, prendre les bytes du prochain token avec contenu
- Avait fixé 5 tests avec les chunk-level stripped entries
- Puis on est passé au word-level stripped → régressé

### 3. Word-level stripped (partition 0x02) → EN COURS, RÉGRESSÉ
- `builder_v3.rs` : nouvelle méthode `add_word_stripped(word_content, content_overlap, ...)`
- `collector_v3.rs` : construit `WordStrippedEntry` dans `add_value()` (groupement par word_id local au value)
- `SfxCollectorDataV3` : champ `word_stripped: Vec<WordStrippedEntry>`
- DAG et test harness : appellent `add_word_stripped` en plus de `add_token`

**Résultat** : 47 passent, 11 échouent (vs 52/6 avant le word-level)

### 4. Resolve relaxé → IMPLÉMENTÉ MAIS PAS TESTÉ
- `resolve_chains_v3_relaxed` avec PosMap + ByteMap
- `intermediates_are_pure_sep` vérifie via ByteMap que les tokens entre deux positions sont pure non-alphanum
- `bytes_in_ranges` ajouté au ByteMap
- Fallback `ByteOrdered` quand PosMap/ByteMap non disponibles

---

## Tests qui échouent (11)

| Test | Query | Texte | Cause probable |
|------|-------|-------|----------------|
| s3 | "mutexlock" | "mutex_lock" | word_stripped pas correctement construit ou pas dans le FST |
| s4 | "mutex lock" | "mutex_lock" | idem (stripped query "mutexlock") |
| s6 | "mutex__lock" | "mutex_lock" | idem |
| s7 | "mutexlock" | "mutex________lock" | word_stripped cross pure-sep tokens |
| f8 | "mutexlock" | "mutex________lock" | idem |
| fz7 | "mutexlock" d=0 | "mutex_lock" | routes vers contains_v3 → même pb |
| t6 | "ab" | "a________b" | word_stripped cross pure-sep |
| x11b | "nationalizationinit" | "internationalization________initialization" | cross-word + multi-chunk |
| x11d | "zationinitial" | idem | cross-word |
| x12b | "ab" | "a" + "_"×20 + "b" | cross pure-sep très long |

## Diagnostic probable

Le `build_word_stripped` dans `add_value` est probablement correct pour le groupement (word_id local au value). Mais l'ancien code chunk-level stripped a été SUPPRIMÉ du builder (`add_token_with_content_overlap` ne génère plus les entrées 0x02). Les word_stripped entries sont censées les remplacer, mais quelque chose ne marche pas.

Possibilités :
1. Le `first_intern_ord` dans WordStrippedEntry pointe vers le mauvais ordinal
2. Le `add_word_stripped` ne produit pas les bonnes clés dans le FST
3. L'ordinal passé au builder est l'intern_ord, mais le builder attend le final_ord → le DAG fait le remap mais le test harness aussi ?

## Fichiers modifiés (état actuel non committé)

```
src/suffix_fst/builder_v3.rs      — add_word_stripped, suppression chunk-level stripped
src/suffix_fst/collector_v3.rs    — WordStrippedEntry, build dans add_value, content_overlap dans TokenMetaV3
src/suffix_fst/bytemap.rs         — bytes_in_ranges
src/suffix_fst/briques/resolve.rs — resolve_chains_v3_relaxed, intermediates_are_pure_sep, AdjacencyMode
src/suffix_fst/briques/composite.rs — find_literal_v3_full avec PosMap/ByteMap
src/suffix_fst/briques/integration_tests.rs — 58 edge case tests
src/indexer/sfx_dag_v3.rs         — word_stripped dans merge + DAG
docs/16-mai-2026-19h29/16-18      — edge cases, bugs, analyse x11b
```

## Prochaine étape

Debug le cas s3 simple ("mutexlock" → "mutex_lock") pour vérifier que :
1. Le collector produit bien un WordStrippedEntry avec word_content="mutex", content_overlap="lo"
2. Le builder indexe bien "mutexlo" dans partition 0x02
3. fst_candidates_v3 avec strict_sep=false trouve bien "mutexlo" quand on cherche "mutexlock"

C'est le cas de base. Une fois qu'il marche, les autres devraient suivre.
