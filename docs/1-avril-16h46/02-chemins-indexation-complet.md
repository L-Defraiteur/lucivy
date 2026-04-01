# 02 — Chemins d'indexation : audit complet

Date : 1 avril 2026

## Vue d'ensemble

3 chemins écrivent des fichiers SFX :

| Chemin | Utilisé en prod | Déclenché par |
|--------|----------------|---------------|
| Segment initial (SfxCollector) | ✅ oui | flush de SegmentWriter |
| Merge DAG (sfx_dag.rs) | ✅ oui | commit → merge policy |
| Legacy merge (merger.rs) | ❌ dead code | anciennes API publiques |
| merge_sfx_deferred | ❌ dead code | jamais appelé |

---

## 1. Segment initial — SfxCollector → registry build()

### Chaîne d'appels

```
IndexWriter::add_document()
  → SegmentWriter::add_document()
    → SfxTokenInterceptor capture les tokens pendant l'indexation
    → collector.add_token() pour chaque token
  → SegmentWriter::finalize()     [quand mem_usage > budget]
    → SfxCollector::build()
      → SuffixFstBuilder.build()  [FST O(E log E)]
      → SfxBuildContext { tokens, postings, gapmap, sibling, sepmap }
      → for index in all_indexes():
          index.build(&ctx) → bytes
    → SegmentSerializer::write_sfx()           [.sfx]
    → SegmentSerializer::write_custom_index()   [7 registry files]
```

### Fichiers écrits

| Fichier | Source | Via registry |
|---------|--------|-------------|
| .sfx | SuffixFstBuilder + GapMap + SiblingTable | direct |
| .sfxpost | SfxPostIndex::build() | ✅ |
| .gapmap | GapMapIndex::build() | ✅ |
| .sibling | SiblingIndex::build() | ✅ |
| .posmap | PosMapIndex::build() | ✅ |
| .bytemap | ByteMapIndex::build() | ✅ |
| .termtexts | TermTextsIndex::build() | ✅ |
| .sepmap | SepMapIndex::build() | ✅ |

**C'est le seul chemin qui utilise le registry.** Zéro re-tokenisation :
l'intercepteur capture les tokens pendant l'indexation standard.

### Fichiers clés

- `src/indexer/segment_writer.rs:126-142` — création des SfxCollectors
- `src/indexer/segment_writer.rs:154-224` — finalize() avec DAG multi-champs
- `src/suffix_fst/collector.rs:253-357` — build() orchestre FST + registry

---

## 2. Merge DAG — commit_dag → merge_dag → sfx_dag

### Chaîne d'appels

```
IndexWriter::commit()
  → PreparedCommit::commit()
    → SegmentUpdater::schedule_commit_with_rebuild(rebuild_sfx=true)
      → handle_commit()
        → build_commit_dag()
          → PrepareNode         [merge policy → candidats]
          → MergeNode ×N        [pour chaque opération de merge]
            → build_merge_dag()
              → InitNode         [doc_id_mapping, fieldnorm]
              → PostingsNode ∥ StoreNode ∥ FastFieldsNode  [parallèle]
              → SfxNode          [pour chaque champ SFX]
                → build_sfx_dag()
                  → CollectTokensNode   [tokens uniques des term dicts sources]
                  → BuildFstNode ∥ CopyGapmapNode ∥ MergeSfxpostNode  [parallèle]
                  → ValidateGapmapNode
                  → ValidateSfxpostNode
                  → WriteSfxNode         [écrit TOUS les fichiers]
              → CloseNode        [ferme les writers]
          → FinalizeNode         [end_merge]
          → SaveMetasNode        [meta.json atomique]
          → GarbageCollectNode   [supprime orphelins]
          → ReloadNode           [reader recharge]
```

### Déclencheurs

1. **Automatique** : merge policy à chaque commit (LogMergePolicy par défaut)
2. **Manuel** : `IndexWriter::merge(&segment_ids)`
3. **Fast commit** : `commit_fast()` → `rebuild_sfx=false` → skip SfxNode

### WriteSfxNode — ce qu'il fait

Fichier : `src/indexer/sfx_dag.rs:262-362`

1. Reçoit en input : FST data, gapmap data, sfxpost data, sibling data, tokens
2. Écrit `.sfx` via `Segment::open_write_custom()`
3. Écrit `.sfxpost` via `Segment::open_write_custom()`
4. **Reconstruit posmap/bytemap/termtexts** depuis sfxpost + tokens (manuellement)
5. Écrit gapmap, sibling via `Segment::open_write_custom()`
6. **NE reconstruit PAS sepmap** ❌

### Helpers dans sfx_merge.rs

