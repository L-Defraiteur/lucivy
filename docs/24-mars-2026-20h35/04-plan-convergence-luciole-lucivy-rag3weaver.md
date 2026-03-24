# Doc 04 — Plan de convergence luciole ↔ lucivy ↔ rag3weaver

Date : 24 mars 2026

## État luciole actuel

### Noeuds disponibles

| Noeud | Description | Utilisé par |
|-------|------------|-------------|
| **Node** (trait) | execute, can_undo, undo, undo_context | lucivy, rag3weaver |
| **PollNode** (trait) | exécution coopérative, yield-friendly WASM | lucivy (merge) |
| **SwitchNode** | routing N-way conditionnel | lucivy (search DAG) |
| **BranchNode** | alias 2-way (then/else) | lucivy (search DAG) |
| **GateNode** | pass/block conditionnel | nouveau, pas encore utilisé |
| **MergeNode** | N inputs → 1 output, merge fn custom | nouveau, pas encore utilisé |
| **ScatterDAG** | fan-out parallèle (closures) | lucivy (SFX build, reader reload) |

### Infrastructure

| Feature | Status | Utilisé par |
|---------|--------|-------------|
| **Dag + execute_dag** | ✅ | lucivy (search, merge, SFX, commit) |
| **fan_out_merge()** | ✅ nouveau | pas encore utilisé |
| **Pool\<M\> + Actor** | ✅ | lucivy (shard workers, indexers) |
| **Scope + drain** | ✅ | lucivy (ShardedHandle) |
| **StreamDag** | ✅ tests OK | **jamais utilisé en prod** |
| **CheckpointStore** | ✅ trait + Memory + File impls | lucivy (merge DAG) |
| **DagEvent bus** | ✅ | lucivy (bench tracing) |
| **TapRegistry** | ✅ | pas encore utilisé |
| **ServiceRegistry** | ✅ struct existe | **pas connecté au NodeContext** |

### Trait Node — ce qui existe déjà

```rust
pub trait Node: Send {
    fn node_type(&self) -> &'static str;
    fn inputs(&self) -> Vec<PortDef> { vec![] }
    fn outputs(&self) -> Vec<PortDef> { vec![] }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String>;
    fn can_undo(&self) -> bool { false }                              // ← existe
    fn undo_context(&self) -> Option<Box<dyn Any + Send>> { None }    // ← existe
    fn undo(&mut self, _ctx: Box<dyn Any + Send>) -> Result<(), String> { // ← existe
        Err("undo not supported".to_string())
    }
}
```

Tout est là sur le trait. Mais le **runtime ne fait pas le rollback**.

## Ce qu'il faut faire — par priorité

### P1. Rollback dans le runtime

Le runtime (`execute_dag`) doit, quand un noeud échoue :
1. Collecter les `undo_context()` de tous les noeuds déjà exécutés
2. Appeler `undo(ctx)` en ordre inverse sur ceux qui ont `can_undo() == true`
3. Émettre un `DagEvent::NodeUndone { node, duration_ms }` pour chaque rollback

**Fichier** : `luciole/src/runtime.rs`

**Implémentation** :
```rust
// Dans execute_dag, quand un noeud échoue :
Err(e) => {
    // Rollback des noeuds précédents
    for (node_name, undo_data) in completed_undo_contexts.iter().rev() {
        let node = dag.node_mut_by_name(node_name);
        if node.can_undo() {
            if let Some(ctx) = undo_data {
                let _ = node.undo(ctx);
            }
        }
    }
    return Err(e);
}
```

**Impact lucivy** : aucun — les noeuds lucivy n'implémentent pas `can_undo`.
**Impact rag3weaver** : critique — c'est leur use case principal.

### P2. ServiceRegistry dans NodeContext

Le `ServiceRegistry` existe dans `luciole/src/node.rs` mais n'est pas accessible
depuis `NodeContext`. Il faut :

