# Plan d'action : pipe_to / collect_to

Ref design : `05-design-pipe-to.md`

## Phase 1 — Primitives luciole (pur framework, zero impact sur lucivy)

### 1.1 Modifier `Inner<T>` dans reply.rs
- Ajouter champ `on_send: Mutex<Option<Box<dyn FnOnce(T) + Send>>>`
- Initialiser à `None` dans `reply()`

### 1.2 Modifier `Reply::send()`
- Avant le chemin normal, v��rifier `on_send`. Si Some, appeler le callback
  avec la value, marquer closed, return (skip le chemin resume).

### 1.3 Ajouter `ReplyReceiver::set_pipe()` (privé)
- Pose le callback `on_send`. Méthode crate-privée.
- Consomme rien — juste stocke le callback pour que les méthodes publiques
  (pipe_to, collect_to) puissent poser le callback AVANT d'envoyer.

### 1.4 Ajouter `ActorRef::pipe_to()`
- Crée Reply/ReplyReceiver
- Pose callback via `set_pipe` (avec WaitGraph register)
- Envoie le message (callback déjà en place → pas de race)

### 1.5 Ajouter `Pool::collect_to()`
- Crée N Reply/ReplyReceiver
- Shared state : `Arc<Mutex<Vec<Option<T>>>>` + `Arc<AtomicUsize>`
- Pose callback sur chaque rx via `set_pipe`
- Envoie les N messages
- Dernier callback fire → collecte → envoie le message résultat

### 1.6 Ajouter `Scheduler::task_pipe_to()`
- Crée Reply/ReplyReceiver
- Pose callback via `set_pipe`
- Soumet la tâche wrappée (résultat envoyé via Reply::send)

### 1.7 Exports dans lib.rs
- `pub use reply::collect_to;` (si free function) ou via Pool/ActorRef
- Documenter dans le module reply ou dans un nouveau module `pipe`

### 1.8 Tests unitaires
- `test_pipe_to_basic` : acteur A pipe_to acteur B, B reçoit le message
- `test_pipe_to_before_reply` : le callback est posé, la reply arrive après
- `test_pipe_to_reply_already_sent` : (ne devrait pas arriver avec la bonne
  API, mais tester que Reply::send gère on_send correctement)
- `test_collect_to_all_workers` : scatter 4 workers, collect 4 résultats
- `test_collect_to_order_preserved` : results[i] = worker i
- `test_collect_to_empty` : 0 workers → message envoyé immédiatement
- `test_task_pipe_to` : tâche CPU → résultat comme message
- `test_wait_graph_pipe_to` : vérifier que l'edge appara��t et disparaît
- `test_chain` : pipe_to → handler → pipe_to → handler (2 étapes)

**Livrable** : `cargo test --lib -p luciole` passe avec les nouvelles
primitives. Aucun code lucivy touché.

## Phase 2 — Migrer ShardActor (premier consommateur)

### 2.1 Ajouter `self_ref` à ShardActor
- Implémenter `on_start` pour stocker le self_ref
- Ajouter le champ `self_ref: Option<ActorRef<ShardMsg>>`

### 2.2 Remplacer le commit Suspend par collect_to
- Supprimer : `PendingShardCommitState`, `JoinResume`, `set_resume`,
  `pending_commit`, tout le `poll_idle`
- Ajouter : `ShardMsg::FlushDone { results, fast, reply }`
- Handler Commit → `collect_to` (ou appel direct à `flush_workers` +
  `collect_to` sur les receivers)
- Handler FlushDone → `finalize_flush_and_prepare` + `commit` + `reply.send`

### 2.3 Flag committing
- `self.committing = true` dans Commit
- `self.committing = false` dans FlushDone
- Insert/Delete pendant committing → queue ou reject

### 2.4 Tester
- Tests existants sharded_handle doivent passer
- `cargo test --lib` (1199+)

**Livrable** : ShardActor utilise collect_to. Plus de Suspend, plus de
poll_idle, plus de PendingShardCommitState.

## Phase 3 — Migrer le reste des waits

### 3.1 Pool::scatter → collect_to
- `Pool::scatter` utilise `scheduler.wait()` (bloquant/coopératif)
- Remplacer par `collect_to` partout où scatter est appelé depuis un acteur
- Garder `scatter` pour les appels depuis des threads externes

### 3.2 Pool::drain → pipe_to ou garder tel quel
- `drain` est utilisé pour synchroniser le pipeline (readers → router → shards)
- C'est appelé depuis `commit()` / `drain_pipeline()` — thread externe
- ��� Garder `scheduler.wait()` ici, c'est le bon pattern pour thread externe

### 3.3 execute_dag dans segment_updater
- `execute_dag` fait des cooperative waits internes
- Si appelé depuis un handler/poll_idle → risque
- → Migrer vers `task_pipe_to` : soumettre le DAG comme tâche, résultat
  revient comme message

### 3.4 Enforcer la règle
- `cooperative_wait inside actor handler` → panic (plus un warning)
- `scheduler.wait() on scheduler thread inside handler` → panic
- Ça expose immédiatement les violations restantes

## Phase 4 — Cleanup

### 4.1 Supprimer le code mort
- `PendingShardCommitState`
- L'ancien `create_shard_actor` (GenericActor legacy)
- `PendingShardCommit`
- `spawn_shard_actors` / `spawn_pipeline_actors` (GenericActor legacy)
- `create_reader_actor` / `create_router_actor` (GenericActor legacy)
- Le legacy Envelope-based message routing

### 4.2 Supprimer les diag eprintln
- Tous les `eprintln!("[shard_{}] commit: phase ...")` 
- Tous les `eprintln!("[scatter] ...")` dans pool.rs
- Tous les `eprintln!("[commit] ...")` dans sharded_handle.rs
- Le WaitGraph + dump_mermaid remplacent ces logs ad-hoc

### 4.3 Remettre min_num_segments à 8
- `src/indexer/log_merge_policy.rs` : `DEFAULT_MIN_NUM_SEGMENTS = 8`
- Les 10 tests qui échouent repassent

### 4.4 Commit final + build emscripten + test playground

## Ordre et dépendances

```
Phase 1 (luciole primitives)
    ↓
Phase 2 (ShardActor migration)
    ↓
Phase 3 (autres waits + enforcement)
    ↓
Phase 4 (cleanup)
```

Chaque phase est un commit. On peut s'arrêter après chaque phase et
tout fonctionne (backward compat).

## Ce qui ne change PAS

- `scheduler.wait()` — reste pour les threads externes
- `Suspend + set_resume` — reste pour les edge cases
- `JoinResume` — reste disponible mais plus utilisé en pratique
- `Pool::drain/shutdown` �� restent pour les threads externes
- Les tests existants — aucun ne casse

## Estimation effort

| Phase | Fichiers touchés | Complexité |
|-------|-----------------|------------|
| 1 | reply.rs, mailbox.rs, pool.rs, scheduler.rs, lib.rs | Moyenne — nouvelles méthodes, tests |
| 2 | sharded_handle.rs | Moyenne — réécriture ShardActor commit flow |
| 3 | pool.rs, segment_updater_actor.rs, reply.rs | Faible — enforcement + migration ponctuelle |
| 4 | sharded_handle.rs, pool.rs, log_merge_policy.rs | Faible — suppression de code |
