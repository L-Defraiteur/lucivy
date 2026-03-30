# 15 — Audit : chemins d'indexation et usage du registre

Date : 29 mars 2026

## Contexte

Le registre (`all_indexes()` dans `index_registry.rs`) centralise la construction
et l'écriture de tous les fichiers SFX annexes : sfxpost, posmap, bytemap,
termtexts, gapmap, sibling. Chaque `SfxIndexFile` impl a un `build()` et un
`merge()`.

Le problème découvert : **3 chemins de merge sur 3 ne passaient pas par le registre**,
donc les segments mergés n'avaient ni posmap ni termtexts. Le WASM (qui merge
automatiquement via merge policy) était cassé pour le fuzzy.

## Les 5 chemins d'écriture SFX identifiés

### 1. Indexation initiale — `segment_writer.rs` `finalize()`

**Fichier** : `src/indexer/segment_writer.rs` lignes 154-224

**Quand** : chaque commit crée un nouveau segment.

**Comment** :
- `SfxCollector::build()` construit le .sfx + appelle `all_indexes()` via
  `SfxBuildContext` → produit `SfxBuildOutput { sfx, registry_files }`
- `write_output` helper écrit :
  - `serializer.write_sfx()` pour le .sfx
  - `serializer.write_custom_index()` pour chaque registry file
- Deux sous-chemins : séquentiel (≤1 champ text) et parallèle DAG (>1 champ)
- Les deux utilisent le même `write_output` helper

**Registre** : OUI, correctement utilisé via `SfxCollector::build()`

**Fichiers écrits** : sfx, sfxpost, posmap, bytemap, termtexts, gapmap, sibling

---

### 2. Merge DAG — `sfx_dag.rs` `WriteSfxNode`

**Fichier** : `src/indexer/sfx_dag.rs` lignes 262-322

**Quand** : merge automatique déclenché par la merge policy (le chemin principal
pour les merges en production, utilisé par le WASM et le Python binding).

**Comment** :
- `SfxNode` dans `merge_dag.rs` appelle `build_sfx_dag()` par champ
- Le sous-DAG a des nodes : `CollectTokensNode`, `BuildFstNode`,
  `CopyGapmapNode`, `MergeSfxpostNode`, `MergeSiblingsNode`, `WriteSfxNode`
- `WriteSfxNode` reçoit fst, gapmap, sfxpost, siblings, tokens
- Il reconstruit posmap/bytemap/termtexts depuis le sfxpost sérialisé

**Registre** : NON — ne passait PAS par le registre avant ce fix.
Écrivait seulement .sfx + .sfxpost.

**Fix appliqué** : `WriteSfxNode` reconstruit maintenant posmap, bytemap,
termtexts depuis le sfxpost data (via `SfxPostReaderV2`), et écrit gapmap +
sibling comme fichiers séparés. Tout via `segment.open_write_custom()`.

**Fichiers écrits (après fix)** : sfx, sfxpost, posmap, bytemap, termtexts,
gapmap, sibling

**Note** : ne passe toujours pas par `all_indexes()` — reconstruit manuellement.
Acceptable car le merge a des données intermédiaires (sfxpost sérialisé)
qui ne correspondent pas au `SfxBuildContext` du registre.

---

### 3. Merge legacy — `merger.rs` `write()` (N-way merge path)

**Fichier** : `src/indexer/merger.rs` lignes 685-920

**Quand** : appelé par `segment_updater.rs` quand le merge n'utilise PAS le
merge DAG (ex: `drain_merges()` dans le Python binding, ou anciens index).

**Comment** :
- N-way merge sort sur les term dicts des segments sources
- Construit sfxpost, posmap, bytemap, termtexts, gapmap, sibling manuellement
- Écrit via `serializer.write_custom_index()` pour chaque fichier

**Registre** : NON — ne passait PAS par le registre avant ce fix.
Utilisait `serializer.write_sfxpost()`, `write_posmap()`, `write_bytemap()`
(méthodes legacy). Pas de termtexts.