1. Ajouter `services: &ServiceRegistry` dans `NodeContext`
2. Ajouter `ctx.service::<T>(name)` comme raccourci
3. Passer le registry dans `execute_dag(dag, Some(services))`

**Fichier** : `luciole/src/node.rs` + `luciole/src/runtime.rs`

**Impact lucivy** : aucun — on capture à la construction.
**Impact rag3weaver** : critique — 14 noeuds × 10+ services.

### P3. StreamDag en production

Le StreamDag existe et a 3 tests OK mais n'est **jamais utilisé en prod**.

Deux consommateurs potentiels :

**a) lucivy ShardedHandle** — pipeline d'ingestion :
```
readers [4] → router [1] → shards [4]
```
Actuellement câblé manuellement avec `Pool::spawn` + `DrainNode` dans le search DAG.
Le StreamDag remplacerait le drain manuel.

**b) rag3weaver** — pipeline d'ingestion streaming :
```
source → chunk → embed → store → flush
```
Actuellement séquentiel. Le StreamDag + acteurs donnerait du streaming pipeliné.

**Plan de validation** :
1. Intégrer StreamDag dans `ShardedHandle::new()` pour le pipeline readers/router/shards
2. Remplacer `DrainNode` dans le search DAG par `pipeline.drain()`
3. Vérifier que les 1155 tests passent
4. Bench pour vérifier pas de régression

### P4. node_config() pour checkpoint recovery

Ajouter une méthode optionnelle au trait Node :
```rust
fn node_config(&self) -> Option<Box<dyn Any + Send>> { None }
```

Utilisé par rag3weaver pour reconstruire les noeuds au restart après crash.
Pas nécessaire pour lucivy.

### P5. NodeFactory / NodeRegistry

Pas dans luciole — rag3weaver l'implémente au-dessus. Pattern :
```rust
trait NodeFactory: Send + Sync {
    fn node_type(&self) -> &'static str;
    fn create(&self, name: &str, config: Value) -> Box<dyn Node>;
}
```

Peut rester dans rag3weaver. Pas de changement luciole nécessaire.

## Intersection lucivy ↔ rag3weaver

| Besoin | lucivy | rag3weaver | Solution |
|--------|--------|-----------|----------|
| DAG one-shot | search, merge, SFX | ingestion | luciole::Dag ✅ |
| Streaming pipeline | readers→router→shards | source→chunk→embed→store | luciole::StreamDag (à valider) |
| Parallélisme | 4 shards en parallèle | vector+BM25+sparse en parallèle | luciole niveaux topologiques ✅ |
| Undo/rollback | pas besoin | critique (delete entities on fail) | runtime rollback (P1) |
| Services partagés | capture à la construction | ServiceRegistry dans ctx | P2 |
| Conditional routing | SwitchNode (prescan ou pas) | pas encore | SwitchNode ✅ |
| Fan-out/merge | prescan shards | search fusion | fan_out_merge ✅ |
| Checkpoint | merge DAG | ingestion DAG | CheckpointStore ✅ |
| WASM | search + commit thread | playground | wait_cooperative ✅ |

## Ordre d'implémentation

```
1. Rollback dans runtime          [petit, critique rag3weaver]
2. ServiceRegistry dans NodeContext [petit, critique rag3weaver]
3. StreamDag → ShardedHandle       [moyen, valide le StreamDag pour les deux]
4. node_config()                   [petit, utile rag3weaver]
5. Adapter search DAG avec fan_out_merge [moyen, simplifie lucivy]
```

## Fichiers luciole à toucher

| Fichier | Changement |
|---------|-----------|
| `runtime.rs` | Rollback loop dans execute_dag |
| `node.rs` | ServiceRegistry dans NodeContext, ctx.service::\<T\>() |
| `dag.rs` | node_mut_by_name() pour rollback |
| `runtime.rs` | Passer ServiceRegistry en paramètre execute_dag |
| `lib.rs` | Exports mis à jour |
