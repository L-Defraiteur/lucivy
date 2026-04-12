# Plan — Handle unifié (ShardedHandle everywhere)

## Vision

Un seul type de handle pour tous les bindings : `ShardedHandle`. Même avec
1 shard, on passe par `ShardedHandle`. Le DAG de recherche route
automatiquement vers un chemin simplifié (sans merge) quand `num_shards == 1`.

Pas de `if` externe, c'est le DAG qui décide.

## Pourquoi

- **Maintenance** : deux chemins de recherche/ingestion (LucivyHandle vs
  ShardedHandle) = double travail. Chaque feature doit être implémentée
  deux fois (ex: `search_filtered` manque sur ShardedHandle).
- **Bindings** : les bindings n'ont pas à savoir s'ils manipulent un index
  single ou shardé. Un seul type, une seule API.
- **Snapshot** : `import_snapshot` retourne toujours un `ShardedHandle`,
  qu'il soit shardé ou non. Pas de `import_index` vs `import_sharded`.

## État actuel

### LucivyHandle — API publique

| Méthode | Description |
|---------|-------------|
| `create(dir, config)` | Crée un index |
| `open(dir)` | Ouvre un index existant |
| `search(config, top_k, sink)` | Prescan séquentiel → build_weight → collect |
| `search_filtered(config, top_k, node_ids, sink)` | Idem + FilterCollector |
| `close()` | Release writer lock |
| `has_uncommitted()` | Check dirty flag |
| `field(name)` | Lookup champ par nom |

Accès directs : `reader`, `index`, `schema`, `config`, `field_map`.

Chemin search : `build_query → prescan segments (boucle) → inject cache →
build_weight → collect_top_docs (boucle segments)`. Séquentiel, pas de DAG.

### ShardedHandle — API publique

| Méthode | Description |
|---------|-------------|
| `create(path, config)` / `create_with_storage(storage, config)` | Crée N shards |
| `open(path)` / `open_with_storage(storage)` | Ouvre N shards existants |
| `search(config, top_k, sink)` | DAG : prescan ∥ → merge → weight → search ∥ → merge |
| `search_with_global_stats(config, top_k, stats, sink)` | Mode distribué |
| `search_with_docs(config, top_k, sink)` | Convenience : résout docs + highlights |
| `export_stats(config)` | Export BM25 stats pour agrégation externe |
| `add_document(doc, node_id)` | Insert routé |
| `add_documents(docs)` | Batch insert |
| `add_document_with_hashes(doc, node_id, hashes)` | Insert pré-hashé |
| `commit()` / `commit_fast()` | Commit explicite |
| `close()` | Shutdown coordonné |
| `delete_by_node_id(node_id)` | Delete routé |
| `num_shards()`, `num_docs()`, `shard(i)`, `index()`, `router_stats()` | Observabilité |

**Manquant** : `search_filtered()`.

### Overhead ShardedHandle avec 1 shard

- 3-4 actors (ShardActor, ReaderActors, RouterActor) + mailboxes
- ShardRouter (HashMap node_id → shard_id, inutile pour 1 shard)
- StreamDag pipeline (drain coordination)
- DAG search : 7+ nodes construits et exécutés même pour fan-out trivial

Estimé ~10-30% overhead search, ~5-20% ingestion vs LucivyHandle direct.

## Plan : DAG conditionnel single vs multi shard

### Principe

Un seul DAG construit avec **tous les chemins**. Un `BranchNode("is_multi")`
route au runtime vers le chemin avec ou sans merge. Les nodes du chemin
inactif sont **skip gratuitement** par luciole (trigger input non satisfait
→ `continue`, coût ~0).

### Mécanisme luciole

