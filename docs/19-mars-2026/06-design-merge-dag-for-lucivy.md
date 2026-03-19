# Doc 06 — Design : DAG de merge pour lucivy

Date : 19 mars 2026
Basé sur : analyse du DAG rag3weaver + pipeline merge lucivy

## Le problème structurel

Les bugs de cette session (docs 01-05) ont tous la même racine :
les merges, le GC, le save_metas et le reload sont des opérations
concurrentes gérées par des flags et des locks, pas par la structure
d'exécution. Résultat : des races condition impossibles à éliminer
avec des patches ponctuels.

Le scheduler d'acteurs luciole est excellent pour le **travail background**
(indexation, merges async entre les commits). Mais le **commit final**
nécessite un séquencement strict que les acteurs ne garantissent pas.

## Analyse du DAG rag3weaver

Le framework dataflow de rag3weaver (~3800 lignes core) offre :

### Structure (graph.rs — 463 lignes)
- DAG typé : nœuds avec ports d'entrée/sortie typés
- Validation au connect : types compatibles entre ports
- Tri topologique (Kahn) : ordre d'exécution garanti par la structure
- Détection de cycles et deadlocks

### Exécution (runtime.rs — 1433 lignes)
- Itératif : à chaque itération, exécute les nœuds "ready" (inputs disponibles)
- **Séquentiel par itération** (pas parallèle) — checkpoint entre chaque nœud
- Fan-in : merge_port_values() combine les outputs de plusieurs sources
- Fan-out : clone les outputs vers plusieurs destinations
- Events broadcast (async_broadcast, WASM-compatible)

### Observabilité
- **DataflowEvent** : NodeStarted, NodeCompleted (avec métriques + durée), NodeFailed
- **NodeContext** : `log_metric(key, value)`, `debug/info/warn/error(text)`
- **TapRegistry** : capture les données circulant sur des edges spécifiques (zero-cost quand inactif)
- **Checkpoints** : état persisted par nœud (outputs, undo context, timing)

### Points forts pour lucivy
1. **Séquencement par construction** : GC après merges, reload après save_metas
2. **Observabilité intégrée** : métriques par phase, events structurés
3. **Checkpoint / resume** : crash recovery, replay
4. **Error isolation** : si un merge fail, rien n'est commité
5. **Edge taps** : debugger les données entre les phases sans modifier le code

### Points faibles / à adapter
1. **Exécution séquentielle** : les merges pourraient être parallèles (rayon/scheduler)
2. **Async trait** : lucivy est sync (pas de tokio), WASM avec coopérative
3. **PortValue enum** : trop spécifique à RAG (Results, Children, etc.)
4. **Checkpoint via Cypher** : lucivy n'a pas de DB, faudrait du filesystem
5. **BatchPayload type erasure** : complexe, Arc<Mutex<Option<Box<dyn Any>>>>

## Ce qu'on peut extraire et adapter

### Option A : Mini-DAG sync intégré à lucivy (~200 lignes)

Pas de framework générique. Juste un `CommitDAG` codé en dur pour le commit :

```rust
struct CommitDAG;

impl CommitDAG {
    fn execute(writer: &IndexWriter, merge_candidates: Vec<MergeOp>) -> Result<()> {
        // Phase 1: Merges parallèles (rayon ou scheduler existant)
        let results: Vec<MergeResult> = merge_candidates
            .into_par_iter()
            .map(|op| {
                let mut state = MergeState::new(&op)?;
                while state.step() == Continue {} // merge complet, sfx inclus
                state.into_result()
            })
            .collect::<Result<_>>()?;
        // Barrier implicite (par_iter bloque)

        // Phase 2: Update segment manager
        for result in &results {
            segment_manager.end_merge(result.source_ids, result.new_entry);
        }

        // Phase 3: Save metas
        save_metas(opstamp)?;

        // Phase 4: GC (safe, aucun merge en cours)
        garbage_collect_files()?;

        // Phase 5: Reload reader
        reader.reload()?;

        Ok(())
    }
}
```

**Avantages** : simple, pas de dépendance, code linéaire lisible
**Inconvénients** : pas d'observabilité structurée, pas de checkpoint,
pas extensible, hard-codé

### Option B : DAG générique sync extrait de rag3weaver (~500-800 lignes)

Extraire le cœur du DAG (graph + node + runtime) dans une lib séparée,
adaptée sync et multi-threadée :

```
lucivy-dag/
  src/
    graph.rs   — DAG structure, ports typés, topo sort
    node.rs    — trait Node sync, NodeContext avec métriques
    runtime.rs — exécution par niveaux (parallèle intra-niveau)
    port.rs    — PortValue simplifié
    lib.rs
```

