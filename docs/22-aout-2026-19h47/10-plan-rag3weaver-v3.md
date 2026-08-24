# rag3weaver → lucivy v3 en Rust direct (ShardedHandle + ACID)

23 août 2026. État des lieux fait sur `rag3db/extension/rag3weaver` (agent, faits
vérifiés fichier:ligne) + validation côté lucivy le jour même.

## Ce qui est prêt côté lucivy

- `ShardedHandle` est le chemin le plus validé du moteur : panels spans-exactes
  pour `search`, `search_filtered(allowed_ids)`, `delete_by_node_id`, deltas
  LUCIDS, distribué 2 nœuds, fuzzy/regex multi-shards, `close()` (draine les
  merges), `query_warnings`. Voir 07 §G/H et les tests `v3_distributed_*`,
  `v3_sharded_*`.
- **ACID v3 validé** (`test_acid_blob_v3.rs`) : `BlobShardStorage::new(store,
  name, cache_base)` + `create_with_storage`/`open_with_storage` ; blobs =
  source de vérité, cache local mmap (`NativeDirectory` = `MmapDirectory`),
  réouverture depuis les blobs seuls, tous modes exacts, écriture continue.
- Le trait attendu est `lucivy_core::blob_store::BlobStore`
  (save/load/delete/exists/list) — exactement ce que `CypherBlobStore` et
  `PostgresBlobStore` de rag3weaver implémentent déjà (ils ne servent
  aujourd'hui qu'au sparse).

## rag3weaver aujourd'hui

Tout le FTS passe par l'extension C++ (`CALL CREATE/QUERY/FLUSH/CLOSE/
DROP_LUCIVY_INDEX`), zéro handle Rust (TODO de migration déjà noté
`catalog.rs:680`, plan `docs/15-mars-2026-00h00/02`). Les inserts lucivy sont
implicites dans les hooks C++ `NodeTable::insert` — avec un bug vivant :
`._ngram`/`._raw` non peuplés, mode `Contains` cassé. Les ids sont des
**offsets u64** (NodeIdCache), le pattern sparse (`handle.insert(offset, …)`)
est le modèle à suivre.

## Mapping opération par opération

| rag3weaver (Cypher) | ShardedHandle |
|---|---|
| `CREATE_LUCIVY_INDEX(table, fields, filter_fields)` | `create_with_storage(BlobShardStorage::new(store, table, cache), &SchemaConfig{fields, sfx_version: 3, shards})` |
| insert (hook C++) | `add_document_json(offset, &fields)` (champs par nom, types vérifiés, erreurs explicites) ou `add_document(doc, offset)` — `_node_id` estampillé automatiquement depuis le 24 août, id contradictoire refusé |
| delete (implicite) | `delete_by_node_id(offset)` |
| `FLUSH_LUCIVY_INDEX` | `commit()` (idempotent, merges policy auto) |
| `CLOSE_LUCIVY_INDEX` | `close()` |
| `DROP_LUCIVY_INDEX` | manque — faire `store.list(prefix)` + `delete` (ou exposer une API) |
| `QUERY_LUCIVY_INDEX(json, limit, allowed_ids)` | `search_filtered(&QueryConfig, limit, sink, allowed_ids)` — même JSON (`serde` QueryConfig), tous les modes BM25Mode couverts par le compat layer |
| highlights JSON `{"field":[[a,b]]}` | `search_with_docs` → `SearchHit.highlights: HashMap<String, Vec<[usize;2]>>` (1:1) |
| `filter_fields` | `QueryConfig.filters` (FilterClause) — parité à vérifier au branchement |

`Consistency::Strict/Eventual` : `commit()` couvre flush + reload ; pour un
état totalement fusionné avant snapshot, drainer (`writer.drain_merges()` par
shard, cf. tests).

## Les manques, à traiter côté lucivy

1. **Chargement paresseux — FAIT le 24 août, optionnel.**
   `BlobShardStorage::new(...).with_load_mode(BlobLoadMode::Lazy)` : rien à
   l'ouverture sauf `meta.json`/`.managed.json`/`_config.json` (3,6 Ko mesurés
   sur un index de 104 Ko) ; les sondes de footer sont servies par plage
   depuis le store (`load_range`, ≤ 64 Ko) ; à la 4e lecture distante d'un
   fichier, il est matérialisé en cache mmap. Le défaut reste **Eager**
   (latence d'ouverture prévisible, premier search gratuit) — à benchmarker
   par cas. Prérequis côté store pour le plein effet : implémenter
   `blob_len` et `load_range` (défauts rétro-compatibles : `None` = repli
   sur téléchargement complet au premier accès).
2. **Publication crates.io** : le pin `lucivy-core = "2.0.0"` de rag3weaver est
   un instantané pré-v3 ; le workspace local est aussi en 2.0.0 (non publié
   depuis). Bump 2.1.0 + publish (avec `ld-lucivy`, `luciole`, `lucistore`), ou
   dépendance par chemin/git en attendant.
3. API « drop index » (suppression des blobs d'un index) à exposer si on ne
   veut pas la logique préfixe côté rag3weaver.
4. WASM : rag3weaver compile déjà son Rust en wasm ; utiliser `lucivy_core`
   directement dans ce build (StdFsDirectory/OPFS, règles WASM du CLAUDE.md)
   au lieu du pont C emscripten — plus simple que le binding C.

## Ordre proposé

1. Brancher un `LucivyHandle`/`ShardedHandle` map dans `Catalog` à côté de
   `sparse_handles`, sur `BlobShardStorage` + `CypherBlobStore` existant.
2. Écrire par offsets au fil des inserts Rust (comme sparse), `commit()` dans
   `FlushNode` à la place de `FLUSH_LUCIVY_INDEX`.
3. Recherche : remplacer `QUERY_LUCIVY_INDEX` par `search_filtered` +
   highlights du sink ; garder le JSON QueryConfig tel quel.
4. Supprimer les hooks C++ (et leur bug `_ngram`) une fois la parité mesurée.
5. Ensuite seulement : chargement paresseux par fichier côté lucivy si le
   volume d'index par catalogue le justifie.
