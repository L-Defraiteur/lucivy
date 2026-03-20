# Doc 06 — Progression : pipeline unifié + suppression stemming

Date : 20 mars 2026

## Ce qui a été fait

### Pipeline commit/merge unifié (doc 05 implémenté)

**Supprimé (~780 lignes) :**
- `ActiveMerge`, `ExplicitMerge` structs
- `active_merge`, `explicit_merge`, `pending_merges`, `segments_in_merge` dans SegmentUpdaterState
- `drain_all_merges()`, `do_end_merge()`, `handle_start_merge()`, `start_next_incremental_merge()`
- `schedule_merge_step()`, `track_segments()`, `untrack_segments()`, `enqueue_merge_candidates()`
- `finish_explicit_merge()`, `finish_incremental_merge()`, `emit_step_completed()`
- `SuMergeStepMsg`, `SuDrainMergesMsg` et leurs handlers
- `gc_protected_segments` (hack `0..10` field_ids dans list_files)

**Nouveau design :**
- `handle_commit()` : boucle cascade — collecte merge candidates, exécute DAG, re-check, repeat
- `handle_merge()` : exécute un DAG avec le merge op spécifique
- `wait_merging_threads()` : no-op (plus rien à drainer)
- Merge candidates : pool unifié (committed + uncommitted ensemble)
- Un seul chemin pour tout : le DAG

### Bug merge_sfxpost corrigé

Le check d'erreur Phase 3 n'était pas dans un `else` :
```rust
// AVANT (bug) — s'exécute toujours, même avec sfxpost présent
if let Some(reader) = sfxpost_reader { ... }
if token_to_ordinal[seg_ord].contains_key(...) { return Err(...); }

// APRÈS — s'exécute seulement quand sfxpost absent
if let Some(reader) = sfxpost_reader { ... }
else if token_to_ordinal[seg_ord].contains_key(...) { return Err(...); }
```

Ce bug causait : merge retourne `Done(None)` → `end_merge(ids, None)` → segments source supprimés sans remplacement → **données perdues**.

### step() ne swallows plus les erreurs

```rust
// AVANT — erreur silencieuse
pub fn step(&mut self) -> StepResult {
    match self.do_step() {
        Err(e) => { warn!("..."); StepResult::Done(None) }  // DONNÉES PERDUES
    }
}

// APRÈS — erreur propagée
pub fn step(&mut self) -> crate::Result<StepResult> {
    match self.do_step() {
        Err(e) => Err(e),  // l'appelant gère
    }
}
```

### Suppression du stemming (Phase 4)

- Supprimé `sfx_raw_analyzer: Option<TextAnalyzer>` de SegmentWriter
- Supprimé le branching `use_double_tok` et le chemin double tokenization
- Un seul chemin : `SfxTokenInterceptor` capture les tokens pendant le BM25 indexing
- Le stemming n'a pas de sens pour du code search

## État actuel

- **1194 tests pass, 0 fail**
- **Net : -779 lignes** (344 ajoutées, 879 supprimées)
- Branche : `feature/luciole-dag`
- Commit : `d108c42`

## Phases complétées (doc 01)

| Phase | Status |
|-------|--------|
| 1. SegmentComponent natif | ✅ |
| 2. Validation systématique | ✅ |
| 3. Merge fiable | ✅ (bug corrigé) |
| 4. Tokenization unique | ✅ (stemming supprimé) |
| 5. GC propre | ✅ (gc_protected_segments supprimé, GC fiable dans le DAG) |

## Prochaines étapes

### Brancher sfx_dag.rs dans MergeState

Le sfx_dag.rs existe (nodes pour build_fst, copy_gapmap, merge_sfxpost en parallèle)
mais n'est pas branché. Le merge SFX est séquentiel dans step_sfx().

### Autres DAG-ifications possibles

1. **SFX merge → sous-DAG** : build_fst ∥ copy_gapmap ∥ merge_sfxpost (sfx_dag.rs)
2. **Segment writing** : paralléliser l'indexation par champ
3. **Index opening** : charger les SegmentReaders en parallèle

### Bench 5K

Lancer `LUCIVY_VERIFY=1` pour vérifier que les 16 docs manquants du contains search sont trouvés.
