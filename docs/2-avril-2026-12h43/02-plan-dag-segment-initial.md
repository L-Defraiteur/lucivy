# 02 — Plan : DAG pour le segment initial (SfxCollector)

Date : 2 avril 2026

## Situation actuelle

Le SfxCollector::build() fait tout séquentiellement :

```
build()
  1. Sort tokens (intern → final ordinals)       ~séquentiel
  2. SuffixFstBuilder.build()                     ~O(E log E), le plus lourd
  3. SiblingTableWriter                           ~léger
  4. SfxFileWriter (FST + gapmap + sibling)       ~sérialisation
  5. SfxPostWriterV2                              ~séquentiel
  6. build_derived_indexes()                      ~single-pass (posmap, bytemap, termtexts, sepmap)
```

Les étapes 2, 3, 5 sont indépendantes et pourraient tourner en parallèle.

## Proposition : DAG de build SFX

```
sort_tokens ──┬── build_fst ─────────────────┐
              ├── build_sibling ──────────────┼── write_sfx
              ├── build_sfxpost ─────────────┤
              │                               │
              └── (gapmap déjà construit) ────┘
                                              │
                                    build_derived_indexes
                                    (posmap, bytemap, termtexts, sepmap)
```

### Nodes

| Node | Input | Output | Parallélisable |
|------|-------|--------|----------------|
| SortTokensNode | SfxCollector data | tokens (BTreeSet), intern_to_final | source node |
| BuildFstNode | tokens | (fst_data, parent_data) | oui |
| BuildSiblingNode | tokens, sibling_pairs, intern_to_final | sibling_data | oui |
| BuildSfxPostNode | tokens, token_postings, intern_to_final | sfxpost_data | oui |
| WriteSfxNode | fst, sibling, gapmap, sfxpost, tokens | fichiers écrits | sink node |

### Ce qui change

- `SfxCollector::build()` retourne un `Dag` au lieu de `SfxBuildOutput`
- Le caller (segment_writer.rs) exécute le DAG
- WriteSfxNode appelle `build_derived_indexes()` (déjà unifié)
- Le DAG du segment initial a la **même structure** que le DAG de merge

### Ce qui ne change pas

- La collecte de tokens pendant l'indexation (SfxTokenInterceptor, add_token)
- Le GapMap est toujours construit progressivement pendant l'indexation
- Le SepMap est toujours construit via build_derived_indexes

### Avantages

1. **Parallélisme** : FST build (le plus lourd) tourne en parallèle avec sfxpost + sibling
2. **Cohérence** : même architecture DAG pour initial et merge
3. **Composabilité** : les nodes sont réutilisables (BuildFstNode existe déjà dans sfx_dag.rs)

### Réutilisation des nodes existants

Le merge DAG (sfx_dag.rs) a déjà :
- `BuildFstNode` — construit FST depuis tokens
- `WriteSfxNode` — écrit .sfx + appelle build_derived_indexes

Pour le segment initial, on peut réutiliser `BuildFstNode`. Les autres nodes sont spécifiques (le merge a CopyGapmap/MergeSfxpost, le build initial a BuildSfxPost/BuildSibling depuis les données brutes).

### Alternative : factoriser les nodes communs

| Node | Merge DAG | Build DAG | Factorisable |
|------|-----------|-----------|-------------|
| BuildFstNode | ✅ depuis tokens | ✅ depuis tokens | **oui, identique** |
| WriteSfxNode | ✅ | ✅ | **oui, identique** |
| CollectTokensNode | depuis term dicts | depuis SfxCollector | non (sources différentes) |
| Sfxpost | MergeSfxpostNode (N-way merge) | BuildSfxPostNode (depuis raw data) | non |
| Gapmap | CopyGapmapNode (copy+remap) | déjà construit | non |
| Sibling | MergeSiblingLinksNode (OR-merge) | BuildSiblingNode (depuis pairs) | non |

→ `BuildFstNode` et `WriteSfxNode` sont directement réutilisables.

### Étapes d'implémentation

1. Extraire les données brutes du SfxCollector dans une struct `SfxCollectorData`
   (tokens, postings, gapmap, sibling_pairs, intern_to_final)
2. Créer `BuildSfxPostNode` et `BuildSiblingNode` dans sfx_dag.rs
3. Créer `build_initial_sfx_dag()` qui compose les nodes
4. Modifier `segment_writer.rs:finalize()` pour exécuter le DAG
5. Simplifier `SfxCollector::build()` → `SfxCollector::into_data()`
   (retourne les données brutes, le DAG fait le reste)
