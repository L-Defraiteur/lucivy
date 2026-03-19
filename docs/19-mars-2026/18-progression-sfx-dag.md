# Doc 18 — Progression : merge_sfx sub-DAG

Date : 19 mars 2026

## Fait

### sfx_merge.rs — 6 fonctions standalone
Extraites de la monolithique `merge_sfx` (230 lignes → 6 fonctions) :
1. `load_sfx_data` — charger les .sfx de chaque segment source
2. `collect_tokens` — tokens uniques (avec gestion deletes)
3. `build_fst` — construire le FST suffix
4. `copy_gapmap` — copier les gapmaps par doc en merge order
5. `merge_sfxpost` — merger les postings avec remapping doc_id
6. `validate_gapmap` — valider l'intégrité
7. `write_sfx` — assembler et écrire .sfx + .sfxpost

### sfx_dag.rs — DAG prêt (pas encore branché)
```
collect_tokens ──┬── build_fst ──────────────┐
                 ├── copy_gapmap ── validate ─┼── write_sfx
                 └── merge_sfxpost ───────────┘
```
build_fst, copy_gapmap, merge_sfxpost sont PARALLÈLES.

### merger.rs — thin wrapper
merge_sfx appelle maintenant les 6 fonctions standalone.
Même comportement, même tests (1188 pass).

## Reste à faire

### Brancher sfx_dag dans MergeState
- step_sfx() construit et exécute le sfx DAG
- Problème : le serializer est dans MergeState (&mut self)
- Solution : le prêter au DAG via Arc<Mutex<Option<Serializer>>>
  (même pattern que WriteSfxNode)
- Les readers aussi : clonés ou Arc-wrappés dans SfxContext

### Observabilité résultante
Chaque merge aura dans ses métriques :
```
merge_0 120ms
  init_ms=1 postings_ms=15 store_ms=3 fast_fields_ms=2 close_ms=5
  sfx_ms=94
    sfx.collect_tokens   12ms  unique_tokens=8500
    sfx.build_fst        35ms  fst_bytes=102400
    sfx.copy_gapmap      18ms  docs=5000 gapmap_bytes=1048576
    sfx.validate          1ms  errors=0
    sfx.merge_sfxpost    22ms  sfxpost_bytes=512000
    sfx.write             6ms
```

Et on peut tapper `copy_gapmap → validate` pour capturer
les bytes bruts en cas de bug.
