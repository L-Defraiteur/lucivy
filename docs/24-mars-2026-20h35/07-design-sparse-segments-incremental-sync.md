# Doc 07 — Design : segments WORM pour sparse_vector + incremental sync

Date : 24 mars 2026

## Problème

Le sparse index rewrite `sparse.mmap` en entier à chaque commit.
Pas de delta sync possible, pas de snapshot isolation, commit bloquant.

## Solution : segments immutables (même pattern que lucivy)

### Concept

Au lieu d'un seul fichier `sparse.mmap`, l'index est composé de **segments** :

```
sparse_index/
  meta.json                    ← liste des segments actifs
  seg_a1b2c3.mmap              ← segment immutable (posting lists)
  seg_a1b2c3.vectors.bin       ← vecteurs du segment
  seg_a1b2c3.dims.bin          ← dim mapping du segment
  seg_d4e5f6.mmap              ← autre segment
  seg_d4e5f6.vectors.bin
  seg_d4e5f6.dims.bin
```

### Lifecycle

1. **Insert** : accumule en RAM (comme aujourd'hui)
2. **Commit** : écrit un NOUVEAU segment (UUID), met à jour meta.json
3. **Search** : itère les posting lists de TOUS les segments, merge les scores
4. **Merge** : combine N segments en 1 (background, comme lucivy)
5. **Delete** : marque les IDs supprimés dans un bitset par segment (alive_bitset)

### Format segment

Identique au format mmap actuel — chaque segment EST un sparse.mmap :
```
[FileHeader]                    16 bytes
[DimHeader × num_dims]          16 bytes × N
[PostingEntry × total_entries]  16 bytes × M
```

Plus les side files vectors.bin + dims.bin.

### meta.json

```json
{
  "segments": [
    {
      "id": "a1b2c3",
      "num_vectors": 5000,
      "num_dims": 12000,
      "created_at": "2026-03-24T20:00:00Z"
    },
    {
      "id": "d4e5f6",
      "num_vectors": 3000,
      "num_dims": 11500,
      "created_at": "2026-03-24T20:05:00Z"
    }
  ],
  "deletes": {
    "a1b2c3": [42, 99, 1337],
    "d4e5f6": []
  }
}
```

### Search multi-segments

Le `SearchContext` actuel itère les posting lists d'un seul mmap.
Pour multi-segments, deux approches :

**A. Merge iterators** : pour chaque dimension, chaîner les iterators
de tous les segments. Le `SearchContext` ne change pas — il reçoit un
iterator qui est en fait un merge de N iterators.

```rust
// Pour chaque query dimension :
let iterators: Vec<MergePostingIter> = query.indices.iter()
    .map(|&dim| {
        let segment_iters = segments.iter()
            .filter_map(|seg| seg.posting_iter(dim));
        MergePostingIter::new(segment_iters)
    })
    .collect();
```

**B. Search per segment + merge results** : chercher dans chaque segment
séparément puis fusionner les top-K.

Option A est plus correcte (le WAND pruning fonctionne mieux sur l'ensemble)
mais plus complexe. Option B est plus simple et parallélisable.

**Recommandation** : Option B pour commencer (parallélisable via luciole
fan_out_merge). Option A pour optimiser plus tard.

### Merge de segments

Quand trop de segments s'accumulent (>N, ou taille totale > seuil) :
1. Lire les posting lists de tous les segments source
2. Filtrer les deletes
3. Écrire un nouveau segment avec toutes les entrées
4. Mettre à jour meta.json (remplacer N segments par 1)
5. Supprimer les anciens fichiers

Peut être fait en background (luciole PollNode ou task).

### Deletes

Deux options :
- **Bitset** : un `HashSet<u64>` par segment des IDs supprimés, stocké dans meta.json
- **Tombstone segment** : segment spécial qui liste les IDs supprimés

Le bitset dans meta.json est plus simple. Au search, skip les IDs dans le bitset.
Au merge, les deletes sont appliqués (entrées filtrées).

## Incremental sync (delta)

Avec des segments WORM, le delta est identique à lucivy (doc 05) :

```
Delta = {
  added_segments: [seg_d4e5f6.mmap + .vectors.bin + .dims.bin],
  removed_segments: ["old_seg_id"],
  meta: meta.json (nouveau)
}
```

Le client compare les segment IDs, télécharge les manquants, supprime les obsolètes.

## Luciole — ce que l'autre instance peut utiliser

| Feature luciole | Usage sparse segments |
|----------------|----------------------|
| **fan_out_merge** | Search parallèle multi-segments |
| **StreamDag** | Pipeline ingestion (insert → commit → merge) |
| **CheckpointStore** | Crash recovery du merge en cours |
| **PollNode** | Merge en background (coopératif, WASM compatible) |
| **Pool** | Workers de merge (si multi-segment merge en parallèle) |
| **SwitchNode** | Route search: 1 segment → direct, N segments → fan-out |

## Partage du travail

### Instance A (nous — lucivy)

1. Extraire le pattern sharding en quelque chose de réutilisable :
   - `ShardedSparseHandle` : round-robin insert, fan-out search, merge top-K
   - Utilise luciole Pool + fan_out_merge
   - Fonctionne avec le format actuel (1 SparseHandle par shard)

2. S'assurer que luciole est bien exportable en crate indépendant
   - README, exemples, Cargo.toml propre

### Instance B (sparse_vector)

1. Segmenter le `SparseIndex` :
   - `SparseSegment` : un mmap + vectors + dims (immutable)
   - `SegmentedSparseIndex` : Vec de segments + meta.json + deletes bitset
   - Commit crée un nouveau segment
   - Search merge les résultats de tous les segments

2. Incremental sync :
   - `export_delta(since_version)` → segments ajoutés/supprimés
   - `apply_delta(delta)` → appliquer côté client

3. Merge background :
   - Combiner N segments en 1
   - Via luciole PollNode pour WASM compat

### Interfaces partagées

```rust
// Les deux instances utilisent :
luciole::Pool<M>                    // workers
luciole::fan_out_merge()            // search parallèle
luciole::StreamDag                  // pipeline topology
luciole::CheckpointStore            // crash recovery
lucivy_core::blob_store::BlobStore  // persistance (déjà partagé)
```

## Étapes d'implémentation (instance B)

```
Phase 1 : SparseSegment
  - SparseSegment = existing MmapPostingData + vectors + dims
  - Segment UUID, immutable après création
  - SparseSegment::create(entries) → fichiers on disk
  - SparseSegment::open(path) → mmap

Phase 2 : SegmentedSparseIndex
  - meta.json : liste des segments actifs + deletes
  - Commit : écrire nouveau segment, update meta
  - Search : fan-out par segment, merge top-K
  - Delete : ajouter ID au bitset dans meta

Phase 3 : Merge
  - Combiner N segments en 1, appliquer deletes
  - Background via PollNode ou thread

Phase 4 : Incremental sync
  - Delta export/import (même pattern que lucivy doc 05)
  - Version = hash du meta.json
```

## Risques

- **Search multi-segments plus lent** que single-mmap pour petits index
  (overhead d'itérer N mmaps au lieu de 1). Mitigation : merge agressif,
  garder peu de segments actifs.
- **WAND pruning** : fonctionne par segment (option B), pas globalement.
  Acceptable — le pruning reste efficace par segment, et le merge top-K final
  est O(N × K).
- **Complexité** : passer de 1 fichier à N fichiers + meta + deletes + merge.
  Mais le pattern est prouvé (lucivy fait exactement ça).
