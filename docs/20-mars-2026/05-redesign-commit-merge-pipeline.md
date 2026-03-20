# Doc 05 — Redesign : un seul pipeline commit/merge via DAG

Date : 20 mars 2026

## Le problème

Le code actuel est un empilement de 4 couches d'adaptations :

1. **Tantivy original** : commit → segment_manager.commit() → save_metas → gc
2. **+ merges** : active_merge / explicit_merge / pending_merges, state machine avec SuMergeStepMsg en self-scheduling
3. **+ luciole** : drain_all_merges() avant le DAG (ancien chemin) puis commit DAG (nouveau chemin)
4. **+ SFX** : merge_sfx parallélisé dans MergeState::step()

Résultat : `handle_commit_dag()` fait `drain_all_merges()` (qui appelle `do_end_merge()` → `save_metas()` → `gc()`) PUIS exécute un commit DAG qui fait encore `save_metas()` → `gc()`. Le `segment_manager.commit()` dans le PrepareNode efface le segment mergé ajouté par `end_merge()`.

**Bug concret** : les 20 tests d'aggregation qui mergent → le segment mergé est créé par `drain_all_merges`, puis effacé par `segment_manager.commit()` dans le PrepareNode, puis le GC supprime ses fichiers. Résultat : données perdues, aggregation retourne Null.

## Le design propre

### Principe : un seul chemin, le DAG

Tout commit et tout merge passe par le même DAG. Pas d'état machine à côté. Pas de double save_metas. Pas de double GC.

```
commit(opstamp, payload):
  1. candidates = merge_policy.compute()
  2. dag = build_commit_dag(candidates, opstamp, payload)
  3. execute_dag(dag)

merge(segment_ids):  // explicit merge API
  1. op = MergeOperation::new(segment_ids)
  2. dag = build_commit_dag([op], opstamp, payload)
  3. execute_dag(dag)

wait_merging_threads():
  // Plus rien à drainer — les merges sont synchrones dans le DAG
  // Juste shutdown les workers
```

### Le DAG (inchangé dans sa structure)

```
prepare ──┬── merge_0 ──┐
          ├── merge_1 ──┼── finalize ── save ── gc ── reload
          └── merge_2 ──┘
```

Quand il n'y a pas de merges (la majorité des commits) :

```
prepare ── save ── gc ── reload
```

Le DAG existe déjà et fonctionne. C'est le code AUTOUR qui est le problème.

## Ce qui dégage

### Dans SegmentUpdaterState

```rust
// SUPPRIMÉ — plus de state machine de merge
active_merge: Option<ActiveMerge>,
explicit_merge: Option<ExplicitMerge>,
pending_merges: VecDeque<MergeOperation>,
segments_in_merge: HashSet<SegmentId>,

// SUPPRIMÉ — plus de méthodes de merge hors-DAG
fn drain_all_merges(&mut self)
fn do_end_merge(&mut self, ...)
fn handle_start_merge(&mut self, ...)
fn start_next_incremental_merge(&mut self, ...)
fn schedule_merge_step(&mut self, ...)
fn track_segments(&mut self, ...)
fn untrack_segments(&mut self, ...)
fn enqueue_merge_candidates(&mut self, ...)
```

### Dans les messages

```rust
// SUPPRIMÉ — le merge ne passe plus par des messages auto-schedulés
SuMergeStepMsg          // self-scheduling loop
SuStartMergeMsg         // explicit merge start (remplacé par inline DAG)

// SIMPLIFIÉ
SuDrainMergesMsg        // renommé ou absorbé dans wait_merging_threads
```

### Structs

```rust
// SUPPRIMÉ
struct ExplicitMerge { merge_operation, state, start_time, reply }
struct ActiveMerge { merge_operation, state, start_time }
```

## Ce qui reste / change

### SegmentUpdaterState simplifié

```rust
struct SegmentUpdaterState {
    shared: Arc<SegmentUpdaterShared>,
    // Plus de merge state — tout est dans le DAG
}
```

### Handlers simplifiés

```rust
// SuCommitMsg → handle_commit(opstamp, payload)
fn handle_commit(&mut self, opstamp: Opstamp, payload: Option<String>) -> Result<Opstamp> {
    let candidates = self.collect_merge_candidates();
    let dag = build_commit_dag(self.shared.clone(), candidates, opstamp, payload)?;
    let result = execute_dag(&mut dag, None)?;
    eprintln!("{}", result.display_summary());
    Ok(opstamp)
}

// SuStartMergeMsg → handle_merge(segment_ids)
fn handle_merge(&mut self, segment_ids: Vec<SegmentId>) -> Result<()> {
    let meta = self.shared.load_meta();
    let op = MergeOperation::new(meta.opstamp, segment_ids);
    let dag = build_commit_dag(self.shared.clone(), vec![op], meta.opstamp, meta.payload)?;
    let result = execute_dag(&mut dag, None)?;
    eprintln!("{}", result.display_summary());
    Ok(())
}

// SuDrainMergesMsg → plus rien à drainer
// wait_merging_threads() → juste shutdown
```

