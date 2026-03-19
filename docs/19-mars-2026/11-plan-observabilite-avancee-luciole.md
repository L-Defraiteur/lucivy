# Doc 11 — Plan : observabilité avancée luciole

Date : 19 mars 2026
Basé sur : analyse de rag3weaver dataflow (observe.rs, runtime.rs, report.rs)

## État actuel

### Ce qu'on a dans luciole

| Feature | Status | Détail |
|---------|--------|--------|
| DagEvent bus global | ✅ fait | `subscribe_dag_events()` depuis n'importe quel thread |
| NodeStarted/Completed/Failed | ✅ fait | Émis en temps réel par le runtime |
| LevelStarted/Completed | ✅ fait | Timing par niveau |
| DagCompleted/DagFailed | ✅ fait | Status global |
| Métriques par nœud | ✅ fait | `ctx.metric("key", value)` → dans DagResult |
| Logs par nœud | ✅ fait | `ctx.info/warn/error/debug()` → dans DagResult |
| Zero-cost quand pas de subscriber | ✅ fait | Check atomique avant emit |

### Ce qui manque (inspiré de rag3weaver)

| Feature | Status | Impact |
|---------|--------|--------|
| Edge taps (intercepter données entre nœuds) | 🔧 en cours | Debug data flow sans modifier le code des nœuds |
| NodeLog event temps réel | ❌ manque | Les logs sont dans DagResult après, pas émis live |
| Input/output snapshots dans les events | ❌ manque | Voir ce qui rentre/sort de chaque nœud |
| DagResult formatter | ❌ manque | Pretty-print post-mortem pour le bench |
| Filtrage par nœud | ❌ manque | Ne recevoir que les events d'un nœud spécifique |

## Ce qu'on veut implémenter

### 1. Edge Taps (observe.rs) — ÉCRIT, pas branché

Le fichier `observe.rs` est déjà écrit avec :

```rust
pub struct TapRegistry {
    specs: Vec<TapSpec>,     // edges spécifiques à tapper
    all: bool,               // tap all edges
    bus: Arc<EventBus<TapEvent>>,
}

pub struct TapEvent {
    pub from_node: String,
    pub from_port: String,
    pub to_node: String,
    pub to_port: String,
    pub value: PortValue,    // clone Arc, cheap
}
```

**Zero-cost** : `check_and_emit()` return immédiat si pas actif.
**Usage** : le runtime appelle `taps.check_and_emit(edge, value)` quand
il propage les outputs d'un nœud vers les inputs du suivant.

**Ce qui reste à faire** :
- Intégrer TapRegistry dans le Dag ou dans execute_dag
- Le runtime appelle check_and_emit lors de la propagation des données
- API sur Dag : `dag.tap("merge_0", "result", "end_merge", "in_0")`
- Tests d'intégration avec execute_dag

### 2. NodeLog event temps réel

Aujourd'hui les logs sont accumulés dans `NodeContext.logs` et retournés
dans `NodeResult.logs` après exécution. Mais pendant l'exécution, rien
n'est émis en live.

**Ajout** : nouveau variant `DagEvent::NodeLog` :

```rust
DagEvent::NodeLog {
    node: String,
    node_type: String,
    level: LogLevel,
    text: String,
}
```

**Comment** : deux options :

Option A — Émettre après execute() en drainant les logs :
```rust
// Après node.execute() réussit :
for (level, text) in ctx.logs() {
    emit(DagEvent::NodeLog { node, node_type, level, text });
}
```
Simple, pas de changement à NodeContext. Les logs arrivent en batch
après le nœud, pas pendant.

Option B — NodeContext a une référence au bus, émet en direct :
```rust
impl NodeContext {
    pub fn info(&mut self, msg: &str) {
        self.logs.push((LogLevel::Info, msg.to_string()));
        if let Some(bus) = &self.event_bus {
            bus.emit(DagEvent::NodeLog { ... });
        }
    }
}
```
Plus complexe, NodeContext a besoin du node name et du bus.
Pas nécessaire pour la v1.

**Recommandation** : Option A. Simple et suffisant.

### 3. Input/output snapshots dans les events

Rag3weaver capture les ports d'entrée dans NodeStarted et les ports de
sortie dans NodeCompleted via `PortSnapshot` :

```rust
pub struct PortSnapshot {
    pub name: String,
    pub port_type: String,   // description du type
    pub summary: String,     // e.g. "Vec<SegmentId>(4)" ou "Trigger"
}
```

Pour luciole, les PortValues sont `Arc<dyn Any>` — on ne peut pas les
sérialiser. Mais on peut donner un résumé :

```rust
impl PortValue {
    pub fn summary(&self) -> String {
        match self {
            PortValue::Trigger => "Trigger".to_string(),
            PortValue::Data(arc) => {
                // TypeId doesn't give names in stable Rust.
                // On peut juste dire "Data" ou utiliser un trait.
                "Data(...)".to_string()
            }
        }
    }
}
```