**Fix appliqué** : remplacé par `write_custom_index()` pour tout. Ajouté
`TermTextsWriter` dans la boucle N-way. Ajouté écriture gapmap + sibling
comme fichiers séparés.

**Fichiers écrits (après fix)** : sfx, sfxpost, posmap, bytemap, termtexts,
gapmap, sibling

---

### 4. Merge legacy fallback — `merger.rs` `write()` (fallback path)

**Fichier** : `src/indexer/merger.rs` lignes 1020-1090

**Quand** : fallback quand le N-way merge ne s'applique pas (ex: pas de
sfxpost dans les segments sources, utilise `sfx_merge::write_sfx()`).

**Comment** :
- Utilise `sfx_merge::write_sfx()` pour .sfx + .sfxpost
- Reconstruit posmap et bytemap depuis le term dict + sfxpost des segments
- Construit termtexts depuis `unique_tokens`

**Registre** : NON — ne passait PAS par le registre avant ce fix.

**Fix appliqué** : `write_posmap` et `write_bytemap` remplacés par
`write_custom_index`. Ajouté termtexts. `sfx_merge::write_sfx()` aussi
fixé pour utiliser `write_custom_index` pour le sfxpost.

**Fichiers écrits (après fix)** : sfx (via sfx_merge), sfxpost, posmap,
bytemap, termtexts

**Manque encore** : gapmap et sibling séparés (seulement dans le .sfx).
Acceptable pour l'instant car ce path est un fallback legacy.

---

### 5. `sfx_merge.rs` `write_sfx()`

**Fichier** : `src/indexer/sfx_merge.rs` lignes 440-465

**Quand** : appelé par le merge legacy fallback (chemin 4) et potentiellement
par `merge_sfx_deferred`.

**Comment** :
- Construit le `SfxFileWriter` avec FST + parent lists + gapmap + sibling
- Écrit le .sfx
- Écrit le .sfxpost si présent

**Registre** : NON — utilisait `serializer.write_sfxpost()` avant ce fix.

**Fix appliqué** : remplacé par `serializer.write_custom_index()`.

**Fichiers écrits** : sfx, sfxpost

---

## Tableau récapitulatif

| Chemin | Fichier | Utilisé par | Registre | Fichiers écrits |
|--------|---------|-------------|----------|-----------------|
| 1. Indexation initiale | segment_writer.rs | Tout commit | OUI (via SfxCollector::build) | Tous les 7 |
| 2. Merge DAG | sfx_dag.rs | Merge auto (WASM, prod) | Reconstruit manuellement (fixé) | Tous les 7 |
| 3. Merge N-way | merger.rs (legacy) | drain_merges (Python) | write_custom_index (fixé) | Tous les 7 |
| 4. Merge fallback | merger.rs (fallback) | Vieux index sans sfxpost | write_custom_index (fixé) | 5/7 (pas gapmap/sibling séparés) |
| 5. sfx_merge helper | sfx_merge.rs | Appelé par 4 | write_custom_index (fixé) | sfx + sfxpost |

## Ce qui reste à faire

1. **Unifier** : idéalement tous les chemins de merge devraient utiliser le
   registre via `SfxIndexFile::merge()` au lieu de reconstruire manuellement.
   C'est un refactor futur — pour l'instant les 3 chemins principaux écrivent
   tous les fichiers correctement.

2. **Chemin 4 (fallback)** : manque gapmap et sibling séparés. Acceptable car
   c'est un fallback pour les vieux index et le .sfx contient encore ces
   données inline (backward compat).

3. **Supprimer les méthodes legacy** : `write_sfxpost()`, `write_posmap()`,
   `write_bytemap()` dans `segment_serializer.rs` ne sont plus utilisées.
   À supprimer dans un cleanup futur.

4. **Tests** : le test `test_fuzzy_ground_truth` valide le chemin 1 (indexation)
   + merge via `drain_merges()` (chemin 3). Le WASM valide le chemin 2
   (merge DAG). Ajouter un test natif qui force le merge DAG path.