| Fonction | Utilisée par | Rôle |
|----------|-------------|------|
| `load_sfx_data()` | SfxNode (merge_dag) | Charge .sfx sources |
| `collect_tokens()` | CollectTokensNode | Tokens uniques via term dicts |
| `build_fst()` | BuildFstNode | Compile FST depuis tokens |
| `copy_gapmap()` | CopyGapmapNode | Copie + remap doc_ids |
| `merge_sfxpost()` | MergeSfxpostNode | N-way merge des postings |
| `merge_sibling_links()` | BuildFstNode | OR-merge des sibling tables |

### Fichiers écrits

| Fichier | Comment | Status |
|---------|---------|--------|
| .sfx | SfxFileWriter direct | ✅ |
| .sfxpost | write_custom_index | ✅ |
| .posmap | reconstruit depuis sfxpost + tokens | ✅ |
| .bytemap | reconstruit depuis sfxpost + tokens | ✅ |
| .termtexts | reconstruit depuis tokens | ✅ |
| .gapmap | copié + remappé | ✅ |
| .sibling | OR-merge | ✅ |
| **.sepmap** | — | **❌ MANQUANT** |

---

## 3. Legacy merge — merger.rs (DEAD CODE)

### Status

`merge_sfx_legacy()` est marqué `#[allow(dead_code)]` (ligne 942).
Appelé depuis `IndexMerger::write()` (ligne 597), qui est encore une API publique
mais **n'est plus appelé dans le flow normal commit_dag → merge_dag**.

Les 2 call sites dans `segment_updater.rs` (lignes 187 et 480) sont des
utilitaires publics pour outils offline (merge_indices, merge_filtered_segments).

### Ce qu'il faisait

Même étapes que le DAG mais séquentiel :
1. load_sfx_data
2. collect_tokens
3. build_fst + merge_sibling_links
4. copy_gapmap
5. merge_sfxpost
6. Reconstruit posmap/bytemap/termtexts manuellement
7. Write tout via write_custom_index

### Fichiers écrits : mêmes que DAG (sepmap manquant aussi)

---

## 4. merge_sfx_deferred — DEAD CODE

Fichier : `merger.rs:616-936`

Approche expérimentale : skip la compilation FST au merge, écrire un FST vide,
rebuilder le FST au prochain commit. Remplacé par le flag `rebuild_sfx` dans
`commit_fast()` qui est plus propre. Jamais appelé.

---

## 5. WASM (Emscripten)

Même chemin que natif :
```
lucivy_commit_async()  [extern "C"]
  → writer.commit()
    → commit_dag → merge_dag → SfxNode → WriteSfxNode
```

Le commit tourne sur un pthread dédié (limite ASYNCIFY stack).
Status communiqué via SharedArrayBuffer + Atomics polling.
Pas de chemin de merge spécial.

---

## 6. Résumé des gaps

### WriteSfxNode ne passe PAS par le registry

Le segment initial itère `all_indexes()` → `build()`. Parfait.

Le merge DAG dans WriteSfxNode reconstruit chaque fichier manuellement :
- sfxpost : écrit directement
- posmap/bytemap/termtexts : reconstruits depuis sfxpost + tokens
- gapmap/sibling : copiés/mergés par les nodes dédiés

**Il n'appelle jamais `SfxIndexFile::merge()`.**

### Conséquences

1. **Sepmap oublié** : pas de code pour le reconstruire dans WriteSfxNode
2. **Fragilité** : ajouter un nouveau fichier au registry ne suffit pas —
   il faut aussi l'ajouter manuellement dans WriteSfxNode
3. **Duplication** : la logique de reconstruction posmap/bytemap/termtexts
   existe dans WriteSfxNode ET dans les implémentations `build()` du registry

### Fix proposé

Remplacer la reconstruction manuelle dans WriteSfxNode par une boucle
sur `all_indexes()` :

```rust
// Dans WriteSfxNode::execute()
let merge_ctx = SfxMergeContext { ... };
for index in all_indexes() {
    let merged = index.merge(&source_bytes, &merge_ctx);
    write_file(segment, index.extension(), &merged)?;
}
```

Cela garantit que tout fichier du registry est automatiquement mergé,
y compris sepmap et tout futur ajout.

---

## 7. Fichiers source

| Fichier | Rôle |
|---------|------|
| `src/indexer/segment_writer.rs` | Création segment + SfxCollector |
| `src/indexer/commit_dag.rs` | Orchestration commit |
| `src/indexer/merge_dag.rs` | Structure DAG merge |
| `src/indexer/sfx_dag.rs` | Sous-DAG SFX par champ |
| `src/indexer/sfx_merge.rs` | Helpers merge (6 étapes) |
| `src/indexer/merger.rs` | Legacy merge (dead code) |
| `src/suffix_fst/collector.rs` | SfxCollector::build() |
| `src/suffix_fst/index_registry.rs` | Registry trait + all_indexes() |
| `bindings/emscripten/src/lib.rs` | WASM commit trigger |