- `BranchNode(|| condition)` = `SwitchNode` 2-way ("then" / "else")
- Nodes downstream d'une branche inactive : trigger required non satisfait
  → skippés automatiquement par le runtime (pas d'exécution, pas de thread)
- Deux edges vers le même input port : seul celui dont la source a produit
  des données est collecté. L'autre (source skippée) est ignoré.

Vérifié dans `runtime.rs:collect_inputs()` : itère les edges, collecte
celles dont la clé `(from_node, from_port)` existe dans `port_data`.

### DAG de recherche unifié

```
drain → flush → needs_prescan?
                 ├── then → is_multi?
                 │           ├── then → [prescan_0..N ∥] → merge_prescan ──→ build_weight
                 │           └── else → prescan_0 ─────────────────────────→ build_weight
                 └── else ─────────────────────────────────────────────────→ build_weight

build_weight → is_multi?
                ├── then → [search_0..N ∥] → merge_results → output
                └── else → search_0 ──────────────────────→ output
```

**Nodes partagés** : `drain`, `flush`, `build_weight` sont les mêmes
instances, utilisés quel que soit le chemin.

**Nodes conditionnels** :
- `merge_prescan` : présent dans le DAG, skippé si single shard
- `merge_results` : présent dans le DAG, skippé si single shard
- `prescan_1..N` : présents dans le DAG, skippés si single shard
- `search_1..N` : présents dans le DAG, skippés si single shard

**Node `output`** : convergence finale. Reçoit de `merge_results` OU de
`search_0` selon la branche active. Forward le résultat.

### Changements sur les nodes

1. **SearchShardNode** : sort `Vec<ShardedSearchResult>` au lieu de
   `Vec<(usize, f32, DocAddress)>`. La conversion shard_id → struct se fait
   dans le node, pas dans le merge.

2. **MergeResultsNode** : reçoit `Vec<ShardedSearchResult>`, fait juste le
   heap merge. Plus de conversion de type.

3. **OutputNode** (nouveau) : node trivial de convergence. Reçoit des
   résultats de la branche active, forward.

4. **Extraction** : `ShardedHandle::search()` fait
   `result.take_output("output", "results")` — un seul point d'extraction
   quel que soit le chemin.

### DAG de recherche filtrée (search_filtered)

Même structure, mais `SearchShardNode` utilise un `FilterCollector` au lieu
de `TopDocs`. Le filtre `allowed_node_ids` est injecté dans le node.

```
drain → flush → needs_prescan? → ... → build_weight → is_multi?
                                                        ├── then → [search_filtered_0..N ∥] → merge → output
                                                        └── else → search_filtered_0 ────────────── → output
```

Ajouter `search_filtered()` sur `ShardedHandle` avec un
`FilteredSearchShardNode` ou un flag sur `SearchShardNode`.

### DAG d'ingestion

Même principe. Actuellement l'ingestion passe par le `StreamDag` pipeline
(readers → router → shard actors). Pour 1 shard :

- Le router est trivial (toujours shard_0)
- Les readers tokenisent puis forward direct

Le `StreamDag` d'ingestion gère déjà N=1 naturellement (1 reader, 1 shard
actor, pas de routing decision). L'overhead est le pipeline lui-même
(messages, mailboxes). Un shortcut possible : pour 1 shard, le `StreamDag`
skip le router et connecte directement reader → shard_actor.

Alternative : `BranchNode` dans le pipeline qui skip le router pour
single shard. Ou simplement accepter l'overhead pipeline pour 1 shard
(messages passent par le router qui route toujours vers shard_0 — coût
~1μs par doc, négligeable).

**Recommandation** : garder le pipeline inchangé pour l'ingestion. L'overhead
router pour 1 shard est négligeable. Optimiser seulement le search DAG.

### DAG de commit

Actuellement : drain pipeline → scatter commit à tous les shards → reload
readers → resync router → persist stats.

Pour 1 shard : pas besoin de scatter, commit direct. Mais le scatter avec
N=1 est déjà trivial (1 message). Pas d'optimisation nécessaire.

## Migration des bindings

### Étape 1 : ajouter `search_filtered` sur ShardedHandle

Pré-requis pour que tous les bindings puissent migrer.

### Étape 2 : DAG conditionnel dans `build_search_dag`

Modifier `search_dag.rs` pour construire le DAG avec les deux chemins
(single et multi) et les `BranchNode`. Un seul `build_search_dag`,
un seul chemin d'extraction.

### Étape 3 : migrer les bindings un par un

Chaque binding remplace `LucivyHandle` par `ShardedHandle` :

| Binding | Handle actuel | Migration |
|---------|--------------|-----------|
| CXX bridge rag3db | LucivyHandle | ShardedHandle (search_filtered requis) |
| Emscripten | LucivyHandle | ShardedHandle (prioritaire pour playground) |
| wasm-bindgen | LucivyHandle | ShardedHandle |
| Node.js | LucivyHandle | ShardedHandle |
| Python | LucivyHandle + ShardedIndex | ShardedHandle unifié |
| C++ standalone | LucivyHandle | ShardedHandle |

Pour chaque binding :
- Remplacer `LucivyHandle` par `ShardedHandle` (config `shards: Some(1)`)
- Supprimer les helpers dupliqués (`execute_top_docs`, etc.)
- Adapter les accès (`handle.reader` → `handle.shard(0).reader`, etc.)

### Étape 4 : snapshot unifié

`import_snapshot` retourne toujours un `ShardedHandle` :
- Non-shardé → `ShardedHandle` avec 1 shard
- Shardé → `ShardedHandle` avec N shards

Plus de `import_index` vs `import_sharded` dans les bindings.

### Étape 5 : cleanup

- Supprimer `LucivyHandle::search()` et `search_filtered()` (le chemin
  séquentiel est remplacé par le DAG conditionnel)
- `LucivyHandle` reste comme wrapper bas-niveau d'un shard (reader, writer,
  index) mais sans API de recherche publique
- Supprimer les re-exports dupliqués

## Risques

- **Overhead DAG pour 1 shard** : construction du DAG (allocations nodes +
  edges) même si la plupart sont skip. Estimé ~10-50μs, négligeable vs
  le prescan/search (~1-300ms).
- **Régression perf** : le chemin single shard passe maintenant par
  `shard_pool.worker(0).request()` au lieu d'appels directs. Ajout d'un
  message actor (~1-5μs). Mesurer avant/après.
- **Complexité DAG** : le DAG a plus de nodes (chemins inactifs). La
  lisibilité est bonne grâce au skip explicite (logs "skipped" dans les
  events). Mais debugging plus verbeux.

## Ordre de priorité

1. `search_filtered` sur ShardedHandle
2. DAG conditionnel (search_dag.rs)
3. Binding emscripten → ShardedHandle (playground)
4. Snapshot unifié (LUCE v2)
5. Autres bindings
6. Cleanup LucivyHandle