Mieux : les nœuds qui veulent être inspectables peuvent implémenter
un trait optionnel `Summarize` sur leurs types de données. Mais c'est
du travail côté lucivy.

**Recommandation** : ajouter le summary basique dans les events. Les
nœuds lucivy pourront enrichir via leurs métriques (`ctx.metric()`).

### 4. DagResult formatter

Un `Display` ou une méthode pour pretty-print le post-mortem :

```rust
impl DagResult {
    pub fn display_summary(&self) -> String {
        let mut lines = Vec::new();
        lines.push(format!("DAG completed in {}ms ({} nodes)",
            self.duration_ms, self.node_results.len()));
        lines.push(String::new());

        for (name, nr) in &self.node_results {
            let metrics_str = nr.metrics.iter()
                .map(|(k, v)| format!("{}={}", k, v))
                .collect::<Vec<_>>()
                .join(" ");
            lines.push(format!("  {:30} {:6}ms  {}",
                name, nr.duration_ms, metrics_str));

            for (level, text) in &nr.logs {
                lines.push(format!("    [{:?}] {}", level, text));
            }
        }
        lines.join("\n")
    }
}
```

Usage dans le bench :
```rust
let result = scope.execute_dag(&mut dag, None)?;
eprintln!("{}", result.display_summary());
```

### 5. Filtrage par nœud

Optionnel — un helper qui wrappe EventReceiver et filtre par node name :

```rust
pub fn subscribe_dag_node_events(node_name: &str) -> FilteredReceiver {
    let rx = subscribe_dag_events();
    FilteredReceiver { rx, node_name: node_name.to_string() }
}
```

**Recommandation** : pas prioritaire. Le subscriber peut filtrer lui-même.
Ajouter si le besoin se présente.

## Ordre d'implémentation

1. **NodeLog events** — Option A (batch après execute). ~20 lignes dans runtime.rs
2. **DagResult::display_summary()** — ~30 lignes dans runtime.rs
3. **Brancher observe.rs** — TapRegistry dans Dag, check_and_emit dans runtime. ~40 lignes
4. **Port summary dans events** — Enrichir NodeStarted/NodeCompleted. ~20 lignes
5. **Tests** — tap intégration, NodeLog live, display

## Ce qu'on ne fait PAS (pas nécessaire pour lucivy)

- **PortSnapshot avec sérialisation JSON** — nos PortValues sont Any, pas sérialisables
- **CheckpointStore** — pas de crash recovery pour l'instant
- **DataflowRecorder** — pas de persistence des rapports en DB
- **Mermaid export** — nice-to-have mais pas prioritaire
- **subscribe_nodes()** filtré — le subscriber filtre lui-même

## Estimation

```
observe.rs          ~120 lignes (déjà écrit, 4 tests)
runtime.rs changes  ~80 lignes  (NodeLog emit, taps, summary)
DagResult display   ~30 lignes
                    ──────
                    ~230 lignes total
```

## Convergence luciole ↔ rag3weaver : un seul DAG pour les deux

### Le problème

Aujourd'hui il y a deux DAG frameworks quasi-identiques :
- `luciole/src/{dag,node,runtime,port,observe}.rs` (~1000 lignes)
- `rag3weaver/src/dataflow/{graph,node,runtime,port,observe}.rs` (~3800 lignes)

Les deux font la même chose : nœuds avec ports typés, topo sort, exécution
par niveaux, events structurés, taps, métriques. La seule personne qui
maintient les deux c'est Lucie. Dupliquer le code c'est dupliquer les bugs.

### Ce qui bloque aujourd'hui

| Aspect | luciole | rag3weaver | Gap |
|--------|---------|------------|-----|
| Threading | sync, pool threads | async, tokio | **bloquant** |
| Node trait | `fn execute(&mut self, ctx)` | `async fn execute(&mut self, ctx)` | bloquant |
| PortValue | `Arc<dyn Any + Send + Sync>` | `enum PortValue { Results, Children, ... }` | adaptable |
| Services | capture à la construction | `ServiceRegistry` string-keyed | adaptable |
| Checkpoint | pas encore | `CheckpointStore` async | ajout futur |
| Taps | `EventBus<TapEvent>` sync | `async_broadcast` | adaptable |

Le seul vrai bloqueur c'est **sync vs async**. Rag3weaver a besoin
d'async parce que `DbConnection` (kuzu) est async.

### La solution : trait Node avec feature gate

```rust
// luciole/src/node.rs

#[cfg(not(feature = "async"))]
pub trait Node: Send {
    fn node_type(&self) -> &'static str;
    fn inputs(&self) -> Vec<PortDef> { vec![] }
    fn outputs(&self) -> Vec<PortDef> { vec![] }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String>;
}

#[cfg(feature = "async")]
#[async_trait::async_trait]
pub trait Node: Send {
    fn node_type(&self) -> &'static str;
    fn inputs(&self) -> Vec<PortDef> { vec![] }
    fn outputs(&self) -> Vec<PortDef> { vec![] }
    async fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String>;
}
```