**Différences avec rag3weaver** :

| Aspect | rag3weaver | lucivy-dag |
|--------|-----------|------------|
| Threading | async (tokio) | sync, rayon pour parallélisme intra-niveau |
| WASM | async_broadcast | lucivy_trace! + EventBus existant |
| PortValue | 15+ variants (Results, Children...) | 3-4 variants (Segments, MergeResult, Empty) |
| Checkpoint | Cypher DB | filesystem ou in-memory |
| Services | String-keyed DI | Direct references (pas besoin de DI) |
| Exécution | Séquentiel par itération | Parallèle par niveau topologique |

**Le trait Node sync** :
```rust
pub trait Node: Send + Sync {
    fn name(&self) -> &str;
    fn inputs(&self) -> &[PortDef];
    fn outputs(&self) -> &[PortDef];
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String>;

    // Observabilité
    fn node_type(&self) -> &'static str;
}
```

**Exécution par niveaux** (parallélisme structural) :
```
Niveau 0: [PlanMerges]           — séquentiel, 1 nœud
Niveau 1: [Merge_0, Merge_1, Merge_2, Merge_3]  — parallèle (rayon)
Niveau 2: [EndMerge]            — séquentiel, agrège les résultats
Niveau 3: [SaveMetas]           — séquentiel
Niveau 4: [GC]                  — séquentiel
Niveau 5: [Reload]              — séquentiel
```

Les nœuds d'un même niveau sont indépendants → exécutés en parallèle.
Les niveaux sont exécutés séquentiellement → séquencement garanti.

**Avantages** :
- Parallélisme des merges par construction (pas besoin de drain)
- Séquencement GC/save/reload garanti par la structure
- Extensible : ajouter un nœud = ajouter un nœud au DAG
- Observabilité : métriques par nœud, events structurés
- Réutilisable hors du contexte merge (ingestion pipeline, search pipeline)

**Inconvénients** :
- Plus de code initial (~500-800 lignes)
- Nouveau crate à maintenir

### Option C : Utiliser le DAG rag3weaver tel quel via dépendance

Ajouter rag3weaver comme dépendance de lucivy-core et utiliser son
runtime directement.

**Avantages** : zéro code de framework à écrire, battle-tested
**Inconvénients** :
- Dépendance lourde (tokio, async_broadcast, cypher checkpoint)
- Contraint lucivy à async (incompatible WASM coopératif)
- Couplage rag3weaver ↔ lucivy (versions, breaking changes)
- PortValue trop RAG-spécifique

**Verdict** : à éviter. Le code est inspirant mais pas réutilisable directement.

## Recommandation : Option B

L'option B offre le meilleur rapport puissance/simplicité :
- Le parallélisme par niveaux résout le merge bottleneck
- Le séquencement structural résout les races GC/reload
- L'observabilité structurée remplace les eprintln de debug
- L'extensibilité permet l'évolution future (deferred sfx, sparse merge, etc.)

## Design détaillé Option B

### PortValue pour lucivy

```rust
pub enum PortValue {
    /// Liste de segment IDs (input du nœud merge)
    SegmentIds(Vec<SegmentId>),
    /// Résultat d'un merge : segment entry + source IDs supprimés
    MergeResult(MergeResultData),
    /// Liste de résultats agrégés (output de EndMerge)
    MergeResults(Vec<MergeResultData>),
    /// Signal de complétion (trigger)
    Done,
}

pub struct MergeResultData {
    pub source_ids: Vec<SegmentId>,
    pub new_entry: Option<SegmentEntry>,
    pub duration_ms: u64,
    pub docs_merged: u32,
    pub sfx_terms: u32,
}
```

### Nœuds du CommitDAG

```
PlanMergesNode
  inputs: [] (lit les candidates depuis le SegmentManager)
  outputs: [("merge_ops", SegmentIds)] × N (fan-out dynamique)
  action: collect_merge_candidates(), crée N merge ops

MergeNode (un par merge)
  inputs: [("segments", SegmentIds)]
  outputs: [("result", MergeResult)]
  action: MergeState::new + step() loop complet (InvIndex + SFX + Close)

EndMergeNode
  inputs: [("results", MergeResults)] (fan-in de tous les MergeNodes)
  outputs: [("done", Done)]
  action: segment_manager.end_merge() pour chaque résultat

SaveMetasNode
  inputs: [("done", Done)]
  outputs: [("done", Done)]
  action: save_metas(opstamp, payload)

GCNode
  inputs: [("done", Done)]
  outputs: [("done", Done)]
  action: garbage_collect_files()

ReloadNode
  inputs: [("done", Done)]
  outputs: []
  action: reader.reload()
```

