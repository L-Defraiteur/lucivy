# Décisions — Helper thread & guard cooperative wait (3 mai 2026)

## Décision 1 : Retirer le helper thread

### Contexte

Le helper thread (commit c037d41) a été ajouté comme safety net pour
les cooperative waits : un thread persistant qui pompe `run_one_step_impl`
et garantit qu'il y a toujours un thread libre pour traiter les
finalizers quand tous les scheduler threads sont bloqués en cooperative
wait dans leurs handlers.

### Pourquoi on le retire

Avec `ActorStatus::Suspend`, le problème n'existe plus :

1. Un handler qui attend un autre actor retourne `Suspend` → le thread
   est libéré immédiatement
2. Le thread revient dans sa boucle `run_loop` (ou cooperative wait
   externe) et traite les finalizers normalement
3. Même avec 4 shards en parallèle, chaque thread tourne librement

Le helper thread traitait le **symptôme** (threads capturés). Suspend
élimine la **cause** (handlers qui bloquent). Le helper est désormais
du code mort.

### Ce qui est retiré

- `helper_loop()` dans scheduler.rs
- Spawn du helper dans `Scheduler::start()`
- PTHREAD_POOL_SIZE 9 → 8 dans build.sh (1 thread en moins)
- `reserved_for_others` 4 → 3 dans emscripten/lib.rs
- Logs diag du helper

## Décision 2 : Guard — panic si cooperative wait dans un handler

### Le problème

`wait_cooperative` (boucle qui pompe `run_one_step`) est légitime
pour les callers **externes** : le commit thread, le main thread,
Pool::scatter appelé hors d'un handler. Mais c'est un **anti-pattern**
quand c'est appelé depuis l'intérieur d'un `Actor::handle()`, parce
que ça capture un scheduler thread pour la durée du wait.

Avec Suspend disponible, il n'y a plus aucune raison de faire un
cooperative wait dans un handler. Tout handler qui attend un autre
actor doit utiliser Suspend.

### L'implémentation

Thread-local `IN_ACTOR_HANDLER` (bool), set/unset par le scheduler
autour de chaque `try_handle_one()` :

```rust
thread_local! {
    static IN_ACTOR_HANDLER: Cell<bool> = Cell::new(false);
}
```

Dans `wait_cooperative_named` :

```rust
#[cfg(debug_assertions)]
if crate::scheduler::in_actor_handler() {
    panic!(
        "cooperative wait inside actor handler is forbidden — \
         use ActorStatus::Suspend with ctx.resume_handle() instead"
    );
}
```

### Propriétés

- **Debug only** (`cfg(debug_assertions)`) : zéro overhead en release
- **Catch immédiat** : le développeur voit le panic dès le premier
  test, pas après 300s de deadlock en prod
- **Message clair** : dit exactement quoi faire à la place
- **Pas de faux positifs** : les callers externes (commit thread,
  main thread) ne sont pas dans un handler, le flag est false

### Impact

Le guard force toute future utilisation de luciole à suivre le bon
pattern. Plus jamais de deadlock par cooperative wait dans un handler.
C'est une garantie au niveau du framework, pas juste une convention.
