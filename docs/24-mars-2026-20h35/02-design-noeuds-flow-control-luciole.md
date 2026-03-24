# Doc 02 — Design : noeuds flow-control luciole

Date : 24 mars 2026

Inspiré de Houdini TOPS/PDG, Unreal Blueprints.

## 1. SwitchNode — routing N-way

Remplace et généralise `BranchNode`. Évalue une expression et active
exactement une sortie parmi N. Les branches inactives sont auto-skippées
par le runtime (trigger non satisfait → skip cascade).

### API

```rust
// Construction
let switch = SwitchNode::new(
    vec!["fast", "prescan", "distributed"],  // noms des outputs
    move || match mode {                      // sélecteur → index
        Mode::Fast => 0,
        Mode::Prescan => 1,
        Mode::Distributed => 2,
    },
);

// Dans le DAG
dag.add_node("route", switch);
dag.connect("flush", "done", "route", "trigger")?;
dag.connect("route", "fast", "build_weight", "trigger")?;
dag.connect("route", "prescan", "prescan_0", "trigger")?;
dag.connect("route", "distributed", "export_stats", "trigger")?;
```

### Implémentation

```rust
pub struct SwitchNode<F: FnMut() -> usize + Send> {
    outputs: Vec<&'static str>,
    selector: F,
}

impl<F: FnMut() -> usize + Send> Node for SwitchNode<F> {
    fn node_type(&self) -> &'static str { "switch" }
    fn inputs(&self) -> Vec<PortDef> {
        vec![PortDef::trigger("trigger")]
    }
    fn outputs(&self) -> Vec<PortDef> {
        self.outputs.iter().map(|name| PortDef::trigger(name)).collect()
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        let idx = (self.selector)();
        if idx < self.outputs.len() {
            ctx.trigger(self.outputs[idx]);
            ctx.metric("switch_index", idx as f64);
        }
        Ok(())
    }
}
```

### BranchNode → SwitchNode

BranchNode devient un cas particulier :
```rust
pub fn BranchNode<F>(condition: F) -> SwitchNode<impl FnMut() -> usize>
where F: FnMut() -> bool + Send {
    SwitchNode::new(vec!["then", "else"], move || if (condition)() { 0 } else { 1 })
}
```

Ou on garde BranchNode comme alias pour lisibilité.

## 2. FanOutMerge — parallélisation + agrégation

Pattern récurrent : spawner N tâches parallèles, attendre toutes,
merger les résultats. Actuellement fait manuellement (N noeuds + merge node).

### API

```rust
// Helper sur Dag
dag.fan_out_merge::<ResultType>(
    "prescan",                              // name prefix
    num_shards,                             // parallelism N
    |i| Box::new(PrescanShardNode::new(i)), // node factory
    "work",                                 // output port name on each worker
    |results: Vec<ResultType>| {            // merge function
        merge_prescan_results(results)
    },
)?;
// Crée automatiquement :
//   prescan_0, prescan_1, ..., prescan_N-1   (workers)
//   prescan_merge                             (merger)
// Connections : trigger → workers ∥ → merger
// Résultat disponible sur "prescan_merge" / "merged"
```

### Implémentation

Deux parties :

**a) GenericMergeNode** — noeud générique qui collecte N inputs et applique une fn :

```rust
pub struct GenericMergeNode<T, F>
where
    T: Send + 'static,
    F: FnOnce(Vec<T>) -> T + Send,
{
    num_inputs: usize,
    merge_fn: Option<F>,  // Option pour take() dans execute
    _phantom: PhantomData<T>,
}

impl<T, F> Node for GenericMergeNode<T, F> { ... }
```

**b) `Dag::fan_out_merge()` helper** :

```rust
impl Dag {
    pub fn fan_out_merge<T: Send + 'static>(
        &mut self,
        prefix: &str,
        count: usize,
        node_factory: impl Fn(usize) -> Box<dyn Node>,
        output_port: &str,
        merge_fn: impl FnOnce(Vec<T>) -> T + Send + 'static,
    ) -> Result<(), String> {
        // Add N worker nodes
        for i in 0..count {
            self.add_node(&format!("{prefix}_{i}"), node_factory(i));
        }
        // Add merge node
        self.add_node(
            &format!("{prefix}_merge"),
            GenericMergeNode::new(count, merge_fn),
        );
        // Wire workers → merge
        for i in 0..count {
            self.connect(
                &format!("{prefix}_{i}"), output_port,
                &format!("{prefix}_merge"), &format!("in_{i}"),
            )?;
        }
        Ok(())
    }
}
```

### Usage dans search_dag

```rust
// Avant (23 lignes)
for i in 0..num_shards {
    dag.add_node(&format!("prescan_{i}"), PrescanShardNode::new(...));
    dag.connect("needs_prescan", "then", &format!("prescan_{i}"), "trigger")?;
}
dag.add_node("merge_prescan", MergePrescanNode::new(num_shards));
for i in 0..num_shards {
    dag.connect(&format!("prescan_{i}"), "prescan",
                "merge_prescan", &format!("prescan_{i}"))?;
}

// Après (5 lignes)
dag.fan_out_merge::<PrescanResult>(
    "prescan", num_shards,
    |i| Box::new(PrescanShardNode::new(shards[i].clone(), prescan_params.clone())),
    "prescan",
    |results| merge_prescan_results(results),
)?;
```