Ou mieux : **deux traits**, un sync, un async, avec un adapter :

```rust
// Toujours disponible :
pub trait Node: Send {
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String>;
}

// Feature "async" :
#[cfg(feature = "async")]
#[async_trait::async_trait]
pub trait AsyncNode: Send {
    async fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String>;
}

// Adapter : un AsyncNode dans un Node (block_on)
#[cfg(feature = "async")]
pub struct AsyncNodeAdapter<N: AsyncNode> {
    node: N,
    runtime: tokio::runtime::Handle,
}

impl<N: AsyncNode> Node for AsyncNodeAdapter<N> {
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        self.runtime.block_on(self.node.execute(ctx))
    }
}
```

Rag3weaver utiliserait `AsyncNode` pour ses nœuds qui font du I/O async.
Lucivy utiliserait `Node` directement (sync). Le runtime est toujours
sync (submit_task sur le pool). Les nœuds async font block_on à
l'intérieur — ce qui est safe parce que chaque nœud tourne sur un
thread dédié du pool (pas sur le reactor tokio).

### PortValue : generic vs enum

Rag3weaver a un `enum PortValue` avec 15 variants spécifiques (Results,
Children, Query, etc.). Luciole a `Arc<dyn Any>`. Les deux marchent.

Pour unifier : rag3weaver migrerait vers `PortValue::Data(Arc<dyn Any>)`
pour les données, avec des helpers typés :

```rust
// Rag3weaver écrirait :
ctx.set_output("results", PortValue::new(results));
let results: Vec<UnifiedResult> = ctx.take_input("results")?.take()?;

// Au lieu de :
ctx.set_output("results", PortValue::Results(results));
let PortValue::Results(results) = ctx.take_input("results")? else { ... };
```

C'est un changement mécanique (search-replace), pas un redesign.

### ServiceRegistry vs capture

Rag3weaver utilise `ServiceRegistry` parce que les nœuds sont construits
par des factories (`NodeRegistry`) et ne connaissent pas le graphe.
Lucivy capture les services à la construction.

**Solution** : rendre ServiceRegistry optionnel dans luciole.

```rust
pub struct NodeContext {
    inputs: HashMap<String, PortValue>,
    outputs: HashMap<String, PortValue>,
    metrics: Vec<(String, f64)>,
    logs: Vec<(LogLevel, String)>,
    services: Option<Arc<ServiceRegistry>>,  // optionnel
}

impl NodeContext {
    pub fn service<T: Send + Sync + 'static>(&self, key: &str) -> Option<&T> {
        self.services.as_ref()?.get(key)
    }
}
```

Lucivy n'utilise pas les services (capture). Rag3weaver les utilise.
Le même NodeContext supporte les deux.

### Checkpoint

Ajouter un trait `CheckpointStore` optionnel dans luciole :

```rust
pub trait CheckpointStore: Send + Sync {
    fn save_node_completed(&self, dag_id: &str, node: &str, outputs: &[u8]);
    fn load_checkpoint(&self, dag_id: &str) -> Option<DagCheckpoint>;
    fn mark_completed(&self, dag_id: &str);
}
```

Le runtime utilise le store s'il est fourni, sinon pas de checkpoint.
Lucivy n'en a pas besoin. Rag3weaver en a besoin et fournit un
`CypherCheckpointStore`.

### Plan de migration rag3weaver → luciole

Phase 1 (luciole, sans casser rien) :
- Feature gate `async` optionnel
- `AsyncNode` trait + `AsyncNodeAdapter`
- `ServiceRegistry` optionnel dans NodeContext
- Publier luciole sur crates.io (ou git dependency)

Phase 2 (rag3weaver, progressive) :
- `Cargo.toml` : ajouter `luciole = { features = ["async"] }`
- Migrer les nœuds un par un : `impl AsyncNode for KBSearchNode`
- Remplacer `PortValue::Results(v)` par `PortValue::new(v)`
- Supprimer `rag3weaver/src/dataflow/` (graph, node, runtime, port)
- Garder observe.rs, report.rs (adaptés pour luciole)

Phase 3 (bonus) :
- CheckpointStore dans luciole
- Report builder dans luciole (inspiré de rag3weaver report.rs)
- Un seul framework, maintenu une seule fois

### Estimation

Phase 1 : ~100 lignes dans luciole (feature gates, ServiceRegistry)
Phase 2 : ~500 lignes changées dans rag3weaver (migration mécanique)
Phase 3 : ~200 lignes dans luciole (checkpoint, report)

Résultat : suppression de ~3800 lignes dans rag3weaver, remplacées par
une dépendance sur luciole. Un seul DAG framework, battle-tested,
utilisé par les deux projets.