### Observabilité

Chaque nœud émet via `NodeContext` :

```rust
// Dans MergeNode::execute :
ctx.log_metric("docs_merged", 1250);
ctx.log_metric("sfx_terms", 8422);
ctx.log_metric("postings_ms", 450.0);
ctx.log_metric("sfx_ms", 1200.0);
ctx.log_metric("total_ms", 1800.0);
```

Le runtime émet des events structurés :

```rust
pub enum DagEvent {
    NodeStarted { node: String, node_type: &'static str },
    NodeCompleted {
        node: String,
        node_type: &'static str,
        duration_ms: u64,
        metrics: HashMap<String, f64>,
    },
    NodeFailed { node: String, error: String },
    LevelStarted { level: usize, nodes: Vec<String> },
    LevelCompleted { level: usize, duration_ms: u64 },
    DagCompleted { total_ms: u64 },
    DagFailed { error: String },
}
```

Remplace tous les `eprintln!("[merge]...")` et `lucivy_trace!` par des
events structurés consommables par le bench, le monitoring, ou les tests.

### Intégration avec le scheduler existant

Le DAG n'intervient que dans `commit()`. Les acteurs background ne changent pas :

```
commit_fast() :
  → flush indexers (via acteurs)
  → save_metas
  → PAS de DAG, PAS de drain, PAS de GC
  → Les merges async continuent en background

commit() :
  → flush indexers (via acteurs)
  → STOP les merges async (cancel pending, wait active)
  → CommitDAG::execute(merge_candidates)
      └─ PlanMerges → Merge[0..N] (parallèle) → EndMerge → SaveMetas → GC → Reload
  → RESTART les merges async
```

### Edge Taps pour le diagnostique

Inspiré de rag3weaver `observe.rs` — zero-cost quand inactif :

```rust
// Avant le commit :
let tap = dag.tap("merge_0", "result");  // capture le MergeResult

dag.execute()?;

// Après :
if let Some(event) = tap.try_recv() {
    eprintln!("Merge 0 produced: {} docs, {} sfx terms",
        event.value.docs_merged, event.value.sfx_terms);
}
```

Utilisable dans le bench post-mortem sans modifier le code du merge.

### Crash recovery (futur)

Le checkpoint filesystem permet de reprendre un commit interrompu :

```
lucivy_indexes/table_name/.commit_checkpoint/
  execution.json  — status + graph hash
  merge_0.json    — completed, output: MergeResult
  merge_1.json    — completed, output: MergeResult
  merge_2.json    — pending (crash ici)
```

Au restart : reprendre merge_2, skip merge_0 et merge_1, continuer
EndMerge → SaveMetas → GC → Reload.

## Plan d'implémentation

### Phase 1 : crate lucivy-dag (~300 lignes)
- `graph.rs` : Node trait sync, PortDef, edges, topo sort
- `runtime.rs` : exécution par niveaux, rayon pour parallélisme
- `port.rs` : PortValue minimal (SegmentIds, MergeResult, Done)
- Tests unitaires

### Phase 2 : CommitDAG dans lucivy-core (~200 lignes)
- Nœuds : PlanMerges, Merge, EndMerge, SaveMetas, GC, Reload
- Intégration dans `handle_commit(rebuild_sfx=true)`
- Le drain_all_merges est remplacé par le DAG

### Phase 3 : Observabilité (~100 lignes)
- DagEvent enum + broadcast
- Métriques par nœud dans MergeNode
- Intégration bench post-mortem

### Phase 4 : Edge taps + diagnostics (~100 lignes)
- TapRegistry zero-cost
- Intégration diagnostics.rs

### Phase 5 (futur) : Crash recovery
- CheckpointStore filesystem
- Resume logic dans le runtime

## Fichiers rag3weaver à consulter pour l'implémentation

| Fichier | Ce qu'on en tire |
|---------|-----------------|
| `dataflow/graph.rs` | Structure DAG + topo sort (adapter sync) |
| `dataflow/node.rs` | Trait Node + NodeContext (simplifier, drop async) |
| `dataflow/runtime.rs` | Boucle d'exécution (adapter par niveaux) |
| `dataflow/port.rs` | merge_port_values pour fan-in (simplifier) |
| `dataflow/observe.rs` | TapRegistry zero-cost (copier presque tel quel) |
| `events.rs` | EventBus pattern (déjà dans luciole, étendre) |