## 3. GateNode — pass/block conditionnel

Un noeud qui laisse passer ou bloque un flux de données basé sur une condition.
Différent du SwitchNode : le Gate ne route pas, il filtre.

```rust
pub struct GateNode<F: FnMut() -> bool + Send> {
    condition: F,
}

impl<F: FnMut() -> bool + Send> Node for GateNode<F> {
    fn node_type(&self) -> &'static str { "gate" }
    fn inputs(&self) -> Vec<PortDef> {
        vec![
            PortDef::trigger("trigger"),
            PortDef::optional("data", PortType::Any),
        ]
    }
    fn outputs(&self) -> Vec<PortDef> {
        vec![
            PortDef::trigger("pass"),
            PortDef::optional("data", PortType::Any),
        ]
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        if (self.condition)() {
            ctx.trigger("pass");
            // Forward data if present
            if let Some(data) = ctx.take_input("data") {
                ctx.set_output("data", data);
            }
        }
        // Si condition false : rien en sortie → downstream skippé
        Ok(())
    }
}
```

### Cas d'usage

```rust
// Skip le flush si pas de données dirty
dag.add_node("need_flush", GateNode::new(|| handle.has_uncommitted()));
dag.connect("drain", "done", "need_flush", "trigger")?;
dag.connect("need_flush", "pass", "flush", "trigger")?;
```

## 4. LoopNode — itération conditionnelle (future)

Un noeud qui ré-exécute un sous-DAG jusqu'à ce qu'une condition soit remplie.
Plus complexe — nécessite de supporter les cycles dans le DAG ou un mécanisme
de sub-DAG.

```rust
// Concept
dag.add_loop("retry_merge",
    max_iterations: 3,
    |sub_dag| {
        sub_dag.add_node("attempt", MergeNode::new(...));
        sub_dag.add_node("check", GateNode::new(|| merge_ok()));
    },
    break_on: "check.pass",
);
```

**Complexité** : les DAGs sont acycliques par définition. Le LoopNode
devrait wrapper un sub-DAG qu'il ré-exécute, pas créer un cycle.
Similaire au `PollNode` existant mais au niveau DAG.

**Priorité** : basse. Le retry peut être fait dans le noeud lui-même
via `PollNode`.

## 5. TimeoutNode — wrapper avec deadline (future)

```rust
dag.add_node("search_with_timeout", TimeoutNode::new(
    Duration::from_secs(5),
    SearchNode::new(...),
));
```

**Priorité** : basse. Le timeout peut être géré au niveau du scheduler
(`submit_task` avec deadline) plutôt qu'au niveau du DAG.

## Priorité d'implémentation

| Noeud | Effort | Impact | Priorité |
|-------|--------|--------|----------|
| **SwitchNode** | petit | haut — remplace BranchNode, multi-way routing | 1 |
| **FanOutMerge** | moyen | haut — simplifie tous les DAGs fan-out | 2 |
| **GateNode** | petit | moyen — skip conditionnel propre | 3 |
| LoopNode | gros | moyen — sub-DAG cycles | 4 (future) |
| TimeoutNode | moyen | bas — scheduler peut gérer | 5 (future) |

## Impact sur le search DAG

Avec SwitchNode + FanOutMerge, le search DAG de lucivy deviendrait :

```rust
fn build_search_dag(...) -> Result<Dag, String> {
    let mut dag = Dag::new();

    dag.add_node("drain", DrainNode::new(...));
    dag.add_node("flush", FlushNode::new(...));
    dag.chain(&["drain", "flush"])?;

    // Route: fast path vs prescan path
    dag.add_node("route", SwitchNode::new(
        vec!["fast", "prescan"],
        move || if needs_prescan { 1 } else { 0 },
    ));
    dag.connect("flush", "done", "route", "trigger")?;

    // Prescan path: fan-out + merge
    dag.fan_out_merge("prescan", num_shards,
        |i| Box::new(PrescanShardNode::new(shards[i].clone(), params.clone())),
        "prescan",
        merge_prescan_results,
    )?;
    dag.connect("route", "prescan", "prescan_0", "trigger")?;
    // (fan_out_merge could auto-connect the trigger)

    // Build weight (accepts prescan or direct trigger)
    dag.add_node("build_weight", BuildWeightNode::new(shards, query));
    dag.connect("prescan_merge", "merged", "build_weight", "prescan")?;
    dag.connect("route", "fast", "build_weight", "trigger")?;

    // Search: fan-out + merge
    dag.fan_out_merge("search", num_shards,
        |i| Box::new(SearchShardNode::new(pool.clone(), i, top_k)),
        "hits",
        |hits| merge_top_k(hits, top_k),
    )?;
    dag.connect("build_weight", "weight", "search_0", "weight")?;
    // (broadcast weight to all search nodes)

    Ok(dag)
}
```

De ~50 lignes actuellement à ~25 lignes. Plus lisible, plus proche du diagramme ASCII.
