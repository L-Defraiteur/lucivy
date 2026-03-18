# Bench baseline — merge_sfx timing

Date : 17 mars 2026
Config : release, 5K docs, COMMIT_EVERY=500, rag3db clone

## Résumé des steps

| Step | Description | Gros merge (~40K tokens) | Moyen merge (~20K tokens) | Petit merge (~1K tokens) |
|------|------------|--------------------------|---------------------------|--------------------------|
| 1 | Collect tokens (alive check) | ~90ms | ~50ms | ~2ms |
| 2 | Build suffix FST | **~470ms** | **~220ms** | ~5ms |
| 3 | GapMap copy | ~2ms | ~1ms | ~0ms |
| 4 | sfxpost merge | ~195ms | ~70ms | ~3ms |
| **Total** | | **~757ms** | **~341ms** | **~10ms** |

## Répartition

- **Step 2 (build FST) = ~60%** du temps total → Phase B cible
- Step 4 (sfxpost merge) = ~25% → Phase C cible
- Step 1 (collect tokens) = ~12% → Phase A cible
- Step 3 (gapmap) = ~0.3% → négligeable

## Données brutes (gros merges extraits)

```
merge_sfx step1 (collect tokens): 93.5ms, 41883 unique tokens
merge_sfx step2 (build FST): 469.8ms
merge_sfx step3 (gapmap): 22.5ms
merge_sfx step4 (sfxpost merge): 195.1ms

merge_sfx step1 (collect tokens): 89.7ms, 38371 unique tokens
merge_sfx step2 (build FST): 379.5ms
merge_sfx step3 (gapmap): 2.2ms
merge_sfx step4 (sfxpost merge): 198.0ms

merge_sfx step1 (collect tokens): 151.5ms, 50904 unique tokens
merge_sfx step2 (build FST): 787.7ms
merge_sfx step4 (sfxpost merge): 700.5ms

merge_sfx step1 (collect tokens): 65.6ms, 34209 unique tokens
merge_sfx step2 (build FST): 325.5ms
merge_sfx step4 (sfxpost merge): 111.2ms

merge_sfx step1 (collect tokens): 89.1ms, 31610 unique tokens
merge_sfx step2 (build FST): 443.6ms
merge_sfx step4 (sfxpost merge): 147.2ms
```

## Index times avec merges forcés (COMMIT_EVERY=500)

```
Index time:  1-shard 3.54s  |  TA-4sh 4.63s  |  RR-4sh 3.71s
```

(Plus lent que sans merges car les merges bloquent l'indexation.)

## Ce que Phase B doit battre

Le build FST (step 2) sur un merge de ~40K tokens prend **~470ms**.
Objectif Phase B : <100ms (N-way merge sort O(E log N) au lieu de O(E log E)).


claude --resume c267404f-7e44-42f7-8b4c-7717ae0b16b2 

claude --resume d9be3d17-c972-4416-a522-7c57b26aaeae