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

## Pourquoi PAS async_trait (update post-réflexion)

Le plan ci-dessus proposait `#[cfg(feature = "async")]` + `async_trait`.
Mauvaise idée :

1. **async_trait = tokio** en pratique. Pas de tokio en WASM.
2. **Feature gates** = double maintenance, double testing, bugs subtils
3. **Rag3weaver doit être WASM compatible** à terme (rag3db compile en WASM)
4. En WASM, kuzu est sync de toute façon (pas de réseau, pas d'I/O async)

### La vraie solution : poll coopératif

Le pattern existe déjà dans luciole :
- `Actor::poll_idle()` → travail incrémental quand la mailbox est vide
- `Reply::wait_cooperative(|| scheduler.run_one_step())` → pompe le scheduler en attendant
- `MergeState::step()` → une étape de merge, retourne Continue ou Done

**C'est le même pattern pour les nœuds** : un nœud qui a du travail long
(merge, requête DB, embedding) peut *yield* entre les étapes. Le runtime
le re-schedule. En multi-thread : pas de différence (le thread est dédié).
En single-thread WASM : le nœud partage le thread coopérativement.

### Design : Node + PollNode

```rust
// Le Node sync classique — la majorité des cas
pub trait Node: Send {
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String>;
}

// Pour les nœuds qui veulent yield (long-running, "async" sans tokio)
pub trait PollNode: Send {
    fn node_type(&self) -> &'static str;
    fn inputs(&self) -> Vec<PortDef> { vec![] }
    fn outputs(&self) -> Vec<PortDef> { vec![] }

    /// Avance d'un pas. Retourne Ready quand terminé.
    fn poll_execute(&mut self, ctx: &mut NodeContext) -> Result<NodePoll, String>;
}

pub enum NodePoll {
    /// Le nœud a terminé.
    Ready,
    /// Le nœud a du travail restant. Le runtime le re-schedule.
    Pending,
}
```

### Comment le runtime gère PollNode

```rust
// Pour un Node sync classique : une seule task sur le pool
scheduler.submit_task(Priority::High, move || {
    node.execute(&mut ctx)
});

// Pour un PollNode : boucle coopérative
scheduler.submit_task(Priority::High, move || {
    loop {
        match node.poll_execute(&mut ctx) {
            Ok(NodePoll::Ready) => return Ok(()),
            Ok(NodePoll::Pending) => {
                // Yield : laisse le scheduler traiter d'autres work items
                std::thread::yield_now();
            }
            Err(e) => return Err(e),
        }
    }
});
```

En multi-thread natif : `yield_now()` est quasi-gratuit, le thread revient
immédiatement (ou laisse un autre thread tourner brièvement).

En single-thread WASM : le submit_task est exécuté via `run_one_step()`.
Le yield n'aide pas directement, mais le runtime pourrait alterner entre
nœuds PollNode et autres work items si on structure la boucle autrement.

### Cas concret : rag3weaver search node

```rust
// Aujourd'hui dans rag3weaver (async) :
#[async_trait]
impl Node for KBSearchNode {
    async fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        let results = self.db.query(&self.cypher).await?;  // async DB call
        ctx.set_output("results", PortValue::new(results));
        Ok(())
    }
}

// Demain dans luciole (sync, WASM-safe) :
impl Node for KBSearchNode {
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        // En natif : la connexion DB est sync (ou block_on interne)
        // En WASM : kuzu est sync de toute façon
        let results = self.db.query_sync(&self.cypher)?;
        ctx.set_output("results", PortValue::new(results));
        Ok(())
    }
}
```

L'async disparaît. La query DB est sync dans les deux cas. Pas besoin
de PollNode ici — la query prend quelques ms.

### Cas concret : lucivy merge node (long-running)

```rust
// Un merge peut prendre 30 secondes sur un gros segment
impl PollNode for MergeNode {
    fn poll_execute(&mut self, ctx: &mut NodeContext) -> Result<NodePoll, String> {
        match self.state.step() {
            StepResult::Continue => {
                ctx.metric("docs_so_far", self.state.docs_processed() as f64);
                Ok(NodePoll::Pending)  // yield, reviens me voir
            }
            StepResult::Done(result) => {
                ctx.set_output("result", PortValue::new(result));
                ctx.metric("total_docs", self.state.total_docs() as f64);
                Ok(NodePoll::Ready)
            }
        }
    }
}
```

En multi-thread : le thread exécute la boucle poll jusqu'à Ready.
Les yields sont quasi-gratuits.

En WASM single-thread : le runtime peut intercaler d'autres work items
(acteurs, autres nœuds) entre chaque poll du merge. Le merge avance
pas à pas sans bloquer le thread pendant 30 secondes.

### PollNode → Node adapter

Pour que le runtime n'ait qu'un seul chemin d'exécution, un PollNode
s'adapte en Node automatiquement :

```rust
impl<N: PollNode> Node for PollNodeAdapter<N> {
    fn node_type(&self) -> &'static str { self.inner.node_type() }
    fn inputs(&self) -> Vec<PortDef> { self.inner.inputs() }
    fn outputs(&self) -> Vec<PortDef> { self.inner.outputs() }

    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        loop {
            match self.inner.poll_execute(ctx)? {
                NodePoll::Ready => return Ok(()),
                NodePoll::Pending => std::thread::yield_now(),
            }
        }
    }
}
```

Le runtime ne voit que des `Node`. Les PollNode sont wrappés
automatiquement. Zéro complexité dans le runtime.

### Plan révisé pour la convergence

Phase 1 (luciole) :
- ~~Feature gate async~~ → **PollNode trait + adapter** (~50 lignes)
- ServiceRegistry optionnel dans NodeContext (~30 lignes)
- Tests : PollNode avec merge simulé, adapter

Phase 2 (rag3weaver) :
- Supprimer async_trait des nœuds
- Remplacer `async fn execute` par `fn execute` (sync)
- Les DB calls deviennent sync (block_on en natif, sync en WASM)
- Importer luciole au lieu de dataflow/

Pas de feature gate. Pas de tokio dans luciole. Un seul trait Node.
WASM compatible par construction.
