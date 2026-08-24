# Sharding, distribué et persistance pour l'index sparse — design

Le crate `sparse_vector/` vient d'entrer dans le workspace (`e344b93`). Il a
un `SparseHandle` mono-index : `create/open(_with_store)`, `insert(node_id,
&SparseVector)`, `remove`, `search(query, limit)`, `search_filtered`,
`commit_inner`, `len`. Ce document dit comment lui donner ce que le FTS a —
shards, routage par `node_id`, filtre, deltas, distribué — en réutilisant
ce qui existe, et ce qui dépend (peu) de la réécriture WAND en cours.

## 1. Ce qui ne dépend pas de la réécriture

Le sharding vit **au-dessus** des postings : routage des documents, scatter
des recherches, fusion des top-k, stockage par shard. Il ne voit du moteur
que `search(query, limit, filter) -> Vec<(id, score)>`. Tout ce qui suit
peut être écrit contre l'API actuelle de `SparseHandle` et rebranché sur le
module `wand/` sans changement de forme.

Une différence de fond avec le FTS, qui simplifie tout : **un produit
scalaire est local**. Le score d'un document ne dépend d'aucune statistique
globale (pas d'idf, pas de longueur moyenne). Fusionner N shards, ou N
machines, c'est fusionner N listes `(id, score)` par score décroissant. Le
mécanisme `ExportableStats → merge → search_with_global_stats` du FTS n'a
pas d'équivalent à construire.

## 2. Les briques réutilisées telles quelles

| Besoin | Brique | État |
|---|---|---|
| Stockage par shard (fs, blob, RAM) | `lucistore::shard_storage::ShardStorage` + `FsShardStorage` / `BlobShardStorage` | déjà écrit pour ça : « concrete handle creation (LucivyHandle, **SparseHandle**) is left to the consumer » |
| Cache local d'un shard blob | `lucivy_core::blob_directory::BlobDirectory` (Eager/Lazy, `load_range`, drop propre) | à déplacer dans `lucistore` — il ne dépend de `ld_lucivy` que par le trait `Directory` ; pour le sparse, une version « fichiers plats » suffit (`lucistore::blob_cache::BlobCache` existe déjà : `write_through`, `read_cached`, `materialize`) |
| Routage des documents | `lucivy_core::shard_router::ShardRouter` : `route(tokens)` (round-robin ↔ co-localisation par `balance_weight`), `record_node_id` / `shard_for_node_id` / `remove_node_id`, `to_bytes` / `from_bytes` | à déplacer dans `lucistore` (aucune dépendance FTS : des hachés `u64`), ré-exporté par `lucivy_core` |
| Deltas par shard | `lucistore::delta_sharded` : `ShardVersion`, `ShardedDelta`, `compute_shard_versions` | générique sur des répertoires |
| Snapshot | `lucistore::snapshot::export_snapshot_sharded` / `import_snapshot` | générique sur des fichiers |
| Acteurs, scatter, close inerte | `luciole::Pool` + `scatter` / `drain` / `shutdown` (tolérants depuis `a37d330`) | tel quel |

## 3. `ShardedSparseHandle` — la forme

```
ShardedSparseHandle
  ├── storage: Box<dyn ShardStorage>          (fs / blob / ram)
  ├── shards: Vec<Arc<SparseHandle>>          (un par shard_path)
  ├── router: Mutex<ShardRouter>              (persisté en _sparse_router.bin)
  ├── pool: luciole::Pool<SparseShardMsg>     (Insert / Remove / Search / Commit / Shutdown)
  └── closed: AtomicBool
```

- **`insert(node_id, vector)`** : `router.route(dims_hachées)` choisit le
  shard, `record_node_id(node_id, shard)`, message `Insert` au shard. Pas de
  pipeline readers/tokenizer : un vecteur sparse est déjà tokenisé, l'insert
  est une écriture de postings, le message suffit.
- **Routage** : `balance_weight = 1.0` (round-robin) par défaut, comme le
  FTS. `0.2` co-localise les vecteurs qui partagent leurs dimensions ; en
  WAND ça rend les postings d'une dimension denses sur un shard et vides sur
  les autres, qui terminent tout de suite — un gain pour le fuzzy des
  vecteurs appris (BGE-M3 sparse a des dimensions très inégales). À
  mesurer, pas à supposer.
- **`remove(node_id)`** : `shard_for_node_id` → un seul shard ; sinon
  broadcast (même politique que `delete_by_node_id` du FTS).
- **`search(query, k, filter)`** : `scatter` du même `(query, k, filter)`
  aux N shards, chaque shard rend son top-k local, fusion par k-way sur le
  score, tie-break par id croissant (celui que le module `wand/` doit
  documenter). `search_filtered(allowed_ids)` = le filtre `Fn(id) -> bool`
  déjà supporté par `SparseHandle`, transmis tel quel.
- **`commit()` / `close()` / `drop_index()`** : contrats du FTS,
  y compris « aucun appel au store après close » (le test-sentinelle de
  `test_acid_blob_v3` se transpose au sparse mot pour mot).

Pas de DAG de recherche : sans statistiques globales à construire avant le
scatter, `Pool::scatter` suffit. Si une fusion cross-shard plus fine devient
utile (§5), un DAG à deux niveaux comme `search_dag.rs` s'ajoute alors.

## 4. Persistance : d'abord le cache, ensuite les segments

**Étape A — cache unifié.** `SparseHandle` écrit aujourd'hui trois fichiers
(`sparse.mmap`, `sparse_vectors.bin`, `sparse_dims.bin`) dans un tmpdir et
les pousse au store au commit. Les faire passer par `BlobCache` /
`BlobDirectory` donne l'ouverture lazy, le `load_range`, et la même politique
de cache que le FTS ; pas de fsync à retirer (c'est `fs::write`), pas de
`.managed.json` (pas de segments). Petit, sans risque, indépendant de la
réécriture.

**Étape B — deltas.** Avec trois fichiers réécrits entiers à chaque commit,
un delta LUCIDS vaut « le shard entier s'il a changé » : les shards
inchangés sont sautés (c'est déjà l'intérêt du LUCIDS), mais un shard touché
repart complet. Pour des deltas vraiment incrémentaux il faut des
**générations** : des fichiers de postings immuables ajoutés au commit
(`gen_0007.mmap`), fusionnés par une policy, et une recherche qui ouvre un
curseur par génération et par dimension — c'est exactement ce que la
`Frontier` du module `wand/` sait faire si elle accepte plusieurs curseurs
pour une même dimension. **Ce point-là dépend de la proposition de
l'agent** ; c'est le seul.

## 5. Distribué

Même protocole que le FTS moins les statistiques : un nœud reçoit `(query,
k, filter)`, rend `Vec<(id, score)>`, le coordinateur fusionne. Rien à
sérialiser de plus qu'aujourd'hui.

Optimisation possible plus tard, et c'est là que la `Frontier` paie encore :
partager le **seuil du k-ième score** entre shards pendant une recherche
(chaque shard élague avec le meilleur seuil connu de tous), au lieu que
chacun calcule son top-k complet. Ça demande une recherche par étapes
(`search_step(threshold) -> (candidats, seuil local)`) plutôt qu'un
`search` monolithique — à garder en tête en relisant l'API que l'agent
propose, sans l'exiger de lui maintenant.

## 6. Ordre proposé

1. Déplacer `ShardRouter` (et `BlobDirectory`, ou une variante fichiers
   plats) dans `lucistore` — mécanique, à faire quand on touche lucistore
   pour la publication 2.1.0.
2. Étape A (cache unifié) sur le `SparseHandle` actuel.
3. `ShardedSparseHandle` (§3) avec `FsShardStorage` puis `BlobShardStorage`,
   tests calqués sur `v3_sharded_filter_delete_delta` et `test_acid_blob_v3`
   (dont le sentinelle « rien après close »).
4. Rebranchage sur `wand/` une fois la réécriture acceptée ; puis étape B
   (générations) si les deltas sparse doivent être incrémentaux.

Les points 1-3 peuvent commencer sans attendre l'agent ; 4 est le seul
couplage, et il est à la frontière (§4-B, §5), pas au cœur.