### PrepareNode corrigé

Le PrepareNode actuel fait `purge_deletes()` + `segment_manager.commit()` + `start_merge()`.

Ça reste correct QUAND il n'y a pas de `drain_all_merges` avant. Le bug c'est le double-traitement, pas le PrepareNode lui-même.

Avec le redesign, `drain_all_merges` n'existe plus → PrepareNode est le seul à toucher au segment_manager → plus de conflit.

### merge() dans IndexWriter

```rust
// Avant : envoie SuStartMergeMsg, le merge tourne en background via messages
// Après : envoie SuStartMergeMsg, le handler exécute le DAG inline (synchrone)
pub fn merge(&mut self, segment_ids: &[SegmentId]) -> Result<Option<SegmentMeta>> {
    // Synchrone : le DAG s'exécute entièrement dans le handler
    let (env, rx) = SuStartMergeMsg.into_request_with_local(
        MergeOperation::new(self.committed_opstamp, segment_ids.to_vec())
    );
    self.segment_updater.actor_ref.send(env)?;
    rx.wait_cooperative()?;
    Ok(None)
}
```

### wait_merging_threads()

```rust
pub fn wait_merging_threads(self) -> Result<()> {
    // Plus de merges à drainer — tout est synchrone dans le DAG
    // Juste shutdown les workers
    let _ = self.worker_pool.broadcast(|| IndexerShutdownMsg.into_envelope());
    Ok(())
}
```

## Le commit incrémental (merge policy)

Aujourd'hui le merge policy auto-trigger des merges après chaque commit. Le flow :

1. `commit()` → segments ajoutés au manager
2. Merge policy évalue → peut-être 1 ou 2 merge ops
3. Merges dans le DAG (parallèle si plusieurs)

Ça reste pareil. La différence c'est que tout est dans le même DAG, pas dans un state machine à côté.

Si le merge policy retourne des candidats, ils sont dans le DAG. Sinon, le DAG est juste prepare → save → gc → reload.

## Les merges cascade

Aujourd'hui : après un merge, le merge policy peut trouver de nouveaux candidats (ex: 2 segments de 100 docs mergés → 1 segment de 200 docs → maintenant il y a assez de segments pour un autre merge).

Avec le DAG : un seul passage de merge policy avant le DAG. Si après exécution il y a de nouveaux candidats, il faudra un autre commit.

Options :
1. **Ignorer** — les cascades se feront au prochain commit naturel (simple, suffisant en pratique)
2. **Boucle** — après le DAG, re-check merge policy, si candidats → nouveau DAG (plus agressif)
3. **Cascade dans le DAG** — FinalizeNode re-check et ajoute des MergeNodes (complexe, pas nécessaire)

L'option 1 est la plus simple et la plus propre. Si on veut forcer un merge complet, `merge(&all_segment_ids)` le fait explicitement.

## Plan d'implémentation

### Étape 1 : simplifier handle_commit_dag → handle_commit

1. Supprimer l'appel à `drain_all_merges()` dans `handle_commit_dag()`
2. Supprimer `drain_all_merges()`, `do_end_merge()`
3. Supprimer `ExplicitMerge`, `ActiveMerge`, `active_merge`, `explicit_merge`
4. Supprimer `pending_merges`, `segments_in_merge`
5. Supprimer `SuMergeStepMsg` et son handler
6. Supprimer `track_segments`, `untrack_segments`, `schedule_merge_step`, etc.
7. Renommer `handle_commit_dag` → `handle_commit`

### Étape 2 : simplifier handle_start_merge → handle_merge

1. Le handler de `SuStartMergeMsg` exécute le DAG inline (pas de state machine)
2. Plus de `explicit_merge` state
3. Le merge est synchrone : l'appelant attend la fin du DAG

### Étape 3 : simplifier wait_merging_threads

1. Plus de `SuDrainMergesMsg`
2. `wait_merging_threads()` fait juste shutdown des workers

### Étape 4 : nettoyer gc_protected_segments

1. `gc_protected_segments` n'est plus nécessaire — le GC dans le DAG tourne APRÈS le merge
2. Le GCNode a la vue correcte du segment_manager (post-finalize)

### Étape 5 : tests

1. Vérifier que les 20 tests d'aggregation passent
2. Vérifier que les tests commit_dag passent toujours
3. Vérifier les tests de merge explicite
4. `cargo test --lib` complet

## Estimation

```
Code supprimé : ~250 lignes (state machine, drain, do_end_merge, messages)
Code ajouté   : ~30 lignes (handle_merge simplifié)
Code modifié  : ~50 lignes (handlers, wait_merging_threads)
Net           : -170 lignes
```

## Résultat attendu

- Plus de double save_metas / double GC
- Plus de perte de données après merge
- Plus de state machine complexe (active/explicit/pending)
- Le DAG est le seul chemin — observable, testable, debuggable
- Moins de code, moins de surface de bugs
